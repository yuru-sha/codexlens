use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub path: PathBuf,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonlLine {
    pub line: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadLine {
    Line(JsonlLine),
    Oversized { line: usize, byte_count: usize },
}

pub trait RolloutLineReader {
    fn next_line(&mut self) -> io::Result<Option<ReadLine>>;
}

pub struct PlainJsonlReader<R> {
    reader: BufReader<R>,
    max_line_bytes: usize,
    next_line: usize,
}

impl<R: Read> PlainJsonlReader<R> {
    pub fn new(reader: R) -> Self {
        Self::with_max_line_bytes(reader, DEFAULT_MAX_LINE_BYTES)
    }

    pub fn with_max_line_bytes(reader: R, max_line_bytes: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            max_line_bytes,
            next_line: 1,
        }
    }

    pub fn next_line(&mut self) -> io::Result<Option<ReadLine>> {
        let line = self.next_line;
        let mut bytes = Vec::new();
        let mut byte_count = 0usize;
        let mut oversized = false;

        loop {
            let (consumed, ended) = {
                let chunk = self.reader.fill_buf()?;
                if chunk.is_empty() {
                    if byte_count == 0 {
                        return Ok(None);
                    }
                    (0, true)
                } else {
                    let newline = chunk.iter().position(|byte| *byte == b'\n');
                    let content_len = newline.unwrap_or(chunk.len());
                    if !oversized {
                        let remaining = self.max_line_bytes.saturating_sub(byte_count);
                        if content_len <= remaining {
                            bytes.extend_from_slice(&chunk[..content_len]);
                        } else {
                            bytes.extend_from_slice(&chunk[..remaining]);
                            oversized = true;
                        }
                    }
                    byte_count = byte_count.saturating_add(content_len);
                    (
                        newline.map_or(chunk.len(), |index| index + 1),
                        newline.is_some(),
                    )
                }
            };

            if consumed != 0 {
                self.reader.consume(consumed);
            }
            if ended {
                self.next_line = self.next_line.saturating_add(1);
                return Ok(Some(if oversized {
                    ReadLine::Oversized { line, byte_count }
                } else {
                    ReadLine::Line(JsonlLine { line, bytes })
                }));
            }
        }
    }
}

