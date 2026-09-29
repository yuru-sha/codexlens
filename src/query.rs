use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::types::ValueRef;

use anyhow::{Result, bail};

use super::{CLI_VIEW_ROW_LIMIT, OutputFormat, bounded_text};

pub(super) const QUERY_SQL_MAX_BYTES: usize = 64 * 1024;
const QUERY_COLUMN_LIMIT: usize = 50;

pub(super) struct Query<'a> {
    sql: &'a str,
}

impl<'a> Query<'a> {
    pub(super) fn new(sql: &'a str) -> Result<Self> {
        validate_query_input(sql)?;
        Ok(Self { sql })
    }

    pub(super) fn execute(self, connection: &rusqlite::Connection) -> Result<QueryResult> {
        connection.authorizer(Some(read_only_sql_authorizer));
        execute_query(connection, self.sql)
    }
}

pub(super) struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    omitted_columns: usize,
    omitted_rows: usize,
}

impl QueryResult {
    pub(super) fn render(self, format: OutputFormat, command: &str) -> Result<String> {
        match format {
            OutputFormat::Table => Ok(render_query_table(&self, command)),
            OutputFormat::Markdown => Ok(render_query_markdown(&self, command)),
            OutputFormat::Json => {
                let document = serde_json::json!({
                    "schema_version": 1,
                    "command": command,
                    "data": {
                        "columns": self.columns,
                        "rows": self.rows,
                        "omitted_column_count": self.omitted_columns,
                        "omitted_count": self.omitted_rows,
                    },
                });
                let mut output = serde_json::to_string_pretty(&document)?;
                output.push('\n');
                Ok(output)
            }
        }
    }
}

fn validate_query_input(sql: &str) -> Result<()> {
    if sql.is_empty() {
        bail!("query requires SQL as an argument or on stdin");
    }
    if sql.len() > QUERY_SQL_MAX_BYTES {
        bail!("query exceeds the {QUERY_SQL_MAX_BYTES}-byte limit");
    }
    if sql.as_bytes().contains(&0) {
        bail!("query contains an unsupported NUL byte");
    }
    Ok(())
}

fn execute_query(connection: &rusqlite::Connection, sql: &str) -> Result<QueryResult> {
    let mut statement = connection.prepare(sql).map_err(|error| {
        if error.sqlite_error_code() == Some(rusqlite::ErrorCode::AuthorizationForStatementDenied) {
            anyhow::anyhow!("query must be a single read-only SQL statement")
        } else {
            anyhow::anyhow!("query must contain one valid SQL statement")
        }
    })?;
    if !statement.readonly() || is_pragma_assignment(sql) {
        bail!("query must be a single read-only SQL statement");
    }

    let total_columns = statement.column_count();
    let columns = statement
        .column_names()
        .into_iter()
        .take(QUERY_COLUMN_LIMIT)
        .map(bounded_text)
        .collect::<Vec<_>>();
    let omitted_columns = total_columns.saturating_sub(columns.len());
    let mut rows = statement
        .query([])
        .map_err(|_| anyhow::anyhow!("query could not start"))?;
    let mut result_rows = Vec::new();
    let mut omitted_rows = 0;
    while let Some(row) = rows
        .next()
        .map_err(|_| anyhow::anyhow!("query could not read result rows"))?
    {
        if result_rows.len() >= CLI_VIEW_ROW_LIMIT {
            omitted_rows += 1;
            continue;
        }
        let mut result_row = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            let value = row
                .get_ref(index)
                .map_err(|_| anyhow::anyhow!("query returned an unreadable value"))?;
            result_row.push(query_value(value));
        }
        result_rows.push(result_row);
    }
    Ok(QueryResult {
        columns,
        rows: result_rows,
        omitted_columns,
        omitted_rows,
    })
}

fn read_only_sql_authorizer(context: rusqlite::hooks::AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Read { .. } | AuthAction::Select | AuthAction::Recursive => {
            Authorization::Allow
        }
        AuthAction::Function { function_name }
            if !function_name.eq_ignore_ascii_case("load_extension") =>
        {
            Authorization::Allow
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if pragma_value.is_none() && read_only_pragma_without_argument(pragma_name) => {
            Authorization::Allow
        }
        AuthAction::Pragma { pragma_name, .. }
            if READ_ONLY_PRAGMA_ARGUMENTS
                .iter()
                .any(|candidate| pragma_name.eq_ignore_ascii_case(candidate)) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }
}

