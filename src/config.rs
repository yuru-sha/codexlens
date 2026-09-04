use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionConfig {
    pub project_doc_fallback_filenames: Vec<String>,
    pub project_doc_max_bytes: usize,
}

impl Default for InstructionConfig {
    fn default() -> Self {
        Self {
            project_doc_fallback_filenames: Vec::new(),
            project_doc_max_bytes: DEFAULT_PROJECT_DOC_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDiagnosticKind {
    Unreadable,
    Malformed,
    InvalidValue,
}

impl ConfigDiagnosticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unreadable => "unreadable",
            Self::Malformed => "malformed",
            Self::InvalidValue => "invalid_value",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub kind: ConfigDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigReadResult {
    pub path: PathBuf,
    pub config: InstructionConfig,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

pub fn load_config(codex_home: &Path) -> ConfigReadResult {
    load_config_at(codex_home, None)
}

pub fn load_config_at(codex_home: &Path, explicit_path: Option<&Path>) -> ConfigReadResult {
    let path = explicit_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| codex_home.join("config.toml"));
    read_config_file(&path, explicit_path.is_none())
}

pub fn read_config(path: &Path) -> ConfigReadResult {
    read_config_file(path, false)
}

fn read_config_file(path: &Path, missing_is_ok: bool) -> ConfigReadResult {
    match fs::read_to_string(path) {
        Ok(content) => parse_config(path, &content),
        Err(error) if missing_is_ok && error.kind() == std::io::ErrorKind::NotFound => {
            ConfigReadResult {
                path: path.to_path_buf(),
                config: InstructionConfig::default(),
                diagnostics: Vec::new(),
            }
        }
        Err(error) => ConfigReadResult {
            path: path.to_path_buf(),
            config: InstructionConfig::default(),
            diagnostics: vec![ConfigDiagnostic {
                path: path.to_path_buf(),
                line: None,
                kind: ConfigDiagnosticKind::Unreadable,
                message: bounded_message(&error.to_string()),
            }],
        },
    }
}

// ponytail: keep the parser limited to the two required root settings; add a
// TOML dependency only when the supported config surface grows.
pub fn parse_config(path: &Path, content: &str) -> ConfigReadResult {
    let mut config = InstructionConfig::default();
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    let mut section = None;

    for (line_index, raw_line) in content.lines().enumerate() {
        let line_number = line_index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line.ends_with(']') {
                section = Some(line[1..line.len() - 1].trim().to_owned());
            } else {
                diagnostics.push(diagnostic(
                    path,
                    line_number,
                    ConfigDiagnosticKind::Malformed,
                    "unterminated table header",
                ));
            }
            continue;
        }
        let Some(equal) = find_unquoted(line, '=') else {
            diagnostics.push(diagnostic(
                path,
                line_number,
                ConfigDiagnosticKind::Malformed,
                "expected a key/value assignment",
            ));
            continue;
        };
        if section.as_deref().is_some_and(|value| !value.is_empty()) {
            continue;
        }
        let key = line[..equal].trim();
        let value = line[equal + 1..].trim();
        if !matches!(
            key,
            "project_doc_fallback_filenames" | "project_doc_max_bytes"
        ) {
            continue;
        }
        if !seen.insert(key) {
            diagnostics.push(diagnostic(
                path,
                line_number,
                ConfigDiagnosticKind::Malformed,
                "duplicate instruction setting",
            ));
            continue;
        }

        match key {
            "project_doc_fallback_filenames" => match parse_string_array(value) {
                Ok(names) => {
                    let mut valid = Vec::new();
                    for name in names {
                        if valid_filename(&name) {
                            if !valid.contains(&name) {
                                valid.push(name);
                            }
                        } else {
                            diagnostics.push(diagnostic(
                                path,
                                line_number,
                                ConfigDiagnosticKind::InvalidValue,
                                "fallback filename must be a simple filename",
                            ));
                        }
                    }
                    config.project_doc_fallback_filenames = valid;
                }
                Err(message) => diagnostics.push(diagnostic(
                    path,
                    line_number,
                    ConfigDiagnosticKind::Malformed,
                    &message,
                )),
            },
            "project_doc_max_bytes" => match parse_positive_integer(value) {
                Some(max_bytes) => config.project_doc_max_bytes = max_bytes,
                None => diagnostics.push(diagnostic(
                    path,
                    line_number,
                    ConfigDiagnosticKind::InvalidValue,
                    "project_doc_max_bytes must be a positive integer",
                )),
            },
            _ => unreachable!(),
        }
    }

    ConfigReadResult {
        path: path.to_path_buf(),
        config,
        diagnostics,
    }
}

fn diagnostic(
    path: &Path,
    line: usize,
    kind: ConfigDiagnosticKind,
    message: &str,
) -> ConfigDiagnostic {
    ConfigDiagnostic {
        path: path.to_path_buf(),
        line: Some(line),
        kind,
        message: bounded_message(message),
    }
}

fn valid_filename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

fn parse_positive_integer(value: &str) -> Option<usize> {
    let value = value.trim().replace('_', "");
    let value = value.strip_prefix('+').unwrap_or(&value);
    let value = value.parse::<usize>().ok()?;
    (value > 0).then_some(value)
}

fn parse_string_array(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return Err("fallback filenames must be an array".to_owned());
    }
    let inner = &value[1..value.len() - 1];
    let mut position = 0;
    let mut values = Vec::new();
    loop {
        position = skip_whitespace(inner, position);
        if position == inner.len() {
            return Ok(values);
        }
        let (parsed, next) = parse_string(inner, position)?;
        values.push(parsed);
        position = skip_whitespace(inner, next);
        if position == inner.len() {
            return Ok(values);
        }
        if inner.as_bytes()[position] != b',' {
            return Err("fallback filenames must be comma-separated".to_owned());
        }
        position += 1;
        if skip_whitespace(inner, position) == inner.len() {
            return Ok(values);
        }
    }
}

fn parse_string(input: &str, start: usize) -> Result<(String, usize), String> {
    let quote = *input
        .as_bytes()
        .get(start)
        .ok_or_else(|| "missing quoted filename".to_owned())?;
    if quote != b'\'' && quote != b'"' {
        return Err("fallback filenames must be quoted strings".to_owned());
    }
    let mut value = String::new();
    let mut escaped = false;
    for (offset, character) in input[start + 1..].char_indices() {
        if quote == b'"' && escaped {
            value.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                _ => return Err("unsupported string escape".to_owned()),
            });
            escaped = false;
            continue;
        }
        if quote == b'"' && character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote as char {
            return Ok((value, start + 1 + offset + character.len_utf8()));
        }
        value.push(character);
    }
    Err("unterminated quoted filename".to_owned())
}

