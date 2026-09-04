use std::fmt::Debug;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::rollout::SourceLocation;

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_SUMMARY_BYTES: usize = 8 * 1024;
pub const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;
pub const CANONICAL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Rollout,
    State,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub kind: SourceKind,
    pub path: PathBuf,
    pub line: Option<usize>,
    pub ingested_at: Option<String>,
    pub parser_schema_version: u32,
}

impl SourceRef {
    pub fn rollout(path: PathBuf, line: usize) -> Self {
        Self {
            kind: SourceKind::Rollout,
            path,
            line: Some(line),
            ingested_at: None,
            parser_schema_version: CANONICAL_SCHEMA_VERSION,
        }
    }

    pub fn state(path: PathBuf) -> Self {
        Self {
            kind: SourceKind::State,
            path,
            line: None,
            ingested_at: None,
            parser_schema_version: CANONICAL_SCHEMA_VERSION,
        }
    }

    pub(crate) fn stamp_ingest_time(&mut self, timestamp: &str) {
        self.ingested_at = Some(timestamp.to_owned());
    }
}

pub(crate) fn normalize_path(path: &str) -> String {
    let path = path
        .strip_prefix("file://")
        .unwrap_or(path)
        .replace('\\', "/");
    let absolute = path.starts_with('/');
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.last().is_some_and(|value| *value != "..") {
                    components.pop();
                } else if !absolute {
                    components.push("..");
                }
            }
            value => components.push(value),
        }
    }
    let prefix = if absolute { "/" } else { "" };
    format!("{prefix}{}", components.join("/"))
}

impl From<SourceLocation> for SourceRef {
    fn from(source: SourceLocation) -> Self {
        Self::rollout(source.path, source.line)
    }
}