fn read_only_pragma_without_argument(name: &str) -> bool {
    [
        "application_id",
        "compile_options",
        "data_version",
        "encoding",
        "foreign_keys",
        "freelist_count",
        "page_count",
        "page_size",
        "recursive_triggers",
        "schema_version",
        "user_version",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn query_value(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(value) => serde_json::json!(value),
        ValueRef::Real(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ValueRef::Text(value) => {
            serde_json::Value::String(bounded_text(&String::from_utf8_lossy(value)))
        }
        ValueRef::Blob(value) => serde_json::Value::String(format!("<blob {} bytes>", value.len())),
    }
}

fn query_human_value(value: &serde_json::Value) -> String {
    let value = match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::String(value) => value.clone(),
        value => value.to_string(),
    };
    value.replace(['\r', '\n'], " ")
}

fn render_query_table(result: &QueryResult, command: &str) -> String {
    let mut output = format!("{}\n", command.to_ascii_uppercase());
    if result.columns.is_empty() {
        output.push_str("No columns.\n");
        return output;
    }
    output.push_str("Columns: ");
    output.push_str(&result.columns.join(" | "));
    output.push('\n');
    for row in &result.rows {
        output.push_str("- ");
        output.push_str(
            &row.iter()
                .map(query_human_value)
                .collect::<Vec<_>>()
                .join(" | "),
        );
        output.push('\n');
    }
    output.push_str(&format!(
        "Rows: {}\nOmitted rows: {}\nOmitted columns: {}\n",
        result.rows.len(),
        result.omitted_rows,
        result.omitted_columns
    ));
    output
}

fn markdown_cell(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

fn render_query_markdown(result: &QueryResult, command: &str) -> String {
    let mut output = format!("# {}\n\n", command.to_ascii_uppercase());
    if result.columns.is_empty() {
        output.push_str("No columns.\n");
        return output;
    }
    output.push('|');
    for column in &result.columns {
        output.push(' ');
        output.push_str(&markdown_cell(column));
        output.push_str(" |");
    }
    output.push('\n');
    output.push('|');
    for _ in &result.columns {
        output.push_str(" --- |");
    }
    output.push('\n');
    for row in &result.rows {
        output.push('|');
        for value in row {
            output.push(' ');
            output.push_str(&markdown_cell(&query_human_value(value)));
            output.push_str(" |");
        }
        output.push('\n');
    }
    output.push_str(&format!(
        "\nRows: {}\nOmitted rows: {}\nOmitted columns: {}\n",
        result.rows.len(),
        result.omitted_rows,
        result.omitted_columns
    ));
    output
}

const READ_ONLY_PRAGMA_ARGUMENTS: &[&str] = &[
    "foreign_key_check",
    "foreign_key_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "quick_check",
    "table_list",
    "table_info",
    "table_xinfo",
];

fn is_pragma_assignment(sql: &str) -> bool {
    let Some(mut rest) = after_sql_keyword(sql, "pragma") else {
        return false;
    };
    rest = skip_sql_space_and_comments(rest);
    let Some((mut after_name, _)) = take_sql_identifier(rest) else {
        return true;
    };
    after_name = skip_sql_space_and_comments(after_name);
    if let Some(after_schema) = after_name.strip_prefix('.') {
        let Some((qualified_rest, _)) =
            take_sql_identifier(skip_sql_space_and_comments(after_schema))
        else {
            return true;
        };
        after_name = qualified_rest;
    }
    skip_sql_space_and_comments(after_name).starts_with('=')
}

fn after_sql_keyword<'a>(sql: &'a str, keyword: &str) -> Option<&'a str> {
    let sql = skip_sql_space_and_comments(sql);
    let prefix = sql.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    if sql
        .get(keyword.len()..)
        .and_then(|tail| tail.chars().next())
        .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return None;
    }
    sql.get(keyword.len()..)
}

fn skip_sql_space_and_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(comment) = sql.strip_prefix("--") {
            let Some(end) = comment.find('\n') else {
                return "";
            };
            sql = &comment[end + 1..];
            continue;
        }
        if let Some(comment) = sql.strip_prefix("/*") {
            let Some(end) = comment.find("*/") else {
                return "";
            };
            sql = &comment[end + 2..];
            continue;
        }
        return sql;
    }
}

fn take_sql_identifier(sql: &str) -> Option<(&str, &str)> {
    let end = sql
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(sql.len());
    (end > 0).then(|| (&sql[end..], &sql[..end]))
}
