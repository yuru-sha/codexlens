use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::{Map, Value};

use crate::instructions::{
    InstructionResolver, join_sessions, snapshot_from_resolution, snapshot_from_rollout,
    unavailable_snapshot,
};
use crate::model::{
    CanonicalData, CanonicalDiagnostic, DiagnosticKind, FileOperation, MAX_MESSAGE_BYTES,
    MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_SUMMARY_BYTES, Message, MessageRole, OutcomeSource, Record,
    RecordKind, Session, SourceKind, SourceRef, TokenUsage, ToolCall, ToolOutcome, ToolResult,
    Turn, TurnLifecycleEvent, merge_session_fields, normalize_path,
};
use crate::rollout::{
    KnownRecordType, ParseDiagnostic, RolloutInstructionContext, RolloutParseResult, RolloutRecord,
    rollout_id_from_path,
};
use crate::state::StateReadResult;

pub(crate) const MAX_PENDING_TOOL_CALLS: usize = 1024;
const MAX_RECENT_TOOL_RESULTS: usize = MAX_PENDING_TOOL_CALLS;

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

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RolloutNormalizationContext {
    pub(crate) session: Option<Session>,
    pub(crate) turn: Option<Turn>,
    pub(crate) pending_tool_calls: Vec<ToolCall>,
    pub(crate) recent_tool_results: Vec<ToolResult>,
    pub(crate) invalidated_file_operations: Vec<FileOperation>,
}

pub(crate) fn normalize_rollout_incremental(
    result: &RolloutParseResult,
    state: &[Session],
    resolver: &InstructionResolver,
    context: Option<&RolloutNormalizationContext>,
    sequence_start: usize,
) -> (CanonicalData, RolloutNormalizationContext) {
    let (mut data, context) = normalize_records_with_resolver(
        &result.records,
        state,
        Some(resolver),
        context,
        sequence_start,
    );
    data.diagnostics
        .extend(result.diagnostics.iter().map(canonical_parse_diagnostic));
    data.diagnostics.sort_by(|left, right| {
        left.source
            .path
            .cmp(&right.source.path)
            .then_with(|| left.source.line.cmp(&right.source.line))
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
    });
    (data, context)
}

fn normalize_rollout_with_resolver(
    result: &RolloutParseResult,
    state: &[Session],
    resolver: Option<&InstructionResolver>,
) -> CanonicalData {
    let (mut data, _) = normalize_records_with_resolver(&result.records, state, resolver, None, 0);
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
                session_id: diagnostic.session_id.clone(),
                message: diagnostic.message.clone(),
            }),
    );
    data
}

pub fn normalize_records(records: &[RolloutRecord], state: &[Session]) -> CanonicalData {
    normalize_records_with_resolver(records, state, None, None, 0).0
}

fn normalize_records_with_resolver(
    records: &[RolloutRecord],
    state: &[Session],
    resolver: Option<&InstructionResolver>,
    context: Option<&RolloutNormalizationContext>,
    sequence_start: usize,
) -> (CanonicalData, RolloutNormalizationContext) {
    let mut data = CanonicalData::default();
    let prior_tool_calls = context
        .map(|context| context.pending_tool_calls.clone())
        .unwrap_or_default();
    let prior_tool_results = context
        .map(|context| context.recent_tool_results.clone())
        .unwrap_or_default();
    let mut sessions = BTreeMap::new();
    if let Some(session) = context.and_then(|context| context.session.clone()) {
        sessions.insert(session.id.clone(), session);
    }
    let source_path = records.first().map(|record| record.source.path.clone());
    let matched_state_session_id =
        matching_state_session(source_path.as_deref(), state).map(|session| session.id.clone());
    let mut current_session_id = context
        .and_then(|context| context.session.as_ref())
        .map(|session| session.id.clone())
        .or_else(|| matched_state_session_id.clone());
    if context
        .and_then(|context| context.session.as_ref())
        .is_none()
    {
        if let Some(session_id) = current_session_id.as_deref() {
            if let Some(session) = state.iter().find(|session| session.id == session_id) {
                sessions.insert(session.id.clone(), session.clone());
            }
        }
    }
    let mut current_turn_id = context.and_then(|context| {
        context.turn.as_ref().map(|turn| {
            data.turns.push(turn.clone());
            turn.id.clone()
        })
    });

    for (index, record) in records.iter().enumerate() {
        let sequence = sequence_start + index + 1;
        let source = SourceRef::from(&record.source);
        let payload = known_payload(record);
        let explicit_turn_id = payload
            .and_then(Value::as_object)
            .and_then(|payload| string_field(payload, &["turn_id"]));
        let mut record_turn_id = explicit_turn_id.clone().or_else(|| current_turn_id.clone());

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
                    let previous_session_id = current_session_id.clone();
                    let previous_was_state_fallback =
                        previous_session_id.as_deref().is_some_and(|id| {
                            sessions
                                .get(id)
                                .is_some_and(|session| session.provenance.kind == SourceKind::State)
                        });
                    let session_id = merge_rollout_session(
                        &mut sessions,
                        candidate,
                        state,
                        &mut data.diagnostics,
                    );
                    if previous_was_state_fallback {
                        if let Some(previous_session_id) = previous_session_id
                            .as_deref()
                            .filter(|previous| *previous != session_id)
                        {
                            rekey_session_references(
                                &mut data,
                                &mut sessions,
                                previous_session_id,
                                &session_id,
                            );
                        }
                    }
                    current_session_id = Some(session_id);
                    current_turn_id = None;
                }
            }
            crate::rollout::RolloutRecordKind::Known {
                record_type: KnownRecordType::TurnContext,
                payload,
                ..
            } => {
                current_turn_id = explicit_turn_id.clone();
                record_turn_id = current_turn_id.clone();
                if let Some(turn_id) = current_turn_id.clone() {
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
                    record.instruction_context.as_ref(),
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
                        push_tool_call(
                            &mut data,
                            tool_call_from_payload(
                                payload,
                                nested_type.as_deref(),
                                current_session_id.clone(),
                                turn_id.clone(),
                                source.clone(),
                            ),
                        );
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
                        push_tool_call(
                            &mut data,
                            tool_call_from_event(
                                payload,
                                nested_type.as_deref(),
                                current_session_id.clone(),
                                event_turn_id.clone(),
                                source.clone(),
                            ),
                        );
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
        let error_category = canonical_error_category(original_nested_type.as_deref());
        data.records.push(Record {
            session_id: current_session_id.clone(),
            turn_id: record_turn_id,
            timestamp: record.timestamp.clone(),
            sequence,
            original_record_type,
            is_error: error_category.is_some(),
            is_terminal: is_terminal_nested_type(original_nested_type.as_deref()),
            original_nested_type,
            error_category,
            kind: record_kind(record),
            provenance: source,
        });
    }

    let next_session = current_session_id
        .as_deref()
        .and_then(|session_id| sessions.get(session_id))
        .cloned();
    let next_turn = current_turn_id
        .as_deref()
        .and_then(|turn_id| data.turns.iter().find(|turn| turn.id == turn_id).cloned());
    data.sessions = sessions.into_values().collect();
    if let Some(resolver) = resolver {
        data.instruction_joins = join_sessions(&data.sessions, resolver);
    }
    let invalidated_file_operations =
        recompute_derived(&mut data, &prior_tool_calls, &prior_tool_results);
    let next_pending_tool_calls =
        next_pending_tool_calls(&prior_tool_calls, &data.tool_calls, &data.tool_results);
    let next_recent_tool_results =
        next_recent_tool_results(&prior_tool_results, &data.tool_results);
    let context = RolloutNormalizationContext {
        session: next_session,
        turn: next_turn,
        pending_tool_calls: next_pending_tool_calls,
        recent_tool_results: next_recent_tool_results,
        invalidated_file_operations,
    };
    (data, context)
}

fn recompute_derived(
    data: &mut CanonicalData,
    prior_tool_calls: &[ToolCall],
    prior_tool_results: &[ToolResult],
) -> Vec<FileOperation> {
    deduplicate_token_usage(&mut data.token_usage);
    let mut correlation_calls = prior_tool_calls.to_vec();
    correlation_calls.extend(data.tool_calls.clone());
    mark_tool_results(
        prior_tool_results,
        &correlation_calls,
        &mut data.tool_results,
    );
    mark_duplicate_tool_results(prior_tool_results, &mut data.tool_results);
    extract_file_operations(data, prior_tool_calls, &correlation_calls)
}