impl From<&SourceLocation> for SourceRef {
    fn from(source: &SourceLocation) -> Self {
        Self::rollout(source.path.clone(), source.line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub source: Option<String>,
    pub thread_source: Option<String>,
    pub rollout_path: Option<String>,
    pub archive_state: Option<bool>,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub parent_id: Option<String>,
    pub cli_version: Option<String>,
    pub originator: Option<String>,
    pub history_mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionConflict {
    pub field: &'static str,
    pub existing: String,
    pub incoming: String,
}

pub(crate) fn merge_session_fields(
    target: &mut Session,
    incoming: &Session,
) -> Vec<SessionConflict> {
    let mut conflicts = Vec::new();
    merge_session_field(
        "created_at",
        &mut target.created_at,
        &incoming.created_at,
        &mut conflicts,
    );
    merge_session_field(
        "updated_at",
        &mut target.updated_at,
        &incoming.updated_at,
        &mut conflicts,
    );
    merge_session_field("cwd", &mut target.cwd, &incoming.cwd, &mut conflicts);
    merge_session_field(
        "project",
        &mut target.project,
        &incoming.project,
        &mut conflicts,
    );
    merge_session_field("model", &mut target.model, &incoming.model, &mut conflicts);
    merge_session_field(
        "provider",
        &mut target.provider,
        &incoming.provider,
        &mut conflicts,
    );
    merge_session_field(
        "source",
        &mut target.source,
        &incoming.source,
        &mut conflicts,
    );
    merge_session_field(
        "thread_source",
        &mut target.thread_source,
        &incoming.thread_source,
        &mut conflicts,
    );
    merge_session_field(
        "rollout_path",
        &mut target.rollout_path,
        &incoming.rollout_path,
        &mut conflicts,
    );
    merge_session_field(
        "archive_state",
        &mut target.archive_state,
        &incoming.archive_state,
        &mut conflicts,
    );
    merge_session_field("title", &mut target.title, &incoming.title, &mut conflicts);
    merge_session_field(
        "preview",
        &mut target.preview,
        &incoming.preview,
        &mut conflicts,
    );
    merge_session_field(
        "parent_id",
        &mut target.parent_id,
        &incoming.parent_id,
        &mut conflicts,
    );
    merge_session_field(
        "cli_version",
        &mut target.cli_version,
        &incoming.cli_version,
        &mut conflicts,
    );
    merge_session_field(
        "originator",
        &mut target.originator,
        &incoming.originator,
        &mut conflicts,
    );
    merge_session_field(
        "history_mode",
        &mut target.history_mode,
        &incoming.history_mode,
        &mut conflicts,
    );
    merge_session_field(
        "reasoning_effort",
        &mut target.reasoning_effort,
        &incoming.reasoning_effort,
        &mut conflicts,
    );
    conflicts
}

fn merge_session_field<T: Clone + Debug + PartialEq>(
    field: &'static str,
    target: &mut Option<T>,
    incoming: &Option<T>,
    conflicts: &mut Vec<SessionConflict>,
) {
    match (target.as_ref(), incoming.as_ref()) {
        (None, Some(value)) => *target = Some(value.clone()),
        (Some(existing), Some(value)) if existing != value => conflicts.push(SessionConflict {
            field,
            existing: format!("{existing:?}"),
            incoming: format!("{value:?}"),
        }),
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLifecycleEvent {
    pub kind: String,
    pub timestamp: Option<String>,
    pub sequence: usize,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub session_id: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub sequence: usize,
    pub lifecycle: Vec<TurnLifecycleEvent>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    Other(String),
}

impl MessageRole {
    pub fn as_str(&self) -> &str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Other(role) => role,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub role: Option<MessageRole>,
    pub content: Option<String>,
    pub timestamp: Option<String>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Unknown,
}

impl ToolOutcome {
    pub(crate) fn from_status(status: &str) -> Option<Self> {
        let status = status
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        if matches!(
            status.as_str(),
            "success" | "succeeded" | "complete" | "completed" | "ok"
        ) {
            Some(Self::Succeeded)
        } else if matches!(
            status.as_str(),
            "failure"
                | "failed"
                | "error"
                | "cancelled"
                | "canceled"
                | "aborted"
                | "timeout"
                | "timed_out"
        ) {
            Some(Self::Failed)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutcomeSource {
    ExitCode,
    Status,
    OutputText,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: Option<String>,
    pub call_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub tool_name: Option<String>,
    pub input_summary: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub status: Option<String>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: Option<String>,
    pub call_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: Option<i64>,
    pub exit_code: Option<i64>,
    pub status: Option<String>,
    pub outcome: ToolOutcome,
    pub outcome_source: OutcomeSource,
    pub matched_call: bool,
    pub deduplication_key: Option<String>,
    pub equivalent_to: Option<SourceRef>,
    pub is_duplicate: bool,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    SessionMetadata,
    TurnContext,
    ResponseItem,
    EventMessage,
    Compacted,
    WorldState,
    Unknown {
        record_type: Option<String>,
        nested_type: Option<String>,
        raw_json: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub timestamp: Option<String>,
    pub sequence: usize,
    pub original_record_type: Option<String>,
    pub original_nested_type: Option<String>,
    #[serde(default)]
    pub error_category: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub is_terminal: bool,
    pub kind: RecordKind,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub timestamp: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub sequence: usize,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOperation {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub path: String,
    pub operation: String,
    pub timestamp: Option<String>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionScope {
    Global,
    ProjectRoot,
    ProjectNested,
}

impl InstructionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::ProjectRoot => "project_root",
            Self::ProjectNested => "project_nested",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionFileKind {
    Override,
    Standard,
    Fallback(String),
    Observed,
}

impl InstructionFileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Standard => "standard",
            Self::Fallback(_) => "fallback",
            Self::Observed => "observed",
        }
    }

    pub fn fallback_name(&self) -> Option<&str> {
        match self {
            Self::Fallback(name) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionFileState {
    Selected,
    Empty,
    Missing,
    Unreadable,
    Truncated,
}

impl InstructionFileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Empty => "empty",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Truncated => "truncated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub scope: InstructionScope,
    pub kind: InstructionFileKind,
    pub state: InstructionFileState,
    pub chain_position: Option<usize>,
    pub content: Option<String>,
    pub content_hash: Option<String>,
    pub byte_count: usize,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectRootStatus {
    Known,
    Missing,
    Conflict,
    Unavailable,
}

impl ProjectRootStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Known => "known",
            Self::Missing => "missing",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionDiagnosticKind {
    Config,
    GlobalScopeUnavailable,
    MissingProjectRoot,
    MissingCwd,
    ProjectRootNotDirectory,
    CwdOutsideProjectRoot,
    RelativePath,
    Unreadable,
    Truncated,
}

impl InstructionDiagnosticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::GlobalScopeUnavailable => "global_scope_unavailable",
            Self::MissingProjectRoot => "missing_project_root",
            Self::MissingCwd => "missing_cwd",
            Self::ProjectRootNotDirectory => "project_root_not_directory",
            Self::CwdOutsideProjectRoot => "cwd_outside_project_root",
            Self::RelativePath => "relative_path",
            Self::Unreadable => "unreadable",
            Self::Truncated => "truncated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionDiagnostic {
    pub path: Option<PathBuf>,
    pub kind: InstructionDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionResolution {
    pub project_root: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub project_root_status: ProjectRootStatus,
    pub files: Vec<InstructionFile>,
    pub chain: Vec<InstructionFile>,
    pub effective_content: Option<String>,
    pub effective_chain_hash: Option<String>,
    pub byte_count: usize,
    pub truncated: bool,
    pub diagnostics: Vec<InstructionDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionSnapshotSource {
    Rollout,
    FilesystemAtIngest,
    Unavailable,
}

impl InstructionSnapshotSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rollout => "rollout",
            Self::FilesystemAtIngest => "filesystem_at_ingest",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionSnapshotAccuracy {
    Observed,
    Reconstructed,
    Unavailable,
}

impl InstructionSnapshotAccuracy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Reconstructed => "reconstructed",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSnapshotEntry {
    pub path: PathBuf,
    pub scope: Option<InstructionScope>,
    pub kind: InstructionFileKind,
    pub state: InstructionFileState,
    pub chain_position: usize,
    pub content_hash: Option<String>,
    pub byte_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSnapshot {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub source: InstructionSnapshotSource,
    pub accuracy: InstructionSnapshotAccuracy,
    pub content: Option<String>,
    pub content_hash: Option<String>,
    pub byte_count: usize,
    pub chain: Vec<InstructionSnapshotEntry>,
    pub effective_chain_hash: Option<String>,
    pub truncated: bool,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionJoin {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub project_root_status: ProjectRootStatus,
    pub resolution: InstructionResolution,
    pub nearest_path: Option<PathBuf>,
    pub nearest_scope: Option<InstructionScope>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticKind {
    MalformedJson,
    OversizedLine,
    Unreadable,
    StateSchemaMismatch,
    StateQuery,
    MetadataConflict,
    UnsupportedReader,
}

impl DiagnosticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MalformedJson => "malformed_json",
            Self::OversizedLine => "oversized_line",
            Self::Unreadable => "unreadable",
            Self::StateSchemaMismatch => "state_schema_mismatch",
            Self::StateQuery => "state_query",
            Self::MetadataConflict => "metadata_conflict",
            Self::UnsupportedReader => "unsupported_reader",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalDiagnostic {
    pub kind: DiagnosticKind,
    pub source: SourceRef,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalData {
    pub sessions: Vec<Session>,
    pub turns: Vec<Turn>,
    pub records: Vec<Record>,
    pub messages: Vec<Message>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub file_operations: Vec<FileOperation>,
    pub token_usage: Vec<TokenUsage>,
    pub diagnostics: Vec<CanonicalDiagnostic>,
    pub instruction_snapshots: Vec<InstructionSnapshot>,
    pub instruction_joins: Vec<InstructionJoin>,
}
