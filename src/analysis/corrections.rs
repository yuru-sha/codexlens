//! Correction lens.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::model::{CanonicalData, Message, MessageRole, SourceRef};

use super::{
    Activity, ActivityKind, AnalysisOptions, CORRECTION_MARKERS, DEFAULT_MIN_OCCURRENCES,
    DEFAULT_MIN_SESSIONS, EvidenceRole, Finding, FindingConfidence, FindingSeverity, FindingType,
    annotate_snapshot_limitations, bounded_excerpt, bounded_fingerprint, compare_positions,
    distinct_sessions, evidence_for, majority_scope, normalize_fact, position_for_message,
    position_for_source, push_evidence, redact_sensitive, sort_findings,
};

#[derive(Debug, Clone)]
struct CorrectionEvent {
    session_id: String,
    key: String,
    text: String,
    message: Message,
    preceding: Activity,
}

#[derive(Debug, Clone)]
pub(super) struct CorrectionFact {
    pub(super) session_id: String,
    pub(super) key: String,
    pub(super) text: String,
    pub(super) source: SourceRef,
    pub(super) excerpt: String,
}

pub(super) fn analyze(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut grouped: BTreeMap<String, Vec<CorrectionEvent>> = BTreeMap::new();
    for event in correction_events(data) {
        grouped.entry(event.key.clone()).or_default().push(event);
    }

    let mut findings = grouped
        .into_iter()
        .filter_map(|(key, mut events)| {
            events.sort_by(|left, right| {
                compare_positions(
                    &position_for_message(data, &left.message),
                    &position_for_message(data, &right.message),
                )
            });
            let sessions = distinct_sessions(events.iter().map(|event| event.session_id.as_str()));
            if events.len() < DEFAULT_MIN_OCCURRENCES || sessions.len() < DEFAULT_MIN_SESSIONS {
                return None;
            }
            let scope = majority_scope(data, events.iter().map(|event| event.session_id.as_str()));
            let severity = if events.len() >= 3 {
                FindingSeverity::High
            } else {
                FindingSeverity::Medium
            };
            let mut evidence = Vec::new();
            for event in &events {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(event.session_id.clone()),
                        event.message.provenance.clone(),
                        EvidenceRole::Observation,
                        event.message.content.as_deref(),
                        options,
                    ),
                );
                let action = &event.preceding;
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(event.session_id.clone()),
                        action.source.clone(),
                        EvidenceRole::PrecedingAction,
                        Some(&action.description),
                        options,
                    ),
                );
            }
            Some(Finding {
                kind: FindingType::Correction,
                severity,
                confidence: FindingConfidence::Medium,
                scope,
                key: key.clone(),
                summary: format!(
                    "Repeated correction marker for {:?} observed {} times across {} sessions",
                    key,
                    events.len(),
                    sessions.len()
                ),
                evidence,
                occurrences: events.len(),
                distinct_sessions: sessions.len(),
                affected_paths: Vec::new(),
                observed_commands: Vec::new(),
                sequence: events
                    .iter()
                    .map(|event| {
                        bounded_excerpt(&event.preceding.description, options.excerpt_max_bytes)
                    })
                    .collect(),
                suggested_action: format!(
                    "Record the bounded project fact {:?} in the narrowest applicable instructions",
                    key
                ),
                limitations: vec![
                    "Only role, ordering, and the documented marker set were used; sentiment and intent were not inferred".to_owned(),
                ],
                verification_status: None,
            })
        })
        .collect::<Vec<_>>();
    annotate_snapshot_limitations(data, &mut findings);
    sort_findings(&mut findings);
    findings
}

pub(super) fn facts(data: &CanonicalData, options: &AnalysisOptions) -> Vec<CorrectionFact> {
    correction_events(data)
        .into_iter()
        .map(|event| CorrectionFact {
            session_id: event.session_id,
            key: event.key,
            text: event.text,
            source: event.message.provenance.clone(),
            excerpt: bounded_excerpt(
                event.message.content.as_deref().unwrap_or_default(),
                options.excerpt_max_bytes,
            ),
        })
        .collect()
}