fn next_pending_tool_calls(
    prior: &[ToolCall],
    current: &[ToolCall],
    results: &[ToolResult],
) -> Vec<ToolCall> {
    let mut pending = prior
        .iter()
        .chain(current)
        .filter(|call| {
            !results.iter().any(|result| {
                result.call_id == call.call_id
                    && call_result_context_matches(
                        result.session_id.as_deref(),
                        call.session_id.as_deref(),
                    )
                    && call_result_context_matches(
                        result.turn_id.as_deref(),
                        call.turn_id.as_deref(),
                    )
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if pending.len() > MAX_PENDING_TOOL_CALLS {
        let remove = pending.len() - MAX_PENDING_TOOL_CALLS;
        pending.drain(..remove);
    }
    pending
}

fn next_recent_tool_results(prior: &[ToolResult], current: &[ToolResult]) -> Vec<ToolResult> {
    let mut results = prior.iter().chain(current).cloned().collect::<Vec<_>>();
    if results.len() > MAX_RECENT_TOOL_RESULTS {
        let remove = results.len() - MAX_RECENT_TOOL_RESULTS;
        results.drain(..remove);
    }
    results
}

pub(crate) fn pending_tool_calls_for_source(
    data: &CanonicalData,
    source_path: &Path,
) -> Vec<ToolCall> {
    let calls = data
        .tool_calls
        .iter()
        .filter(|call| call.provenance.path == source_path)
        .cloned()
        .collect::<Vec<_>>();
    let results = data
        .tool_results
        .iter()
        .filter(|result| result.provenance.path == source_path)
        .cloned()
        .collect::<Vec<_>>();
    next_pending_tool_calls(&[], &calls, &results)
}

pub(crate) fn recent_tool_results_for_source(
    data: &CanonicalData,
    source_path: &Path,
) -> Vec<ToolResult> {
    let mut results = data
        .tool_results
        .iter()
        .filter(|result| result.provenance.path == source_path)
        .cloned()
        .collect::<Vec<_>>();
    if results.len() > MAX_RECENT_TOOL_RESULTS {
        let remove = results.len() - MAX_RECENT_TOOL_RESULTS;
        results.drain(..remove);
    }
    results
}

fn extract_file_operations(
    data: &mut CanonicalData,
    prior_tool_calls: &[ToolCall],
    correlation_calls: &[ToolCall],
) -> Vec<FileOperation> {
    let calls = data.tool_calls.clone();
    let mut call_operations = HashSet::new();
    let mut unpaired_no_id_calls = Vec::new();
    let mut invalidated_file_operations = Vec::new();
    for call in prior_tool_calls {
        let operations = call_file_operations(data, call);
        if call_is_failed(&data.tool_results, correlation_calls, call)
            || call_has_failed_result(&data.tool_results, correlation_calls, call)
        {
            invalidated_file_operations
                .extend(operations.into_iter().map(|(operation, _)| operation));
            continue;
        }
        for (operation, key) in operations {
            call_operations.insert(key.clone());
            if call.call_id.is_none() {
                unpaired_no_id_calls.push((operation, key.2));
            }
        }
    }
    for call in &calls {
        if call_is_failed(&data.tool_results, correlation_calls, call)
            || call_has_failed_result(&data.tool_results, correlation_calls, call)
        {
            continue;
        }
        for (operation, key) in call_file_operations(data, call) {
            if call_operations.insert(key.clone()) {
                if call.call_id.is_none() {
                    unpaired_no_id_calls.push((operation.clone(), key.2));
                }
                data.file_operations.push(operation);
            }
        }
    }
    for result in &data.tool_results {
        if result.is_duplicate || result_is_failed(result) {
            continue;
        }
        let command = result.command.as_deref().unwrap_or_default();
        let timestamp = record_timestamp(data, &result.provenance);
        let matched_call = matching_call(correlation_calls, result);
        if matched_call
            .is_some_and(|call| call_is_failed(&data.tool_results, correlation_calls, call))
        {
            continue;
        }
        let tool_name = matched_call
            .and_then(|call| call.tool_name.as_deref())
            .unwrap_or_default();
        let cwd = result
            .cwd
            .clone()
            .or_else(|| matched_call.and_then(|call| call.cwd.clone()))
            .or_else(|| {
                context_cwd(
                    data,
                    result.session_id.as_deref(),
                    result.turn_id.as_deref(),
                )
                .map(str::to_owned)
            });
        let mut extracted = Vec::new();
        append_observed_file_operations(
            &mut extracted,
            result.session_id.clone(),
            result.turn_id.clone(),
            tool_name,
            command,
            &result.provenance,
            timestamp,
        );
        for operation in extracted {
            let Some(session_id) = operation.session_id.as_ref() else {
                continue;
            };
            let operation_path = operation_identity_path(&operation.path, cwd.as_deref());
            if result.call_id.is_none() {
                let matching_call =
                    unpaired_no_id_calls
                        .iter()
                        .position(|(call_operation, call_path)| {
                            call_operation.session_id == operation.session_id
                                && call_result_context_matches(
                                    call_operation.turn_id.as_deref(),
                                    operation.turn_id.as_deref(),
                                )
                                && call_operation.operation == operation.operation
                                && *call_path == operation_path
                                && source_precedes(
                                    &call_operation.provenance,
                                    &operation.provenance,
                                )
                        });
                if let Some(index) = matching_call {
                    unpaired_no_id_calls.remove(index);
                    continue;
                }
            }
            let result_identity = result.call_id.clone().unwrap_or_else(|| {
                format!(
                    "missing-call-id:result:{}:{}",
                    result.provenance.path.display(),
                    result.provenance.line.unwrap_or_default()
                )
            });
            if call_operations.insert((
                session_id.clone(),
                result_identity,
                operation_path,
                operation.operation.clone(),
            )) {
                data.file_operations.push(operation);
            }
        }
    }
    data.file_operations.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then_with(|| left.timestamp.cmp(&right.timestamp))
            .then_with(|| left.provenance.path.cmp(&right.provenance.path))
            .then_with(|| left.provenance.line.cmp(&right.provenance.line))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.operation.cmp(&right.operation))
    });
    data.file_operations.dedup_by(|right, left| {
        right.session_id == left.session_id
            && right.turn_id == left.turn_id
            && right.path == left.path
            && right.operation == left.operation
            && right.provenance.path == left.provenance.path
            && right.provenance.line == left.provenance.line
    });
    invalidated_file_operations
}

fn call_file_operations(
    data: &CanonicalData,
    call: &ToolCall,
) -> Vec<(FileOperation, (String, String, String, String))> {
    if is_wrapper_tool(call.tool_name.as_deref()) {
        return Vec::new();
    }
    let tool = normalize_token(call.tool_name.as_deref().unwrap_or_default()).to_ascii_lowercase();
    let command = match call.command.as_deref() {
        Some(command) => command,
        None if !is_shell_tool(&tool) => call.input_summary.as_deref().unwrap_or_default(),
        _ => "",
    };
    let cwd = call.cwd.clone().or_else(|| {
        context_cwd(data, call.session_id.as_deref(), call.turn_id.as_deref()).map(str::to_owned)
    });
    let timestamp = record_timestamp(data, &call.provenance);
    let mut extracted = Vec::new();
    append_observed_file_operations(
        &mut extracted,
        call.session_id.clone(),
        call.turn_id.clone(),
        call.tool_name.as_deref().unwrap_or_default(),
        command,
        &call.provenance,
        timestamp,
    );
    extracted
        .into_iter()
        .filter_map(|operation| {
            let session_id = operation.session_id.clone()?;
            let call_identity = call.call_id.clone().unwrap_or_else(|| {
                format!(
                    "missing-call-id:call:{}:{}",
                    call.provenance.path.display(),
                    call.provenance.line.unwrap_or_default()
                )
            });
            let operation_path = operation_identity_path(&operation.path, cwd.as_deref());
            let operation_kind = operation.operation.clone();
            Some((
                operation,
                (session_id, call_identity, operation_path, operation_kind),
            ))
        })
        .collect()
}

fn append_observed_file_operations(
    operations: &mut Vec<FileOperation>,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_name: &str,
    command: &str,
    provenance: &SourceRef,
    timestamp: Option<String>,
) {
    if session_id.is_none() {
        return;
    }
    let tool = normalize_token(tool_name).to_ascii_lowercase();
    let file_tool = is_file_operation_tool(&tool);
    let shell_tool = is_shell_tool(&tool);
    let wrapper_tool = is_wrapper_tool(Some(tool_name));
    let mut observed = Vec::new();
    if !shell_tool && !wrapper_tool {
        let text = command_payload_text(command);
        let mut patch_operation = None;
        for line in text.lines() {
            let Some((operation, path)) =
                PATCH_FILE_MARKERS.iter().find_map(|(marker, operation)| {
                    line.trim()
                        .strip_prefix(marker)
                        .map(|path| (*operation, path.trim().to_owned()))
                })
            else {
                continue;
            };
            patch_operation = Some(operation);
            if likely_file_path(&path) {
                observed.push((operation.to_owned(), path));
            }
        }
        if observed.is_empty() && file_tool {
            if let Some(path) = json_string_field(command, &["path", "file_path", "filename"])
                .filter(|path| likely_file_path(path))
                .or_else(|| {
                    let path = command.trim();
                    likely_file_path(path).then(|| path.to_owned())
                })
            {
                observed.push((
                    if tool == "create_file" {
                        "create".to_owned()
                    } else {
                        patch_operation.unwrap_or("edit").to_owned()
                    },
                    path,
                ));
            }
        }
    }
    if shell_tool && !has_patch_marker(command) {
        observed.extend(
            shell_redirection_targets(command)
                .into_iter()
                .map(|path| ("write".to_owned(), path)),
        );
    } else if !shell_tool && observed.is_empty() {
        return;
    }
    for (operation, path) in observed {
        let Some(path) = normalize_file_operation_path(&path) else {
            continue;
        };
        operations.push(FileOperation {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            path,
            operation,
            timestamp: timestamp.clone(),
            provenance: provenance.clone(),
        });
    }
}

const PATCH_FILE_MARKERS: [(&str, &str); 3] = [
    ("*** Update File:", "edit"),
    ("*** Add File:", "create"),
    ("*** Delete File:", "delete"),
];

fn has_patch_marker(text: &str) -> bool {
    text.lines().any(|line| {
        PATCH_FILE_MARKERS
            .iter()
            .any(|(marker, _)| line.trim().starts_with(marker))
    })
}

fn is_file_operation_tool(tool: &str) -> bool {
    matches!(
        tool,
        "apply_patch" | "edit_file" | "write_file" | "create_file" | "replace_file"
    )
}

fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "exec_command" | "shell")
}

fn shell_redirection_targets(command: &str) -> Vec<String> {
    if serde_json::from_str::<Value>(command).is_ok() {
        return Vec::new();
    }
    let chars = command.chars().collect::<Vec<_>>();
    let mut targets = Vec::new();
    let mut index = 0;
    let mut quote = None;
    while index < chars.len() {
        let character = chars[index];
        if let Some(delimiter) = quote {
            if character == '\\' && delimiter == '"' {
                index = (index + 2).min(chars.len());
                continue;
            }
            if character == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            index += 1;
            continue;
        }
        if character != '>' {
            index += 1;
            continue;
        }
        let mut target_start = index + 1;
        if chars.get(target_start) == Some(&'>') {
            target_start += 1;
        }
        while chars
            .get(target_start)
            .is_some_and(|character| character.is_whitespace())
        {
            target_start += 1;
        }
        if chars.get(target_start) == Some(&'&') {
            index = target_start + 1;
            continue;
        }
        let (target, next_index) = shell_redirection_target(&chars, target_start);
        if let Some(target) = target {
            if normalize_file_operation_path(&target).is_some() {
                targets.push(target);
            }
        }
        index = next_index.max(index + 1);
    }
    targets
}

fn shell_redirection_target(chars: &[char], start: usize) -> (Option<String>, usize) {
    let Some(&first) = chars.get(start) else {
        return (None, chars.len());
    };
    if shell_redirection_boundary(first) {
        return (None, start + 1);
    }
    if matches!(first, '\'' | '"') {
        let mut index = start + 1;
        while index < chars.len() {
            if chars[index] == '\\' && first == '"' {
                index = (index + 2).min(chars.len());
                continue;
            }
            if chars[index] == first {
                let next = index + 1;
                let fragmented = chars
                    .get(next)
                    .is_some_and(|character| !shell_redirection_boundary(*character));
                return (
                    (!fragmented).then(|| chars[start + 1..index].iter().collect()),
                    next,
                );
            }
            index += 1;
        }
        return (None, chars.len());
    }
    let mut end = start;
    while chars
        .get(end)
        .is_some_and(|character| !shell_redirection_boundary(*character))
    {
        end += 1;
    }
    (Some(chars[start..end].iter().collect()), end)
}

fn shell_redirection_boundary(character: char) -> bool {
    character.is_whitespace() || matches!(character, ';' | '|' | '&' | '<' | '>' | '(' | ')')
}

fn normalize_file_operation_path(path: &str) -> Option<String> {
    let path = path.trim();
    let path = match (path.chars().next(), path.chars().last()) {
        (Some(first), Some(last)) if matches!(first, '\'' | '"') => {
            if first != last {
                return None;
            }
            let start = first.len_utf8();
            let end = path.len().checked_sub(last.len_utf8())?;
            path.get(start..end)?
        }
        (Some(_), Some('\'' | '"')) => return None,
        _ => path,
    }
    .trim();
    if path.is_empty()
        || path
            .chars()
            .any(|character| character == '\n' || character == '\r')
        || path.starts_with(['{', '['])
    {
        return None;
    }
    let normalized = normalize_path(path);
    let lower = normalized.to_ascii_lowercase();
    if normalized.is_empty()
        || lower == "/dev/null"
        || lower.starts_with("/dev/null/")
        || normalized == "="
        || normalized.starts_with('&')
        || normalized == "const"
        || (lower.starts_with("s:") && lower.ends_with("});"))
        || lower.ends_with("});")
        || normalized.contains(['<', '>'])
    {
        return None;
    }
    Some(normalized)
}

