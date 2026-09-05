//! Knowledge and rediscovery lens.

use std::collections::{BTreeMap, HashSet};

use crate::model::{CanonicalData, SourceRef};

use super::{
    AnalysisOptions, DISCOVERY_MARKERS, EvidenceRole, Finding, FindingConfidence, FindingSeverity,
    FindingType, MISSING_SNAPSHOT_LIMITATION, annotate_snapshot_limitations, bounded_excerpt,
    bounded_fingerprint, corrections, distinct_sessions, evidence_for, majority_path,
    majority_project, majority_scope, normalize_fact, push_evidence, snapshot_is_usable,
    sort_findings,
};

const LONG_FACT_BYTES: usize = 240;

#[derive(Debug, Clone)]
struct FactEvent {
    session_id: String,
    key: String,
    text: String,
    source: SourceRef,
    excerpt: String,
    role: EvidenceRole,
}

pub(super) fn analyze(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut facts = correction_facts(data, options);
    let correction_sources = facts
        .iter()
        .map(|fact| (fact.source.path.clone(), fact.source.line))
        .collect::<HashSet<_>>();
    facts.extend(discovery_facts(data, options).into_iter().filter(|fact| {
        !correction_sources.contains(&(fact.source.path.clone(), fact.source.line))
    }));
    let mut grouped: BTreeMap<String, Vec<FactEvent>> = BTreeMap::new();
    for fact in facts {
        grouped.entry(fact.key.clone()).or_default().push(fact);
    }

    let mut findings = grouped
        .into_iter()
        .filter_map(|(key, mut facts)| {
            facts.sort_by(|left, right| super::compare_sources(&left.source, &right.source));
            let sessions = distinct_sessions(facts.iter().map(|fact| fact.session_id.as_str()));
            if facts.len() < super::DEFAULT_MIN_OCCURRENCES
                || sessions.len() < super::DEFAULT_MIN_SESSIONS
            {
                return None;
            }
            let scope = majority_scope(data, facts.iter().map(|fact| fact.session_id.as_str()));
            let target = instruction_target(data, sessions.iter().map(String::as_str));
            let long = facts.iter().any(|fact| fact.text.len() > LONG_FACT_BYTES);
            let mut evidence = Vec::new();
            for fact in &facts {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(fact.session_id.clone()),
                        fact.source.clone(),
                        fact.role.clone(),
                        Some(&fact.excerpt),
                        options,
                    ),
                );
            }
            let missing_snapshot = sessions.iter().any(|session| {
                !data.instruction_snapshots.iter().any(|snapshot| {
                    snapshot.session_id.as_deref() == Some(session) && snapshot_is_usable(snapshot)
                })
            });
            let mut limitations = vec![
                "This candidate uses repeated bounded lexical facts; it does not perform semantic summarization".to_owned(),
            ];
            if missing_snapshot {
                limitations.push(MISSING_SNAPSHOT_LIMITATION.to_owned());
            }
            let suggested_action = if long {
                format!(
                    "Add a short index/link in {target} to a scoped page such as docs/knowledge.md; keep the detailed fact there"
                )
            } else {
                format!("Add the concise fact to the narrowest applicable instruction file {target}")
            };
            Some(Finding {
                kind: FindingType::Knowledge,
                severity: if facts.len() >= 3 {
                    FindingSeverity::High
                } else {
                    FindingSeverity::Medium
                },
                confidence: FindingConfidence::Medium,
                scope,
                key,
                summary: format!(
                    "A bounded project fact was rediscovered {} times across {} sessions",
                    facts.len(),
                    sessions.len()
                ),
                evidence,
                occurrences: facts.len(),
                distinct_sessions: sessions.len(),
                affected_paths: Vec::new(),
                observed_commands: Vec::new(),
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

fn correction_facts(data: &CanonicalData, options: &AnalysisOptions) -> Vec<FactEvent> {
    corrections::facts(data, options)
        .into_iter()
        .map(|event| FactEvent {
            session_id: event.session_id,
            key: event.key,
            text: event.text,
            source: event.source,
            excerpt: event.excerpt,
            role: EvidenceRole::Observation,
        })
        .collect()
}

fn discovery_facts(data: &CanonicalData, options: &AnalysisOptions) -> Vec<FactEvent> {
    let mut facts = Vec::new();
    for message in &data.messages {
        let Some(session_id) = message.session_id.clone() else {
            continue;
        };
        let Some(content) = message.content.as_deref() else {
            continue;
        };
        let normalized = normalize_fact(content);
        let Some(fact) = DISCOVERY_MARKERS
            .iter()
            .find_map(|marker| normalized.strip_prefix(marker))
            .map(str::to_owned)
        else {
            continue;
        };
        if fact.is_empty() || content.trim_end().ends_with('?') {
            continue;
        }
        facts.push(FactEvent {
            session_id,
            key: bounded_fingerprint(&fact),
            text: fact,
            source: message.provenance.clone(),
            excerpt: bounded_excerpt(content, options.excerpt_max_bytes),
            role: EvidenceRole::Observation,
        });
    }
    facts
}

fn instruction_target<'a, I>(data: &CanonicalData, sessions: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let sessions = sessions.into_iter().collect::<Vec<_>>();
    let nearest = sessions
        .iter()
        .filter_map(|session_id| {
            data.instruction_joins
                .iter()
                .find(|join| join.session_id == *session_id)
                .and_then(|join| join.nearest_path.as_ref())
                .cloned()
        })
        .collect::<Vec<_>>();
    if let Some(path) = majority_path(nearest) {
        return path.display().to_string();
    }
    if let Some(project) = majority_project(data, sessions.iter().copied()) {
        return project.join("AGENTS.md").display().to_string();
    }
    "AGENTS.md".to_owned()
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
    fn knowledge_requires_recurrence_across_sessions() {
        let data = fixture_data();
        let findings = analyze(&data, &AnalysisOptions::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingType::Knowledge);
        assert_eq!(findings[0].distinct_sessions, 2);

        let mut one_session = data;
        one_session
            .messages
            .retain(|message| message.session_id.as_deref() == Some("fixture-analysis-session-a"));
        assert!(analyze(&one_session, &AnalysisOptions::default()).is_empty());
    }
}
