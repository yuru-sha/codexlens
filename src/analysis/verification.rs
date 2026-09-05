//! Verification lens.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::model::{CanonicalData, SourceRef, ToolCall};

use super::{
    Activity, ActivityKind, AnalysisOptions, DEFAULT_EXCERPT_BYTES, EditEvent, EvidenceRole,
    Finding, FindingConfidence, FindingSeverity, FindingType, VerificationStatus,
    annotate_snapshot_limitations, bounded_excerpt, command_tokens, compare_activity_positions,
    compare_positions, context_matches, edit_events, evidence_for, majority_scope, matching_call,
    path_scope, position_for_source, push_evidence, sort_findings, strip_command_wrappers,
};

#[derive(Debug, Clone)]
struct VerificationEvent {
    session_id: String,
    turn_id: Option<String>,
    command: String,
    kind: String,
    position: super::Position,
    source: SourceRef,
}

pub(super) fn classify_command(command: &str) -> Option<String> {
    let mut tokens = command_tokens(command);
    strip_command_wrappers(&mut tokens);
    let lower = tokens
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if lower
        .iter()
        .any(|token| matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return None;
    }
    let first = lower.first()?.as_str();
    let kind = match first {
        "cargo" => lower.get(1).and_then(|value| match value.as_str() {
            "test" | "nextest" => Some("test"),
            "fmt" => Some("format"),
            "clippy" | "check" => Some("check"),
            "build" => Some("build"),
            _ => None,
        }),
        "go" => lower.get(1).and_then(|value| match value.as_str() {
            "test" => Some("test"),
            "fmt" => Some("format"),
            "vet" => Some("lint"),
            "build" => Some("build"),
            _ => None,
        }),
        "pytest" => Some("test"),
        "ruff" => lower.get(1).and_then(|value| match value.as_str() {
            "check" => Some("lint"),
            "format" => Some("format"),
            _ => None,
        }),
        "mypy" | "eslint" => Some("lint"),
        "prettier" => Some("format"),
        "python" | "python3" if lower.get(1).is_some_and(|value| value == "-m") => lower
            .get(2)
            .and_then(|value| (value == "pytest").then_some("test")),
        "npm" | "pnpm" | "yarn" | "bun" => command_manager_kind(&lower),
        "make" | "just" => lower.iter().skip(1).find_map(|value| target_kind(value)),
        "git" if lower.get(1).is_some_and(|value| value == "diff") => lower
            .iter()
            .any(|value| value == "--check")
            .then_some("check"),
        _ => None,
    }?;
    Some(kind.to_owned())
}

fn command_manager_kind(tokens: &[String]) -> Option<&'static str> {
    let mut values = tokens.iter().skip(1);
    let subcommand = values.next()?;
    let target = if subcommand == "run" {
        values.next().map(String::as_str).unwrap_or_default()
    } else {
        subcommand.as_str()
    };
    target_kind(target)
}

fn target_kind(target: &str) -> Option<&'static str> {
    match target {
        "test" => Some("test"),
        "lint" | "check" | "vet" => Some("lint"),
        "format" | "fmt" => Some("format"),
        "build" => Some("build"),
        _ => None,
    }
}

fn same_call(left: &ToolCall, right: &ToolCall) -> bool {
    left.call_id == right.call_id
        && left.session_id == right.session_id
        && left.turn_id == right.turn_id
        && left.provenance == right.provenance
}

fn call_has_observed_result(data: &CanonicalData, call: &ToolCall) -> bool {
    if let Some(call_id) = call.call_id.as_ref() {
        return data.tool_results.iter().any(|result| {
            !result.is_duplicate
                && result.call_id.as_ref() == Some(call_id)
                && matching_call(data, result).is_some_and(|candidate| same_call(candidate, call))
        });
    }

    let no_id_calls = data
        .tool_calls
        .iter()
        .filter(|candidate| {
            candidate.call_id.is_none()
                && context_matches(candidate.session_id.as_deref(), call.session_id.as_deref())
                && context_matches(candidate.turn_id.as_deref(), call.turn_id.as_deref())
        })
        .count();
    if no_id_calls != 1 {
        return false;
    }
    data.tool_results
        .iter()
        .filter(|result| {
            !result.is_duplicate
                && result.call_id.is_none()
                && context_matches(result.session_id.as_deref(), call.session_id.as_deref())
                && context_matches(result.turn_id.as_deref(), call.turn_id.as_deref())
        })
        .count()
        == 1
}

fn turn_is_complete(data: &CanonicalData, session_id: &str, turn_id: Option<&str>) -> bool {
    if let Some(turn_id) = turn_id {
        if data.turns.iter().any(|turn| {
            turn.session_id.as_deref() == Some(session_id)
                && turn.id == turn_id
                && turn.completed_at.is_some()
        }) {
            return true;
        }
    }
    data.records.iter().any(|record| {
        record.session_id.as_deref() == Some(session_id)
            && context_matches(record.turn_id.as_deref(), turn_id)
            && record.is_terminal
    })
}