fn operation_identity_path(path: &str, cwd: Option<&str>) -> String {
    let path = normalize_path(path);
    if path.starts_with('/') {
        return path;
    }
    cwd.filter(|cwd| cwd.starts_with('/'))
        .map_or(path.clone(), |cwd| normalize_path(&format!("{cwd}/{path}")))
}

fn likely_file_path(path: &str) -> bool {
    let path = path.trim();
    (!path.chars().any(char::is_whitespace) || path.starts_with(['/', '.', '~']))
        && normalize_file_operation_path(path).is_some()
}

fn canonical_command_value(value: &Value) -> String {
    let command = match value {
        Value::String(value) => command_payload_text(value),
        _ => command_payload_text(&value.to_string()),
    };
    bounded_to(&command, MAX_TOOL_SUMMARY_BYTES)
}

fn input_summary_value(value: &Value) -> Option<String> {
    let summary = canonical_command_value(value);
    (!summary.trim().is_empty() && !is_wrapper_artifact(&summary)).then_some(summary)
}

fn structured_command_value(value: &Value) -> Option<String> {
    let command = match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .join(" "),
        Value::Object(object) => ["argv", "cmd", "command"]
            .iter()
            .find_map(|key| object.get(*key).and_then(structured_command_value))?,
        _ => return None,
    };
    let command = bounded_to(&command, MAX_TOOL_SUMMARY_BYTES);
    (!command.trim().is_empty() && !is_wrapper_artifact(&command)).then_some(command)
}

fn structured_input_command(payload: &Map<String, Value>) -> Option<String> {
    ["input", "arguments"].iter().find_map(|key| {
        payload.get(*key).and_then(|value| match value {
            Value::String(encoded) => serde_json::from_str::<Value>(encoded)
                .ok()
                .filter(|value| value.is_object() || value.is_array())
                .and_then(|value| structured_command_value(&value)),
            value if value.is_object() || value.is_array() => structured_command_value(value),
            _ => None,
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NestedToolCall {
    tool_name: String,
    command: Option<String>,
}

fn nested_tool_call(value: &Value) -> Option<NestedToolCall> {
    let source = value.as_str()?;
    if source.len() > MAX_TOOL_SUMMARY_BYTES {
        return None;
    }
    // ponytail: parse only the bounded call subset; unsupported JavaScript stays opaque.

    let mut search = 0;
    let mut found = None;
    while let Some(relative) = source.get(search..)?.find("tools.") {
        let start = search + relative;
        if !js_code_at(source, start) {
            search = start + "tools.".len();
            continue;
        }
        let preceding = start
            .checked_sub(1)
            .and_then(|index| source.as_bytes().get(index))
            .copied();
        if preceding
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'.'))
        {
            search = start + "tools.".len();
            continue;
        }
        let method_start = start + "tools.".len();
        let method_end = source[method_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                (!character.is_ascii_alphanumeric() && character != '_')
                    .then_some(method_start + offset)
            })
            .unwrap_or(source.len());
        if method_end == method_start {
            search = method_start + 1;
            continue;
        }
        let call_start = skip_js_whitespace(source, method_end);
        if source.as_bytes().get(call_start) != Some(&b'(') {
            search = method_end;
            continue;
        }
        let object_start = skip_js_whitespace(source, call_start + 1);
        if source.as_bytes().get(object_start) != Some(&b'{') {
            return None;
        }
        let mut parser = JsLiteralParser::new(&source[object_start..]);
        let arguments = parser.parse_object()?;
        let after_object = skip_js_whitespace(source, object_start + parser.index);
        if source.as_bytes().get(after_object) != Some(&b')') {
            return None;
        }
        if found.is_some() {
            return None;
        }
        found = Some((&source[method_start..method_end], arguments));
        search = after_object + 1;
    }

    let (tool_name, arguments) = found?;
    let tool_name = valid_tool_name(tool_name.to_owned())?;
    let command = if is_shell_tool(&tool_name) {
        ["argv", "cmd", "command"]
            .iter()
            .find_map(|key| arguments.get(*key).and_then(structured_command_value))?
    } else if is_file_operation_tool(&tool_name) {
        arguments.get("patch").and_then(structured_command_value)?
    } else {
        return Some(NestedToolCall {
            tool_name,
            command: None,
        });
    };
    Some(NestedToolCall {
        tool_name,
        command: Some(command),
    })
}

fn js_code_at(source: &str, target: usize) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut quote = None;
    let mut line_comment = false;
    let mut block_comment = false;
    while index < target {
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if bytes[index] == b'\\' {
                index = (index + 2).min(target);
            } else {
                if bytes[index] == delimiter {
                    quote = None;
                }
                index += 1;
            }
            continue;
        }
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                line_comment = true;
                index += 2;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                block_comment = true;
                index += 2;
            }
            b'\'' | b'"' | b'`' => {
                quote = Some(bytes[index]);
                index += 1;
            }
            _ => index += 1,
        }
    }
    quote.is_none() && !line_comment && !block_comment
}

fn skip_js_whitespace(source: &str, mut index: usize) -> usize {
    while source
        .as_bytes()
        .get(index)
        .is_some_and(u8::is_ascii_whitespace)
    {
        index += 1;
    }
    index
}

struct JsLiteralParser<'a> {
    source: &'a str,
    index: usize,
}

impl<'a> JsLiteralParser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, index: 0 }
    }

    fn parse_object(&mut self) -> Option<Map<String, Value>> {
        self.expect(b'{')?;
        let mut object = Map::new();
        loop {
            self.skip_whitespace();
            if self.consume(b'}') {
                return Some(object);
            }
            let key = self.parse_key()?;
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.parse_value()?;
            object.insert(key, value);
            self.skip_whitespace();
            if self.consume(b'}') {
                return Some(object);
            }
            self.expect(b',')?;
        }
    }

    fn parse_array(&mut self) -> Option<Value> {
        self.expect(b'[')?;
        let mut values = Vec::new();
        loop {
            self.skip_whitespace();
            if self.consume(b']') {
                return Some(Value::Array(values));
            }
            values.push(self.parse_value()?);
            self.skip_whitespace();
            if self.consume(b']') {
                return Some(Value::Array(values));
            }
            self.expect(b',')?;
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_whitespace();
        match self.source.as_bytes().get(self.index).copied()? {
            b'{' => self.parse_object().map(Value::Object),
            b'[' => self.parse_array(),
            b'\'' | b'"' => self.parse_string().map(Value::String),
            _ => {
                let start = self.index;
                while self
                    .source
                    .as_bytes()
                    .get(self.index)
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !b",}]".contains(byte))
                {
                    self.index += 1;
                }
                serde_json::from_str(self.source.get(start..self.index)?).ok()
            }
        }
    }

    fn parse_key(&mut self) -> Option<String> {
        self.skip_whitespace();
        match self.source.as_bytes().get(self.index).copied()? {
            b'\'' | b'"' => self.parse_string(),
            _ => {
                let start = self.index;
                while self.source.as_bytes().get(self.index).is_some_and(|byte| {
                    byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$'
                }) {
                    self.index += 1;
                }
                (start != self.index).then(|| self.source[start..self.index].to_owned())
            }
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        let delimiter = self.source.as_bytes().get(self.index).copied()?;
        if !matches!(delimiter, b'\'' | b'"') {
            return None;
        }
        self.index += 1;
        let mut value = String::new();
        while self.index < self.source.len() {
            let character = self.source[self.index..].chars().next()?;
            self.index += character.len_utf8();
            if character == delimiter as char {
                return Some(value);
            }
            if character == '\\' {
                let escaped = self.source[self.index..].chars().next()?;
                self.index += escaped.len_utf8();
                value.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    _ => return None,
                });
            } else {
                value.push(character);
            }
        }
        None
    }

    fn skip_whitespace(&mut self) {
        self.index = skip_js_whitespace(self.source, self.index);
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.source.as_bytes().get(self.index) == Some(&byte)).then(|| {
            self.index += 1;
        })
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.source.as_bytes().get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }
}

fn result_command_value(value: &Value) -> Option<String> {
    structured_command_value(value)
}

fn valid_tool_name(value: String) -> Option<String> {
    let value = normalize_token(&value);
    if value.is_empty() || value.split_whitespace().count() != 1 || is_wrapper_artifact(&value) {
        return None;
    }
    Some(bounded_to(&value, MAX_TOOL_SUMMARY_BYTES))
}

fn is_wrapper_artifact(value: &str) -> bool {
    let value = normalize_token(value).to_ascii_lowercase();
    value.starts_with("exec_command const")
        || value.starts_with("exec_command s:")
        || (value.starts_with("s:") && value.ends_with("});"))
}

fn command_payload_text(command: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(command) else {
        return command.to_owned();
    };
    if let Some(value) = value.as_str() {
        return value.to_owned();
    }
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
    }
    if let Some(object) = value.as_object() {
        for key in [
            "argv",
            "cmd",
            "command",
            "args",
            "patch",
            "path",
            "file_path",
            "filename",
        ] {
            if let Some(value) = object.get(key) {
                let text = if let Some(value) = value.as_str() {
                    command_payload_text(value)
                } else {
                    command_payload_text(&value.to_string())
                };
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    command.to_owned()
}

fn json_string_field(command: &str, names: &[&str]) -> Option<String> {
    let value = serde_json::from_str::<Value>(command).ok()?;
    if let Some(nested) = value.as_str() {
        return json_string_field(nested, names);
    }
    let object = value.as_object()?;
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str).map(str::to_owned))
}

fn record_timestamp(data: &CanonicalData, provenance: &SourceRef) -> Option<String> {
    data.records
        .iter()
        .find(|record| {
            record.provenance.path == provenance.path && record.provenance.line == provenance.line
        })
        .and_then(|record| record.timestamp.clone())
}

fn context_cwd<'a>(
    data: &'a CanonicalData,
    session_id: Option<&str>,
    turn_id: Option<&str>,
) -> Option<&'a str> {
    if let Some(turn_id) = turn_id {
        if let Some(cwd) = data
            .turns
            .iter()
            .find(|turn| {
                turn.id == turn_id && context_matches(turn.session_id.as_deref(), session_id)
            })
            .and_then(|turn| turn.cwd.as_deref())
        {
            return Some(cwd);
        }
    }
    session_id.and_then(|session_id| {
        data.sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.cwd.as_deref())
    })
}

fn turn_context_snapshot(
    context: Option<&RolloutInstructionContext>,
    session_id: Option<String>,
    turn_id: Option<String>,
    sessions: &BTreeMap<String, Session>,
    resolver: Option<&InstructionResolver>,
    provenance: SourceRef,
) -> crate::model::InstructionSnapshot {
    let rollout_instructions = context.and_then(|context| context.instruction_text.as_deref());
    if let Some(content) = rollout_instructions {
        return snapshot_from_rollout(session_id, turn_id, Some(content), provenance);
    }
    let Some(resolver) = resolver else {
        return unavailable_snapshot(session_id, turn_id, provenance);
    };
    let session = session_id.as_deref().and_then(|id| sessions.get(id));
    let project_root = context
        .and_then(|context| context.project_root.as_deref())
        .or_else(|| session.and_then(|session| session.project.as_deref()));
    let cwd = context
        .and_then(|context| context.cwd.as_deref())
        .or_else(|| session.and_then(|session| session.cwd.as_deref()));
    let resolution = resolver.resolve(project_root.map(Path::new), cwd.map(Path::new));
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
        session_id: None,
        message: diagnostic.message.clone(),
    }
}

