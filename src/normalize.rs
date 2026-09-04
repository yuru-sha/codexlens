use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::model::{
    CanonicalData, CanonicalDiagnostic, DiagnosticKind, MAX_MESSAGE_BYTES, MAX_TOOL_OUTPUT_BYTES,
    MAX_TOOL_SUMMARY_BYTES, Message, MessageRole, OutcomeSource, Record, RecordKind, Session,
    SourceRef, TokenUsage, ToolCall, ToolOutcome, ToolResult, Turn, TurnLifecycleEvent,
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
    let mut data = normalize_records(&result.records, state);
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
                kind: match diagnostic.kind {
                    crate::state::StateDiagnosticKind::Unreadable => DiagnosticKind::Unreadable,
                    crate::state::StateDiagnosticKind::SchemaMismatch => {
                        DiagnosticKind::StateSchemaMismatch
                    }
                    crate::state::StateDiagnosticKind::Query => DiagnosticKind::StateQuery,
                },
                source: diagnostic.source.clone(),
                message: diagnostic.message.clone(),
            }),
    );
    data
}

pub fn normalize_records(records: &[RolloutRecord], state: &[Session]) -> CanonicalData {
    let mut data = CanonicalData::default();
    let mut sessions = BTreeMap::new();
    let source_path = records.first().map(|record| record.source.path.clone());
    let mut current_session_id =
        matching_state_session(source_path.as_deref(), state).map(|session| {
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

        data.records.push(Record {
            session_id: current_session_id.clone(),
            turn_id: record_turn_id,
            timestamp: record.timestamp.clone(),
            sequence,
            kind: record_kind(record),
            provenance: source,
        });
    }

    data.sessions = sessions.into_values().collect();
    mark_tool_results(&mut data.tool_calls, &mut data.tool_results);
    mark_duplicate_tool_results(&mut data.tool_results);
    data
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
            rollout_path == path
                || std::fs::canonicalize(rollout_path)
                    .ok()
                    .is_some_and(|resolved| resolved == path)
        })
    })
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
    merge_string(
        "created_at",
        &mut target.created_at,
        incoming.created_at.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "updated_at",
        &mut target.updated_at,
        incoming.updated_at.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "cwd",
        &mut target.cwd,
        incoming.cwd.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "project",
        &mut target.project,
        incoming.project.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "model",
        &mut target.model,
        incoming.model.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "provider",
        &mut target.provider,
        incoming.provider.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "source",
        &mut target.source,
        incoming.source.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "thread_source",
        &mut target.thread_source,
        incoming.thread_source.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "rollout_path",
        &mut target.rollout_path,
        incoming.rollout_path.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_bool(
        "archive_state",
        &mut target.archive_state,
        incoming.archive_state,
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "title",
        &mut target.title,
        incoming.title.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "preview",
        &mut target.preview,
        incoming.preview.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "parent_id",
        &mut target.parent_id,
        incoming.parent_id.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "cli_version",
        &mut target.cli_version,
        incoming.cli_version.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "originator",
        &mut target.originator,
        incoming.originator.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "history_mode",
        &mut target.history_mode,
        incoming.history_mode.clone(),
        &incoming.provenance,
        diagnostics,
    );
    merge_string(
        "reasoning_effort",
        &mut target.reasoning_effort,
        incoming.reasoning_effort.clone(),
        &incoming.provenance,
        diagnostics,
    );
}

fn merge_string(
    field: &str,
    target: &mut Option<String>,
    incoming: Option<String>,
    source: &SourceRef,
    diagnostics: &mut Vec<CanonicalDiagnostic>,
) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        None => *target = Some(incoming),
        Some(existing) if existing == &incoming => {}
        Some(existing) => diagnostics.push(CanonicalDiagnostic {
            kind: DiagnosticKind::MetadataConflict,
            source: source.clone(),
            message: bounded(&format!(
                "session metadata conflict for {field}: {existing:?} vs {incoming:?}"
            )),
        }),
    }
}

