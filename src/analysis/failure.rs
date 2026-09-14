//! Failure lens.

use std::collections::BTreeMap;
use std::path::Path;

use crate::model::{OutcomeSource, ToolOutcome, ToolResult};

#[cfg(test)]
use crate::model::CanonicalData;

use super::{
    Activity, ActivityKind, AnalysisContext, AnalysisOptions, DEFAULT_EXCERPT_BYTES,
    DEFAULT_MIN_OCCURRENCES, DEFAULT_MIN_SESSIONS, EvidenceRole, FailureEvent, Finding,
    FindingConfidence, FindingSeverity, FindingType, annotate_snapshot_limitations,
    bounded_excerpt, command_tokens, distinct_sessions, evidence_for, majority_scope,
    normalize_fragment, position_for_source, push_evidence, redact_sensitive, sort_findings,
    strip_command_wrappers,
};

fn command_family(command: &str) -> String {
    let mut tokens = command_tokens(&redact_sensitive(command));
    if tokens.is_empty() {
        return "unknown_command".to_owned();
    }
    strip_command_wrappers(&mut tokens);
    if tokens.is_empty() {
        return "unknown_command".to_owned();
    }
    let executable = Path::new(&tokens[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&tokens[0])
        .to_ascii_lowercase();
    let mut family = vec![executable];
    for token in tokens.iter().skip(1) {
        let normalized = token.to_ascii_lowercase();
        if token.starts_with('-') || !SAFE_COMMAND_WORDS.contains(&normalized.as_str()) {
            continue;
        }
        family.push(normalized);
        if family.len() == 3 {
            break;
        }
    }
    family.join(" ")
}

const SAFE_COMMAND_WORDS: &[&str] = &[
    "build",
    "check",
    "clippy",
    "diff",
    "eslint",
    "fmt",
    "format",
    "lint",
    "mypy",
    "nextest",
    "prettier",
    "pytest",
    "run",
    "test",
    "typecheck",
    "vet",
];

fn normalize_tool(tool: &str) -> String {
    let normalized = normalize_fragment(tool);
    match normalized.as_str() {
        "" => "unknown_tool".to_owned(),
        _ => normalized,
    }
}

pub(super) fn analyze(data: &AnalysisContext<'_>, options: &AnalysisOptions) -> Vec<Finding> {
    let mut grouped: BTreeMap<String, Vec<&FailureEvent>> = BTreeMap::new();
    for event in data.failure_events() {
        grouped.entry(event.key.clone()).or_default().push(event);
    }

    let mut findings = grouped
        .into_iter()
        .filter_map(|(key, mut events)| {
            events.sort_by(|left, right| super::compare_positions(&left.position, &right.position));
            let sessions = distinct_sessions(events.iter().map(|event| event.session_id.as_str()));
            if events.len() < DEFAULT_MIN_OCCURRENCES || sessions.len() < DEFAULT_MIN_SESSIONS {
                return None;
            }
            let first = events.first()?;
            let scope = majority_scope(data, events.iter().map(|event| event.session_id.as_str()));
            let structured = events.iter().all(|event| event.structured);
            let severity = if events.len() >= 3 {
                FindingSeverity::High
            } else {
                FindingSeverity::Medium
            };
            let confidence = if structured {
                FindingConfidence::High
            } else {
                FindingConfidence::Medium
            };
            let mut evidence = Vec::new();
            for event in &events {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(event.session_id.clone()),
                        event.source.clone(),
                        EvidenceRole::Observation,
                        Some(&event.description),
                        options,
                    ),
                );
            }
            let mut limitations = vec![
                "This is recurring observational evidence, not proof that the command is always incorrect".to_owned(),
            ];
            let (observed_commands, suggested_action, summary) = if first.has_canonical_command {
                (
                    vec![bounded_excerpt(&first.family, options.excerpt_max_bytes)],
                    format!(
                        "Document the prerequisite or preferred command for {} in the applicable instructions",
                        first.family
                    ),
                    format!(
                        "Repeated failure for {} {} ({}) observed {} times across {} sessions",
                        first.tool,
                        first.family,
                        first.category,
                        events.len(),
                        sessions.len()
                    ),
                )
            } else {
                limitations.push(
                    "No canonical command was available; the wrapper or non-shell tool input was not safely unpacked, so no shell-command prerequisite was inferred".to_owned(),
                );
                (
                    Vec::new(),
                    "Review the wrapper or non-shell tool outcome; no shell-command prerequisite was inferred"
                        .to_owned(),
                    format!(
                        "Repeated failure for {} without a canonical command ({}) observed {} times across {} sessions",
                        first.tool,
                        first.category,
                        events.len(),
                        sessions.len()
                    ),
                )
            };
            Some(Finding {
                kind: FindingType::Failure,
                severity,
                confidence,
                scope,
                key,
                summary,
                evidence,
                occurrences: events.len(),
                distinct_sessions: sessions.len(),
                affected_paths: Vec::new(),
                observed_commands,
                sequence: Vec::new(),
                suggested_action,
                limitations,
                verification_status: None,
            })
        })
        .collect::<Vec<_>>();
    annotate_snapshot_limitations(data, &mut findings);
    sort_findings(&mut findings);
    findings
}