fn matching_state_session<'a>(
    path: Option<&std::path::Path>,
    state: &'a [Session],
) -> Option<&'a Session> {
    let path = path?;
    state
        .iter()
        .find(|session| rollout_path_matches(session.rollout_path.as_deref(), path))
}

fn matching_state_session_for_candidate<'a>(
    candidate: &Session,
    state: &'a [Session],
) -> Option<&'a Session> {
    state
        .iter()
        .find(|session| session.id == candidate.id)
        .or_else(|| {
            state.iter().find(|session| {
                candidate.rollout_path.as_deref().is_some_and(|path| {
                    rollout_path_matches(session.rollout_path.as_deref(), Path::new(path))
                })
            })
        })
        .or_else(|| {
            state
                .iter()
                .find(|session| identities_match(candidate, session))
        })
}

fn rekey_session_references(
    data: &mut CanonicalData,
    sessions: &mut BTreeMap<String, Session>,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }
    let update = |session_id: &mut Option<String>| {
        if session_id.as_deref() == Some(from) {
            *session_id = Some(to.to_owned());
        }
    };
    for session in data.sessions.iter_mut() {
        update(&mut session.parent_id);
    }
    for session in sessions.values_mut() {
        update(&mut session.parent_id);
    }
    for turn in &mut data.turns {
        update(&mut turn.session_id);
    }
    for record in &mut data.records {
        update(&mut record.session_id);
    }
    for message in &mut data.messages {
        update(&mut message.session_id);
    }
    for tool_call in &mut data.tool_calls {
        update(&mut tool_call.session_id);
    }
    for tool_result in &mut data.tool_results {
        update(&mut tool_result.session_id);
    }
    for operation in &mut data.file_operations {
        update(&mut operation.session_id);
    }
    for usage in &mut data.token_usage {
        update(&mut usage.session_id);
    }
    for snapshot in &mut data.instruction_snapshots {
        update(&mut snapshot.session_id);
    }
    for join in &mut data.instruction_joins {
        if join.session_id == from {
            join.session_id = to.to_owned();
        }
    }
}

fn identities_match(left: &Session, right: &Session) -> bool {
    if left.id == right.id {
        return true;
    }
    if let (Some(left), Some(right)) = (left.thread_id.as_deref(), right.thread_id.as_deref()) {
        return left == right;
    }
    let has_fallback_thread = left.thread_id.is_none() || right.thread_id.is_none();
    left.rollout_id
        .as_deref()
        .zip(right.rollout_id.as_deref())
        .is_some_and(|(left, right)| left == right)
        || left
            .session_id
            .as_deref()
            .zip(right.session_id.as_deref())
            .is_some_and(|(left, right)| left == right && has_fallback_thread)
}

fn same_rollout_source(left: &Session, right: &Session) -> bool {
    rollout_paths_match(left, right)
        && ((left.rollout_id.is_some() || right.rollout_id.is_some())
            || left.provenance.kind == SourceKind::State
            || right.provenance.kind == SourceKind::State)
}

fn same_rollout_identity(left: &Session, right: &Session) -> bool {
    rollout_paths_match(left, right)
        && left
            .rollout_id
            .as_deref()
            .zip(right.rollout_id.as_deref())
            .is_some_and(|(left, right)| left == right)
}

fn rollout_paths_match(left: &Session, right: &Session) -> bool {
    left.rollout_path
        .as_deref()
        .zip(right.rollout_path.as_deref())
        .is_some_and(|(left, right)| rollout_path_matches(Some(left), Path::new(right)))
}

fn rollout_path_matches(left: Option<&str>, right: &Path) -> bool {
    left.is_some_and(|left| same_path(Path::new(left), right))
}

fn source_is_archived(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "archived_sessions")
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
) -> String {
    let state_session = matching_state_session_for_candidate(&candidate, state);
    if let Some(state_session) = state_session {
        if !same_rollout_identity(&candidate, state_session)
            && !identities_match(&candidate, state_session)
        {
            diagnostics.push(CanonicalDiagnostic {
                kind: DiagnosticKind::MetadataConflict,
                source: state_session.provenance.clone(),
                session_id: Some(candidate.id.clone()),
                message: bounded(&format!(
                    "state and rollout session identities differ: state={:?}, rollout={:?}",
                    state_session.id, candidate.id
                )),
            });
        }
        if state_session
            .rollout_path
            .as_deref()
            .is_some_and(|path| !rollout_path_matches(Some(path), &candidate.provenance.path))
        {
            diagnostics.push(CanonicalDiagnostic {
            kind: DiagnosticKind::MetadataConflict,
            source: candidate.provenance.clone(),
            session_id: Some(candidate.id.clone()),
            message: bounded(
                "state and rollout session paths differ; state metadata was retained as enrichment",
            ),
            });
        }
    }
    let existing_id = sessions
        .get(&candidate.id)
        .map(|session| session.id.clone())
        .or_else(|| {
            sessions
                .iter()
                .find(|(_, session)| same_rollout_source(session, &candidate))
                .map(|(id, _)| id.clone())
        });
    if let Some(existing_id) = existing_id {
        let mut existing = sessions
            .remove(&existing_id)
            .expect("existing session was found in the session map");
        if same_rollout_source(&existing, &candidate) {
            if existing.provenance.kind == SourceKind::State
                && candidate.provenance.kind == SourceKind::Rollout
            {
                let mut merged = candidate.clone();
                merge_session(&mut merged, &existing, diagnostics);
                if let Some(state_session) = state_session {
                    merge_session(&mut merged, state_session, diagnostics);
                }
                let id = merged.id.clone();
                sessions.insert(id.clone(), merged);
                return id;
            }
            merge_session(&mut existing, &candidate, diagnostics);
            if let Some(state_session) = state_session {
                merge_session(&mut existing, state_session, diagnostics);
            }
            let id = existing.id.clone();
            sessions.insert(id.clone(), existing);
            return id;
        }
        let mut merged = candidate.clone();
        merge_session(&mut merged, &existing, diagnostics);
        let id = merged.id.clone();
        sessions.insert(id.clone(), merged);
        return id;
    }
    let mut merged = candidate.clone();
    if let Some(state_session) = state_session {
        merge_session(&mut merged, state_session, diagnostics);
    }
    let id = merged.id.clone();
    sessions.insert(id.clone(), merged);
    id
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
            session_id: Some(target.id.clone()),
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
    let id = string_field(payload, &["id"]).filter(|value| !value.is_empty());
    let explicit_session_id =
        string_field(payload, &["session_id"]).filter(|value| !value.is_empty());
    let explicit_thread_id =
        string_field(payload, &["thread_id"]).filter(|value| !value.is_empty());
    let rollout_id = string_field(payload, &["rollout_id"])
        .filter(|value| !value.is_empty())
        .or_else(|| rollout_id_from_path(&source.path));
    let thread_id = explicit_thread_id.clone().or(id.clone());
    let session_id = explicit_session_id;
    let identity = thread_id
        .clone()
        .or_else(|| session_id.clone())
        .or_else(|| rollout_id.clone());
    if let (Some(id), Some(thread_id)) = (id.as_ref(), thread_id.as_ref()) {
        if id != thread_id && explicit_thread_id.is_some() {
            diagnostics.push(CanonicalDiagnostic {
                kind: DiagnosticKind::MetadataConflict,
                source: source.clone(),
                session_id: identity.clone(),
                message: "session metadata contains conflicting thread identity fields".to_owned(),
            });
        }
    }
    let parent_thread_id =
        string_field(payload, &["parent_thread_id"]).filter(|value| !value.is_empty());
    let parent_id_alias = string_field(payload, &["parent_id"]).filter(|value| !value.is_empty());
    if let (Some(parent_thread_id), Some(parent_id)) =
        (parent_thread_id.as_ref(), parent_id_alias.as_ref())
    {
        if parent_thread_id != parent_id {
            diagnostics.push(CanonicalDiagnostic {
                kind: DiagnosticKind::MetadataConflict,
                source: source.clone(),
                session_id: identity.clone(),
                message: "session metadata contains conflicting parent identity fields".to_owned(),
            });
        }
    }
    let id = identity?;
    Some(Session {
        id,
        rollout_id,
        session_id,
        thread_id,
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
        archive_state: payload
            .get("archived")
            .or_else(|| payload.get("is_archived"))
            .or_else(|| payload.get("archive_state"))
            .and_then(Value::as_bool)
            .or_else(|| source_is_archived(&source.path).then_some(true)),
        title: string_field(payload, &["title"]),
        preview: string_field(payload, &["preview", "first_user_message"]),
        parent_id: parent_thread_id.or(parent_id_alias),
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
    let input = payload.get("input").or_else(|| payload.get("arguments"));
    let tool_name = string_field(payload, &["name", "tool_name"])
        .and_then(valid_tool_name)
        .or_else(|| (nested_type == Some("exec_command")).then(|| "exec_command".to_owned()));
    let nested = is_wrapper_tool(tool_name.as_deref()).then(|| input.and_then(nested_tool_call));
    let (tool_name, command) = match nested.flatten() {
        Some(nested) => (Some(nested.tool_name), nested.command),
        None => {
            let command = if is_wrapper_tool(tool_name.as_deref()) {
                None
            } else {
                structured_input_command(payload)
                    .or_else(|| payload.get("command").and_then(structured_command_value))
            };
            (tool_name, command)
        }
    };
    ToolCall {
        id: string_field(payload, &["id"]),
        call_id: string_field(payload, &["call_id"]),
        session_id,
        turn_id,
        tool_name,
        input_summary: payload
            .get("input")
            .or_else(|| payload.get("arguments"))
            .or_else(|| payload.get("query"))
            .or_else(|| payload.get("patch"))
            .or_else(|| payload.get("command"))
            .or_else(|| payload.get("path"))
            .or_else(|| payload.get("file_path"))
            .or_else(|| payload.get("filename"))
            .and_then(input_summary_value),
        command,
        cwd: string_field(payload, &["cwd"]),
        status: string_field(payload, &["status"]),
        provenance,
    }
}

fn is_wrapper_tool(tool: Option<&str>) -> bool {
    matches!(
        tool.map(normalize_token).as_deref(),
        Some("exec" | "js" | "wait")
    )
}

fn push_tool_call(data: &mut CanonicalData, call: ToolCall) {
    if matches!(call.tool_name.as_deref(), Some("exec" | "js" | "wait")) && call.command.is_none() {
        data.diagnostics.push(CanonicalDiagnostic {
            kind: DiagnosticKind::OpaqueToolInput,
            source: call.provenance.clone(),
            session_id: call.session_id.clone(),
            message: "Wrapper input was retained as opaque because no single safely structured nested tool call was available".to_owned(),
        });
    }
    data.tool_calls.push(call);
}

fn tool_call_from_event(
    payload: &Map<String, Value>,
    nested_type: Option<&str>,
    session_id: Option<String>,
    turn_id: Option<String>,
    provenance: SourceRef,
) -> ToolCall {
    let mut call = tool_call_from_payload(payload, nested_type, session_id, turn_id, provenance);
    if call.tool_name.is_none() {
        call.tool_name = match nested_type {
            Some("exec_command_begin") => Some("exec_command".to_owned()),
            Some("patch_apply_begin") => Some("apply_patch".to_owned()),
            _ => None,
        };
    }
    call
}

fn tool_result_from_payload(
    payload: &Map<String, Value>,
    session_id: Option<String>,
    turn_id: Option<String>,
    provenance: SourceRef,
) -> ToolResult {
    let renderer_output = payload.get("output").map(value_summary);
    let stdout = payload.get("stdout").map(value_summary);
    let stderr = payload.get("stderr").map(value_summary);
    let exit_code = payload
        .get("exit_code")
        .or_else(|| payload.get("exit"))
        .and_then(value_i64);
    let status = string_field(payload, &["status"]);
    let renderer = parse_renderer_status(&[renderer_output.as_deref()]);
    let (outcome, outcome_source) = classify_outcome(
        exit_code,
        status.as_deref(),
        renderer,
        stdout.as_deref(),
        stderr.as_deref(),
    );
    let exit_code =
        exit_code.or_else(|| (status.is_none()).then(|| renderer.exit_code()).flatten());
    let status = status.or_else(|| (exit_code.is_none()).then(|| renderer.status()).flatten());
    ToolResult {
        id: string_field(payload, &["id"]),
        call_id: string_field(payload, &["call_id"]),
        session_id,
        turn_id,
        command: payload.get("command").and_then(result_command_value),
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

fn mark_tool_results(prior_results: &[ToolResult], calls: &[ToolCall], results: &mut [ToolResult]) {
    for result in results {
        result.matched_call = result.call_id.as_ref().is_some_and(|call_id| {
            calls.iter().any(|call| {
                call.call_id.as_ref() == Some(call_id)
                    && call_result_context_matches(
                        result.session_id.as_deref(),
                        call.session_id.as_deref(),
                    )
                    && call_result_context_matches(
                        result.turn_id.as_deref(),
                        call.turn_id.as_deref(),
                    )
            }) || prior_results
                .iter()
                .any(|prior| prior.matched_call && equivalent_results(prior, result))
        });
    }
}

fn context_matches(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        _ => false,
    }
}

fn call_result_context_matches(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn matching_call<'a>(calls: &'a [ToolCall], result: &ToolResult) -> Option<&'a ToolCall> {
    let candidates = calls
        .iter()
        .filter(|call| {
            call.call_id == result.call_id
                && call.call_id.is_some()
                && call_result_context_matches(
                    call.session_id.as_deref(),
                    result.session_id.as_deref(),
                )
                && call_result_context_matches(call.turn_id.as_deref(), result.turn_id.as_deref())
        })
        .collect::<Vec<_>>();
    let exact = candidates
        .iter()
        .copied()
        .filter(|call| {
            context_matches(call.session_id.as_deref(), result.session_id.as_deref())
                && context_matches(call.turn_id.as_deref(), result.turn_id.as_deref())
        })
        .collect::<Vec<_>>();
    if let Some(call) = exact.first().copied() {
        return Some(call);
    }
    (candidates.len() == 1)
        .then(|| candidates.into_iter().next())
        .flatten()
}

fn same_call(left: &ToolCall, right: &ToolCall) -> bool {
    left.call_id == right.call_id
        && left.session_id == right.session_id
        && left.turn_id == right.turn_id
        && left.provenance == right.provenance
}

fn source_precedes(left: &SourceRef, right: &SourceRef) -> bool {
    left.kind == right.kind
        && left.path == right.path
        && matches!((left.line, right.line), (Some(left), Some(right)) if left < right)
}

fn matching_results<'a>(
    results: &'a [ToolResult],
    calls: &[ToolCall],
    call: &ToolCall,
) -> Vec<&'a ToolResult> {
    if call.call_id.is_none()
        && calls
            .iter()
            .filter(|candidate| {
                candidate.call_id.is_none()
                    && context_matches(candidate.session_id.as_deref(), call.session_id.as_deref())
                    && context_matches(candidate.turn_id.as_deref(), call.turn_id.as_deref())
            })
            .count()
            != 1
    {
        return Vec::new();
    }

    let candidates = results
        .iter()
        .filter(|result| {
            !result.is_duplicate
                && result.call_id == call.call_id
                && call_result_context_matches(
                    result.session_id.as_deref(),
                    call.session_id.as_deref(),
                )
                && call_result_context_matches(result.turn_id.as_deref(), call.turn_id.as_deref())
                && (call.call_id.is_none()
                    || matching_call(calls, result)
                        .is_some_and(|candidate| same_call(candidate, call)))
        })
        .collect::<Vec<_>>();
    let exact = candidates
        .iter()
        .copied()
        .filter(|result| {
            context_matches(result.session_id.as_deref(), call.session_id.as_deref())
                && context_matches(result.turn_id.as_deref(), call.turn_id.as_deref())
        })
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }
    if candidates.len() == 1 {
        candidates
    } else {
        Vec::new()
    }
}