fn correction_events(data: &CanonicalData) -> Vec<CorrectionEvent> {
    let mut actions = Vec::new();
    for message in &data.messages {
        if message.role == Some(MessageRole::Assistant)
            && message
                .content
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        {
            actions.push(Activity {
                session_id: message.session_id.clone().unwrap_or_default(),
                turn_id: message.turn_id.clone(),
                position: position_for_message(data, message),
                description: message.content.clone().unwrap_or_default(),
                source: message.provenance.clone(),
                path: None,
                kind: ActivityKind::Action,
            });
        }
    }
    actions.extend(data.tool_calls.iter().filter_map(|call| {
        let session_id = call.session_id.clone()?;
        let command = call
            .command
            .as_deref()
            .or(call.input_summary.as_deref())
            .unwrap_or("tool action");
        Some(Activity {
            session_id,
            turn_id: call.turn_id.clone(),
            position: position_for_source(data, &call.provenance, None),
            description: format!(
                "{}: {}",
                call.tool_name.as_deref().unwrap_or("tool"),
                command
            ),
            source: call.provenance.clone(),
            path: None,
            kind: ActivityKind::Action,
        })
    }));
    actions.extend(data.tool_results.iter().filter_map(|result| {
        if result.is_duplicate || result.matched_call {
            return None;
        }
        let session_id = result.session_id.clone()?;
        Some(Activity {
            session_id,
            turn_id: result.turn_id.clone(),
            position: position_for_source(data, &result.provenance, None),
            description: format!(
                "tool result: {}",
                result.command.as_deref().unwrap_or("tool action")
            ),
            source: result.provenance.clone(),
            path: None,
            kind: ActivityKind::Action,
        })
    }));
    actions.sort_by(super::compare_activity_positions);

    let mut events = Vec::new();
    for message in data.messages.iter().filter(|message| {
        message.role == Some(MessageRole::User)
            && message
                .content
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
    }) {
        let Some(text) = message.content.as_deref() else {
            continue;
        };
        let Some(key) = correction_fact(text) else {
            continue;
        };
        let Some(session_id) = message.session_id.clone() else {
            continue;
        };
        let message_position = position_for_message(data, message);
        let preceding = actions
            .iter()
            .filter(|action| {
                action.session_id == session_id
                    && compare_positions(&action.position, &message_position) == Ordering::Less
            })
            .max_by(|left, right| compare_positions(&left.position, &right.position))
            .cloned();
        let Some(preceding) = preceding else {
            continue;
        };
        events.push(CorrectionEvent {
            session_id,
            key,
            text: normalize_fact(text),
            message: message.clone(),
            preceding,
        });
    }
    events
}

fn correction_fact(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.ends_with('?') || question_prefix(trimmed) {
        return None;
    }
    let normalized = normalize_fact(&redact_sensitive(trimmed));
    for marker in CORRECTION_MARKERS {
        if let Some(rest) = normalized.strip_prefix(marker) {
            let rest = rest.strip_suffix(" instead").unwrap_or(rest).trim();
            if !rest.is_empty() {
                return Some(bounded_fingerprint(rest));
            }
        }
    }
    None
}

fn question_prefix(text: &str) -> bool {
    let first = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|character: char| !character.is_ascii_alphabetic())
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "can" | "could" | "would" | "what" | "why" | "how" | "where" | "when" | "which" | "should"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    use crate::analysis::FindingScope;
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
    fn correction_questions_and_new_requirements_are_not_markers() {
        assert_eq!(correction_fact("How should this be tested?"), None);
        assert_eq!(correction_fact("Implement the new parser."), None);
        assert_eq!(
            correction_fact("Use cargo test instead."),
            Some("cargo test".to_owned())
        );
        assert_eq!(
            correction_fact("Use src/a.rs."),
            Some("src/a.rs".to_owned())
        );
        assert_ne!(
            correction_fact("Use src/a.rs."),
            correction_fact("Use src/b.rs.")
        );
        let data = fixture_data();
        let findings = analyze(&data, &AnalysisOptions::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingType::Correction);
        assert_eq!(
            findings[0].scope,
            FindingScope::Project(PathBuf::from("/fixture/project"))
        );
        assert_eq!(findings[0].confidence, FindingConfidence::Medium);
        assert_eq!(findings[0].occurrences, 2);
        assert_eq!(
            findings[0]
                .evidence
                .iter()
                .filter(|evidence| evidence.role == EvidenceRole::Observation)
                .map(|evidence| evidence.source.line)
                .collect::<Vec<_>>(),
            vec![Some(7), Some(15)]
        );
        assert!(findings[0].evidence.iter().any(|evidence| {
            evidence.role == EvidenceRole::PrecedingAction && evidence.source.line == Some(6)
        }));

        let mut one_session = data;
        one_session
            .messages
            .retain(|message| message.session_id.as_deref() == Some("fixture-analysis-session-a"));
        assert!(analyze(&one_session, &AnalysisOptions::default()).is_empty());
    }

    #[test]
    fn bounded_fingerprints_redact_all_values_and_keep_marker_contract() {
        use super::super::{MAX_FACT_KEY_BYTES, normalize_fact, normalize_guidance};

        let long = format!(
            "Use token=first-token token=second-token {}",
            "fact ".repeat(MAX_FACT_KEY_BYTES)
        );
        let fingerprint = correction_fact(&long).unwrap();
        assert!(fingerprint.len() <= MAX_FACT_KEY_BYTES);
        assert!(!fingerprint.contains("first-token"));
        assert!(!fingerprint.contains("second-token"));
        let common = "fact ".repeat(40);
        let first = correction_fact(&format!("Use {common}alpha")).unwrap();
        let second = correction_fact(&format!("Use {common}beta")).unwrap();
        assert_ne!(first, second);
        assert!(first.len() <= MAX_FACT_KEY_BYTES);
        assert!(second.len() <= MAX_FACT_KEY_BYTES);
        assert_eq!(
            normalize_guidance(
                "Use /fixture/project/src/lib.rs fixture-00000000-0000-0000-0000-000000000001"
            ),
            normalize_fact(
                "Use /fixture/project/src/lib.rs fixture-00000000-0000-0000-0000-000000000001"
            )
        );
        assert_eq!(correction_fact("The repo uses cargo test."), None);
    }
}
