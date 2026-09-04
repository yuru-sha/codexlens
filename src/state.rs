use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

use crate::model::{DiagnosticKind, Session, SourceRef, merge_session_fields};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDiagnosticKind {
    Unreadable,
    SchemaMismatch,
    Query,
    MetadataConflict,
}

impl StateDiagnosticKind {
    pub(crate) fn canonical_kind(self) -> DiagnosticKind {
        match self {
            Self::Unreadable => DiagnosticKind::Unreadable,
            Self::SchemaMismatch => DiagnosticKind::StateSchemaMismatch,
            Self::Query => DiagnosticKind::StateQuery,
            Self::MetadataConflict => DiagnosticKind::MetadataConflict,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDiagnostic {
    pub source: SourceRef,
    pub kind: StateDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateReadResult {
    pub sessions: Vec<Session>,
    pub diagnostics: Vec<StateDiagnostic>,
}

pub type StateMetadata = StateReadResult;

const FIELD_ALIASES: &[(&str, &[&str])] = &[
    ("id", &["id", "thread_id", "session_id"]),
    ("rollout_path", &["rollout_path", "rollout_file", "rollout"]),
    (
        "created_at",
        &["created_at", "created", "created_timestamp", "created_time"],
    ),
    (
        "updated_at",
        &["updated_at", "updated", "updated_timestamp", "updated_time"],
    ),
    ("cwd", &["cwd", "working_directory"]),
    (
        "project",
        &[
            "project",
            "project_path",
            "project_root",
            "project_name",
            "git_repo_root",
            "git_repository_root",
            "workspace",
            "workspace_root",
        ],
    ),
    ("model", &["model"]),
    ("provider", &["model_provider", "provider"]),
    ("source", &["source"]),
    ("thread_source", &["thread_source"]),
    (
        "archive_state",
        &["archived", "is_archived", "archive_state"],
    ),
    ("title", &["title"]),
    ("preview", &["preview", "first_user_message"]),
    ("parent_id", &["parent_thread_id", "parent_id"]),
    ("cli_version", &["cli_version", "version"]),
    ("originator", &["originator"]),
    ("history_mode", &["history_mode"]),
    ("reasoning_effort", &["reasoning_effort"]),
];

pub fn read_state_database(path: &Path) -> StateReadResult {
    let source = SourceRef::state(path.to_path_buf());
    let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(error) => {
            return StateReadResult {
                sessions: Vec::new(),
                diagnostics: vec![StateDiagnostic {
                    source,
                    kind: StateDiagnosticKind::Unreadable,
                    message: bounded_message(&error.to_string()),
                }],
            };
        }
    };

    normalize_state_result(read_connection(path, &connection))
}

pub fn read_state(path: &Path) -> StateReadResult {
    read_state_database(path)
}

pub fn read_state_databases<I, P>(paths: I) -> StateReadResult
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    merge_state_results(
        paths
            .into_iter()
            .map(|path| read_state_database(path.as_ref())),
    )
}

pub(crate) fn merge_state_results<I>(results: I) -> StateReadResult
where
    I: IntoIterator<Item = StateReadResult>,
{
    let mut result = StateReadResult::default();
    for read in results {
        result.sessions.extend(read.sessions);
        result.diagnostics.extend(read.diagnostics);
    }
    normalize_state_result(result)
}

fn normalize_state_result(mut result: StateReadResult) -> StateReadResult {
    let mut sessions = BTreeMap::new();
    for session in std::mem::take(&mut result.sessions) {
        if let Some(existing) = sessions.get_mut(&session.id) {
            merge_state_session(existing, &session, &mut result.diagnostics);
        } else {
            sessions.insert(session.id.clone(), session);
        }
    }
    result.sessions = sessions.into_values().collect();
    result
        .sessions
        .sort_by(|left, right| left.id.cmp(&right.id));
    result.diagnostics.sort_by(|left, right| {
        left.source
            .path
            .cmp(&right.source.path)
            .then_with(|| left.source.line.cmp(&right.source.line))
            .then_with(|| left.message.cmp(&right.message))
    });
    result
}

fn merge_state_session(
    target: &mut Session,
    incoming: &Session,
    diagnostics: &mut Vec<StateDiagnostic>,
) {
    for conflict in merge_session_fields(target, incoming) {
        diagnostics.push(StateDiagnostic {
            source: incoming.provenance.clone(),
            kind: StateDiagnosticKind::MetadataConflict,
            message: format!(
                "state metadata conflict for {}: {} vs {}",
                conflict.field, conflict.existing, conflict.incoming
            ),
        });
    }
}

fn read_connection(path: &Path, connection: &Connection) -> StateReadResult {
    let source = SourceRef::state(path.to_path_buf());
    let table = match find_thread_table(connection) {
        Ok(Some(table)) => table,
        Ok(None) => {
            return StateReadResult {
                sessions: Vec::new(),
                diagnostics: vec![StateDiagnostic {
                    source,
                    kind: StateDiagnosticKind::SchemaMismatch,
                    message: "no compatible threads table found".to_owned(),
                }],
            };
        }
        Err(error) => {
            return StateReadResult {
                sessions: Vec::new(),
                diagnostics: vec![StateDiagnostic {
                    source,
                    kind: StateDiagnosticKind::Query,
                    message: bounded_message(&error.to_string()),
                }],
            };
        }
    };

    let columns = match table_columns(connection, &table) {
        Ok(columns) => columns,
        Err(error) => {
            return StateReadResult {
                sessions: Vec::new(),
                diagnostics: vec![StateDiagnostic {
                    source,
                    kind: StateDiagnosticKind::Query,
                    message: bounded_message(&error.to_string()),
                }],
            };
        }
    };
    let selected = selected_columns(&columns);
    if !selected.iter().any(|column| column.key == "id") {
        return StateReadResult {
            sessions: Vec::new(),
            diagnostics: vec![StateDiagnostic {
                source,
                kind: StateDiagnosticKind::SchemaMismatch,
                message: format!("table {table:?} has no compatible thread identity column"),
            }],
        };
    }

    let query = format!(
        "SELECT {} FROM {} ORDER BY {}",
        selected
            .iter()
            .map(|column| quote_identifier(&column.name))
            .collect::<Vec<_>>()
            .join(", "),
        quote_identifier(&table),
        quote_identifier(
            &selected
                .iter()
                .find(|column| column.key == "id")
                .expect("identity column was checked")
                .name,
        ),
    );
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(error) => {
            return StateReadResult {
                sessions: Vec::new(),
                diagnostics: vec![StateDiagnostic {
                    source,
                    kind: StateDiagnosticKind::Query,
                    message: bounded_message(&error.to_string()),
                }],
            };
        }
    };

    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(error) => {
            return StateReadResult {
                sessions: Vec::new(),
                diagnostics: vec![StateDiagnostic {
                    source,
                    kind: StateDiagnosticKind::Query,
                    message: bounded_message(&error.to_string()),
                }],
            };
        }
    };
    let mut result = StateReadResult::default();
    let mut row_number = 0usize;
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(error) => {
                result.diagnostics.push(StateDiagnostic {
                    source: source.clone(),
                    kind: StateDiagnosticKind::Query,
                    message: bounded_message(&error.to_string()),
                });
                break;
            }
        };
        row_number += 1;
        let values = match selected
            .iter()
            .enumerate()
            .map(|(index, _)| row.get::<_, Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()
        {
            Ok(values) => values,
            Err(error) => {
                result.diagnostics.push(StateDiagnostic {
                    source: source.clone(),
                    kind: StateDiagnosticKind::Query,
                    message: format!("row {row_number}: {}", bounded_message(&error.to_string())),
                });
                continue;
            }
        };
        let values = values
            .into_iter()
            .enumerate()
            .map(|(index, value)| (selected[index].key, value))
            .collect::<HashMap<_, _>>();
        let Some(id) = value_string(values.get("id")).filter(|id| !id.is_empty()) else {
            result.diagnostics.push(StateDiagnostic {
                source: source.clone(),
                kind: StateDiagnosticKind::Query,
                message: format!("row {row_number} has no thread identity"),
            });
            continue;
        };

        result.sessions.push(Session {
            id,
            created_at: value_string(values.get("created_at")),
            updated_at: value_string(values.get("updated_at")),
            cwd: value_string(values.get("cwd")),
            project: value_string(values.get("project")),
            model: value_string(values.get("model")),
            provider: value_string(values.get("provider")),
            source: value_string(values.get("source")),
            thread_source: value_string(values.get("thread_source")),
            rollout_path: value_string(values.get("rollout_path")),
            archive_state: value_bool(values.get("archive_state")),
            title: value_string(values.get("title")),
            preview: value_string(values.get("preview")),
            parent_id: value_string(values.get("parent_id")),
            cli_version: value_string(values.get("cli_version")),
            originator: value_string(values.get("originator")),
            history_mode: value_string(values.get("history_mode")),
            reasoning_effort: value_string(values.get("reasoning_effort")),
            provenance: source.clone(),
        });
    }

    result
        .sessions
        .sort_by(|left, right| left.id.cmp(&right.id));
    result
}