fn call_is_failed(results: &[ToolResult], calls: &[ToolCall], call: &ToolCall) -> bool {
    let matched = matching_results(results, calls, call);
    if matched.iter().any(|result| result.exit_code.is_some()) {
        return matched
            .iter()
            .any(|result| result.exit_code.is_some_and(|code| code != 0));
    }
    if matched.iter().any(|result| result.status.is_some()) {
        return matched.iter().any(|result| result_is_failed(result));
    }
    call.status.as_deref().and_then(ToolOutcome::from_status) == Some(ToolOutcome::Failed)
}

fn result_is_failed(result: &ToolResult) -> bool {
    if let Some(code) = result.exit_code {
        return code != 0;
    }
    result.outcome == ToolOutcome::Failed
        || result.status.as_deref().and_then(ToolOutcome::from_status) == Some(ToolOutcome::Failed)
}

fn call_has_failed_result(results: &[ToolResult], calls: &[ToolCall], call: &ToolCall) -> bool {
    matching_results(results, calls, call)
        .into_iter()
        .any(result_is_failed)
}

fn mark_duplicate_tool_results(prior: &[ToolResult], results: &mut [ToolResult]) {
    // ponytail: O(n^2) result dedup; index by call_id if large histories make it measurable.
    let mut representatives = Vec::new();
    for index in 0..results.len() {
        if let Some(previous) = prior
            .iter()
            .find(|previous| equivalent_results(previous, &results[index]))
        {
            results[index].deduplication_key = previous
                .deduplication_key
                .clone()
                .or_else(|| Some(result_key(previous)));
            results[index].equivalent_to = Some(previous.provenance.clone());
            results[index].is_duplicate = true;
            continue;
        }
        let Some(previous) = representatives
            .iter()
            .copied()
            .find(|previous| equivalent_results(&results[*previous], &results[index]))
        else {
            if results[index].call_id.is_some() {
                results[index].deduplication_key = Some(result_key(&results[index]));
            }
            representatives.push(index);
            continue;
        };
        if result_quality(&results[index]) > result_quality(&results[previous]) {
            results[previous].deduplication_key = Some(result_key(&results[index]));
            results[previous].equivalent_to = Some(results[index].provenance.clone());
            results[previous].is_duplicate = true;
            results[index].deduplication_key = Some(result_key(&results[index]));
            if let Some(representative) = representatives
                .iter_mut()
                .find(|representative| **representative == previous)
            {
                *representative = index;
            }
        } else if results[index].call_id.is_some() {
            results[index].deduplication_key = results[previous]
                .deduplication_key
                .clone()
                .or_else(|| Some(result_key(&results[previous])));
            results[index].equivalent_to = Some(results[previous].provenance.clone());
            results[index].is_duplicate = true;
        }
    }
}

fn result_quality(result: &ToolResult) -> u8 {
    if result.exit_code.is_some() {
        3
    } else if result.status.is_some() {
        2
    } else if !combined_output(result).is_empty() {
        1
    } else {
        0
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
    if left.outcome == ToolOutcome::Unknown && right.outcome == ToolOutcome::Unknown {
        return left.command == right.command
            && left.cwd == right.cwd
            && left.stdout == right.stdout
            && left.stderr == right.stderr
            && left.duration_ms == right.duration_ms
            && left.exit_code == right.exit_code
            && left.status == right.status
            && left.outcome_source == right.outcome_source;
    }
    if left.outcome != ToolOutcome::Unknown
        && right.outcome != ToolOutcome::Unknown
        && left.outcome != right.outcome
    {
        let one_exit_code = left.exit_code.is_some() ^ right.exit_code.is_some();
        let output_representation = left.outcome_source == OutcomeSource::OutputText
            || right.outcome_source == OutcomeSource::OutputText;
        if !one_exit_code && !output_representation {
            return false;
        }
    }
    true
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
    renderer: RendererParse,
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
    if let Some(status) = status {
        return (
            ToolOutcome::from_status(status).unwrap_or(ToolOutcome::Unknown),
            OutcomeSource::Status,
        );
    }
    if let RendererParse::Known(status) = renderer {
        return (status.outcome(), OutcomeSource::ParsedRenderer);
    }
    if output_indicates_failure(stdout, stderr) {
        return (ToolOutcome::Failed, OutcomeSource::OutputText);
    }
    (ToolOutcome::Unknown, OutcomeSource::Unknown)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendererParse {
    NotRenderer,
    Malformed,
    Known(RendererStatus),
}

impl RendererParse {
    fn exit_code(self) -> Option<i64> {
        match self {
            Self::Known(status) => status.exit_code(),
            Self::NotRenderer | Self::Malformed => None,
        }
    }

    fn status(self) -> Option<String> {
        match self {
            Self::Known(status) => status.status().map(str::to_owned),
            Self::NotRenderer | Self::Malformed => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendererStatus {
    Succeeded {
        exit_code: Option<i64>,
    },
    Failed {
        exit_code: Option<i64>,
        status: &'static str,
    },
}

impl RendererStatus {
    fn outcome(self) -> ToolOutcome {
        match self {
            Self::Succeeded { .. } => ToolOutcome::Succeeded,
            Self::Failed { .. } => ToolOutcome::Failed,
        }
    }

    fn exit_code(self) -> Option<i64> {
        match self {
            Self::Succeeded { exit_code } | Self::Failed { exit_code, .. } => exit_code,
        }
    }

    fn status(self) -> Option<&'static str> {
        match self {
            Self::Succeeded { .. } => Some("completed"),
            Self::Failed { status, .. } => Some(status),
        }
    }
}

fn parse_renderer_status(outputs: &[Option<&str>]) -> RendererParse {
    let mut malformed = false;
    for parsed in outputs.iter().copied().flatten().map(parse_renderer_text) {
        match parsed {
            RendererParse::Known(status) => return RendererParse::Known(status),
            RendererParse::Malformed => malformed = true,
            RendererParse::NotRenderer => {}
        }
    }
    if malformed {
        RendererParse::Malformed
    } else {
        RendererParse::NotRenderer
    }
}

fn parse_renderer_text(output: &str) -> RendererParse {
    let Some(line) = output.trim_start().lines().next().map(str::trim) else {
        return RendererParse::NotRenderer;
    };
    let line = line.to_ascii_lowercase();
    for prefix in ["process exited with code", "exit code"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let rest = rest.trim();
            let code = rest.strip_prefix(':').unwrap_or(rest).trim().parse::<i64>();
            return code.map_or(RendererParse::Malformed, |exit_code| {
                if exit_code == 0 {
                    RendererParse::Known(RendererStatus::Succeeded { exit_code: Some(0) })
                } else {
                    RendererParse::Known(RendererStatus::Failed {
                        exit_code: Some(exit_code),
                        status: "failed",
                    })
                }
            });
        }
    }
    match line.as_str() {
        "script completed" => RendererParse::Known(RendererStatus::Succeeded { exit_code: None }),
        "script failed" => RendererParse::Known(RendererStatus::Failed {
            exit_code: None,
            status: "failed",
        }),
        "script timed out" | "script timeout" => RendererParse::Known(RendererStatus::Failed {
            exit_code: None,
            status: "timeout",
        }),
        _ if line.starts_with("process exited with code")
            || line.starts_with("exit code")
            || line.starts_with("script ") =>
        {
            RendererParse::Malformed
        }
        _ => RendererParse::NotRenderer,
    }
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
    matches!(
        kind,
        Some("exec_command_begin" | "mcp_tool_call_begin" | "patch_apply_begin")
    )
}

fn is_event_tool_result_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("exec_command_end" | "mcp_tool_call_end" | "patch_apply_end")
    )
}

fn is_lifecycle_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("task_started" | "task_complete" | "turn_started" | "turn_complete" | "turn_aborted")
    )
}

