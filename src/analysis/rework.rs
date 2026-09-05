//! Rework and stuck lenses.

use std::collections::{BTreeMap, HashSet};

use crate::model::CanonicalData;

use super::{
    Activity, ActivityKind, AnalysisOptions, EditEvent, EvidenceRole, Finding, FindingConfidence,
    FindingSeverity, FindingType, Position, annotate_snapshot_limitations, bounded_excerpt,
    compare_activity_positions, compare_positions, edit_events, evidence_for, failure,
    limit_evidence, path_scope, push_evidence, sort_findings,
};

pub(super) fn analyze(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let edits = edit_events(data, options);
    let failures = failure::activities(data);
    let mut grouped: BTreeMap<(String, String), Vec<EditEvent>> = BTreeMap::new();
    for edit in edits {
        grouped
            .entry((edit.session_id.clone(), edit.path.clone()))
            .or_default()
            .push(edit);
    }

    let mut findings = Vec::new();
    for ((session_id, path), mut edits) in grouped {
        edits.sort_by(|left, right| compare_positions(&left.position, &right.position));
        let Some(window) = qualifying_edit_window(&edits, options.rework_window_seconds) else {
            continue;
        };
        let session_ids = vec![session_id.as_str()];
        let scope = path_scope(data, &session_ids, &path);
        let evidence = edits
            .iter()
            .enumerate()
            .filter(|(index, _)| window.contains(index))
            .map(|(_, edit)| {
                evidence_for(
                    Some(edit.session_id.clone()),
                    edit.source.clone(),
                    EvidenceRole::FileOperation,
                    Some(&format!("{} {}", edit.operation, edit.path)),
                    options,
                )
            })
            .collect::<Vec<_>>();
        let evidence = limit_evidence(evidence);
        let count = window.len();
        findings.push(Finding {
            kind: FindingType::Rework,
            severity: FindingSeverity::Medium,
            confidence: FindingConfidence::High,
            scope,
            key: path.clone(),
            summary: format!(
                "{} edits to {} occurred within the {} rework window",
                count,
                path,
                rework_window_label(options.rework_window_seconds)
            ),
            evidence,
            occurrences: count,
            distinct_sessions: 1,
            affected_paths: vec![path.clone()],
            observed_commands: Vec::new(),
            sequence: edits
                .iter()
                .enumerate()
                .filter(|(index, _)| window.contains(index))
                .map(|(_, edit)| {
                    bounded_excerpt(
                        &format!("edit {}", edit.path),
                        options.excerpt_max_bytes,
                    )
                })
                .collect(),
            suggested_action: "Review whether the repeated edit reflects a missing scoped instruction; iteration may be intentional".to_owned(),
            limitations: vec![
                "The heuristic uses only timestamped file operations and does not treat iteration as inherently wasteful".to_owned(),
            ],
            verification_status: None,
        });

        let loop_activities = stuck_window(&edits, &failures, options.rework_window_seconds);
        if let Some(activities) = loop_activities {
            let mut loop_evidence = Vec::new();
            let mut sequence = Vec::new();
            for activity in &activities {
                sequence.push(bounded_excerpt(
                    &activity.description,
                    options.excerpt_max_bytes,
                ));
                push_evidence(
                    &mut loop_evidence,
                    evidence_for(
                        Some(activity.session_id.clone()),
                        activity.source.clone(),
                        EvidenceRole::Observation,
                        Some(&activity.description),
                        options,
                    ),
                );
            }
            let occurrences = activities
                .iter()
                .filter(|activity| activity.kind == ActivityKind::Edit)
                .count();
            let observed_commands = activities
                .iter()
                .filter(|activity| activity.kind == ActivityKind::Failure)
                .map(|activity| bounded_excerpt(&activity.description, options.excerpt_max_bytes))
                .collect();
            findings.push(Finding {
                kind: FindingType::Stuck,
                severity: FindingSeverity::High,
                confidence: FindingConfidence::Medium,
                scope: path_scope(data, &session_ids, &path),
                key: format!("{path}|loop"),
                summary: format!("A failure/edit loop or edit burst was observed for {path}"),
                evidence: loop_evidence,
                occurrences,
                distinct_sessions: 1,
                affected_paths: vec![path.clone()],
                observed_commands,
                sequence,
                suggested_action: "Check the failure/edit sequence and add only the narrowly supported prerequisite or verification guidance".to_owned(),
                limitations: vec![
                    "Repeated edits can be intentional; this candidate is only a short-window sequence heuristic".to_owned(),
                ],
                verification_status: None,
            });
        }
    }
    annotate_snapshot_limitations(data, &mut findings);
    sort_findings(&mut findings);
    findings
}

