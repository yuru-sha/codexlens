//! Failure lens.

use std::collections::BTreeMap;
use std::path::Path;

use crate::model::{CanonicalData, OutcomeSource, SourceRef, ToolOutcome, ToolResult};

use super::{
    Activity, ActivityKind, AnalysisOptions, DEFAULT_EXCERPT_BYTES, DEFAULT_MIN_OCCURRENCES,
    DEFAULT_MIN_SESSIONS, EvidenceRole, Finding, FindingConfidence, FindingSeverity, FindingType,
    annotate_snapshot_limitations, bounded_excerpt, command_tokens, distinct_sessions,
    evidence_for, majority_scope, matching_call, normalize_fragment, position_for_source,
    push_evidence, redact_sensitive, sort_findings, strip_command_wrappers,
};

#[derive(Debug, Clone)]
struct FailureEvent {
    session_id: String,
    turn_id: Option<String>,
    key: String,
    tool: String,
    family: String,
    category: String,
    structured: bool,
    description: String,
    position: super::Position,
    source: SourceRef,
}

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
        "shell" | "exec" | "exec_command" => "exec_command".to_owned(),
        "" => "unknown_tool".to_owned(),
        _ => normalized,
    }
}

pub(super) fn analyze(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut grouped: BTreeMap<String, Vec<FailureEvent>> = BTreeMap::new();
    for event in failure_events(data) {
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
            Some(Finding {
                kind: FindingType::Failure,
                severity,
                confidence,
                scope,
                key,
                summary: format!(
                    "Repeated failure for {} {} ({}) observed {} times across {} sessions",
                    first.tool,
                    first.family,
                    first.category,
                    events.len(),
                    sessions.len()
                ),
                evidence,
                occurrences: events.len(),
                distinct_sessions: sessions.len(),
                affected_paths: Vec::new(),
                observed_commands: vec![bounded_excerpt(
                    &first.family,
                    options.excerpt_max_bytes,
                )],
                sequence: Vec::new(),
                suggested_action: format!(
                    "Document the prerequisite or preferred command for {} in the applicable instructions",
                    first.family
                ),
                limitations: vec![
                    "This is recurring observational evidence, not proof that the command is always incorrect".to_owned(),
                ],
                verification_status: None,
            })
        })
        .collect::<Vec<_>>();
    annotate_snapshot_limitations(data, &mut findings);
    sort_findings(&mut findings);
    findings
}

pub(super) fn activities(data: &CanonicalData) -> Vec<Activity> {
    failure_events(data)
        .into_iter()
        .map(|event| Activity {
            session_id: event.session_id,
            turn_id: event.turn_id,
            position: event.position,
            description: event.description,
            source: event.source,
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

fn failure_events(data: &CanonicalData) -> Vec<FailureEvent> {
    let mut events = Vec::new();
    for result in &data.tool_results {
        if result.is_duplicate || !is_failed(result) {
            continue;
        }
        let Some(session_id) = result.session_id.clone() else {
            continue;
        };
        let call = matching_call(data, result);
        let tool = call
            .and_then(|call| call.tool_name.as_deref())
            .map(normalize_tool)
            .unwrap_or_else(|| "unknown_tool".to_owned());
        let command = result
            .command
            .as_deref()
            .or_else(|| call.and_then(|call| call.command.as_deref()))
            .or_else(|| call.and_then(|call| call.input_summary.as_deref()))
            .unwrap_or_default();
        let family = command_family(command);
        let category = failure_category(result);
        let key = format!("{tool}|{family}|{category}");
        let description = failure_description(&tool, &family, &category, result);
        events.push(FailureEvent {
            session_id,
            turn_id: result.turn_id.clone(),
            key,
            tool,
            family,
            category,
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
            structured: true,
            description: "explicit error event".to_owned(),
            position: position_for_source(data, &record.provenance, record.timestamp.as_deref()),
            source: record.provenance.clone(),
        });
    }
    events
}

fn result_is_structured_failure(result: &ToolResult) -> bool {
    matches!(
        result.outcome_source,
        OutcomeSource::ExitCode | OutcomeSource::Status
    ) || result.exit_code.is_some()
        || result.status.as_deref().is_some_and(status_is_failed)
}

fn status_is_failed(status: &str) -> bool {
    ToolOutcome::from_status(status) == Some(ToolOutcome::Failed)
}

fn failure_category(result: &ToolResult) -> String {
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
            _ => "failed_status".to_owned(),
        };
    }
    let output = combined_result_output(result);
    let normalized = normalize_fragment(&output);
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

fn failure_description(tool: &str, family: &str, category: &str, result: &ToolResult) -> String {
    let output = combined_result_output(result);
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
        let events = failure_events(&output_only)
            .into_iter()
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
            !failure_events(&status_only)
                .into_iter()
                .filter(|event| event.tool != "event")
                .collect::<Vec<_>>()
                .is_empty()
        );
    }
}
