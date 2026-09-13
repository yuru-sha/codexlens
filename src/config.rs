use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::model::{CanonicalData, Session};
pub use crate::model::{Surface, SurfaceKind, SurfaceLoadMode, SurfaceScope, SurfaceUsageState};

pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;
const MAX_SURFACE_NAME_BYTES: usize = 128;
const MAX_SURFACE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceInventoryOptions {
    pub include_subagents: bool,
    pub usage_evidence_complete: bool,
}

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

    let mut lines = content.lines().enumerate();
    while let Some((line_index, raw_line)) = lines.next() {
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
        let mut value = line[equal + 1..].trim().to_owned();
        while value.trim_start().starts_with('[') && !array_complete(&value) {
            let Some((_, next_raw_line)) = lines.next() else {
                break;
            };
            let next_line = strip_comment(next_raw_line).trim();
            if !next_line.is_empty() {
                value.push(' ');
                value.push_str(next_line);
            }
        }
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
            "project_doc_fallback_filenames" => match parse_string_array(&value) {
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
            "project_doc_max_bytes" => match parse_positive_integer(&value) {
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

fn array_complete(input: &str) -> bool {
    let mut depth = 0;
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if let Some(active) = quote {
            if active == '"' && escaped {
                escaped = false;
            } else if active == '"' && character == '\\' {
                escaped = true;
            } else if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            ']' => return false,
            _ => {}
        }
    }
    depth == 0 && quote.is_none()
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

#[derive(Debug, Clone)]
struct SurfaceDraft {
    kind: SurfaceKind,
    name: String,
    path: Option<PathBuf>,
    scope: SurfaceScope,
    enabled: Option<bool>,
    load_mode: SurfaceLoadMode,
    static_bytes: Option<usize>,
    startup_bytes: Option<usize>,
    limitations: Vec<String>,
}

#[derive(Debug, Clone)]
struct ConfigSurfaceDeclaration {
    kind: SurfaceKind,
    name: String,
    enabled: Option<bool>,
    invalid_enabled: bool,
}

pub fn discover_surfaces(
    codex_home: &Path,
    data: &CanonicalData,
    options: &SurfaceInventoryOptions,
) -> Vec<Surface> {
    let global_root = fs::canonicalize(codex_home).unwrap_or_else(|_| codex_home.to_path_buf());
    let global_scope = SurfaceScope::Global;
    let mut surfaces = BTreeMap::new();
    let config_path = codex_home.join("config.toml");
    let instruction_config = load_config(codex_home).config;

    add_file_surface(
        &mut surfaces,
        &config_path,
        SurfaceKind::Config,
        "config.toml",
        global_scope.clone(),
        SurfaceLoadMode::StartupFull,
        Some(&global_root),
        true,
    );
    if let Some(content) = read_text(&config_path, Some(&global_root)) {
        add_config_declarations(
            &mut surfaces,
            &config_path,
            &content,
            global_scope.clone(),
            Some(&global_root),
        );
    }
    for name in ["AGENTS.override.md", "AGENTS.md"] {
        add_file_surface(
            &mut surfaces,
            &codex_home.join(name),
            SurfaceKind::Instruction,
            name,
            global_scope.clone(),
            SurfaceLoadMode::StartupFull,
            Some(&global_root),
            false,
        );
    }
    for path in collect_surface_files(&codex_home.join("rules"), ".rules", Some(&global_root)) {
        let name = file_name(&path);
        add_file_surface(
            &mut surfaces,
            &path,
            SurfaceKind::Rule,
            &name,
            global_scope.clone(),
            SurfaceLoadMode::StartupFull,
            Some(&global_root),
            false,
        );
    }
    for path in collect_surface_files(&codex_home.join("skills"), "SKILL.md", Some(&global_root)) {
        let name = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .map_or_else(|| file_name(&path), str::to_owned);
        add_file_surface(
            &mut surfaces,
            &path,
            SurfaceKind::Skill,
            &name,
            global_scope.clone(),
            SurfaceLoadMode::Unknown,
            Some(&global_root),
            false,
        );
    }
    add_hook_file(
        &mut surfaces,
        &codex_home.join("hooks.json"),
        global_scope.clone(),
        Some(&global_root),
    );

    let project_scopes = project_scopes(&data.sessions, options.include_subagents);
    for (project_root, cwds) in project_scopes {
        let project_scope = SurfaceScope::Project(project_root.clone());
        let project_config_path = project_root.join(".codex/config.toml");
        if path_exists(&project_config_path) {
            add_file_surface(
                &mut surfaces,
                &project_config_path,
                SurfaceKind::Config,
                "config.toml",
                project_scope.clone(),
                SurfaceLoadMode::StartupFull,
                None,
                false,
            );
            if let Some(content) = read_text(&project_config_path, None) {
                add_config_declarations(
                    &mut surfaces,
                    &project_config_path,
                    &content,
                    project_scope.clone(),
                    None,
                );
            }
        }
        add_hook_file(
            &mut surfaces,
            &project_root.join(".codex/hooks.json"),
            project_scope.clone(),
            None,
        );

        for directory in project_directories(&project_root, &cwds) {
            let scope = if directory == project_root {
                project_scope.clone()
            } else {
                SurfaceScope::Nested(directory.clone())
            };
            for name in instruction_names(&instruction_config) {
                let path = directory.join(&name);
                if path_exists(&path) {
                    add_file_surface(
                        &mut surfaces,
                        &path,
                        SurfaceKind::Instruction,
                        &name,
                        scope.clone(),
                        SurfaceLoadMode::StartupFull,
                        None,
                        false,
                    );
                }
            }
        }
        for rules_root in [
            project_root.join("rules"),
            project_root.join(".codex/rules"),
        ] {
            for path in collect_surface_files(&rules_root, ".rules", None) {
                let name = file_name(&path);
                add_file_surface(
                    &mut surfaces,
                    &path,
                    SurfaceKind::Rule,
                    &name,
                    project_scope.clone(),
                    SurfaceLoadMode::StartupFull,
                    None,
                    false,
                );
            }
        }
        for skills_root in [
            project_root.join(".agents/skills"),
            project_root.join(".codex/skills"),
        ] {
            for path in collect_surface_files(&skills_root, "SKILL.md", None) {
                let name = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|value| value.to_str())
                    .map_or_else(|| file_name(&path), str::to_owned);
                add_file_surface(
                    &mut surfaces,
                    &path,
                    SurfaceKind::Skill,
                    &name,
                    project_scope.clone(),
                    SurfaceLoadMode::Unknown,
                    None,
                    false,
                );
            }
        }
    }

    let mut surfaces = surfaces.into_values().collect::<Vec<_>>();
    apply_usage(&mut surfaces, data, options);
    surfaces.sort_by(|left, right| {
        left.kind
            .as_str()
            .cmp(right.kind.as_str())
            .then_with(|| left.scope.as_str().cmp(right.scope.as_str()))
            .then_with(|| left.scope.path().cmp(&right.scope.path()))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.id.cmp(&right.id))
    });
    surfaces
}

fn add_config_declarations(
    surfaces: &mut BTreeMap<String, Surface>,
    config_path: &Path,
    content: &str,
    scope: SurfaceScope,
    global_root: Option<&Path>,
) {
    let path = resolved_path(config_path, global_root).0;
    for declaration in parse_surface_declarations(content) {
        let mut limitations = Vec::new();
        if declaration.kind == SurfaceKind::McpServer {
            limitations.push("MCP schema was not available in local state".to_owned());
        }
        let draft = SurfaceDraft {
            kind: declaration.kind,
            name: declaration.name,
            path: Some(path.clone()),
            scope: scope.clone(),
            enabled: (!declaration.invalid_enabled).then(|| declaration.enabled.unwrap_or(true)),
            load_mode: match declaration.kind {
                SurfaceKind::McpServer => SurfaceLoadMode::ToolSchema,
                SurfaceKind::Hook => SurfaceLoadMode::OnDemand,
                _ => SurfaceLoadMode::Unknown,
            },
            static_bytes: None,
            startup_bytes: None,
            limitations,
        };
        insert_surface(surfaces, draft);
    }
}

fn add_hook_file(
    surfaces: &mut BTreeMap<String, Surface>,
    path: &Path,
    scope: SurfaceScope,
    global_root: Option<&Path>,
) {
    if path_exists(path) {
        add_file_surface(
            surfaces,
            path,
            SurfaceKind::Hook,
            "hooks.json",
            scope,
            SurfaceLoadMode::OnDemand,
            global_root,
            false,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn add_file_surface(
    surfaces: &mut BTreeMap<String, Surface>,
    path: &Path,
    kind: SurfaceKind,
    name: &str,
    scope: SurfaceScope,
    requested_mode: SurfaceLoadMode,
    global_root: Option<&Path>,
    include_missing: bool,
) {
    if !path_exists(path) && !include_missing {
        return;
    }
    let (identity, readable, mut limitations) = resolved_path(path, global_root);
    let mut draft = SurfaceDraft {
        kind,
        name: bounded_name(name),
        path: Some(identity),
        scope,
        enabled: None,
        load_mode: requested_mode,
        static_bytes: None,
        startup_bytes: None,
        limitations: Vec::new(),
    };
    draft.limitations.append(&mut limitations);

    if !readable {
        draft
            .limitations
            .push("global symlink escapes configured Codex home".to_owned());
        insert_surface(surfaces, draft);
        return;
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            draft
                .limitations
                .push("surface path is not a regular file".to_owned());
            insert_surface(surfaces, draft);
            return;
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && (include_missing || fs::symlink_metadata(path).is_ok()) =>
        {
            draft.limitations.push("surface is missing".to_owned());
            insert_surface(surfaces, draft);
            return;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            draft.limitations.push("surface is unreadable".to_owned());
            insert_surface(surfaces, draft);
            return;
        }
    };
    let bytes = usize::try_from(metadata.len())
        .unwrap_or(MAX_SURFACE_BYTES)
        .min(MAX_SURFACE_BYTES);
    draft.static_bytes = Some(bytes);
    if metadata.len() > u64::try_from(MAX_SURFACE_BYTES).unwrap_or(u64::MAX) {
        draft
            .limitations
            .push("size estimate is capped at 1048576 bytes".to_owned());
    }

    match read_bounded(path) {
        Ok((raw, truncated)) => {
            if truncated {
                draft
                    .limitations
                    .push("surface content was bounded for classification".to_owned());
            }
            let Ok(content) = String::from_utf8(raw) else {
                draft
                    .limitations
                    .push("surface content is not valid UTF-8".to_owned());
                insert_surface(surfaces, draft);
                return;
            };
            draft.enabled = Some(true);
            if kind == SurfaceKind::Rule && has_path_condition(&content) {
                draft.load_mode = SurfaceLoadMode::PathConditional;
            }
            if kind == SurfaceKind::Skill {
                if let Some(description_bytes) = skill_description_bytes(&content) {
                    draft.load_mode = SurfaceLoadMode::StartupDescription;
                    draft.startup_bytes = Some(description_bytes.min(MAX_SURFACE_BYTES));
                } else {
                    draft
                        .limitations
                        .push("Skill description was not discoverable".to_owned());
                }
            }
        }
        Err(_) => {
            draft.limitations.push("surface is unreadable".to_owned());
        }
    }
    if draft.startup_bytes.is_none() {
        draft.startup_bytes = match draft.load_mode {
            SurfaceLoadMode::StartupFull => draft.static_bytes,
            SurfaceLoadMode::OnDemand => Some(0),
            _ => None,
        };
    }
    insert_surface(surfaces, draft);
}

fn insert_surface(surfaces: &mut BTreeMap<String, Surface>, draft: SurfaceDraft) {
    let id = surface_id(draft.kind, draft.path.as_deref(), &draft.name);
    surfaces.entry(id.clone()).or_insert_with(|| Surface {
        id,
        kind: draft.kind,
        name: draft.name,
        path: draft.path,
        scope: draft.scope,
        enabled: draft.enabled,
        load_mode: draft.load_mode,
        static_bytes: draft.static_bytes,
        startup_bytes: draft.startup_bytes,
        observed_uses: 0,
        observed_sessions: 0,
        usage_state: SurfaceUsageState::Unknown,
        limitations: draft.limitations,
    });
}

fn apply_usage(surfaces: &mut [Surface], data: &CanonicalData, options: &SurfaceInventoryOptions) {
    let selected = data
        .sessions
        .iter()
        .filter(|session| options.include_subagents || session.parent_id.is_none())
        .collect::<Vec<_>>();
    let selected_ids = selected
        .iter()
        .map(|session| session.id.as_str())
        .collect::<BTreeSet<_>>();
    let usage_complete = options.usage_evidence_complete && !selected_ids.is_empty();
    let instruction_complete = usage_complete
        && selected_ids.iter().all(|session_id| {
            data.instruction_joins
                .iter()
                .any(|join| join.session_id == *session_id)
        });
    let mut observed_sessions = BTreeMap::<usize, BTreeSet<String>>::new();

    for join in &data.instruction_joins {
        if !selected_ids.contains(join.session_id.as_str()) {
            continue;
        }
        for file in &join.resolution.chain {
            observe_path_surface(
                surfaces,
                SurfaceKind::Instruction,
                &file.path,
                &join.session_id,
                &mut observed_sessions,
            );
        }
    }
    for call in &data.tool_calls {
        let Some(session_id) = call.session_id.as_deref() else {
            continue;
        };
        if !selected_ids.contains(session_id) {
            continue;
        }
        let Some(tool_name) = call.tool_name.as_deref() else {
            continue;
        };
        observe_tool_surfaces(
            surfaces,
            tool_name,
            session_id,
            &selected,
            &mut observed_sessions,
        );
    }
    for record in &data.records {
        let Some(session_id) = record.session_id.as_deref() else {
            continue;
        };
        if !selected_ids.contains(session_id) {
            continue;
        }
        if let Some(name) = hook_event_name(record.original_nested_type.as_deref()) {
            observe_named_surfaces(
                surfaces,
                SurfaceKind::Hook,
                name,
                session_id,
                &selected,
                &mut observed_sessions,
            );
        }
    }

    for (index, session_ids) in observed_sessions {
        surfaces[index].observed_sessions = session_ids.len();
    }

    for surface in surfaces {
        let complete = if surface.kind == SurfaceKind::Instruction {
            instruction_complete
        } else {
            usage_complete
        };
        if !complete {
            surface
                .limitations
                .push("usage evidence is incomplete".to_owned());
            surface.usage_state = SurfaceUsageState::Unknown;
        } else if surface.enabled != Some(true) {
            surface.usage_state = SurfaceUsageState::Unknown;
        } else {
            surface.usage_state = match surface.observed_uses {
                0 => SurfaceUsageState::Unused,
                1 => SurfaceUsageState::Rare,
                _ => SurfaceUsageState::Used,
            };
        }
    }
}

fn observe_path_surface(
    surfaces: &mut [Surface],
    kind: SurfaceKind,
    path: &Path,
    session_id: &str,
    observed_sessions: &mut BTreeMap<usize, BTreeSet<String>>,
) {
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let matching = surfaces
        .iter()
        .enumerate()
        .filter(|(_, surface)| surface.kind == kind)
        .filter_map(|(index, surface)| {
            let surface_path = surface.path.as_deref()?;
            let surface_identity =
                fs::canonicalize(surface_path).unwrap_or_else(|_| surface_path.to_path_buf());
            (surface_identity == identity).then_some(index)
        })
        .collect::<Vec<_>>();
    for index in matching {
        if surfaces[index].path.is_none() {
            continue;
        }
        observe(surfaces, index, session_id, observed_sessions);
    }
}

fn observe_tool_surfaces(
    surfaces: &mut [Surface],
    tool_name: &str,
    session_id: &str,
    sessions: &[&Session],
    observed_sessions: &mut BTreeMap<usize, BTreeSet<String>>,
) {
    for kind in [
        SurfaceKind::Skill,
        SurfaceKind::McpServer,
        SurfaceKind::Plugin,
        SurfaceKind::Hook,
    ] {
        let matching = surfaces
            .iter()
            .enumerate()
            .filter(|(_, surface)| {
                surface.kind == kind && tool_matches_surface(kind, tool_name, &surface.name)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let global = matching
            .iter()
            .copied()
            .filter(|index| matches!(surfaces[*index].scope, SurfaceScope::Global))
            .collect::<Vec<_>>();
        for index in global {
            observe(surfaces, index, session_id, observed_sessions);
        }
        let best = matching
            .iter()
            .filter(|index| {
                !matches!(surfaces[**index].scope, SurfaceScope::Global)
                    && session_matches_scope(&surfaces[**index], session_id, sessions)
            })
            .max_by_key(|index| {
                surfaces[**index]
                    .scope
                    .path()
                    .map_or(0, |path| path.components().count())
            })
            .copied();
        if let Some(index) = best {
            observe(surfaces, index, session_id, observed_sessions);
        }
    }
}

fn observe_named_surfaces(
    surfaces: &mut [Surface],
    kind: SurfaceKind,
    name: &str,
    session_id: &str,
    sessions: &[&Session],
    observed_sessions: &mut BTreeMap<usize, BTreeSet<String>>,
) {
    let tool_name = format!("{}:{name}", kind.as_str());
    observe_tool_surfaces(
        surfaces,
        &tool_name,
        session_id,
        sessions,
        observed_sessions,
    );
}

fn observe(
    surfaces: &mut [Surface],
    index: usize,
    session_id: &str,
    observed_sessions: &mut BTreeMap<usize, BTreeSet<String>>,
) {
    surfaces[index].observed_uses = surfaces[index].observed_uses.saturating_add(1);
    observed_sessions
        .entry(index)
        .or_default()
        .insert(session_id.to_owned());
}

fn session_matches_scope(surface: &Surface, session_id: &str, sessions: &[&Session]) -> bool {
    let Some(scope_path) = surface.scope.path() else {
        return false;
    };
    let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
        return false;
    };
    [session.cwd.as_deref(), session.project.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let path = Path::new(value);
            path.is_absolute()
                .then(|| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
        })
        .any(|path| path.starts_with(scope_path))
}

fn tool_matches_surface(kind: SurfaceKind, tool_name: &str, surface_name: &str) -> bool {
    match kind {
        SurfaceKind::Skill => {
            normalized_identifier(tool_name) == normalized_identifier(surface_name)
        }
        SurfaceKind::McpServer => mcp_server_name(tool_name)
            .is_some_and(|name| normalized_identifier(name) == normalized_identifier(surface_name)),
        SurfaceKind::Plugin => prefixed_name(tool_name, "plugin")
            .is_some_and(|name| normalized_identifier(name) == normalized_identifier(surface_name)),
        SurfaceKind::Hook => prefixed_name(tool_name, "hook")
            .is_some_and(|name| normalized_identifier(name) == normalized_identifier(surface_name)),
        _ => false,
    }
}

fn normalized_identifier(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('$')
        .strip_prefix("skill:")
        .or_else(|| value.trim().trim_start_matches('$').strip_prefix("skill/"))
        .unwrap_or_else(|| value.trim().trim_start_matches('$'))
        .to_ascii_lowercase()
}

fn mcp_server_name(value: &str) -> Option<&str> {
    let value = value.trim();
    let value = value.strip_prefix("mcp__").unwrap_or(value);
    if let Some((server, tool)) = value.split_once("__") {
        return (!server.is_empty() && !tool.is_empty()).then_some(server);
    }
    if let Some((server, tool)) = value.split_once('/') {
        return (!server.is_empty() && !tool.is_empty())
            .then_some(server.trim_start_matches("mcp_"));
    }
    value
        .split_once("::")
        .and_then(|(server, tool)| (!server.is_empty() && !tool.is_empty()).then_some(server))
}

fn prefixed_name<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let value = value.trim();
    value
        .strip_prefix(&format!("{prefix}:"))
        .or_else(|| value.strip_prefix(&format!("{prefix}/")))
}

fn hook_event_name(value: Option<&str>) -> Option<&str> {
    let value = value?.strip_prefix("hook_")?;
    (!matches!(value, "started" | "completed")).then_some(value)
}

fn project_scopes(
    sessions: &[Session],
    include_subagents: bool,
) -> BTreeMap<PathBuf, BTreeSet<PathBuf>> {
    let mut scopes = BTreeMap::new();
    for session in sessions
        .iter()
        .filter(|session| include_subagents || session.parent_id.is_none())
    {
        let project = session
            .project
            .as_deref()
            .filter(|value| Path::new(value).is_absolute())
            .or_else(|| {
                session
                    .cwd
                    .as_deref()
                    .filter(|value| Path::new(value).is_absolute())
            });
        let Some(project) = project else { continue };
        let project = fs::canonicalize(project).unwrap_or_else(|_| PathBuf::from(project));
        let cwd = session
            .cwd
            .as_deref()
            .filter(|value| Path::new(value).is_absolute())
            .map(PathBuf::from)
            .map(|path| fs::canonicalize(&path).unwrap_or(path));
        scopes
            .entry(project.clone())
            .or_insert_with(BTreeSet::new)
            .insert(
                cwd.filter(|path| path.starts_with(&project))
                    .unwrap_or(project),
            );
    }
    scopes
}

fn project_directories(root: &Path, cwds: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut directories = BTreeSet::new();
    for target in cwds
        .iter()
        .filter(|cwd| cwd.is_dir() && cwd.starts_with(root))
    {
        let mut current = Some(target.as_path());
        while let Some(directory) = current {
            directories.insert(directory.to_path_buf());
            if directory == root {
                break;
            }
            current = directory.parent();
        }
    }
    if directories.is_empty() {
        directories.insert(root.to_path_buf());
    }
    directories.into_iter().collect()
}

fn instruction_names(config: &InstructionConfig) -> Vec<String> {
    let mut names = vec!["AGENTS.override.md".to_owned(), "AGENTS.md".to_owned()];
    for name in &config.project_doc_fallback_filenames {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.clone());
        }
    }
    names
}

fn collect_surface_files(root: &Path, suffix: &str, global_root: Option<&Path>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut visited = BTreeSet::new();
    walk_surface_tree(root, suffix, global_root, &mut visited, &mut files);
    files.sort();
    files
}

fn walk_surface_tree(
    path: &Path,
    suffix: &str,
    global_root: Option<&Path>,
    visited: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) {
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if global_root.is_some_and(|root| !identity.starts_with(root)) || !visited.insert(identity) {
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if path_matches_surface(path, suffix) {
            files.push(path.to_path_buf());
        }
        return;
    }
    if !metadata.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let entry_path = entry.path();
        let file_type = entry.file_type();
        let is_file_like = file_type
            .as_ref()
            .map(|kind| kind.is_file() || kind.is_symlink())
            .unwrap_or(false);
        if is_file_like && path_matches_surface(&entry_path, suffix) {
            files.push(entry_path);
        } else if entry
            .file_type()
            .map(|kind| kind.is_dir() || kind.is_symlink())
            .unwrap_or(false)
        {
            walk_surface_tree(&entry_path, suffix, global_root, visited, files);
        }
    }
}

fn path_matches_surface(path: &Path, suffix: &str) -> bool {
    if suffix == "SKILL.md" {
        path.file_name().is_some_and(|name| name == "SKILL.md")
    } else {
        path.extension()
            .is_some_and(|extension| extension == "rules")
    }
}

fn resolved_path(path: &Path, global_root: Option<&Path>) -> (PathBuf, bool, Vec<String>) {
    let identity = fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    });
    let allowed = global_root.is_none_or(|root| identity.starts_with(root));
    (identity, allowed, Vec::new())
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn read_text(path: &Path, global_root: Option<&Path>) -> Option<String> {
    let (_, readable, _) = resolved_path(path, global_root);
    if !readable {
        return None;
    }
    let (bytes, _) = read_bounded(path).ok()?;
    String::from_utf8(bytes).ok()
}