fn canonical_error_category(kind: Option<&str>) -> Option<String> {
    match kind {
        Some("error") => Some("generic_error".to_owned()),
        Some("stream_error") => Some("stream_error".to_owned()),
        Some("exec_error") => Some("exec_error".to_owned()),
        _ => None,
    }
}

fn is_terminal_nested_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("turn_complete" | "turn_aborted" | "task_complete" | "shutdown_complete")
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
        let missing_root = std::env::temp_dir().join(format!(
            "codexlens-normalize-missing-project-{}",
            std::process::id()
        ));
        let missing_cwd = missing_root.join("project");
        let input = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": "fixture-instruction-session",
                    "cwd": missing_cwd.to_string_lossy(),
                    "project": missing_root.to_string_lossy(),
                },
            }),
            serde_json::json!({
                "type": "turn_context",
                "payload": {
                    "turn_id": "fixture-instruction-turn-001",
                    "cwd": missing_cwd.to_string_lossy(),
                    "user_instructions": instruction_text,
                },
            }),
            serde_json::json!({
                "type": "turn_context",
                "payload": {
                    "turn_id": "fixture-instruction-turn-002",
                    "cwd": missing_cwd.to_string_lossy(),
                    "user_instructions": instruction_text,
                },
            }),
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
    fn normalizes_and_deduplicates_paired_file_operations() {
        let call = |path: &str, line| ToolCall {
            id: None,
            call_id: Some("fixture-paired-call".to_owned()),
            session_id: Some("fixture-session".to_owned()),
            turn_id: Some("fixture-turn".to_owned()),
            tool_name: Some("apply_patch".to_owned()),
            input_summary: None,
            command: Some(format!("*** Update File: {path}")),
            cwd: Some("/fixture/project".to_owned()),
            status: None,
            provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), line),
        };
        let mut data = CanonicalData {
            tool_calls: vec![call("src/lib.rs", 1), call("./src/lib.rs", 2)],
            ..CanonicalData::default()
        };

        let calls = data.tool_calls.clone();
        extract_file_operations(&mut data, &[], &calls);

        assert_eq!(data.file_operations.len(), 1);
        assert_eq!(data.file_operations[0].path, "src/lib.rs");
        assert_eq!(normalize_path("../src/lib.rs"), "../src/lib.rs");
        assert_eq!(
            canonical_command_value(&serde_json::json!({"argv": ["cargo", "test"]})),
            "cargo test"
        );
    }

    #[test]
    fn preserves_path_only_file_payloads_and_deduplicates_the_matched_result() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-file-session","cwd":"/fixture/project","project":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-file-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-file-call","name":"edit_file","input":{"path":"src/lib.rs"}}}
{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"fixture-file-call","command":{"path":"src/lib.rs"},"exit_code":0,"status":"completed"}}"#,
        );

        assert_eq!(data.file_operations.len(), 1);
        assert_eq!(data.file_operations[0].path, "src/lib.rs");
    }

    #[test]
    fn exit_code_takes_precedence_over_a_conflicting_failed_status() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-status-session","cwd":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-status-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-status-call","name":"edit_file","status":"failed","input":{"path":"src/lib.rs"}}}
{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"fixture-status-call","patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n","exit_code":0,"status":"failed"}}"#,
        );

        assert_eq!(data.file_operations.len(), 1);
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Succeeded);
    }

    #[test]
    fn missing_call_id_file_operations_are_deduplicated_and_failed_edits_ignored() {
        let succeeded = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-no-id-session","cwd":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-no-id-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","name":"edit_file","input":{"path":"src/lib.rs"}}}
{"type":"event_msg","payload":{"type":"patch_apply_end","patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n","exit_code":0,"status":"completed"}}"#,
        );
        assert_eq!(succeeded.file_operations.len(), 1);

        let separate_operations = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-file-two-session","cwd":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-file-two-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","name":"edit_file","input":{"path":"src/lib.rs"}}}
{"type":"response_item","payload":{"type":"custom_tool_call","name":"edit_file","input":{"path":"src/lib.rs"}}}
{"type":"event_msg","payload":{"type":"patch_apply_end","patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n","exit_code":0,"status":"completed"}}
{"type":"event_msg","payload":{"type":"patch_apply_end","patch":"*** Update File: src/lib.rs\n@@\n-new\n+newer\n","exit_code":0,"status":"completed"}}"#,
        );
        assert_eq!(separate_operations.file_operations.len(), 2);

        let failed = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-no-id-failed-session","cwd":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-no-id-failed-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","name":"edit_file","input":{"path":"src/lib.rs"}}}
{"type":"event_msg","payload":{"type":"patch_apply_end","patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n","exit_code":1,"status":"failed"}}"#,
        );
        assert!(failed.file_operations.is_empty());
    }

    #[test]
    fn turn_context_without_id_clears_the_previous_turn() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-turn-session"}}
{"type":"turn_context","payload":{"turn_id":"fixture-first-turn"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-first-call","name":"exec_command","input":{"cmd":"cargo test"}}}
{"type":"turn_context","payload":{}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-second-call","name":"exec_command","input":{"cmd":"cargo test"}}}"#,
        );

        assert_eq!(data.tool_calls.len(), 2);
        assert_eq!(
            data.tool_calls[0].turn_id.as_deref(),
            Some("fixture-first-turn")
        );
        assert_eq!(data.tool_calls[1].turn_id, None);
    }

    #[test]
    fn patch_events_are_canonicalized_once_and_failed_edits_are_ignored() {
        let mut data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-patch-session","cwd":"/fixture/project","project":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-patch-turn","cwd":"/fixture/project"}}
{"type":"event_msg","payload":{"type":"patch_apply_begin","call_id":"fixture-patch-call","patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n"}}
{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"fixture-patch-call","patch":"*** Update File: /fixture/project/src/lib.rs\n@@\n-old\n+new\n","status":"completed","output":"applied"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"fixture-patch-call","command":{"path":"/fixture/project/src/lib.rs"},"status":"completed","output":"applied with different wording"}}"#,
        );

        assert_eq!(data.tool_calls.len(), 1);
        assert_eq!(data.tool_results.len(), 2);
        assert_eq!(
            data.tool_results
                .iter()
                .filter(|result| !result.is_duplicate)
                .count(),
            1
        );
        assert_eq!(data.file_operations.len(), 1);

        data.tool_calls[0].turn_id = None;
        data.file_operations.clear();
        let calls = data.tool_calls.clone();
        extract_file_operations(&mut data, &[], &calls);
        assert_eq!(data.file_operations.len(), 1);

        for result in &mut data.tool_results {
            result.turn_id = None;
        }
        data.file_operations.clear();
        let calls = data.tool_calls.clone();
        extract_file_operations(&mut data, &[], &calls);
        assert_eq!(data.file_operations.len(), 1);

        let failed = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-failed-patch-session","cwd":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-failed-patch-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-failed-patch-call","name":"apply_patch","input":{"patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n"}}}
{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"fixture-failed-patch-call","patch":"*** Update File: src/lib.rs\n@@\n-old\n+new\n","exit_code":1,"status":"failed"}}"#,
        );
        assert!(failed.file_operations.is_empty());
        assert_eq!(failed.tool_results[0].command, None);
    }

    #[test]
    fn path_only_payloads_do_not_treat_natural_language_as_a_file() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-path-session","cwd":"/fixture/project"}}
{"type":"turn_context","payload":{"turn_id":"fixture-path-turn","cwd":"/fixture/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-path-call","name":"edit_file","input":{"path":"Please edit src/lib.rs"}}}"#,
        );

        assert!(data.file_operations.is_empty());
    }

    #[test]
    fn file_operations_require_typed_provenance_and_valid_targets() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/parser-artifacts-file-operations.jsonl"
        ));

        assert_eq!(
            data.file_operations
                .iter()
                .map(|operation| operation.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "src/patched.rs",
                "/fixture/project/quoted absolute.txt",
                "src/quoted output.txt",
                "src/quoted marker output.txt",
                "/fixture/project/from-json.txt",
            ]
        );
        assert!(data.file_operations.iter().all(|operation| {
            operation.path != "/dev/null" && !operation.path.starts_with("/dev/null/")
        }));

        assert!(
            crate::analysis::analyze_rework(&data, &crate::analysis::AnalysisOptions::default())
                .is_empty()
        );
        assert!(crate::analysis::views::stuck(&data).rows.is_empty());
        assert!(
            crate::analysis::views::waste(&data)
                .opportunities
                .is_empty()
        );
    }

    #[test]
    fn tool_names_and_commands_only_use_valid_structured_values() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-tool-boundary-session"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-wrapper-name","name":"exec_command const","input":{"cmd":"cargo test"}}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-wrapper-name-short","name":"s:12000});","input":{"cmd":"cargo test"}}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-wrapper-input","name":"exec_command","input":"exec_command s:12000});"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-structured-input-array","name":"exec_command","input":["cargo","test"]}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-structured-command","name":"exec_command","input":{"argv":["cargo","test"]}}}
{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"fixture-unstructured-result","command":["cargo",{"renderer":"test"}],"exit_code":1}}"#,
        );

        let wrapper_name = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-wrapper-name"))
            .unwrap();
        assert_eq!(wrapper_name.tool_name, None);
        assert_eq!(wrapper_name.command, Some("cargo test".to_owned()));

        let wrapper_name_short = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-wrapper-name-short"))
            .unwrap();
        assert_eq!(wrapper_name_short.tool_name, None);

        let wrapper_input = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-wrapper-input"))
            .unwrap();
        assert_eq!(wrapper_input.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(wrapper_input.command, None);

        let structured_input_array = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-structured-input-array"))
            .unwrap();
        assert_eq!(
            structured_input_array.command.as_deref(),
            Some("cargo test")
        );

        let structured = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-structured-command"))
            .unwrap();
        assert_eq!(structured.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(structured.command.as_deref(), Some("cargo test"));

        let unstructured_result = data
            .tool_results
            .iter()
            .find(|result| result.call_id.as_deref() == Some("fixture-unstructured-result"))
            .unwrap();
        assert_eq!(unstructured_result.command, None);
    }

    #[test]
    fn keeps_wrapper_calls_opaque_without_safe_command_fields() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/wrapper-tools.jsonl"
        ));

        let direct = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-direct-shell-a"))
            .unwrap();
        assert_eq!(direct.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(direct.command.as_deref(), Some("cargo test"));

        let nested_shell = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-nested-shell-a"))
            .unwrap();
        assert_eq!(nested_shell.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(nested_shell.command.as_deref(), Some("cargo test"));
        assert_eq!(nested_shell.provenance.line, Some(4));
        assert!(
            nested_shell
                .input_summary
                .as_deref()
                .is_some_and(|input| input.contains("tools.exec_command"))
        );

        let nested_js_shell = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-nested-js-shell-b"))
            .unwrap();
        assert_eq!(nested_js_shell.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(nested_js_shell.command.as_deref(), Some("cargo test"));
        assert!(
            nested_js_shell
                .input_summary
                .as_deref()
                .is_some_and(|input| input.contains("tools.exec_command"))
        );

        let nested_patch = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-nested-patch-a"))
            .unwrap();
        assert_eq!(nested_patch.tool_name.as_deref(), Some("apply_patch"));
        assert_eq!(
            nested_patch.command.as_deref(),
            Some("*** Update File: src/lib.rs\n@@\n-old\n+new\n")
        );
        assert!(
            nested_patch
                .input_summary
                .as_deref()
                .is_some_and(|input| input.contains("tools.apply_patch"))
        );

        assert!(!data.tool_calls.iter().any(|call| {
            call.call_id
                .as_deref()
                .is_some_and(|id| id.contains("/nested/"))
        }));
        assert!(!data.tool_results.iter().any(|result| {
            result
                .call_id
                .as_deref()
                .is_some_and(|id| id.contains("/nested/"))
        }));
        assert_eq!(
            data.file_operations
                .iter()
                .map(|operation| operation.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );

        let opaque_call = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-opaque-wrapper-a"))
            .unwrap();
        assert_eq!(opaque_call.tool_name.as_deref(), Some("exec"));
        assert_eq!(opaque_call.command, None);
        let opaque_result = data
            .tool_results
            .iter()
            .find(|result| result.call_id.as_deref() == Some("fixture-opaque-wrapper-a"))
            .unwrap();
        assert_eq!(opaque_result.provenance.line, Some(9));
        assert!(!opaque_result.is_duplicate);

        let structured_wrapper = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-structured-wrapper-b"))
            .unwrap();
        assert_eq!(structured_wrapper.tool_name.as_deref(), Some("exec"));
        assert_eq!(structured_wrapper.command, None);
        assert!(data.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::OpaqueToolInput
                && diagnostic.session_id.as_deref() == Some("fixture-wrapper-session-b")
        }));

        assert!(
            !data
                .tool_results
                .iter()
                .any(
                    |result| result.call_id.as_deref() == Some("fixture-nested-shell-a")
                        && result.is_duplicate
                )
        );
    }

    #[test]
    fn extracts_one_unambiguous_nested_browser_call_but_keeps_ambiguous_scripts_unknown() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-browser-session"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-browser-call","name":"js","input":"await tools.browser({url: \"https://example.test\"});"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"fixture-browser-call","exit_code":0,"status":"completed"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-ambiguous-wrapper","name":"exec","input":"await tools.exec_command({cmd: \"cargo test\"}); await tools.exec_command({cmd: \"cargo check\"});"}}"#,
        );

        let browser = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-browser-call"))
            .unwrap();
        assert_eq!(browser.tool_name.as_deref(), Some("browser"));
        assert_eq!(browser.command, None);
        assert!(
            browser
                .input_summary
                .as_deref()
                .is_some_and(|input| input.contains("tools.browser"))
        );

        let ambiguous = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.as_deref() == Some("fixture-ambiguous-wrapper"))
            .unwrap();
        assert_eq!(ambiguous.tool_name.as_deref(), Some("exec"));
        assert_eq!(ambiguous.command, None);

        let prefixed = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-prefixed-session"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-prefixed-wrapper","name":"exec","input":"await mytools.exec_command({cmd: \"cargo test\"});"}}"#,
        );
        let prefixed = &prefixed.tool_calls[0];
        assert_eq!(prefixed.tool_name.as_deref(), Some("exec"));
        assert_eq!(prefixed.command, None);
    }

    #[test]
    fn decodes_string_encoded_tool_arguments_without_promoting_arbitrary_text() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/string-encoded-commands.jsonl"
        ));
        let call = |call_id: &str| {
            data.tool_calls
                .iter()
                .find(|call| call.call_id.as_deref() == Some(call_id))
                .unwrap()
        };

        let function_call = call("fixture-string-function-call");
        assert_eq!(function_call.command.as_deref(), Some("cargo run"));
        assert_eq!(function_call.input_summary.as_deref(), Some("cargo run"));
        assert_eq!(function_call.provenance.line, Some(3));

        let argv_call = call("fixture-string-argv-call");
        assert_eq!(argv_call.command.as_deref(), Some("cargo check"));

        let command_array_call = call("fixture-string-command-array-call");
        assert_eq!(command_array_call.command.as_deref(), Some("cargo build"));

        let file_call = call("fixture-string-file-call");
        assert_eq!(
            file_call.command.as_deref(),
            Some("*** Update File: src/lib.rs\n@@\n-old\n+new\n")
        );
        assert_eq!(data.file_operations.len(), 2);
        assert_eq!(data.file_operations[0].path, "src/lib.rs");
        assert_eq!(data.file_operations[1].path, "src/second.rs");

        let verification_call = data
            .tool_calls
            .iter()
            .find(|call| call.call_id.is_none() && call.command.as_deref() == Some("cargo check"))
            .unwrap();
        assert_eq!(verification_call.command.as_deref(), Some("cargo check"));

        let fallback_call = call("fixture-string-fallback-call");
        assert_eq!(fallback_call.command.as_deref(), Some("cargo fmt"));

        let second_file_call = call("fixture-string-second-file-call");
        assert_eq!(
            second_file_call.command.as_deref(),
            Some("*** Update File: src/second.rs\n@@\n-old\n+new\n")
        );

        for call_id in [
            "fixture-string-malformed-call",
            "fixture-string-wrapper-call",
            "fixture-string-natural-call",
        ] {
            let call = call(call_id);
            assert_eq!(call.command, None, "{call_id}");
        }
        assert_eq!(
            crate::analysis::classify_verification_command(
                verification_call.command.as_deref().unwrap()
            ),
            Some("check".to_owned())
        );

        let failure =
            crate::analysis::analyze_failures(&data, &crate::analysis::AnalysisOptions::default())
                .into_iter()
                .find(|finding| {
                    finding.kind == crate::analysis::FindingType::Failure
                        && finding.key.contains("|cargo run|")
                })
                .unwrap();
        assert_eq!(failure.scope.as_str(), "project");
        assert_eq!(failure.occurrences, 2);
        assert_eq!(failure.distinct_sessions, 2);
        assert_eq!(failure.confidence, crate::analysis::FindingConfidence::High);
        assert_eq!(failure.evidence.len(), 2);
        assert!(
            failure
                .evidence
                .iter()
                .all(|evidence| evidence.role == crate::analysis::EvidenceRole::Observation)
        );
        assert_eq!(
            failure
                .evidence
                .iter()
                .map(|evidence| evidence.source.line)
                .collect::<Vec<_>>(),
            vec![Some(14), Some(19)]
        );
        assert!(
            failure
                .evidence
                .iter()
                .all(|evidence| evidence.source.path == Path::new("fixture.jsonl"))
        );

        let verification = crate::analysis::analyze_verification(
            &data,
            &crate::analysis::AnalysisOptions::default(),
        );
        let missing = verification
            .iter()
            .find(|finding| finding.affected_paths == ["src/second.rs"])
            .unwrap();
        assert_eq!(missing.kind, crate::analysis::FindingType::Verification);
        assert_eq!(
            missing.scope,
            crate::analysis::FindingScope::Project(PathBuf::from("/fixture/project"))
        );
        assert_eq!(missing.confidence, crate::analysis::FindingConfidence::Low);
        assert_eq!(missing.occurrences, 1);
        assert_eq!(missing.distinct_sessions, 1);
        assert_eq!(missing.evidence.len(), 2);
        assert!(
            missing
                .observed_commands
                .iter()
                .any(|command| command.contains("cargo check"))
        );
        assert!(missing.evidence.iter().any(|evidence| {
            evidence.role == crate::analysis::EvidenceRole::VerificationCommand
                && evidence.source.path == Path::new("fixture.jsonl")
                && evidence.source.line == Some(8)
        }));

        let function_result = data
            .tool_results
            .iter()
            .find(|result| result.call_id.as_deref() == Some("fixture-string-function-call"))
            .unwrap();
        assert_eq!(function_result.command, None);
    }

    #[test]
    fn archived_rollout_source_sets_archive_state() {
        let parsed = parse_rollout_reader(
            Path::new("archived_sessions/2026/archived.jsonl"),
            PlainJsonlReader::new(Cursor::new(
                include_str!("../tests/fixtures/rollout/archived-selection.jsonl").as_bytes(),
            )),
        );

        let data = normalize_rollout(&parsed);

        assert_eq!(data.sessions[0].archive_state, Some(true));
    }

    #[test]
    fn shutdown_complete_is_a_terminal_record() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-shutdown-session"}}
{"type":"event_msg","payload":{"type":"shutdown_complete"}}"#,
        );

        assert!(data.records.last().is_some_and(|record| record.is_terminal));
    }

    #[test]
    fn streaming_output_delta_is_not_a_completed_tool_result() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-stream-session"}}
{"type":"turn_context","payload":{"turn_id":"fixture-stream-turn"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-stream-call","name":"exec_command","input":{"cmd":"cargo test"}}}
{"type":"event_msg","payload":{"type":"exec_command_output_delta","call_id":"fixture-stream-call","command":["cargo","test"],"output":"partial"}}"#,
        );

        assert!(data.tool_results.is_empty());
        assert_eq!(
            data.records
                .last()
                .and_then(|record| record.original_nested_type.as_deref()),
            Some("exec_command_output_delta")
        );
    }

    #[test]
    fn structured_result_wins_order_independent_deduplication() {
        let base = ToolResult {
            id: None,
            call_id: Some("fixture-result-call".to_owned()),
            session_id: Some("fixture-result-session".to_owned()),
            turn_id: Some("fixture-result-turn".to_owned()),
            command: Some("cargo test".to_owned()),
            cwd: None,
            stdout: None,
            stderr: None,
            duration_ms: None,
            exit_code: None,
            status: None,
            outcome: ToolOutcome::Unknown,
            outcome_source: OutcomeSource::Unknown,
            matched_call: false,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 1),
        };
        let mut results = vec![
            ToolResult {
                stdout: Some("same output".to_owned()),
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 2),
                ..base.clone()
            },
            ToolResult {
                stdout: Some("same output".to_owned()),
                exit_code: Some(1),
                status: Some("failed".to_owned()),
                outcome: ToolOutcome::Failed,
                outcome_source: OutcomeSource::ExitCode,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 3),
                ..base.clone()
            },
        ];

        mark_duplicate_tool_results(&[], &mut results);

        assert!(results[0].is_duplicate);
        assert!(!results[1].is_duplicate);
        assert_eq!(results[1].exit_code, Some(1));

        let mut conflicting = vec![
            ToolResult {
                stdout: Some("error output".to_owned()),
                outcome: ToolOutcome::Failed,
                outcome_source: OutcomeSource::OutputText,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 4),
                ..base.clone()
            },
            ToolResult {
                exit_code: Some(0),
                status: Some("completed".to_owned()),
                outcome: ToolOutcome::Succeeded,
                outcome_source: OutcomeSource::ExitCode,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 5),
                ..base.clone()
            },
        ];
        mark_duplicate_tool_results(&[], &mut conflicting);
        assert!(conflicting[0].is_duplicate);
        assert!(!conflicting[1].is_duplicate);

        let mut unknown = vec![
            ToolResult {
                command: Some("cargo test".to_owned()),
                stdout: Some("first".to_owned()),
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 6),
                ..base.clone()
            },
            ToolResult {
                command: Some("cargo check".to_owned()),
                stdout: Some("second".to_owned()),
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 7),
                ..base
            },
        ];
        mark_duplicate_tool_results(&[], &mut unknown);
        assert!(unknown.iter().all(|result| !result.is_duplicate));
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
    fn parses_renderer_envelopes_before_fallback_text() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/tool-result-envelopes.jsonl"
        ));

        assert_eq!(data.tool_results.len(), 8);
        assert_eq!(data.tool_results[0].exit_code, Some(0));
        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Succeeded);
        assert_eq!(
            data.tool_results[0].outcome_source,
            OutcomeSource::ParsedRenderer
        );
        assert_eq!(data.tool_results[1].exit_code, Some(23));
        assert_eq!(data.tool_results[1].outcome, ToolOutcome::Failed);
        assert_eq!(
            data.tool_results[1].outcome_source,
            OutcomeSource::ParsedRenderer
        );
        assert_eq!(data.tool_results[1].stdout, None);
        let deduplication_key = data.tool_results[1].deduplication_key.as_deref().unwrap();
        assert!(!deduplication_key.contains("command output did not include"));
        assert_eq!(data.tool_results[2].outcome, ToolOutcome::Failed);
        assert_eq!(data.tool_results[2].status.as_deref(), Some("timeout"));
        assert_eq!(
            data.tool_results[2].outcome_source,
            OutcomeSource::ParsedRenderer
        );
        assert_eq!(data.tool_results[3].outcome, ToolOutcome::Failed);
        assert_eq!(data.tool_results[3].status.as_deref(), Some("failed"));
        assert_eq!(
            data.tool_results[3].outcome_source,
            OutcomeSource::ParsedRenderer
        );
        assert_eq!(data.tool_results[4].outcome, ToolOutcome::Unknown);
        assert_eq!(data.tool_results[4].outcome_source, OutcomeSource::Unknown);
        assert_eq!(data.tool_results[5].outcome, ToolOutcome::Succeeded);
        assert_eq!(data.tool_results[5].status.as_deref(), Some("completed"));
        assert_eq!(
            data.tool_results[5].outcome_source,
            OutcomeSource::ParsedRenderer
        );
        assert_eq!(data.tool_results[6].outcome, ToolOutcome::Failed);
        assert_eq!(data.tool_results[6].status, None);
        assert_eq!(
            data.tool_results[6].outcome_source,
            OutcomeSource::OutputText
        );
        assert_eq!(data.tool_results[7].outcome, ToolOutcome::Unknown);
        assert_eq!(data.tool_results[7].outcome_source, OutcomeSource::Unknown);
    }

    #[test]
    fn malformed_renderer_output_does_not_hide_explicit_stderr_failure() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-malformed-renderer-session"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","output":"Process exited with code\nrenderer body","stderr":"permission denied"}}"#,
        );

        assert_eq!(data.tool_results[0].outcome, ToolOutcome::Failed);
        assert_eq!(
            data.tool_results[0].outcome_source,
            OutcomeSource::OutputText
        );
    }

    #[test]
    fn renderer_like_explicit_streams_use_fallback_output() {
        let data = parse(include_str!(
            "../tests/fixtures/rollout/explicit-renderer-streams.jsonl"
        ));

        let result = &data.tool_results[0];
        assert_eq!(result.outcome, ToolOutcome::Failed);
        assert_eq!(result.outcome_source, OutcomeSource::OutputText);
        assert_eq!(result.exit_code, None);
        assert_eq!(result.status, None);
        assert!(
            result
                .stdout
                .as_deref()
                .is_some_and(|value| value.starts_with("Script completed"))
        );
        assert!(
            result
                .stderr
                .as_deref()
                .is_some_and(|value| value.starts_with("Script failed"))
        );
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
    fn missing_result_context_is_not_matched_when_call_id_is_ambiguous() {
        let data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-first-session"}}
{"type":"turn_context","payload":{"turn_id":"fixture-first-turn"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-reused-call","name":"exec_command","input":{"cmd":"cargo test"}}}
{"type":"session_meta","payload":{"id":"fixture-second-session"}}
{"type":"turn_context","payload":{"turn_id":"fixture-second-turn"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-reused-call","name":"exec_command","input":{"cmd":"cargo test"}}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"fixture-reused-call","exit_code":0,"status":"completed"}}"#,
        );
        let mut result = data.tool_results[0].clone();
        result.session_id = None;
        result.turn_id = None;
        assert!(matching_call(&data.tool_calls, &result).is_none());
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
        assert_eq!(data.tool_results[1].outcome, ToolOutcome::Unknown);
        assert_eq!(data.tool_results[1].outcome_source, OutcomeSource::Unknown);
        assert!(!data.tool_results[1].matched_call);
        assert_eq!(data.token_usage.len(), 1);
    }

    #[test]
    fn repeated_rollout_metadata_does_not_create_false_identity_conflicts() {
        let path = Path::new(
            "sessions/2026/01/02/rollout-2026-01-02T00-00-00-fixture-main-thread_fixture-rollout.jsonl",
        );
        let result = parse_rollout_reader(
            path,
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/session-identities.jsonl"
            ))),
        );

        let state = Session {
            id: "fixture-state-key".to_owned(),
            rollout_id: Some("fixture-rollout".to_owned()),
            session_id: Some("fixture-session-tree".to_owned()),
            thread_id: None,
            created_at: None,
            updated_at: None,
            cwd: Some("/fixture/main".to_owned()),
            project: None,
            model: None,
            provider: None,
            source: None,
            thread_source: None,
            rollout_path: Some(path.to_string_lossy().into_owned()),
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
        let data = normalize_rollout_with_state(&result, &[state]);

        assert_eq!(data.sessions.len(), 1);
        assert_eq!(data.sessions[0].id, "fixture-main-thread");
        assert_eq!(
            data.sessions[0].rollout_id.as_deref(),
            Some("fixture-rollout")
        );
        assert_eq!(
            data.sessions[0].session_id.as_deref(),
            Some("fixture-session-tree")
        );
        assert_eq!(
            data.sessions[0].thread_id.as_deref(),
            Some("fixture-main-thread")
        );
        assert!(
            data.records
                .iter()
                .all(|record| record.session_id.as_deref() == Some("fixture-main-thread"))
        );
        assert!(
            data.diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != DiagnosticKind::MetadataConflict)
        );
    }

    #[test]
    fn parent_thread_identity_is_preserved_for_subagents() {
        let result = parse_rollout_reader(
            Path::new(
                "sessions/2026/01/02/rollout-2026-01-02T00-01-00-fixture-child-thread_fixture-child-rollout.jsonl",
            ),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/session-identities-child.jsonl"
            ))),
        );

        let data = normalize_rollout(&result);
        let session = &data.sessions[0];
        assert_eq!(session.id, "fixture-child-thread");
        assert_eq!(session.session_id.as_deref(), Some("fixture-session-tree"));
        assert_eq!(session.thread_id.as_deref(), Some("fixture-child-thread"));
        assert_eq!(session.parent_id.as_deref(), Some("fixture-main-thread"));
    }

    #[test]
    fn state_fallback_survives_identityless_session_metadata() {
        let result = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/identityless.jsonl"
            ))),
        );
        let state = Session {
            id: "fixture-state-session".to_owned(),
            rollout_id: None,
            session_id: None,
            thread_id: None,
            created_at: None,
            updated_at: None,
            cwd: Some("/fixture/main".to_owned()),
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

        let data = normalize_rollout_with_state(&result, &[state]);

        assert_eq!(data.sessions.len(), 1);
        assert_eq!(data.sessions[0].id, "fixture-state-session");
        assert!(
            data.records
                .iter()
                .all(|record| record.session_id.as_deref() == Some("fixture-state-session"))
        );
    }

    #[test]
    fn state_fallback_rekeys_records_when_rollout_identity_arrives() {
        let result = parse_rollout_reader(
            Path::new("fixture.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/state-fallback-rekey.jsonl"
            ))),
        );
        let state = Session {
            id: "fixture-state-session".to_owned(),
            rollout_id: None,
            session_id: None,
            thread_id: None,
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

        let data = normalize_rollout_with_state(&result, &[state]);

        assert_eq!(data.sessions.len(), 1);
        assert_eq!(data.sessions[0].id, "fixture-rollout-session");
        assert!(
            data.records
                .iter()
                .all(|record| record.session_id.as_deref() == Some("fixture-rollout-session"))
        );
    }

    #[test]
    fn state_fallback_rekeys_parent_references_in_memory() {
        let mut data = parse(
            r#"{"type":"session_meta","payload":{"id":"fixture-parent"}}
{"type":"session_meta","payload":{"id":"fixture-child","parent_id":"fixture-parent"}}"#,
        );
        let mut sessions = data
            .sessions
            .iter()
            .cloned()
            .map(|session| (session.id.clone(), session))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            data.sessions
                .iter()
                .find(|session| session.id == "fixture-child")
                .and_then(|session| session.parent_id.as_deref()),
            Some("fixture-parent")
        );

        rekey_session_references(
            &mut data,
            &mut sessions,
            "fixture-parent",
            "fixture-rollout-parent",
        );

        assert_eq!(
            data.sessions
                .iter()
                .find(|session| session.id == "fixture-child")
                .and_then(|session| session.parent_id.as_deref()),
            Some("fixture-rollout-parent")
        );
        assert_eq!(
            sessions
                .get("fixture-child")
                .and_then(|session| session.parent_id.as_deref()),
            Some("fixture-rollout-parent")
        );
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
            rollout_id: None,
            session_id: None,
            thread_id: None,
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
            rollout_id: None,
            session_id: None,
            thread_id: None,
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

        assert_eq!(data.sessions.len(), 1);
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
            rollout_id: None,
            session_id: None,
            thread_id: None,
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
