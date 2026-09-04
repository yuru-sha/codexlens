use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::{Map, Value};

use crate::instructions::{
    InstructionResolver, join_sessions, snapshot_from_resolution, snapshot_from_rollout,
    unavailable_snapshot,
};
use crate::model::{
    CanonicalData, CanonicalDiagnostic, DiagnosticKind, MAX_MESSAGE_BYTES, MAX_TOOL_OUTPUT_BYTES,
    MAX_TOOL_SUMMARY_BYTES, Message, MessageRole, OutcomeSource, Record, RecordKind, Session,
    SourceRef, TokenUsage, ToolCall, ToolOutcome, ToolResult, Turn, TurnLifecycleEvent,
    merge_session_fields,
};
use crate::rollout::{KnownRecordType, ParseDiagnostic, RolloutParseResult, RolloutRecord};
use crate::state::StateReadResult;

pub fn normalize_rollout(result: &RolloutParseResult) -> CanonicalData {
    normalize_rollout_with_state(result, &[])
}

pub fn normalize_rollout_with_state(
    result: &RolloutParseResult,
    state: &[Session],
) -> CanonicalData {
    normalize_rollout_with_resolver(result, state, None)
}

pub fn normalize_rollout_with_instructions(
    result: &RolloutParseResult,
    state: &[Session],
    resolver: &InstructionResolver,
) -> CanonicalData {
    normalize_rollout_with_resolver(result, state, Some(resolver))
}

fn normalize_rollout_with_resolver(
    result: &RolloutParseResult,
    state: &[Session],
    resolver: Option<&InstructionResolver>,
) -> CanonicalData {
    let mut data = normalize_records_with_resolver(&result.records, state, resolver);
    data.diagnostics
        .extend(result.diagnostics.iter().map(canonical_parse_diagnostic));
    data.diagnostics.sort_by(|left, right| {
        left.source
            .path
            .cmp(&right.source.path)
            .then_with(|| left.source.line.cmp(&right.source.line))
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
    });
    data
}

pub fn normalize_rollout_result(
    result: &RolloutParseResult,
    state: &StateReadResult,
) -> CanonicalData {
    let mut data = normalize_rollout_with_state(result, &state.sessions);
    data.diagnostics.extend(
        state
            .diagnostics
            .iter()
            .map(|diagnostic| CanonicalDiagnostic {
                kind: diagnostic.kind.canonical_kind(),
                source: diagnostic.source.clone(),
                message: diagnostic.message.clone(),
            }),
    );
    data
}

pub fn normalize_records(records: &[RolloutRecord], state: &[Session]) -> CanonicalData {
    normalize_records_with_resolver(records, state, None)
}