fn merge_bool(
    field: &str,
    target: &mut Option<bool>,
    incoming: Option<bool>,
    source: &SourceRef,
    diagnostics: &mut Vec<CanonicalDiagnostic>,
) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        None => *target = Some(incoming),
        Some(existing) if *existing == incoming => {}
        Some(existing) => diagnostics.push(CanonicalDiagnostic {
            kind: DiagnosticKind::MetadataConflict,
            source: source.clone(),
            message: format!("session metadata conflict for {field}: {existing:?} vs {incoming:?}"),
        }),
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
    if id.is_some() && session_id.is_some() && id != session_id {
        diagnostics.push(CanonicalDiagnostic {
            kind: DiagnosticKind::MetadataConflict,
            source: source.clone(),
            message: "session metadata contains conflicting id and session_id".to_owned(),
        });
    }
    let id = id.or(session_id)?;
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
        .or_else(|| payload.get("text"))
        .map(extract_message_text)
        .unwrap_or_default();
    Message {
        id: string_field(payload, &["id"]),
        session_id,
        turn_id,
        role: string_field(payload, &["role"]).map(parse_role),
        content: bounded_to(&content, MAX_MESSAGE_BYTES),
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
            calls
                .iter()
                .any(|call| call.call_id.as_ref() == Some(call_id))
        });
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
        .is_some_and(|(left, right)| normalize_token(left) != normalize_token(right))
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
        "call:{}:{}:{}:{}",
        result.call_id.as_deref().unwrap_or(""),
        result
            .exit_code
            .map_or_else(String::new, |value| value.to_string()),
        normalize_token(result.status.as_deref().unwrap_or("")),
        bounded(&combined_output(result)),
    )
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
    let output = [stdout, stderr]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if output.contains("error") || output.contains("failed") {
        (ToolOutcome::Failed, OutcomeSource::OutputText)
    } else {
        (ToolOutcome::Unknown, OutcomeSource::Unknown)
    }
}

fn status_outcome(status: &str) -> Option<ToolOutcome> {
    let status = normalize_token(status);
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
        let bytes = include_bytes!("../tests/fixtures/rollout/basic.jsonl");
        let parsed = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(bytes)),
        );
        let data = normalize_rollout(&parsed);

        assert_eq!(data.sessions[0].id, "fixture-session-001");
        assert_eq!(data.sessions[0].cwd.as_deref(), Some("/fixture/project"));
        assert_eq!(data.turns[0].id, "fixture-turn-001");
        assert_eq!(data.messages.len(), 2);
        assert_eq!(data.messages[0].role, Some(MessageRole::User));
        assert_eq!(
            data.messages[1].content,
            "I will inspect the project first."
        );
        assert_eq!(data.tool_calls.len(), 1);
        assert_eq!(data.tool_results.len(), 2);
        assert_eq!(data.tool_results[0].exit_code, Some(1));
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Failed);
        assert!(data.tool_results[0].matched_call);
        assert!(data.tool_results[1].is_duplicate);
        assert_eq!(data.token_usage[0].input_tokens, Some(120));
    }

    #[test]
    fn structured_outcome_beats_error_text() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"s"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c","name":"x"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c","output":"error text","exit_code":0,"status":"failed"}}"#,
        );
        let result = &data.tool_results[0];
        assert_eq!(result.outcome, ToolOutcome::Succeeded);
        assert_eq!(result.outcome_source, OutcomeSource::ExitCode);
    }

    #[test]
    fn missing_optional_values_stay_unknown() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"s"}}
{"type":"response_item","payload":{"type":"message","content":[]}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","output":"text"}}"#,
        );
        assert_eq!(data.messages[0].role, None);
        assert_eq!(data.tool_results[0].call_id, None);
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Unknown);
    }

    #[test]
    fn rollout_values_win_conflicts_and_state_fills_missing_metadata() {
        let parsed = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(
                br#"{"type":"session_meta","payload":{"id":"s","cwd":"/rollout"}}"#,
            )),
        );
        let state = Session {
            id: "s".to_owned(),
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
}