pub(super) fn activities(data: &AnalysisContext<'_>) -> Vec<Activity> {
    data.failure_events()
        .iter()
        .map(|event| Activity {
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            position: event.position.clone(),
            description: event.description.clone(),
            source: event.source.clone(),
            path: None,
            kind: ActivityKind::Failure,
        })
        .collect()
}

pub(super) fn is_failed(result: &ToolResult) -> bool {
    if let Some(code) = result.exit_code {
        return code != 0;
    }
    result.outcome == ToolOutcome::Failed || result.status.as_deref().is_some_and(status_is_failed)
}

pub(super) fn build_events(data: &AnalysisContext<'_>) -> Vec<FailureEvent> {
    let mut events = Vec::new();
    for result in &data.tool_results {
        if result.is_duplicate || !is_failed(result) {
            continue;
        }
        let Some(session_id) = result.session_id.clone() else {
            continue;
        };
        let call = data.matching_call(result);
        let tool = call
            .and_then(|call| call.tool_name.as_deref())
            .map(normalize_tool)
            .unwrap_or_else(|| "unknown_tool".to_owned());
        let command = if is_non_shell_tool(&tool) {
            ""
        } else {
            result
                .command
                .as_deref()
                .or_else(|| call.and_then(|call| call.command.as_deref()))
                .unwrap_or_default()
        };
        let output = combined_result_output(result);
        let has_canonical_command = !command.trim().is_empty();
        let family = if has_canonical_command {
            command_family(command)
        } else if is_shell_tool(&tool) {
            "unknown_command".to_owned()
        } else {
            "no_canonical_command".to_owned()
        };
        let category = failure_category(result, &output);
        let key = format!("{tool}|{family}|{category}");
        let description = failure_description(&tool, &family, &category, &output);
        events.push(FailureEvent {
            session_id,
            turn_id: result.turn_id.clone(),
            key,
            tool,
            family,
            category,
            has_canonical_command,
            structured: result_is_structured_failure(result),
            description,
            position: position_for_source(data, &result.provenance, None),
            source: result.provenance.clone(),
        });
    }
    for record in &data.records {
        if !record.is_error {
            continue;
        }
        let Some(session_id) = record.session_id.clone() else {
            continue;
        };
        let category = record
            .error_category
            .clone()
            .unwrap_or_else(|| "error".to_owned());
        let key = format!("event|event|{category}");
        events.push(FailureEvent {
            session_id,
            turn_id: record.turn_id.clone(),
            key,
            tool: "event".to_owned(),
            family: "event".to_owned(),
            category,
            has_canonical_command: false,
            structured: true,
            description: "explicit error event".to_owned(),
            position: position_for_source(data, &record.provenance, record.timestamp.as_deref()),
            source: record.provenance.clone(),
        });
    }
    events
}

fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "exec_command" | "shell")
}

fn is_non_shell_tool(tool: &str) -> bool {
    matches!(tool, "exec" | "js" | "wait" | "apply_patch")
}

fn result_is_structured_failure(result: &ToolResult) -> bool {
    matches!(
        result.outcome_source,
        OutcomeSource::ExitCode | OutcomeSource::Status | OutcomeSource::ParsedRenderer
    ) || result.exit_code.is_some()
        || result.status.as_deref().is_some_and(status_is_failed)
}

fn status_is_failed(status: &str) -> bool {
    ToolOutcome::from_status(status) == Some(ToolOutcome::Failed)
}

fn failure_category(result: &ToolResult, output: &str) -> String {
    if let Some(code) = result.exit_code.filter(|code| *code != 0) {
        return match code {
            126 => "permission_denied".to_owned(),
            127 => "command_not_found".to_owned(),
            _ => format!("exit_code_{code}"),
        };
    }
    if let Some(status) = result
        .status
        .as_deref()
        .filter(|status| status_is_failed(status))
    {
        let status = normalize_fragment(status);
        return match status.as_str() {
            "cancelled" | "canceled" => "cancelled".to_owned(),
            "timeout" | "timed_out" => "timeout".to_owned(),
            _ if result.outcome_source == OutcomeSource::ParsedRenderer => {
                "renderer_failed".to_owned()
            }
            _ => "failed_status".to_owned(),
        };
    }
    let normalized = normalize_fragment(output);
    for (marker, category) in [
        ("permission denied", "permission_denied"),
        ("command not found", "command_not_found"),
        ("no such file", "missing_file"),
        ("timed out", "timeout"),
        ("timeout", "timeout"),
        ("parse error", "parse_error"),
        ("syntax error", "syntax_error"),
    ] {
        if normalized.contains(marker) {
            return category.to_owned();
        }
    }
    "output_error".to_owned()
}

fn failure_description(tool: &str, family: &str, category: &str, output: &str) -> String {
    let description = if output.is_empty() {
        format!("{tool} {family} -> {category}")
    } else {
        format!("{tool} {family} -> {category}: {output}")
    };
    bounded_excerpt(&description, DEFAULT_EXCERPT_BYTES)
}