fn normalize_records_with_resolver(
    records: &[RolloutRecord],
    state: &[Session],
    resolver: Option<&InstructionResolver>,
) -> CanonicalData {
    let mut data = CanonicalData::default();
    let mut sessions = BTreeMap::new();
    let source_path = records.first().map(|record| record.source.path.clone());
    let matched_state_session_id =
        matching_state_session(source_path.as_deref(), state).map(|session| session.id.clone());
    let mut current_session_id = matched_state_session_id
        .as_deref()
        .and_then(|session_id| state.iter().find(|session| session.id == session_id))
        .map(|session| {
            sessions.insert(session.id.clone(), session.clone());
            session.id.clone()
        });
    let mut current_turn_id = None;

    for (index, record) in records.iter().enumerate() {
        let sequence = index + 1;
        let source = SourceRef::from(&record.source);
        let payload = known_payload(record);
        let explicit_turn_id = payload
            .and_then(Value::as_object)
            .and_then(|payload| string_field(payload, &["turn_id"]));
        let record_turn_id = explicit_turn_id.clone().or_else(|| current_turn_id.clone());

        match &record.kind {
            crate::rollout::RolloutRecordKind::Known {
                record_type: KnownRecordType::SessionMeta,
                payload,
                ..
            } => {
                if let Some(candidate) = session_from_payload(
                    payload.as_ref(),
                    record.timestamp.as_deref(),
                    source.clone(),
                    &mut data.diagnostics,
                ) {
                    if matched_state_session_id
                        .as_deref()
                        .is_some_and(|state_id| state_id != candidate.id)
                    {
                        data.diagnostics.push(CanonicalDiagnostic {
                            kind: DiagnosticKind::MetadataConflict,
                            source: source.clone(),
                            message: bounded(&format!(
                                "state and rollout session identities differ: state={:?}, rollout={:?}",
                                matched_state_session_id, candidate.id
                            )),
                        });
                    }
                    if let Some(state_session) =
                        state.iter().find(|session| session.id == candidate.id)
                    {
                        if state_session
                            .rollout_path
                            .as_deref()
                            .is_some_and(|path| !same_path(Path::new(path), &source.path))
                        {
                            data.diagnostics.push(CanonicalDiagnostic {
                                kind: DiagnosticKind::MetadataConflict,
                                source: source.clone(),
                                message: bounded(
                                    "state and rollout session paths differ; state metadata was retained as enrichment",
                                ),
                            });
                        }
                    }
                    current_session_id = Some(candidate.id.clone());
                    current_turn_id = None;
                    merge_rollout_session(&mut sessions, candidate, state, &mut data.diagnostics);
                }
            }
            crate::rollout::RolloutRecordKind::Known {
                record_type: KnownRecordType::TurnContext,
                payload,
                ..
            } => {
                if let Some(turn_id) = explicit_turn_id.clone() {
                    current_turn_id = Some(turn_id.clone());
                    add_or_update_turn(
                        &mut data,
                        turn_id,
                        current_session_id.clone(),
                        payload.as_ref(),
                        record.timestamp.clone(),
                        sequence,
                        source.clone(),
                    );
                }
                data.instruction_snapshots.push(turn_context_snapshot(
                    payload.as_ref(),
                    current_session_id.clone(),
                    record_turn_id.clone(),
                    &sessions,
                    resolver,
                    source.clone(),
                ));
            }
            crate::rollout::RolloutRecordKind::Known {
                record_type: KnownRecordType::ResponseItem,
                nested_type,
                payload,
            } => {
                if let Some(payload) = payload.as_ref().and_then(Value::as_object) {
                    let turn_id = explicit_turn_id.clone().or_else(|| current_turn_id.clone());
                    if is_message_type(nested_type.as_deref()) {
                        data.messages.push(message_from_payload(
                            payload,
                            current_session_id.clone(),
                            turn_id.clone(),
                            record.timestamp.clone(),
                            source.clone(),
                        ));
                    } else if is_tool_call_type(nested_type.as_deref()) {
                        data.tool_calls.push(tool_call_from_payload(
                            payload,
                            nested_type.as_deref(),
                            current_session_id.clone(),
                            turn_id.clone(),
                            source.clone(),
                        ));
                    } else if is_tool_result_type(nested_type.as_deref()) {
                        data.tool_results.push(tool_result_from_payload(
                            payload,
                            current_session_id.clone(),
                            turn_id.clone(),
                            source.clone(),
                        ));
                    }
                }
            }
            crate::rollout::RolloutRecordKind::Known {
                record_type: KnownRecordType::EventMessage,
                nested_type,
                payload,
            } => {
                if let Some(payload) = payload.as_ref().and_then(Value::as_object) {
                    let event_turn_id =
                        explicit_turn_id.clone().or_else(|| current_turn_id.clone());
                    if is_lifecycle_type(nested_type.as_deref()) {
                        if let Some(turn_id) = event_turn_id.clone() {
                            if matches!(nested_type.as_deref(), Some("turn_started")) {
                                current_turn_id = Some(turn_id.clone());
                            }
                            add_lifecycle(
                                &mut data,
                                turn_id,
                                current_session_id.clone(),
                                nested_type.as_deref().unwrap_or("lifecycle"),
                                record.timestamp.clone(),
                                sequence,
                                source.clone(),
                            );
                        }
                    }
                    if is_event_tool_call_type(nested_type.as_deref()) {
                        data.tool_calls.push(tool_call_from_event(
                            payload,
                            nested_type.as_deref(),
                            current_session_id.clone(),
                            event_turn_id.clone(),
                            source.clone(),
                        ));
                    } else if is_event_tool_result_type(nested_type.as_deref()) {
                        data.tool_results.push(tool_result_from_payload(
                            payload,
                            current_session_id.clone(),
                            event_turn_id,
                            source.clone(),
                        ));
                    }
                    if nested_type.as_deref() == Some("token_count") {
                        data.token_usage.push(token_usage_from_payload(
                            payload,
                            current_session_id.clone(),
                            record_turn_id.clone(),
                            record.timestamp.clone(),
                            sequence,
                            source.clone(),
                        ));
                    }
                }
            }
            _ => {}
        }

        let (original_record_type, original_nested_type) = original_record_types(record);
        data.records.push(Record {
            session_id: current_session_id.clone(),
            turn_id: record_turn_id,
            timestamp: record.timestamp.clone(),
            sequence,
            original_record_type,
            original_nested_type,
            kind: record_kind(record),
            provenance: source,
        });
    }

    data.sessions = sessions.into_values().collect();
    if let Some(resolver) = resolver {
        data.instruction_joins = join_sessions(&data.sessions, resolver);
    }
    deduplicate_token_usage(&mut data.token_usage);
    mark_tool_results(&mut data.tool_calls, &mut data.tool_results);
    mark_duplicate_tool_results(&mut data.tool_results);
    data
}