fn find_thread_table(connection: &Connection) -> rusqlite::Result<Option<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name COLLATE NOCASE",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ["threads", "thread_metadata", "sessions"]
        .iter()
        .find_map(|candidate| {
            tables
                .iter()
                .find(|table| table.eq_ignore_ascii_case(candidate))
                .cloned()
        })
        .map_or(Ok(None), |table| Ok(Some(table)))
}

fn table_columns(connection: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let query = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut statement = connection.prepare(&query)?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect()
}

#[derive(Debug, Clone)]
struct SelectedColumn {
    key: &'static str,
    name: String,
}

fn selected_columns(columns: &[String]) -> Vec<SelectedColumn> {
    let lookup = columns
        .iter()
        .map(|column| (column.to_ascii_lowercase(), column.clone()))
        .collect::<HashMap<_, _>>();
    FIELD_ALIASES
        .iter()
        .filter_map(|(key, aliases)| {
            aliases.iter().find_map(|alias| {
                lookup
                    .get(&alias.to_ascii_lowercase())
                    .map(|name| SelectedColumn {
                        key,
                        name: name.clone(),
                    })
            })
        })
        .collect()
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn value_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Text(value)) if !value.is_empty() => Some(value.clone()),
        Some(Value::Integer(value)) => Some(value.to_string()),
        Some(Value::Real(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn value_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Integer(value)) => Some(*value != 0),
        Some(Value::Text(value)) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "y" => Some(true),
            "0" | "false" | "no" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn bounded_message(message: &str) -> String {
    const MAX_BYTES: usize = 512;
    if message.len() <= MAX_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_database(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("codexlens-state-{name}-{stamp}.sqlite"))
    }

    fn create_database(path: &Path, schema: &str) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(schema).unwrap();
    }

    #[test]
    fn reads_reduced_extended_and_current_schemas_without_writing() {
        let schemas = [
            (
                "reduced",
                include_str!("../tests/fixtures/state/reduced.sql"),
            ),
            (
                "extended",
                include_str!("../tests/fixtures/state/extended.sql"),
            ),
            (
                "current",
                include_str!("../tests/fixtures/state/current.sql"),
            ),
        ];
        for (name, schema) in schemas {
            let path = temporary_database(name);
            create_database(&path, schema);
            let before = std::fs::read(&path).unwrap();
            let result = read_state_database(&path);
            let after = std::fs::read(&path).unwrap();
            assert_eq!(before, after);
            assert_eq!(result.sessions.len(), 1);
            assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn incompatible_schema_is_an_explicit_diagnostic() {
        let path = temporary_database("incompatible");
        create_database(
            &path,
            include_str!("../tests/fixtures/state/incompatible.sql"),
        );

        let result = read_state_database(&path);
        assert!(result.sessions.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].kind,
            StateDiagnosticKind::SchemaMismatch
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn duplicate_state_rows_are_merged_with_conflicts() {
        let path = temporary_database("conflicting");
        create_database(
            &path,
            include_str!("../tests/fixtures/state/conflicting.sql"),
        );

        let result = read_state_database(&path);

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].cwd.as_deref(), Some("/first"));
        assert_eq!(result.sessions[0].model.as_deref(), Some("model-one"));
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == StateDiagnosticKind::MetadataConflict)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn all_state_databases_are_read_and_merged_deterministically() {
        let first = temporary_database("all-first");
        let second = temporary_database("all-second");
        create_database(&first, include_str!("../tests/fixtures/state/reduced.sql"));
        create_database(
            &second,
            include_str!("../tests/fixtures/state/extended.sql"),
        );

        let result = read_state_databases([&first, &second]);

        assert_eq!(
            result
                .sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fixture-extended-session", "fixture-reduced-session"]
        );
        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
    }
}