fn combined_result_output(result: &ToolResult) -> String {
    [result.stderr.as_deref(), result.stdout.as_deref()]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    use crate::normalize::normalize_rollout;
    use crate::rollout::{PlainJsonlReader, parse_rollout_reader};

    fn fixture_data() -> CanonicalData {
        let parsed = parse_rollout_reader(
            Path::new("fixture-analysis.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../../tests/fixtures/analysis/lenses.jsonl"
            ))),
        );
        normalize_rollout(&parsed)
    }

    #[test]
    fn failure_events_preserve_structured_and_fallback_classification() {
        assert_eq!(
            command_family(r#"cargo test token="a b" fixture-id-001"#),
            "cargo test"
        );

        let mut output_only = fixture_data();
        for result in &mut output_only.tool_results {
            result.exit_code = None;
            result.status = None;
            result.outcome = ToolOutcome::Failed;
            result.outcome_source = OutcomeSource::OutputText;
        }
        let context = AnalysisContext::new(&output_only);
        let events = context
            .failure_events()
            .iter()
            .filter(|event| event.tool != "event")
            .collect::<Vec<_>>();
        assert!(!events.is_empty());
        assert!(events.iter().all(|event| !event.structured));

        let mut status_only = fixture_data();
        for result in &mut status_only.tool_results {
            result.exit_code = None;
            result.status = Some("failed".to_owned());
            result.outcome = ToolOutcome::Unknown;
            result.outcome_source = OutcomeSource::Unknown;
        }
        assert!(
            !AnalysisContext::new(&status_only)
                .failure_events()
                .iter()
                .filter(|event| event.tool != "event")
                .collect::<Vec<_>>()
                .is_empty()
        );
    }

    #[test]
    fn arbitrary_tool_input_is_not_used_as_a_failure_command() {
        let parsed = parse_rollout_reader(
            Path::new("fixture-failure-boundary.jsonl"),
            PlainJsonlReader::new(Cursor::new(
                br#"{"type":"session_meta","payload":{"id":"fixture-failure-boundary"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-failure-call","name":"exec_command","input":"cargo test"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"fixture-failure-call","exit_code":1,"status":"failed"}}"#,
            )),
        );
        let data = normalize_rollout(&parsed);

        let events = build_events(&AnalysisContext::new(&data));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].family, "unknown_command");
    }

    #[test]
    fn wrapper_failures_do_not_become_shell_prerequisite_findings() {
        let parsed = parse_rollout_reader(
            Path::new("fixture-wrapper-tools.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../../tests/fixtures/rollout/wrapper-tools.jsonl"
            ))),
        );
        let data = normalize_rollout(&parsed);
        let context = AnalysisContext::new(&data);
        let events = context.failure_events();

        assert!(
            events
                .iter()
                .any(|event| { event.tool == "exec" && event.family == "no_canonical_command" })
        );
        assert!(
            events
                .iter()
                .any(|event| { event.tool == "js" && event.family == "no_canonical_command" })
        );
        assert!(events.iter().any(|event| {
            event.tool == "exec_command"
                && event.family == "cargo test"
                && event.category == "exit_code_1"
        }));
        assert!(!events.iter().any(|event| {
            matches!(event.tool.as_str(), "exec" | "js" | "wait")
                && event.family == "unknown_command"
        }));

        let findings = analyze(&context, &AnalysisOptions::default());
        assert!(
            findings
                .iter()
                .filter(|finding| { finding.key.contains("no_canonical_command") })
                .all(|finding| {
                    finding
                        .suggested_action
                        .contains("no shell-command prerequisite")
                        && finding
                            .limitations
                            .iter()
                            .any(|limitation| limitation.contains("No canonical command"))
                })
        );

        let parsed_renderer_command = parse_rollout_reader(
            Path::new("fixture-wrapper-renderer-command.jsonl"),
            PlainJsonlReader::new(Cursor::new(
                br#"{"type":"session_meta","payload":{"id":"fixture-wrapper-renderer"}}
{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"fixture-wrapper-renderer-call","name":"exec","input":"return 1;"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"fixture-wrapper-renderer-call","command":"cargo test","exit_code":1,"status":"failed"}}"#,
            )),
        );
        let renderer_data = normalize_rollout(&parsed_renderer_command);
        let renderer_context = AnalysisContext::new(&renderer_data);
        let renderer_events = renderer_context.failure_events();
        assert_eq!(renderer_events[0].family, "no_canonical_command");
    }

    #[test]
    fn parsed_renderer_failures_are_structured_and_malformed_results_are_ignored() {
        let parsed = parse_rollout_reader(
            Path::new("fixture-renderer.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../../tests/fixtures/rollout/tool-result-envelopes.jsonl"
            ))),
        );
        let data = normalize_rollout(&parsed);
        let events = build_events(&AnalysisContext::new(&data));

        assert_eq!(
            events
                .iter()
                .map(|event| event.category.as_str())
                .collect::<Vec<_>>(),
            vec!["exit_code_23", "timeout", "renderer_failed"]
        );
        assert!(events.iter().all(|event| event.structured));
        assert!(
            !events
                .iter()
                .any(|event| event.session_id == "fixture-renderer-unknown")
        );
    }
}