fn turn_context_snapshot(
    payload: Option<&Value>,
    session_id: Option<String>,
    turn_id: Option<String>,
    sessions: &BTreeMap<String, Session>,
    resolver: Option<&InstructionResolver>,
    provenance: SourceRef,
) -> crate::model::InstructionSnapshot {
    let object = payload.and_then(Value::as_object);
    let rollout_instructions = object
        .and_then(|object| object.get("user_instructions"))
        .and_then(Value::as_str);
    if let Some(content) = rollout_instructions {
        return snapshot_from_rollout(session_id, turn_id, Some(content), provenance);
    }
    let Some(resolver) = resolver else {
        return unavailable_snapshot(session_id, turn_id, provenance);
    };
    let session = session_id.as_deref().and_then(|id| sessions.get(id));
    let project_root = object
        .and_then(|object| string_field(object, &["project", "project_root", "project_path"]))
        .or_else(|| session.and_then(|session| session.project.clone()));
    let cwd = object
        .and_then(|object| string_field(object, &["cwd"]))
        .or_else(|| session.and_then(|session| session.cwd.clone()));
    let resolution = resolver.resolve(
        project_root.as_deref().map(std::path::Path::new),
        cwd.as_deref().map(std::path::Path::new),
    );
    snapshot_from_resolution(session_id, turn_id, &resolution, provenance)
}

fn canonical_parse_diagnostic(diagnostic: &ParseDiagnostic) -> CanonicalDiagnostic {
    CanonicalDiagnostic {
        kind: match diagnostic.kind {
            crate::rollout::ParseDiagnosticKind::MalformedJson => DiagnosticKind::MalformedJson,
            crate::rollout::ParseDiagnosticKind::OversizedLine => DiagnosticKind::OversizedLine,
            crate::rollout::ParseDiagnosticKind::Unreadable => DiagnosticKind::Unreadable,
        },
        source: SourceRef::from(&diagnostic.source),
        message: diagnostic.message.clone(),
    }
}

fn matching_state_session<'a>(
    path: Option<&std::path::Path>,
    state: &'a [Session],
) -> Option<&'a Session> {
    let path = path?;
    state.iter().find(|session| {
        session.rollout_path.as_deref().is_some_and(|rollout_path| {
            let rollout_path = std::path::Path::new(rollout_path);
            same_path(rollout_path, path)
        })
    })
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || (left.is_absolute()
            && right.is_absolute()
            && std::fs::canonicalize(left)
                .ok()
                .is_some_and(|resolved| resolved == right))
        || (left.is_absolute()
            && right.is_absolute()
            && std::fs::canonicalize(right)
                .ok()
                .is_some_and(|resolved| resolved == left))
}

fn merge_rollout_session(
    sessions: &mut BTreeMap<String, Session>,
    candidate: Session,
    state: &[Session],
    diagnostics: &mut Vec<CanonicalDiagnostic>,
) {
    if let Some(existing) = sessions.get_mut(&candidate.id) {
        if existing.provenance.path == candidate.provenance.path {
            merge_session(existing, &candidate, diagnostics);
            return;
        }
        let mut merged = candidate.clone();
        merge_session(&mut merged, existing, diagnostics);
        *existing = merged;
        return;
    }
    let mut merged = candidate.clone();
    if let Some(state_session) = state.iter().find(|session| session.id == candidate.id) {
        merge_session(&mut merged, state_session, diagnostics);
    }
    sessions.insert(merged.id.clone(), merged);
}

fn merge_session(
    target: &mut Session,
    incoming: &Session,
    diagnostics: &mut Vec<CanonicalDiagnostic>,
) {
    for conflict in merge_session_fields(target, incoming) {
        diagnostics.push(CanonicalDiagnostic {
            kind: DiagnosticKind::MetadataConflict,
            source: incoming.provenance.clone(),
            message: bounded(&format!(
                "session metadata conflict for {}: {} vs {}",
                conflict.field, conflict.existing, conflict.incoming
            )),
        });
    }
}

fn session_from_payload(
    payload: Option<&Value>,
    envelope_timestamp: Option<&str>,
    source: SourceRef,
    diagnostics: &mut Vec<CanonicalDiagnostic>,
) -> Option<Session> {
    let payload = payload?.as_object()?;
    let id = string_field(payload, &["id"]);
    let session_id = string_field(payload, &["session_id"]);
    let thread_id = string_field(payload, &["thread_id"]);
    let identity = [id.as_ref(), session_id.as_ref(), thread_id.as_ref()]
        .into_iter()
        .flatten()
        .next()
        .cloned();
    if let Some(identity) = identity.as_ref() {
        if [id.as_ref(), session_id.as_ref(), thread_id.as_ref()]
            .into_iter()
            .flatten()
            .any(|candidate| candidate != identity)
        {
            diagnostics.push(CanonicalDiagnostic {
                kind: DiagnosticKind::MetadataConflict,
                source: source.clone(),
                message: "session metadata contains conflicting identity fields".to_owned(),
            });
        }
    }
    let id = identity?;
    Some(Session {
        id,
        created_at: string_field(payload, &["timestamp", "created_at"])
            .or_else(|| envelope_timestamp.map(str::to_owned)),
        updated_at: string_field(payload, &["updated_at"]),
        cwd: string_field(payload, &["cwd"]),
        project: string_field(payload, &["project", "project_path", "project_root"]),
        model: string_field(payload, &["model"]),
        provider: string_field(payload, &["model_provider", "provider"]),
        source: string_field(payload, &["source"]),
        thread_source: string_field(payload, &["thread_source"]),
        rollout_path: Some(source.path.to_string_lossy().into_owned()),
        archive_state: None,
        title: string_field(payload, &["title"]),
        preview: string_field(payload, &["preview", "first_user_message"]),
        parent_id: string_field(payload, &["parent_thread_id", "parent_id"]),
        cli_version: string_field(payload, &["cli_version"]),
        originator: string_field(payload, &["originator"]),
        history_mode: string_field(payload, &["history_mode"]),
        reasoning_effort: string_field(payload, &["reasoning_effort"]),
        provenance: source,
    })
}

