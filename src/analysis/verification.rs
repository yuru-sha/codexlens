//! Verification lens.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::model::{CanonicalData, SourceRef, ToolCall};

use super::{
    Activity, ActivityKind, AnalysisOptions, DEFAULT_EXCERPT_BYTES, EditEvent, EvidenceRole,
    Finding, FindingConfidence, FindingSeverity, FindingType, VerificationStatus,
    annotate_snapshot_limitations, bounded_excerpt, call_has_observed_result,
    compare_activity_positions, compare_positions, context_matches, edit_events, evidence_for,
    majority_scope, matching_call, path_scope, position_for_source, push_evidence, sort_findings,
    turn_is_complete, verification_kind,
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
        let Some(kind) = verification_kind(command) else {
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
    let kind = verification_kind(command)?;
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