fn read_bounded(path: &Path) -> io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let read_limit = u64::try_from(MAX_SURFACE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    file.by_ref().take(read_limit).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_SURFACE_BYTES;
    if truncated {
        bytes.truncate(MAX_SURFACE_BYTES);
    }
    Ok((bytes, truncated))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "unknown".to_owned(), str::to_owned)
}

fn bounded_name(name: &str) -> String {
    if name.len() <= MAX_SURFACE_NAME_BYTES {
        return name.to_owned();
    }
    let mut end = MAX_SURFACE_NAME_BYTES.saturating_sub(3);
    while !name.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &name[..end])
}

fn surface_id(kind: SurfaceKind, path: Option<&Path>, name: &str) -> String {
    let seed = format!(
        "{}|{}|{}",
        kind.as_str(),
        path.map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
        name
    );
    let mut hash = 0xcbf29ce484222325u64;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn parse_surface_declarations(content: &str) -> Vec<ConfigSurfaceDeclaration> {
    let mut declarations = Vec::new();
    let mut current = None;
    for raw_line in content.lines() {
        let line = strip_comment(raw_line).trim();
        if line.starts_with('[') && line.ends_with(']') {
            let header = line[1..line.len() - 1].trim();
            current = parse_surface_header(header).map(|(kind, name)| {
                declarations.push(ConfigSurfaceDeclaration {
                    kind,
                    name: bounded_name(&name),
                    enabled: None,
                    invalid_enabled: false,
                });
                declarations.len() - 1
            });
            continue;
        }
        let Some(index) = current else { continue };
        let Some(equal) = find_unquoted(line, '=') else {
            continue;
        };
        if line[..equal].trim() == "enabled" {
            declarations[index].enabled = parse_bool(line[equal + 1..].trim());
            declarations[index].invalid_enabled = declarations[index].enabled.is_none();
        }
    }
    declarations
}

fn parse_surface_header(header: &str) -> Option<(SurfaceKind, String)> {
    for (prefix, kind) in [
        ("mcp_servers", SurfaceKind::McpServer),
        ("plugins", SurfaceKind::Plugin),
        ("hooks", SurfaceKind::Hook),
    ] {
        let Some(rest) = header
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        let name = parse_table_component(rest)?;
        if !name.is_empty() {
            return Some((kind, name));
        }
    }
    None
}

fn parse_table_component(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('"') || value.starts_with('\'') {
        let (parsed, end) = parse_string(value, 0).ok()?;
        return value[end..].trim().is_empty().then_some(parsed);
    }
    (!value.is_empty() && !value.contains('.') && !value.chars().any(char::is_whitespace))
        .then_some(value.to_owned())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn has_path_condition(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        ["path", "paths", "globs", "include", "exclude"]
            .iter()
            .any(|key| {
                line.starts_with(&format!("{key}:")) || line.starts_with(&format!("{key} ="))
            })
    })
}

