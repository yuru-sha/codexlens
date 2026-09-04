use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::rollout::SourceLocation;

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_SUMMARY_BYTES: usize = 8 * 1024;
pub const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub path: PathBuf,
    pub line: Option<usize>,
}

impl SourceRef {
    pub fn state(path: PathBuf) -> Self {
        Self { path, line: None }
    }
}

impl From<SourceLocation> for SourceRef {
    fn from(source: SourceLocation) -> Self {
        Self {
            path: source.path,
            line: Some(source.line),
        }
    }
}

impl From<&SourceLocation> for SourceRef {
    fn from(source: &SourceLocation) -> Self {
        Self {
            path: source.path.clone(),
            line: Some(source.line),
        }
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
    pub content: String,
    pub timestamp: Option<String>,
    pub provenance: SourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Unknown,
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
}