fn stuck_window(
    edits: &[EditEvent],
    failures: &[Activity],
    window_seconds: i64,
) -> Option<Vec<Activity>> {
    if let Some(window) = qualifying_edit_window(edits, window_seconds) {
        if window.len() >= 3 {
            return Some(
                window
                    .into_iter()
                    .filter_map(|index| edits.get(index))
                    .map(|edit| Activity {
                        session_id: edit.session_id.clone(),
                        turn_id: edit.turn_id.clone(),
                        position: edit.position.clone(),
                        description: format!("edit {}", edit.path),
                        source: edit.source.clone(),
                        path: Some(edit.path.clone()),
                        kind: ActivityKind::Edit,
                    })
                    .collect(),
            );
        }
    }

    let edit_turn_ids = edits
        .iter()
        .filter_map(|edit| edit.turn_id.as_deref())
        .collect::<HashSet<_>>();
    let relevant_failures = failures.iter().filter(|failure| {
        failure.session_id
            == edits
                .first()
                .map(|edit| edit.session_id.as_str())
                .unwrap_or_default()
            && failure
                .turn_id
                .as_deref()
                .is_none_or(|turn_id| edit_turn_ids.contains(turn_id))
    });
    let mut activities = edits
        .iter()
        .map(|edit| Activity {
            session_id: edit.session_id.clone(),
            turn_id: edit.turn_id.clone(),
            position: edit.position.clone(),
            description: format!("edit {}", edit.path),
            source: edit.source.clone(),
            path: Some(edit.path.clone()),
            kind: ActivityKind::Edit,
        })
        .collect::<Vec<_>>();
    activities.extend(relevant_failures.cloned());
    activities.sort_by(compare_activity_positions);
    for start in 0..activities.len() {
        let mut selected = Vec::new();
        for activity in activities.iter().skip(start) {
            let Some(first) = activities.get(start) else {
                break;
            };
            if !within_window(&first.position, &activity.position, window_seconds) {
                break;
            }
            selected.push(activity.clone());
            let edit_count = selected
                .iter()
                .filter(|item| item.kind == ActivityKind::Edit)
                .count();
            let failure_count = selected
                .iter()
                .filter(|item| item.kind == ActivityKind::Failure)
                .count();
            if edit_count >= 2 && failure_count >= 1 && has_kind_transition(&selected) {
                return Some(selected);
            }
        }
    }
    None
}

fn qualifying_edit_window(edits: &[EditEvent], window_seconds: i64) -> Option<Vec<usize>> {
    let mut best = Vec::new();
    for start in 0..edits.len() {
        let mut selected = Vec::new();
        for (index, edit) in edits.iter().enumerate().skip(start) {
            let Some(first) = edits.get(start) else {
                break;
            };
            if !within_window(&first.position, &edit.position, window_seconds) {
                break;
            }
            selected.push(index);
        }
        if selected.len() > best.len() {
            best = selected;
        }
    }
    (best.len() >= 2).then_some(best)
}

fn within_window(left: &Position, right: &Position, window_seconds: i64) -> bool {
    left.timestamp
        .zip(right.timestamp)
        .is_some_and(|(left, right)| {
            right.saturating_sub(left) >= 0 && right - left <= window_seconds.max(0)
        })
}

fn rework_window_label(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds % 60 == 0 {
        format!("{}-minute", seconds / 60)
    } else {
        format!("{seconds}-second")
    }
}

fn has_kind_transition(activities: &[Activity]) -> bool {
    activities
        .windows(2)
        .any(|window| window[0].kind != window[1].kind)
}

#[cfg(test)]
mod tests {
    use super::super::{analyze_rework, stuck};
    use super::*;
    use crate::normalize::normalize_rollout;
    use crate::rollout::{PlainJsonlReader, parse_rollout_reader};
    use std::{io::Cursor, path::Path};

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
    fn stuck_wrapper_filters_rework_and_window_summary_uses_options() {
        let data = fixture_data();
        assert!(
            !stuck(&data).is_empty()
                && stuck(&data)
                    .iter()
                    .all(|finding| finding.kind == FindingType::Stuck)
        );
        let options = AnalysisOptions {
            rework_window_seconds: 180,
            ..AnalysisOptions::default()
        };
        let findings = analyze_rework(&data, &options);
        assert!(
            findings
                .iter()
                .filter(|finding| finding.kind == FindingType::Rework)
                .all(|finding| finding.summary.contains("3-minute rework window"))
        );
    }

    #[test]
    fn rework_requires_edits_inside_the_configured_window() {
        let data = fixture_data();
        let options = AnalysisOptions {
            rework_window_seconds: 30,
            ..AnalysisOptions::default()
        };

        assert!(analyze_rework(&data, &options).is_empty());
    }

    #[test]
    fn unrelated_failures_do_not_create_a_file_loop() {
        let mut data = fixture_data();
        for result in &mut data.tool_results {
            if failure::is_failed(result) {
                result.turn_id = Some("unrelated-turn".to_owned());
            }
        }

        assert!(
            analyze_rework(&data, &AnalysisOptions::default())
                .iter()
                .all(|finding| finding.kind == FindingType::Rework)
        );
    }

    #[test]
    fn missing_turn_ids_still_allow_same_window_stuck_detection() {
        let mut data = fixture_data();
        for operation in &mut data.file_operations {
            operation.turn_id = None;
        }
        for result in &mut data.tool_results {
            if failure::is_failed(result) {
                result.turn_id = None;
            }
        }

        assert!(
            analyze_rework(&data, &AnalysisOptions::default())
                .iter()
                .any(|finding| finding.kind == FindingType::Stuck)
        );
    }
}