fn skip_whitespace(input: &str, mut position: usize) -> usize {
    while input
        .as_bytes()
        .get(position)
        .is_some_and(u8::is_ascii_whitespace)
    {
        position += 1;
    }
    position
}

fn strip_comment(input: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if let Some(active) = quote {
            if active == '"' && escaped {
                escaped = false;
            } else if active == '"' && character == '\\' {
                escaped = true;
            } else if character == active {
                quote = None;
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character);
        } else if character == '#' {
            return &input[..index];
        }
    }
    input
}

fn find_unquoted(input: &str, wanted: char) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if let Some(active) = quote {
            if active == '"' && escaped {
                escaped = false;
            } else if active == '"' && character == '\\' {
                escaped = true;
            } else if character == active {
                quote = None;
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character);
        } else if character == wanted {
            return Some(index);
        }
    }
    None
}

fn bounded_message(message: &str) -> String {
    const MAX_BYTES: usize = 256;
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

    #[test]
    fn parses_instruction_settings_and_ignores_unknown_keys() {
        let result = parse_config(
            Path::new("config.toml"),
            r#"
                project_doc_fallback_filenames = ["PROJECT.md", 'GUIDE.md'] # bounded
                project_doc_max_bytes = 64_000
                unknown_key = "ignored"
            "#,
        );

        assert_eq!(
            result.config.project_doc_fallback_filenames,
            vec!["PROJECT.md", "GUIDE.md"]
        );
        assert_eq!(result.config.project_doc_max_bytes, 64_000);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn malformed_and_invalid_values_keep_safe_defaults() {
        let result = parse_config(
            Path::new("config.toml"),
            "project_doc_max_bytes = 0\nproject_doc_fallback_filenames = [bad]\n",
        );

        assert_eq!(result.config, InstructionConfig::default());
        assert_eq!(result.diagnostics.len(), 2);
    }

    #[test]
    fn explicit_config_path_wins_over_codex_home_path() {
        let root =
            std::env::temp_dir().join(format!("codexlens-config-{}-{}", std::process::id(), 1));
        let home = root.join("codex");
        let explicit = root.join("custom.toml");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "project_doc_max_bytes = 4").unwrap();
        std::fs::write(&explicit, "project_doc_max_bytes = 8").unwrap();

        let result = load_config_at(&home, Some(&explicit));

        assert_eq!(result.path, explicit);
        assert_eq!(result.config.project_doc_max_bytes, 8);
        assert!(result.diagnostics.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_default_config_is_not_a_diagnostic() {
        let root =
            std::env::temp_dir().join(format!("codexlens-config-missing-{}", std::process::id()));

        let result = load_config(&root);

        assert_eq!(result.config, InstructionConfig::default());
        assert!(result.diagnostics.is_empty());
    }
}