pub(super) fn analyze(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let edits = edit_events(data, options);
    let verifications = verification_events(data);
    let unobserved_verifications = unobserved_verification_events(data);
    let mut groups: BTreeMap<(String, Option<String>), Vec<EditEvent>> = BTreeMap::new();
    for edit in &edits {
        groups
            .entry((edit.session_id.clone(), edit.turn_id.clone()))
            .or_default()
            .push(edit.clone());
    }

    let mut findings = Vec::new();
    for ((session_id, turn_id), mut changes) in groups {
        changes.sort_by(|left, right| compare_positions(&left.position, &right.position));
        let last_change_position = changes.last().map(|change| &change.position);
        let next_change_position = last_change_position.and_then(|last| {
            edits
                .iter()
                .filter(|edit| edit.session_id == session_id)
                .filter(|edit| compare_positions(&edit.position, last) == Ordering::Greater)
                .map(|edit| edit.position.clone())
                .min_by(compare_positions)
        });
        let mut checks = verifications
            .iter()
            .filter(|check| {
                if check.session_id != session_id {
                    return false;
                }
                if context_matches(check.turn_id.as_deref(), turn_id.as_deref()) {
                    return true;
                }
                let Some(last) = last_change_position else {
                    return false;
                };
                compare_positions(&check.position, last) == Ordering::Greater
                    && next_change_position.as_ref().is_none_or(|next| {
                        compare_positions(&check.position, next) == Ordering::Less
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        checks.sort_by(|left, right| compare_positions(&left.position, &right.position));
        let has_unobserved_attempt = unobserved_verifications.iter().any(|attempt| {
            attempt.session_id == session_id
                && last_change_position.is_some_and(|last| {
                    compare_positions(&attempt.position, last) == Ordering::Greater
                        && next_change_position.as_ref().is_none_or(|next| {
                            compare_positions(&attempt.position, next) == Ordering::Less
                        })
                })
        });

        let mut pending = Vec::new();
        let mut observed_before_pending = Vec::new();
        let mut change_and_check = changes
            .iter()
            .map(|change| Activity {
                session_id: change.session_id.clone(),
                turn_id: change.turn_id.clone(),
                position: change.position.clone(),
                description: format!("{} {}", change.operation, change.path),
                source: change.source.clone(),
                path: Some(change.path.clone()),
                kind: ActivityKind::Edit,
            })
            .collect::<Vec<_>>();
        change_and_check.extend(checks.iter().map(|check| Activity {
            session_id: check.session_id.clone(),
            turn_id: check.turn_id.clone(),
            position: check.position.clone(),
            description: format!("{} ({})", check.command, check.kind),
            source: check.source.clone(),
            path: None,
            kind: ActivityKind::Verification,
        }));
        change_and_check.sort_by(compare_activity_positions);

        for activity in change_and_check {
            match activity.kind {
                ActivityKind::Edit => pending.push(activity),
                ActivityKind::Verification => {
                    if pending.is_empty() {
                        observed_before_pending.push(activity);
                    } else {
                        pending.clear();
                    }
                }
                ActivityKind::Action => {}
                ActivityKind::Failure => {}
            }
        }
        if pending.is_empty() {
            continue;
        }

        let complete =
            !has_unobserved_attempt && turn_is_complete(data, &session_id, turn_id.as_deref());
        let status = if complete {
            VerificationStatus::Missing
        } else {
            VerificationStatus::NotObserved
        };
        let paths = pending
            .iter()
            .filter_map(|activity| activity.path.clone())
            .collect::<BTreeSet<_>>();
        let paths = paths.into_iter().collect::<Vec<_>>();
        let scope = if let Some(path) = paths.first() {
            path_scope(data, &[session_id.as_str()], path)
        } else {
            majority_scope(data, [session_id.as_str()])
        };
        let mut evidence = Vec::new();
        for activity in &pending {
            push_evidence(
                &mut evidence,
                evidence_for(
                    Some(session_id.clone()),
                    activity.source.clone(),
                    EvidenceRole::FileOperation,
                    Some(&activity.description),
                    options,
                ),
            );
        }
        for check in observed_before_pending.iter().rev().take(2).rev() {
            push_evidence(
                &mut evidence,
                evidence_for(
                    Some(session_id.clone()),
                    check.source.clone(),
                    EvidenceRole::VerificationCommand,
                    Some(&check.description),
                    options,
                ),
            );
        }
        let key = format!("{}|{}", paths.join(","), status.as_str());
        let summary = match status {
            VerificationStatus::Missing => format!(
                "No recognized verification command followed the latest changes to {} before the turn completed",
                paths.join(", ")
            ),
            VerificationStatus::NotObserved => format!(
                "Verification was not observed after changes to {} before the incomplete rollout ended",
                paths.join(", ")
            ),
        };
        findings.push(Finding {
            kind: FindingType::Verification,
            severity: if complete {
                FindingSeverity::Medium
            } else {
                FindingSeverity::Low
            },
            confidence: if complete {
                FindingConfidence::Medium
            } else {
                FindingConfidence::Low
            },
            scope,
            key,
            summary,
            evidence,
            occurrences: pending.len(),
            distinct_sessions: 1,
            affected_paths: paths,
            observed_commands: observed_before_pending
                .iter()
                .map(|activity| bounded_excerpt(&activity.description, options.excerpt_max_bytes))
                .collect(),
            sequence: pending
                .iter()
                .map(|activity| bounded_excerpt(&activity.description, options.excerpt_max_bytes))
                .collect(),
            suggested_action: "If this change batch is expected to be complete, run and record an observed project verification command".to_owned(),
            limitations: vec![match status {
                VerificationStatus::Missing => {
                    "Only the documented command allowlist was recognized; absence is not proof that an unrecognized command was not run".to_owned()
                }
                VerificationStatus::NotObserved => {
                    "The rollout ended before completion was observed, so this is not proof that verification was not run".to_owned()
                }
            }],
            verification_status: Some(status),
        });
    }
    annotate_snapshot_limitations(data, &mut findings);
    sort_findings(&mut findings);
    findings
}

fn verification_events(data: &CanonicalData) -> Vec<VerificationEvent> {
    let mut events = Vec::new();
    let mut call_ids = HashSet::new();
    for call in &data.tool_calls {
        if !call_has_observed_result(data, call) {
            continue;
        }
        let Some(event) = verification_event_for_call(data, call) else {
            continue;
        };
        if let Some(call_id) = call.call_id.as_ref() {
            call_ids.insert((
                event.session_id.clone(),
                event.turn_id.clone(),
                call_id.clone(),
            ));
        }
        events.push(event);
    }
    for result in &data.tool_results {
        let Some(session_id) = result.session_id.clone() else {
            continue;
        };
        if result.call_id.is_some() && {
            matching_call(data, result).is_some_and(|call| {
                call_ids.iter().any(|(call_session, call_turn, call_id)| {
                    call.session_id.as_ref() == Some(call_session)
                        && call.turn_id.as_ref() == call_turn.as_ref()
                        && call.call_id.as_ref() == Some(call_id)
                })
            })
        } {
            continue;
        }
        let command = result.command.as_deref().unwrap_or_default();
        let Some(kind) = classify_command(command) else {
            continue;
        };
        events.push(VerificationEvent {
            session_id,
            turn_id: result.turn_id.clone(),
            command: bounded_excerpt(command, DEFAULT_EXCERPT_BYTES),
            kind,
            position: position_for_source(data, &result.provenance, None),
            source: result.provenance.clone(),
        });
    }
    events
}

fn unobserved_verification_events(data: &CanonicalData) -> Vec<VerificationEvent> {
    data.tool_calls
        .iter()
        .filter(|call| !call_has_observed_result(data, call))
        .filter_map(|call| verification_event_for_call(data, call))
        .collect()
}

fn verification_event_for_call(data: &CanonicalData, call: &ToolCall) -> Option<VerificationEvent> {
    let session_id = call.session_id.clone()?;
    let command = call
        .command
        .as_deref()
        .or(call.input_summary.as_deref())
        .unwrap_or_default();
    let kind = classify_command(command)?;
    Some(VerificationEvent {
        session_id,
        turn_id: call.turn_id.clone(),
        command: bounded_excerpt(command, DEFAULT_EXCERPT_BYTES),
        kind,
        position: position_for_source(data, &call.provenance, None),
        source: call.provenance.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::model::{OutcomeSource, ToolOutcome, ToolResult};

    #[test]
    fn call_without_id_can_supply_observed_verification() {
        let data = CanonicalData {
            tool_calls: vec![ToolCall {
                id: None,
                call_id: None,
                session_id: Some("fixture-session".to_owned()),
                turn_id: Some("fixture-turn".to_owned()),
                tool_name: Some("exec_command".to_owned()),
                input_summary: None,
                command: Some("cargo test".to_owned()),
                cwd: None,
                status: None,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 1),
            }],
            tool_results: vec![ToolResult {
                id: None,
                call_id: None,
                session_id: Some("fixture-session".to_owned()),
                turn_id: Some("fixture-turn".to_owned()),
                command: None,
                cwd: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                exit_code: Some(0),
                status: Some("completed".to_owned()),
                outcome: ToolOutcome::Succeeded,
                outcome_source: OutcomeSource::ExitCode,
                matched_call: false,
                deduplication_key: None,
                equivalent_to: None,
                is_duplicate: false,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 2),
            }],
            ..CanonicalData::default()
        };
        assert_eq!(verification_events(&data).len(), 1);
    }
}