fn add_or_update_turn(
    data: &mut CanonicalData,
    id: String,
    session_id: Option<String>,
    payload: Option<&Value>,
    timestamp: Option<String>,
    sequence: usize,
    source: SourceRef,
) {
    let payload = payload.and_then(Value::as_object);
    let index = data
        .turns
        .iter()
        .position(|turn| turn.id == id && turn.session_id == session_id);
    let turn = if let Some(index) = index {
        &mut data.turns[index]
    } else {
        data.turns.push(Turn {
            id: id.clone(),
            session_id: session_id.clone(),
            started_at: None,
            completed_at: None,
            cwd: None,
            model: None,
            reasoning_effort: None,
            sequence,
            lifecycle: Vec::new(),
            provenance: source,
        });
        data.turns.last_mut().expect("turn was inserted")
    };
    if let Some(payload) = payload {
        turn.cwd = turn.cwd.clone().or_else(|| string_field(payload, &["cwd"]));
        turn.model = turn
            .model
            .clone()
            .or_else(|| string_field(payload, &["model"]));
        turn.reasoning_effort = turn
            .reasoning_effort
            .clone()
            .or_else(|| string_field(payload, &["reasoning_effort"]));
    }
    turn.started_at = turn.started_at.clone().or(timestamp);
}

fn add_lifecycle(
    data: &mut CanonicalData,
    turn_id: String,
    session_id: Option<String>,
    kind: &str,
    timestamp: Option<String>,
    sequence: usize,
    source: SourceRef,
) {
    if !data
        .turns
        .iter()
        .any(|turn| turn.id == turn_id && turn.session_id == session_id)
    {
        data.turns.push(Turn {
            id: turn_id.clone(),
            session_id: session_id.clone(),
            started_at: None,
            completed_at: None,
            cwd: None,
            model: None,
            reasoning_effort: None,
            sequence,
            lifecycle: Vec::new(),
            provenance: source.clone(),
        });
    }
    let turn = data
        .turns
        .iter_mut()
        .find(|turn| turn.id == turn_id && turn.session_id == session_id)
        .expect("turn was inserted or already present");
    if kind == "turn_complete" || kind == "turn_aborted" {
        turn.completed_at = timestamp.clone();
    }
    if kind == "turn_started" {
        turn.started_at = timestamp.clone();
    }
    turn.lifecycle.push(TurnLifecycleEvent {
        kind: kind.to_owned(),
        timestamp,
        sequence,
        provenance: source,
    });
}

fn message_from_payload(
    payload: &Map<String, Value>,
    session_id: Option<String>,
    turn_id: Option<String>,
    timestamp: Option<String>,
    provenance: SourceRef,
) -> Message {
    let content = payload
        .get("content")
        .filter(|value| !value.is_null())
        .or_else(|| payload.get("text"))
        .filter(|value| !value.is_null())
        .map(extract_message_text)
        .map(|content| bounded_to(&content, MAX_MESSAGE_BYTES));
    Message {
        id: string_field(payload, &["id"]),
        session_id,
        turn_id,
        role: string_field(payload, &["role"]).map(parse_role),
        content,
        timestamp,
        provenance,
    }
}

fn tool_call_from_payload(
    payload: &Map<String, Value>,
    nested_type: Option<&str>,
    session_id: Option<String>,
    turn_id: Option<String>,
    provenance: SourceRef,
) -> ToolCall {
    ToolCall {
        id: string_field(payload, &["id"]),
        call_id: string_field(payload, &["call_id"]),
        session_id,
        turn_id,
        tool_name: string_field(payload, &["name", "tool_name"])
            .or_else(|| (nested_type == Some("exec_command")).then(|| "exec_command".to_owned())),
        input_summary: payload
            .get("input")
            .or_else(|| payload.get("arguments"))
            .or_else(|| payload.get("query"))
            .or_else(|| payload.get("command"))
            .map(value_summary),
        command: payload.get("command").map(value_summary),
        cwd: string_field(payload, &["cwd"]),
        status: string_field(payload, &["status"]),
        provenance,
    }
}