fn skill_description_bytes(content: &str) -> Option<usize> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.len());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CanonicalData, Session, SourceRef, ToolCall};

    fn fixture_session(id: &str, project: &Path, cwd: &Path, parent_id: Option<&str>) -> Session {
        Session {
            id: id.to_owned(),
            created_at: None,
            updated_at: None,
            cwd: Some(cwd.display().to_string()),
            project: Some(project.display().to_string()),
            model: None,
            provider: None,
            source: None,
            thread_source: None,
            rollout_path: None,
            archive_state: None,
            title: None,
            preview: None,
            parent_id: parent_id.map(str::to_owned),
            cli_version: None,
            originator: None,
            history_mode: None,
            reasoning_effort: None,
            provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 1),
        }
    }

    fn fixture_tool(session_id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: None,
            call_id: None,
            session_id: Some(session_id.to_owned()),
            turn_id: None,
            tool_name: Some(name.to_owned()),
            input_summary: None,
            command: None,
            cwd: None,
            status: None,
            provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 2),
        }
    }

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
    fn parses_multiline_fallback_filename_arrays() {
        let result = parse_config(
            Path::new("config.toml"),
            r#"
                project_doc_fallback_filenames = [
                    "PROJECT.md",
                    'GUIDE.md',
                ]
            "#,
        );

        assert_eq!(
            result.config.project_doc_fallback_filenames,
            vec!["PROJECT.md", "GUIDE.md"]
        );
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

    #[test]
    fn inventories_surfaces_with_shadowing_and_privacy_boundaries() {
        let root =
            std::env::temp_dir().join(format!("codexlens-surfaces-{}-{}", std::process::id(), 1));
        let home = root.join("codex");
        let project = root.join("project");
        let nested = project.join("src");
        fs::create_dir_all(home.join("rules")).unwrap();
        fs::create_dir_all(home.join("skills/deploy")).unwrap();
        fs::create_dir_all(home.join("skills/broken")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(project.join(".agents/skills/deploy")).unwrap();
        fs::create_dir_all(project.join("rules")).unwrap();
        fs::write(
            home.join("config.toml"),
            r#"
project_doc_max_bytes = 32768
[mcp_servers.docs]
command = "secret-command --token=secret"
env = { API_TOKEN = "secret-value" }
[plugins."editor@market"]
enabled = "secret-value"
[hooks.before]
command = "secret-hook"
"#,
        )
        .unwrap();
        fs::write(home.join("AGENTS.md"), "global guidance").unwrap();
        fs::write(home.join("rules/heavy.rules"), "x".repeat(4096)).unwrap();
        fs::write(
            home.join("skills/deploy/SKILL.md"),
            "---\ndescription: deploy safely\n---\nGlobal body",
        )
        .unwrap();
        fs::write(
            project.join(".agents/skills/deploy/SKILL.md"),
            "---\ndescription: project deploy\n---\nProject body",
        )
        .unwrap();
        fs::write(project.join("rules/project.rules"), "project rule").unwrap();
        fs::write(
            home.join("hooks.json"),
            r#"{"before":{"command":"secret"}}"#,
        )
        .unwrap();
        fs::write(home.join("skills/broken/SKILL.md"), [0xff, 0xfe]).unwrap();

        let data = CanonicalData {
            sessions: vec![
                fixture_session("main", &project, &nested, None),
                fixture_session("sub", &project, &nested, Some("main")),
            ],
            tool_calls: vec![
                fixture_tool("main", "deploy"),
                fixture_tool("main", "mcp__docs__search"),
                fixture_tool("sub", "deploy"),
            ],
            ..CanonicalData::default()
        };
        let project_identity = fs::canonicalize(&project).unwrap();

        let surfaces = discover_surfaces(
            &home,
            &data,
            &SurfaceInventoryOptions {
                usage_evidence_complete: true,
                ..SurfaceInventoryOptions::default()
            },
        );
        let global_skill = surfaces
            .iter()
            .find(|surface| {
                surface.kind == SurfaceKind::Skill
                    && surface.scope == SurfaceScope::Global
                    && surface.name == "deploy"
            })
            .unwrap();
        let project_skill = surfaces
            .iter()
            .find(|surface| {
                surface.kind == SurfaceKind::Skill
                    && surface.scope == SurfaceScope::Project(project_identity.clone())
                    && surface.name == "deploy"
            })
            .unwrap();
        let mcp = surfaces
            .iter()
            .find(|surface| surface.kind == SurfaceKind::McpServer && surface.name == "docs")
            .unwrap();
        let heavy_rule = surfaces
            .iter()
            .find(|surface| surface.name == "heavy.rules")
            .unwrap();
        let project_rule = surfaces
            .iter()
            .find(|surface| surface.name == "project.rules")
            .unwrap();
        let unavailable = surfaces
            .iter()
            .find(|surface| surface.name == "broken")
            .unwrap();
        let invalid_plugin = surfaces
            .iter()
            .find(|surface| surface.kind == SurfaceKind::Plugin)
            .unwrap();

        assert_eq!(global_skill.observed_uses, 1);
        assert_eq!(global_skill.observed_sessions, 1);
        assert_eq!(global_skill.usage_state, SurfaceUsageState::Rare);
        assert_eq!(project_skill.observed_uses, 1);
        assert_eq!(project_skill.observed_sessions, 1);
        assert_eq!(project_skill.usage_state, SurfaceUsageState::Rare);
        assert_eq!(mcp.observed_uses, 1);
        assert_eq!(mcp.usage_state, SurfaceUsageState::Rare);
        assert_eq!(heavy_rule.enabled, Some(true));
        assert_eq!(heavy_rule.static_bytes, Some(4096));
        assert_eq!(heavy_rule.startup_bytes, Some(4096));
        assert_eq!(heavy_rule.usage_state, SurfaceUsageState::Unused);
        assert_eq!(project_rule.scope, SurfaceScope::Project(project_identity));
        assert_eq!(project_rule.usage_state, SurfaceUsageState::Unused);
        assert_eq!(unavailable.enabled, None);
        assert_eq!(unavailable.usage_state, SurfaceUsageState::Unknown);
        assert_eq!(invalid_plugin.enabled, None);
        assert_eq!(invalid_plugin.usage_state, SurfaceUsageState::Unknown);

        let incomplete = discover_surfaces(&home, &data, &SurfaceInventoryOptions::default());
        assert_eq!(
            incomplete
                .iter()
                .find(|surface| surface.name == "heavy.rules")
                .unwrap()
                .usage_state,
            SurfaceUsageState::Unknown
        );

        let serialized = serde_json::to_string(&surfaces).unwrap();
        assert!(!serialized.contains("secret-command"));
        assert!(!serialized.contains("secret-value"));
        assert!(!serialized.contains("API_TOKEN"));

        let _ = fs::remove_dir_all(root);
    }
}