impl<R: Read> RolloutLineReader for PlainJsonlReader<R> {
    fn next_line(&mut self) -> io::Result<Option<ReadLine>> {
        Self::next_line(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutParseOptions {
    pub max_line_bytes: usize,
}

impl Default for RolloutParseOptions {
    fn default() -> Self {
        Self {
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RolloutParseResult {
    pub records: Vec<RolloutRecord>,
    pub diagnostics: Vec<ParseDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseDiagnosticKind {
    MalformedJson,
    OversizedLine,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub source: SourceLocation,
    pub kind: ParseDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RolloutRecord {
    pub source: SourceLocation,
    pub timestamp: Option<String>,
    pub instruction_context: Option<RolloutInstructionContext>,
    pub kind: RolloutRecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutInstructionContext {
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    pub project_root: Option<String>,
    pub instruction_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RolloutRecordKind {
    Known {
        record_type: KnownRecordType,
        nested_type: Option<String>,
        payload: Option<Value>,
    },
    Unknown(UnknownRecord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownRecordType {
    SessionMeta,
    TurnContext,
    ResponseItem,
    EventMessage,
    Compacted,
    WorldState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnknownRecord {
    pub source: SourceLocation,
    pub timestamp: Option<String>,
    pub record_type: Option<String>,
    pub nested_type: Option<String>,
    pub raw: Value,
}

pub type ParseOptions = RolloutParseOptions;
pub type ParseResult = RolloutParseResult;

#[derive(Debug, Deserialize)]
struct RawEnvelope {
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(default, rename = "type")]
    record_type: Option<Value>,
    #[serde(default)]
    payload: Option<Value>,
}

pub fn parse_rollout(path: &Path, options: &RolloutParseOptions) -> RolloutParseResult {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return RolloutParseResult {
                records: Vec::new(),
                diagnostics: vec![diagnostic(
                    path,
                    1,
                    ParseDiagnosticKind::Unreadable,
                    error.to_string(),
                )],
            };
        }
    };

    parse_rollout_reader(
        path,
        PlainJsonlReader::with_max_line_bytes(file, options.max_line_bytes),
    )
}

pub fn parse_rollout_file(path: &Path, options: &RolloutParseOptions) -> RolloutParseResult {
    parse_rollout(path, options)
}

pub fn parse_rollouts<I, P>(paths: I, options: &RolloutParseOptions) -> RolloutParseResult
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut result = RolloutParseResult::default();
    for path in paths {
        let parsed = parse_rollout(path.as_ref(), options);
        result.records.extend(parsed.records);
        result.diagnostics.extend(parsed.diagnostics);
    }
    result
}

pub fn parse_rollout_reader<R: RolloutLineReader>(
    path: &Path,
    mut reader: R,
) -> RolloutParseResult {
    let mut result = RolloutParseResult::default();
    let mut next_line = 1usize;

    loop {
        match reader.next_line() {
            Ok(Some(ReadLine::Line(line))) => {
                next_line = line.line.saturating_add(1);
                parse_json_line(path, line, &mut result);
            }
            Ok(Some(ReadLine::Oversized { line, byte_count })) => {
                next_line = line.saturating_add(1);
                result.diagnostics.push(diagnostic(
                    path,
                    line,
                    ParseDiagnosticKind::OversizedLine,
                    format!("line exceeds the configured limit ({byte_count} bytes)"),
                ));
            }
            Ok(None) => break,
            Err(error) => {
                result.diagnostics.push(diagnostic(
                    path,
                    next_line,
                    ParseDiagnosticKind::Unreadable,
                    error.to_string(),
                ));
                break;
            }
        }
    }

    result
}

fn parse_json_line(path: &Path, line: JsonlLine, result: &mut RolloutParseResult) {
    let bytes = trim_ascii_whitespace(&line.bytes);
    if bytes.is_empty() {
        return;
    }

    let raw = match serde_json::from_slice::<Value>(bytes) {
        Ok(raw) => raw,
        Err(error) => {
            result.diagnostics.push(diagnostic(
                path,
                line.line,
                ParseDiagnosticKind::MalformedJson,
                error.to_string(),
            ));
            return;
        }
    };

    result.records.push(classify_record(
        SourceLocation {
            path: path.to_path_buf(),
            line: line.line,
        },
        raw,
    ));
}

fn classify_record(source: SourceLocation, raw: Value) -> RolloutRecord {
    let Some(object) = raw.as_object() else {
        return unknown_record(source, None, None, None, raw);
    };

    let envelope = match serde_json::from_value::<RawEnvelope>(Value::Object(object.clone())) {
        Ok(envelope) => envelope,
        Err(_) => return unknown_record(source, None, None, None, raw),
    };
    let timestamp = optional_string(envelope.timestamp);
    let record_type = optional_string(envelope.record_type);
    let (nested_type, nested_type_is_invalid) = nested_type(envelope.payload.as_ref());
    let known_type = record_type.as_deref().and_then(known_record_type);
    let nested_is_known = !nested_type_is_invalid
        && match (known_type, nested_type.as_deref()) {
            (Some(KnownRecordType::ResponseItem), Some(nested)) => {
                is_known_response_item_type(nested)
            }
            (Some(KnownRecordType::EventMessage), Some(nested)) => is_known_event_type(nested),
            _ => true,
        };

    if let Some(record_type) = known_type.filter(|_| nested_is_known) {
        let instruction_context = (record_type == KnownRecordType::TurnContext)
            .then(|| parse_instruction_context(envelope.payload.as_ref()))
            .flatten();
        RolloutRecord {
            source,
            timestamp,
            instruction_context,
            kind: RolloutRecordKind::Known {
                record_type,
                nested_type,
                payload: envelope.payload,
            },
        }
    } else {
        unknown_record(source, timestamp, record_type, nested_type, raw)
    }
}

fn unknown_record(
    source: SourceLocation,
    timestamp: Option<String>,
    record_type: Option<String>,
    nested_type: Option<String>,
    raw: Value,
) -> RolloutRecord {
    RolloutRecord {
        source: source.clone(),
        timestamp: timestamp.clone(),
        instruction_context: None,
        kind: RolloutRecordKind::Unknown(UnknownRecord {
            source,
            timestamp,
            record_type,
            nested_type,
            raw,
        }),
    }
}

fn parse_instruction_context(payload: Option<&Value>) -> Option<RolloutInstructionContext> {
    let object = payload?.as_object()?;
    Some(RolloutInstructionContext {
        turn_id: object
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: object.get("cwd").and_then(Value::as_str).map(str::to_owned),
        project_root: ["project", "project_root", "project_path"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .map(str::to_owned),
        instruction_text: object
            .get("user_instructions")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn known_record_type(record_type: &str) -> Option<KnownRecordType> {
    match record_type {
        "session_meta" => Some(KnownRecordType::SessionMeta),
        "turn_context" => Some(KnownRecordType::TurnContext),
        "response_item" => Some(KnownRecordType::ResponseItem),
        "event_msg" => Some(KnownRecordType::EventMessage),
        "compacted" => Some(KnownRecordType::Compacted),
        "world_state" => Some(KnownRecordType::WorldState),
        _ => None,
    }
}

fn is_known_response_item_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "message"
            | "agent_message"
            | "reasoning"
            | "local_shell_call"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "mcp_tool_call"
            | "mcp_tool_call_output"
            | "tool_search_call"
            | "tool_search_output"
            | "web_search_call"
            | "image_generation_call"
            | "computer_call"
            | "computer_call_output"
            | "compaction"
            | "ghost_snapshot"
    )
}

fn is_known_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "error"
            | "warning"
            | "context_compacted"
            | "thread_rolled_back"
            | "task_started"
            | "turn_started"
            | "thread_settings_applied"
            | "task_complete"
            | "turn_complete"
            | "token_count"
            | "agent_message"
            | "user_message"
            | "agent_reasoning"
            | "agent_reasoning_raw_content"
            | "agent_reasoning_section_break"
            | "session_configured"
            | "environment_connected"
            | "environment_disconnected"
            | "thread_goal_updated"
            | "thread_queue_changed"
            | "mcp_startup_update"
            | "mcp_startup_complete"
            | "mcp_tool_call_begin"
            | "mcp_tool_call_end"
            | "web_search_begin"
            | "web_search_end"
            | "image_generation_begin"
            | "image_generation_end"
            | "exec_command_begin"
            | "exec_command_output_delta"
            | "terminal_interaction"
            | "exec_command_end"
            | "exec_approval_request"
            | "request_permissions"
            | "request_user_input"
            | "dynamic_tool_call_request"
            | "dynamic_tool_call_response"
            | "elicitation_request"
            | "apply_patch_approval_request"
            | "guardian_assessment"
            | "deprecation_notice"
            | "stream_error"
            | "patch_apply_begin"
            | "patch_apply_updated"
            | "patch_apply_end"
            | "turn_diff"
            | "plan_update"
            | "turn_aborted"
            | "shutdown_complete"
            | "entered_review_mode"
            | "exited_review_mode"
            | "raw_response_item"
            | "raw_response_completed"
            | "item_started"
            | "item_completed"
            | "hook_started"
            | "hook_completed"
            | "agent_message_content_delta"
            | "plan_delta"
            | "reasoning_content_delta"
            | "reasoning_raw_content_delta"
            | "collab_agent_spawn_begin"
            | "collab_agent_spawn_end"
            | "collab_agent_interaction_begin"
            | "collab_agent_interaction_end"
            | "collab_waiting_begin"
            | "collab_waiting_end"
            | "collab_close_begin"
            | "collab_close_end"
            | "collab_resume_begin"
            | "collab_resume_end"
            | "sub_agent_activity"
    )
}

fn optional_string(value: Option<Value>) -> Option<String> {
    value.and_then(|value| value.as_str().map(str::to_owned))
}

fn nested_type(payload: Option<&Value>) -> (Option<String>, bool) {
    let Some(object) = payload.and_then(Value::as_object) else {
        return (None, false);
    };
    let Some(value) = object.get("type") else {
        return (None, false);
    };
    match value.as_str() {
        Some(value) => (Some(value.to_owned()), false),
        None => (None, true),
    }
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn diagnostic(
    path: &Path,
    line: usize,
    kind: ParseDiagnosticKind,
    message: String,
) -> ParseDiagnostic {
    ParseDiagnostic {
        source: SourceLocation {
            path: path.to_path_buf(),
            line,
        },
        kind,
        message: bounded_message(&message),
    }
}

fn bounded_message(message: &str) -> String {
    if message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return message.to_owned();
    }

    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const BASIC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/rollout/basic.jsonl");
    const DEFENSIVE_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/rollout/defensive.jsonl");

    fn parse_bytes(bytes: &[u8]) -> RolloutParseResult {
        parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(bytes)),
        )
    }

    #[test]
    fn parses_known_records_and_retains_unknown_top_level_records() {
        let result = parse_bytes(BASIC_FIXTURE);

        assert_eq!(result.records.len(), 9);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(matches!(
            result.records[0].kind,
            RolloutRecordKind::Known {
                record_type: KnownRecordType::SessionMeta,
                ..
            }
        ));
        assert_eq!(
            result.records[1]
                .instruction_context
                .as_ref()
                .and_then(|context| context.instruction_text.as_deref()),
            Some("# AGENTS.md instructions for /fixture/project\nRun cargo test.")
        );
        let unknown = match &result.records[8].kind {
            RolloutRecordKind::Unknown(unknown) => unknown,
            other => panic!("expected unknown record, got {other:?}"),
        };
        assert_eq!(unknown.source.path, Path::new("fixture.jsonl"));
        assert_eq!(unknown.source.line, 9);
        assert_eq!(unknown.record_type.as_deref(), Some("future_record"));
        assert_eq!(unknown.raw["payload"]["data"]["keep"], Value::Bool(true));
    }

    #[test]
    fn optional_envelope_fields_and_unknown_envelope_fields_are_tolerated() {
        let result = parse_bytes(DEFENSIVE_FIXTURE);

        assert_eq!(result.records.len(), 4);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].kind,
            ParseDiagnosticKind::MalformedJson
        );
        match &result.records[0].kind {
            RolloutRecordKind::Known { payload, .. } => assert!(payload.is_none()),
            other => panic!("expected known record, got {other:?}"),
        }
    }

    #[test]
    fn unknown_nested_types_keep_the_complete_valid_record() {
        let result = parse_bytes(
            br#"{"type":"response_item","payload":{"type":"future_item","value":1}}
{"type":"event_msg","payload":{"type":"future_event","value":2}}"#,
        );

        assert!(result.diagnostics.is_empty());
        assert_eq!(result.records.len(), 2);
        for (record, expected_type) in result.records.iter().zip(["future_item", "future_event"]) {
            let unknown = match &record.kind {
                RolloutRecordKind::Unknown(unknown) => unknown,
                other => panic!("expected unknown record, got {other:?}"),
            };
            assert_eq!(unknown.nested_type.as_deref(), Some(expected_type));
            assert_eq!(unknown.source.line, record.source.line);
        }
    }

    #[test]
    fn malformed_and_oversized_lines_do_not_stop_following_records() {
        let oversized = format!(
            "{{\"type\":\"session_meta\",\"payload\":\"{}\"}}",
            "x".repeat(128)
        );
        let input = format!("{{broken\n{oversized}\n{{\"type\":\"world_state\"}}\n");
        let result = parse_rollout_reader(
            Path::new("bounded.jsonl"),
            PlainJsonlReader::with_max_line_bytes(Cursor::new(input.into_bytes()), 64),
        );

        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].source.line, 3);
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(result.diagnostics[0].source.line, 1);
        assert_eq!(
            result.diagnostics[0].source.path,
            Path::new("bounded.jsonl")
        );
        assert_eq!(
            result.diagnostics[0].kind,
            ParseDiagnosticKind::MalformedJson
        );
        assert_eq!(result.diagnostics[1].source.line, 2);
        assert_eq!(
            result.diagnostics[1].source.path,
            Path::new("bounded.jsonl")
        );
        assert_eq!(
            result.diagnostics[1].kind,
            ParseDiagnosticKind::OversizedLine
        );
        assert!(
            result
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES)
        );
    }

    #[test]
    fn empty_files_produce_no_records_or_diagnostics() {
        let result = parse_bytes(&[]);

        assert!(result.records.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn multiple_files_keep_valid_results_when_one_file_is_unreadable() {
        let missing = std::env::temp_dir().join(format!(
            "codexlens-missing-rollout-{}-{}.jsonl",
            std::process::id(),
            BASIC_FIXTURE.len()
        ));
        let valid =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rollout/basic.jsonl");
        let result = parse_rollouts([missing, valid], &RolloutParseOptions::default());

        assert_eq!(result.records.len(), 9);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].kind, ParseDiagnosticKind::Unreadable);
    }
}