fn tool_call_from_event(
    payload: &Map<String, Value>,
    nested_type: Option<&str>,
    session_id: Option<String>,
    turn_id: Option<String>,
    provenance: SourceRef,
) -> ToolCall {
    let mut call = tool_call_from_payload(payload, nested_type, session_id, turn_id, provenance);
    if call.tool_name.is_none() && nested_type == Some("exec_command_begin") {
        call.tool_name = Some("exec_command".to_owned());
    }
    call
}

fn tool_result_from_payload(
    payload: &Map<String, Value>,
    session_id: Option<String>,
    turn_id: Option<String>,
    provenance: SourceRef,
) -> ToolResult {
    let stdout = payload
        .get("stdout")
        .or_else(|| payload.get("output"))
        .map(value_summary);
    let stderr = payload.get("stderr").map(value_summary);
    let exit_code = payload
        .get("exit_code")
        .or_else(|| payload.get("exit"))
        .and_then(value_i64);
    let status = string_field(payload, &["status"]);
    let (outcome, outcome_source) = classify_outcome(
        exit_code,
        status.as_deref(),
        stdout.as_deref(),
        stderr.as_deref(),
    );
    ToolResult {
        id: string_field(payload, &["id"]),
        call_id: string_field(payload, &["call_id"]),
        session_id,
        turn_id,
        command: payload.get("command").map(value_summary),
        cwd: string_field(payload, &["cwd"]),
        stdout,
        stderr,
        duration_ms: payload
            .get("duration_ms")
            .or_else(|| payload.get("duration"))
            .and_then(value_i64),
        exit_code,
        status,
        outcome,
        outcome_source,
        matched_call: false,
        deduplication_key: None,
        equivalent_to: None,
        is_duplicate: false,
        provenance,
    }
}

fn token_usage_from_payload(
    payload: &Map<String, Value>,
    session_id: Option<String>,
    turn_id: Option<String>,
    timestamp: Option<String>,
    sequence: usize,
    provenance: SourceRef,
) -> TokenUsage {
    let usage = payload
        .get("info")
        .and_then(Value::as_object)
        .and_then(|info| info.get("total_token_usage"))
        .and_then(Value::as_object)
        .unwrap_or(payload);
    TokenUsage {
        session_id,
        turn_id,
        timestamp,
        input_tokens: usage.get("input_tokens").and_then(value_u64),
        cached_input_tokens: usage.get("cached_input_tokens").and_then(value_u64),
        output_tokens: usage.get("output_tokens").and_then(value_u64),
        reasoning_output_tokens: usage.get("reasoning_output_tokens").and_then(value_u64),
        sequence,
        provenance,
    }
}

fn mark_tool_results(calls: &mut [ToolCall], results: &mut [ToolResult]) {
    for result in results {
        result.matched_call = result.call_id.as_ref().is_some_and(|call_id| {
            calls.iter().any(|call| {
                call.call_id.as_ref() == Some(call_id)
                    && context_matches(result.session_id.as_deref(), call.session_id.as_deref())
                    && context_matches(result.turn_id.as_deref(), call.turn_id.as_deref())
            })
        });
    }
}

fn context_matches(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn mark_duplicate_tool_results(results: &mut [ToolResult]) {
    // ponytail: O(n^2) result dedup; index by call_id if large histories make it measurable.
    for index in 0..results.len() {
        let duplicate_of =
            (0..index).find(|previous| equivalent_results(&results[*previous], &results[index]));
        if let Some(previous) = duplicate_of {
            results[index].deduplication_key = results[previous]
                .deduplication_key
                .clone()
                .or_else(|| Some(result_key(&results[previous])));
            results[index].equivalent_to = Some(results[previous].provenance.clone());
            results[index].is_duplicate = true;
        } else if results[index].call_id.is_some() {
            results[index].deduplication_key = Some(result_key(&results[index]));
        }
    }
}

fn equivalent_results(left: &ToolResult, right: &ToolResult) -> bool {
    let (Some(left_call), Some(right_call)) = (&left.call_id, &right.call_id) else {
        return false;
    };
    if left_call != right_call {
        return false;
    }
    if !context_matches(left.session_id.as_deref(), right.session_id.as_deref())
        || !context_matches(left.turn_id.as_deref(), right.turn_id.as_deref())
    {
        return false;
    }
    if left
        .exit_code
        .zip(right.exit_code)
        .is_some_and(|(left, right)| left != right)
    {
        return false;
    }
    if left
        .status
        .as_deref()
        .zip(right.status.as_deref())
        .is_some_and(|(left, right)| {
            !normalize_token(left).eq_ignore_ascii_case(&normalize_token(right))
        })
    {
        return false;
    }
    if left
        .command
        .as_deref()
        .zip(right.command.as_deref())
        .is_some_and(|(left, right)| normalize_token(left) != normalize_token(right))
    {
        return false;
    }
    if left
        .cwd
        .as_deref()
        .zip(right.cwd.as_deref())
        .is_some_and(|(left, right)| left != right)
    {
        return false;
    }
    let left_output = combined_output(left);
    let right_output = combined_output(right);
    if !left_output.is_empty() && !right_output.is_empty() && left_output != right_output {
        return false;
    }
    !left_output.is_empty()
        || !right_output.is_empty()
        || left.exit_code.is_some()
        || right.exit_code.is_some()
        || left.status.is_some()
        || right.status.is_some()
}

fn result_key(result: &ToolResult) -> String {
    format!(
        "session:{}:turn:{}:call:{}:{}:{}:{}",
        result.session_id.as_deref().unwrap_or(""),
        result.turn_id.as_deref().unwrap_or(""),
        result.call_id.as_deref().unwrap_or(""),
        result
            .exit_code
            .map_or_else(String::new, |value| value.to_string()),
        normalize_token(result.status.as_deref().unwrap_or("")).to_ascii_lowercase(),
        bounded(&combined_output(result)),
    )
}

fn deduplicate_token_usage(usages: &mut Vec<TokenUsage>) {
    let mut seen = HashSet::new();
    usages.retain(|usage| {
        if usage.session_id.is_none() && usage.turn_id.is_none() {
            return true;
        }
        seen.insert((
            usage.session_id.clone(),
            usage.turn_id.clone(),
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
        ))
    });
}

fn combined_output(result: &ToolResult) -> String {
    [result.stdout.as_deref(), result.stderr.as_deref()]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn classify_outcome(
    exit_code: Option<i64>,
    status: Option<&str>,
    stdout: Option<&str>,
    stderr: Option<&str>,
) -> (ToolOutcome, OutcomeSource) {
    if let Some(exit_code) = exit_code {
        return (
            if exit_code == 0 {
                ToolOutcome::Succeeded
            } else {
                ToolOutcome::Failed
            },
            OutcomeSource::ExitCode,
        );
    }
    if let Some(outcome) = status.and_then(status_outcome) {
        return (outcome, OutcomeSource::Status);
    }
    if output_indicates_failure(stdout, stderr) {
        return (ToolOutcome::Failed, OutcomeSource::OutputText);
    }
    (ToolOutcome::Unknown, OutcomeSource::Unknown)
}

fn output_indicates_failure(stdout: Option<&str>, stderr: Option<&str>) -> bool {
    const ERROR_MARKERS: &[&str] = &[
        "error",
        "failed",
        "failure",
        "command not found",
        "permission denied",
        "traceback",
    ];
    let output = [stdout, stderr]
        .into_iter()
        .flatten()
        .map(normalize_token)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    ERROR_MARKERS.iter().any(|marker| output.contains(marker))
}

fn status_outcome(status: &str) -> Option<ToolOutcome> {
    let status = normalize_token(status).to_ascii_lowercase();
    if matches!(
        status.as_str(),
        "success" | "succeeded" | "complete" | "completed" | "ok"
    ) {
        Some(ToolOutcome::Succeeded)
    } else if matches!(
        status.as_str(),
        "failure" | "failed" | "error" | "cancelled" | "aborted"
    ) {
        Some(ToolOutcome::Failed)
    } else {
        None
    }
}

fn record_kind(record: &RolloutRecord) -> RecordKind {
    match &record.kind {
        crate::rollout::RolloutRecordKind::Known { record_type, .. } => match record_type {
            KnownRecordType::SessionMeta => RecordKind::SessionMetadata,
            KnownRecordType::TurnContext => RecordKind::TurnContext,
            KnownRecordType::ResponseItem => RecordKind::ResponseItem,
            KnownRecordType::EventMessage => RecordKind::EventMessage,
            KnownRecordType::Compacted => RecordKind::Compacted,
            KnownRecordType::WorldState => RecordKind::WorldState,
        },
        crate::rollout::RolloutRecordKind::Unknown(unknown) => RecordKind::Unknown {
            record_type: unknown.record_type.clone(),
            nested_type: unknown.nested_type.clone(),
            raw_json: serde_json::to_string(&unknown.raw).expect("JSON values serialize"),
        },
    }
}

fn original_record_types(record: &RolloutRecord) -> (Option<String>, Option<String>) {
    match &record.kind {
        crate::rollout::RolloutRecordKind::Known {
            record_type,
            nested_type,
            ..
        } => (
            Some(known_record_type_name(*record_type).to_owned()),
            nested_type.clone(),
        ),
        crate::rollout::RolloutRecordKind::Unknown(unknown) => {
            (unknown.record_type.clone(), unknown.nested_type.clone())
        }
    }
}

fn known_record_type_name(record_type: KnownRecordType) -> &'static str {
    match record_type {
        KnownRecordType::SessionMeta => "session_meta",
        KnownRecordType::TurnContext => "turn_context",
        KnownRecordType::ResponseItem => "response_item",
        KnownRecordType::EventMessage => "event_msg",
        KnownRecordType::Compacted => "compacted",
        KnownRecordType::WorldState => "world_state",
    }
}

fn known_payload(record: &RolloutRecord) -> Option<&Value> {
    match &record.kind {
        crate::rollout::RolloutRecordKind::Known { payload, .. } => payload.as_ref(),
        crate::rollout::RolloutRecordKind::Unknown(_) => None,
    }
}

fn is_message_type(kind: Option<&str>) -> bool {
    matches!(kind, Some("message" | "agent_message"))
}

fn is_tool_call_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(
            "local_shell_call"
                | "function_call"
                | "custom_tool_call"
                | "mcp_tool_call"
                | "tool_search_call"
                | "web_search_call"
                | "image_generation_call"
                | "computer_call"
        )
    )
}

fn is_tool_result_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(
            "function_call_output"
                | "custom_tool_call_output"
                | "mcp_tool_call_output"
                | "tool_search_output"
                | "computer_call_output"
        )
    )
}

fn is_event_tool_call_type(kind: Option<&str>) -> bool {
    matches!(kind, Some("exec_command_begin" | "mcp_tool_call_begin"))
}

fn is_event_tool_result_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("exec_command_output_delta" | "exec_command_end" | "mcp_tool_call_end")
    )
}

fn is_lifecycle_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("task_started" | "task_complete" | "turn_started" | "turn_complete" | "turn_aborted")
    )
}

fn parse_role(role: String) -> MessageRole {
    match role.as_str() {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => MessageRole::Other(role),
    }
}

fn extract_message_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(extract_message_text)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("content"))
            .or_else(|| object.get("value"))
            .map(extract_message_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn value_summary(value: &Value) -> String {
    bounded_to(
        &match value {
            Value::String(value) => value.clone(),
            _ => serde_json::to_string(value).expect("JSON values serialize"),
        },
        MAX_TOOL_SUMMARY_BYTES,
    )
}

fn string_field(object: &Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str).map(str::to_owned))
}

fn value_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse::<i64>().ok())
}

fn value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
}

fn normalize_token(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bounded(value: &str) -> String {
    bounded_to(value, MAX_TOOL_OUTPUT_BYTES)
}

fn bounded_to(value: &str, max_bytes: usize) -> String {
    let max_bytes = max_bytes.max(3);
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes - 3;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::rollout::{PlainJsonlReader, parse_rollout_reader};
    use std::io::Cursor;

    fn parse(input: &str) -> CanonicalData {
        let parsed = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(input.as_bytes())),
        );
        normalize_rollout(&parsed)
    }

    #[test]
    fn normalizes_fixture_sessions_messages_turns_and_tools() {
        let data = parse(include_str!("../tests/fixtures/rollout/basic.jsonl"));

        assert_eq!(data.sessions[0].id, "fixture-session-001");
        assert_eq!(data.sessions[0].cwd.as_deref(), Some("/fixture/project"));
        assert_eq!(data.turns[0].id, "fixture-turn-001");
        assert_eq!(data.messages.len(), 2);
        assert_eq!(data.messages[0].role, Some(MessageRole::User));
        assert_eq!(
            data.messages[1].content.as_deref(),
            Some("I will inspect the project first.")
        );
        assert_eq!(data.tool_calls.len(), 1);
        assert_eq!(data.tool_results.len(), 2);
        assert_eq!(data.tool_results[0].exit_code, Some(1));
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Failed);
        assert!(data.tool_results[0].matched_call);
        assert!(data.tool_results[1].is_duplicate);
        assert_eq!(data.token_usage.len(), 1);
        assert_eq!(data.token_usage[0].input_tokens, Some(120));
    }

    #[test]
    fn captures_observed_instruction_payloads_without_claiming_filesystem_history() {
        let instruction_text = "synthetic observed instruction";
        let input = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"fixture-instruction-session\",\"cwd\":\"/fixture/project\",\"project\":\"/fixture\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-instruction-turn-001\",\"cwd\":\"/fixture/project\",\"user_instructions\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-instruction-turn-002\",\"cwd\":\"/fixture/project\",\"user_instructions\":\"{}\"}}}}\n",
            instruction_text, instruction_text
        );
        let parsed = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(input.as_bytes())),
        );
        let data = normalize_rollout_with_instructions(
            &parsed,
            &[],
            &crate::instructions::InstructionResolver::default(),
        );

        assert_eq!(data.instruction_snapshots.len(), 2);
        assert!(data.instruction_snapshots.iter().all(|snapshot| {
            snapshot.source == crate::model::InstructionSnapshotSource::Rollout
                && snapshot.accuracy == crate::model::InstructionSnapshotAccuracy::Observed
                && snapshot.content_hash.is_some()
                && snapshot.effective_chain_hash.is_some()
                && snapshot.chain[0].chain_position == 0
        }));
        assert_eq!(data.instruction_joins.len(), 1);
        assert_eq!(
            data.instruction_joins[0].project_root_status,
            crate::model::ProjectRootStatus::Missing
        );
    }

    #[test]
    fn structured_outcome_beats_error_text() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/structured-outcome.jsonl"
        ));
        let result = &data.tool_results[0];
        assert_eq!(result.outcome, ToolOutcome::Succeeded);
        assert_eq!(result.outcome_source, OutcomeSource::ExitCode);
    }

    #[test]
    fn missing_optional_values_stay_unknown() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/missing-fields.jsonl"
        ));
        assert_eq!(data.messages[0].role, None);
        assert_eq!(data.messages[0].content, None);
        assert_eq!(data.tool_results[0].call_id, None);
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Unknown);
    }

    #[test]
    fn thread_ids_tokens_and_tool_contexts_are_preserved() {
        let data = parse(include_str!("../tests/fixtures/rollout/edge-cases.jsonl"));

        assert_eq!(data.sessions.len(), 2);
        assert_eq!(data.sessions[0].id, "fixture-second-session");
        assert_eq!(data.sessions[1].id, "fixture-thread-only");
        assert_eq!(data.messages[0].content, None);
        assert_eq!(data.tool_results.len(), 2);
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Failed);
        assert_eq!(data.tool_results[0].outcome_source, OutcomeSource::Status);
        assert!(!data.tool_results[0].matched_call);
        assert_eq!(data.tool_results[1].outcome, ToolOutcome::Failed);
        assert_eq!(
            data.tool_results[1].outcome_source,
            OutcomeSource::OutputText
        );
        assert!(!data.tool_results[1].matched_call);
        assert_eq!(data.token_usage.len(), 1);
    }

    #[test]
    fn rollout_values_win_conflicts_and_state_fills_missing_metadata() {
        let parsed = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/conflict.jsonl"
            ))),
        );
        let state = Session {
            id: "fixture-conflict-session".to_owned(),
            created_at: None,
            updated_at: None,
            cwd: Some("/state".to_owned()),
            project: Some("/project".to_owned()),
            model: None,
            provider: Some("provider".to_owned()),
            source: None,
            thread_source: None,
            rollout_path: None,
            archive_state: Some(true),
            title: None,
            preview: None,
            parent_id: None,
            cli_version: None,
            originator: None,
            history_mode: None,
            reasoning_effort: None,
            provenance: SourceRef::state(PathBuf::from("state.sqlite")),
        };
        let data = normalize_rollout_with_state(&parsed, &[state]);

        assert_eq!(data.sessions[0].cwd.as_deref(), Some("/rollout"));
        assert_eq!(data.sessions[0].project.as_deref(), Some("/project"));
        assert_eq!(data.sessions[0].provider.as_deref(), Some("provider"));
        assert_eq!(data.sessions[0].archive_state, Some(true));
        assert!(
            data.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::MetadataConflict)
        );
    }

    #[test]
    fn state_and_rollout_identity_mismatch_is_diagnostic() {
        let parsed = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/conflict.jsonl"
            ))),
        );
        let state = Session {
            id: "fixture-state-session".to_owned(),
            created_at: None,
            updated_at: None,
            cwd: None,
            project: None,
            model: None,
            provider: None,
            source: None,
            thread_source: None,
            rollout_path: Some("fixture.jsonl".to_owned()),
            archive_state: None,
            title: None,
            preview: None,
            parent_id: None,
            cli_version: None,
            originator: None,
            history_mode: None,
            reasoning_effort: None,
            provenance: SourceRef::state(PathBuf::from("state.sqlite")),
        };

        let data = normalize_rollout_with_state(&parsed, &[state]);

        assert_eq!(data.sessions.len(), 2);
        assert!(data.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::MetadataConflict
                && diagnostic.message.contains("identities differ")
        }));
    }

    #[test]
    fn stale_state_rollout_path_is_explicitly_reported() {
        let parsed = parse_rollout_reader(
            Path::new("current.jsonl"),
            PlainJsonlReader::new(Cursor::new(
                br#"{"type":"session_meta","payload":{"id":"fixture-stale-session"}}"#,
            )),
        );
        let state = Session {
            id: "fixture-stale-session".to_owned(),
            created_at: None,
            updated_at: None,
            cwd: None,
            project: None,
            model: None,
            provider: None,
            source: None,
            thread_source: None,
            rollout_path: Some("old.jsonl".to_owned()),
            archive_state: None,
            title: None,
            preview: None,
            parent_id: None,
            cli_version: None,
            originator: None,
            history_mode: None,
            reasoning_effort: None,
            provenance: SourceRef::state(PathBuf::from("state.sqlite")),
        };

        let data = normalize_rollout_with_state(&parsed, &[state]);

        assert!(data.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::MetadataConflict
                && diagnostic.message.contains("session paths differ")
        }));
    }
}
