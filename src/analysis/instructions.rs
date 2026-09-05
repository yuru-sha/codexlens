//! Instruction-specific analysis findings.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::model::{
    InstructionFile, InstructionFileState, InstructionJoin, InstructionScope, InstructionSnapshot,
    ProjectRootStatus, SourceRef,
};

use super::{
    AnalysisOptions, CORRECTION_MARKERS, DEFAULT_MAX_EVIDENCE, DISCOVERY_MARKERS, EvidenceRole,
    Finding, FindingConfidence, FindingScope, FindingSeverity, FindingType, VerificationStatus,
    bounded_excerpt, bounded_fingerprint, evidence_for, evidence_sessions, instruction_scope,
    majority_scope, normalize_fact, normalize_guidance, push_evidence, reserve_evidence_slots,
    resolve_instruction_path, snapshot_for_evidence, sort_findings,
};

pub(super) fn analyze(
    data: &crate::model::CanonicalData,
    recurring_findings: &[Finding],
    options: &AnalysisOptions,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(instruction_truncation_findings(data, options));
    findings.extend(instruction_duplicate_findings(data, options));
    findings.extend(instruction_join_findings(data, recurring_findings, options));
    sort_findings(&mut findings);
    findings
}

type InstructionSnapshotEvidence<'a> = (SourceRef, &'a InstructionSnapshot);

fn snapshots_for_finding<'a>(
    data: &'a crate::model::CanonicalData,
    finding: &Finding,
) -> (
    BTreeMap<String, Vec<InstructionSnapshotEvidence<'a>>>,
    BTreeSet<String>,
) {
    let mut selected = BTreeMap::new();
    let mut missing_sessions = BTreeSet::new();
    for session_id in evidence_sessions(finding) {
        let mut snapshots: Vec<InstructionSnapshotEvidence<'a>> = Vec::new();
        for evidence in finding
            .evidence
            .iter()
            .filter(|evidence| evidence.session_id.as_deref() == Some(session_id.as_str()))
        {
            let Some(snapshot) = snapshot_for_evidence(data, evidence) else {
                missing_sessions.insert(session_id.clone());
                continue;
            };
            if !snapshots
                .iter()
                .any(|(_, existing)| existing.provenance == snapshot.provenance)
            {
                snapshots.push((evidence.source.clone(), snapshot));
            }
        }
        if snapshots.is_empty() {
            missing_sessions.insert(session_id.clone());
        }
        selected.insert(session_id, snapshots);
    }
    (selected, missing_sessions)
}

fn instruction_join_findings(
    data: &crate::model::CanonicalData,
    recurring: &[Finding],
    options: &AnalysisOptions,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for finding in recurring {
        if finding.kind == FindingType::Verification
            && finding.verification_status == Some(VerificationStatus::NotObserved)
        {
            continue;
        }
        let session_ids = evidence_sessions(finding);
        if session_ids.is_empty() {
            continue;
        }
        if session_ids.len() < finding.distinct_sessions {
            continue;
        }
        let (snapshots_by_session, missing_sessions) = snapshots_for_finding(data, finding);
        let available_snapshots = snapshots_by_session
            .values()
            .flatten()
            .map(|(source, snapshot)| (source.clone(), *snapshot))
            .collect::<Vec<_>>();
        let historical_mismatch = available_snapshots.iter().any(|(_, snapshot)| {
            !guidance_matches(finding, snapshot.content.as_deref().unwrap_or_default())
        });
        if !available_snapshots.is_empty() && missing_sessions.is_empty() && historical_mismatch {
            let mut evidence = reserve_evidence_slots(
                &finding.evidence,
                available_snapshots.len().min(DEFAULT_MAX_EVIDENCE),
            );
            for (_, snapshot) in &available_snapshots {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        snapshot.session_id.clone(),
                        snapshot.provenance.clone(),
                        EvidenceRole::InstructionSnapshot,
                        snapshot.content.as_deref(),
                        options,
                    ),
                );
            }
            let current_comparison = session_ids.iter().any(|session_id| {
                data.instruction_joins.iter().any(|join| {
                    &join.session_id == session_id
                        && current_instruction_is_usable(join)
                        && join.resolution.effective_content.is_some()
                })
            });
            let mut limitations = vec![
                "Historical instruction content was available for comparison; this is a candidate gap, not proof that guidance is absent today".to_owned(),
            ];
            limitations.push(if current_comparison {
                "A current instruction chain was also available for a separate comparison".to_owned()
            } else {
                "A current instruction chain was unavailable, so the present-file comparison is inconclusive".to_owned()
            });
            findings.push(Finding {
                kind: FindingType::Gap,
                severity: finding.severity,
                confidence: finding.confidence,
                scope: instruction_scope(data, finding, &session_ids),
                key: format!("{}|{}", finding.kind.as_str(), finding.key),
                summary: format!(
                    "Recurring {} evidence has no matching guidance in the historical instruction snapshot",
                    finding.kind.as_str()
                ),
                evidence,
                occurrences: finding.occurrences,
                distinct_sessions: finding.distinct_sessions,
                affected_paths: finding.affected_paths.clone(),
                observed_commands: finding.observed_commands.clone(),
                sequence: finding.sequence.clone(),
                suggested_action: finding.suggested_action.clone(),
                limitations,
                verification_status: None,
            });
        }

        for session_id in &session_ids {
            let Some(join) = data
                .instruction_joins
                .iter()
                .find(|join| &join.session_id == session_id)
            else {
                continue;
            };
            if current_instruction_is_usable(join) && !missing_sessions.contains(session_id) {
                if let Some(snapshots) = snapshots_by_session.get(session_id) {
                    for (_, snapshot) in snapshots {
                        if let Some((path, old_hash, new_hash)) = stale_file(join, snapshot) {
                            let mut evidence = reserve_evidence_slots(&finding.evidence, 2);
                            push_evidence(
                                &mut evidence,
                                evidence_for(
                                    Some(session_id.clone()),
                                    snapshot.provenance.clone(),
                                    EvidenceRole::InstructionSnapshot,
                                    snapshot.content.as_deref(),
                                    options,
                                ),
                            );
                            push_evidence(
                                &mut evidence,
                                evidence_for(
                                    Some(session_id.clone()),
                                    SourceRef::state(path.clone()),
                                    EvidenceRole::InstructionFile,
                                    Some(&format!("{} -> {}", path.display(), new_hash)),
                                    options,
                                ),
                            );
                            findings.push(Finding {
                                kind: FindingType::Stale,
                                severity: FindingSeverity::Medium,
                                confidence: FindingConfidence::High,
                                scope: FindingScope::Instruction(path.clone()),
                                key: format!("{}|{}|{}", finding.key, path.display(), old_hash),
                                summary: format!(
                                    "Current instruction file {} differs from the snapshot associated with the evidence",
                                    path.display()
                                ),
                                evidence,
                                occurrences: 1,
                                distinct_sessions: 1,
                                affected_paths: finding.affected_paths.clone(),
                                observed_commands: Vec::new(),
                                sequence: Vec::new(),
                                suggested_action: "Review the historical/current instruction difference before changing guidance".to_owned(),
                                limitations: vec![
                                    "The comparison used stored snapshot/file hashes; current source files were not reopened by the lens".to_owned(),
                                ],
                                verification_status: None,
                            });
                        }
                    }
                }
            }

            if current_instruction_is_usable(join) {
                if let Some(path) = overscoped_path(join, finding) {
                    let mut evidence = reserve_evidence_slots(&finding.evidence, 1);
                    push_evidence(
                        &mut evidence,
                        evidence_for(
                            Some(session_id.clone()),
                            SourceRef::state(path.clone()),
                            EvidenceRole::InstructionFile,
                            Some(&format!("broader scope: {}", path.display())),
                            options,
                        ),
                    );
                    findings.push(Finding {
                        kind: FindingType::Overscoped,
                        severity: FindingSeverity::Low,
                        confidence: FindingConfidence::Medium,
                        scope: FindingScope::Instruction(path.clone()),
                        key: format!("{}|{}", finding.key, path.display()),
                        summary: format!(
                            "Path-specific evidence is covered only by broader instruction scope {}",
                            path.display()
                        ),
                        evidence,
                        occurrences: finding.occurrences,
                        distinct_sessions: finding.distinct_sessions,
                        affected_paths: finding.affected_paths.clone(),
                        observed_commands: finding.observed_commands.clone(),
                        sequence: Vec::new(),
                        suggested_action: "Review whether this guidance belongs in the nearest nested instruction scope".to_owned(),
                        limitations: vec![
                            "The scope recommendation is based on observed paths and the stored effective chain".to_owned(),
                        ],
                        verification_status: None,
                    });
                }
            }
        }
    }
    findings
}

fn instruction_truncation_findings(
    data: &crate::model::CanonicalData,
    options: &AnalysisOptions,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for snapshot in &data.instruction_snapshots {
        if !snapshot.truncated {
            continue;
        }
        let Some(session_id) = snapshot.session_id.clone() else {
            continue;
        };
        findings.push(Finding {
            kind: FindingType::Truncated,
            severity: FindingSeverity::Low,
            confidence: FindingConfidence::High,
            scope: majority_scope(data, [session_id.as_str()]),
            key: format!("snapshot|{}|{}", session_id, snapshot.turn_id.as_deref().unwrap_or("")),
            summary: "The effective instruction snapshot reached its configured byte limit".to_owned(),
            evidence: vec![evidence_for(
                Some(session_id),
                snapshot.provenance.clone(),
                EvidenceRole::InstructionSnapshot,
                snapshot.content.as_deref(),
                options,
            )],
            occurrences: 1,
            distinct_sessions: 1,
            affected_paths: Vec::new(),
            observed_commands: Vec::new(),
            sequence: Vec::new(),
            suggested_action: "Review the configured instruction byte limit before relying on a complete comparison".to_owned(),
            limitations: vec![
                "The truncated chain may omit relevant instruction content".to_owned(),
            ],
            verification_status: None,
        });
    }
    for join in &data.instruction_joins {
        if !join.resolution.truncated {
            continue;
        }
        let nearest_excerpt = join
            .nearest_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let source = join
            .nearest_path
            .as_ref()
            .map(|path| SourceRef::state(path.clone()))
            .unwrap_or_else(|| join.provenance.clone());
        findings.push(Finding {
            kind: FindingType::Truncated,
            severity: FindingSeverity::Low,
            confidence: FindingConfidence::High,
            scope: join
                .nearest_path
                .clone()
                .map(FindingScope::Instruction)
                .unwrap_or_else(|| majority_scope(data, [join.session_id.as_str()])),
            key: format!("join|{}", join.session_id),
            summary: "The current instruction resolution reached its configured byte limit".to_owned(),
            evidence: vec![evidence_for(
                Some(join.session_id.clone()),
                source,
                EvidenceRole::InstructionFile,
                nearest_excerpt.as_deref(),
                options,
            )],
            occurrences: 1,
            distinct_sessions: 1,
            affected_paths: Vec::new(),
            observed_commands: Vec::new(),
            sequence: Vec::new(),
            suggested_action: "Review the configured instruction byte limit before relying on a complete comparison".to_owned(),
            limitations: vec![
                "The truncated chain may omit relevant instruction content".to_owned(),
            ],
            verification_status: None,
        });
    }
    findings
}

fn instruction_duplicate_findings(
    data: &crate::model::CanonicalData,
    options: &AnalysisOptions,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for join in &data.instruction_joins {
        if !current_instruction_is_usable(join) {
            continue;
        }
        let mut contents: BTreeMap<String, Vec<&InstructionFile>> = BTreeMap::new();
        for file in &join.resolution.chain {
            let Some(content) = file.content.as_deref() else {
                continue;
            };
            let normalized = normalize_guidance(content);
            if !normalized.is_empty() {
                let fingerprint = crate::instructions::content_hash(normalized.as_bytes());
                contents.entry(fingerprint).or_default().push(file);
            }
        }
        for (fingerprint, files) in contents {
            if files.len() < 2 {
                continue;
            }
            let mut evidence = Vec::new();
            for file in &files {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(join.session_id.clone()),
                        SourceRef::state(file.path.clone()),
                        EvidenceRole::InstructionFile,
                        Some(&file.path.to_string_lossy()),
                        options,
                    ),
                );
            }
            findings.push(Finding {
                kind: FindingType::Duplicate,
                severity: FindingSeverity::Low,
                confidence: FindingConfidence::High,
                scope: join
                    .nearest_path
                    .clone()
                    .map(FindingScope::Instruction)
                    .unwrap_or_else(|| majority_scope(data, [join.session_id.as_str()])),
                key: format!("{}|duplicate|{fingerprint}", join.session_id),
                summary: format!(
                    "Equivalent normalized guidance is loaded from {} instruction files",
                    files.len()
                ),
                evidence,
                occurrences: files.len(),
                distinct_sessions: 1,
                affected_paths: Vec::new(),
                observed_commands: Vec::new(),
                sequence: files
                    .iter()
                    .map(|file| {
                        bounded_excerpt(
                            &file.path.display().to_string(),
                            options.excerpt_max_bytes,
                        )
                    })
                    .collect(),
                suggested_action: "Keep equivalent guidance in the narrowest appropriate scope and remove duplication during review".to_owned(),
                limitations: vec![
                    "Equivalence is whitespace/case/punctuation normalization, not semantic equivalence".to_owned(),
                ],
                verification_status: None,
            });
        }
    }
    findings
}

fn stale_file(
    join: &InstructionJoin,
    snapshot: &InstructionSnapshot,
) -> Option<(PathBuf, String, String)> {
    if !current_instruction_is_usable(join) {
        return None;
    }
    for old in &snapshot.chain {
        let Some(old_hash) = old.content_hash.as_deref() else {
            continue;
        };
        let current = join
            .resolution
            .chain
            .iter()
            .find(|file| file.path == old.path);
        let Some(new_hash) = current.and_then(|file| file.content_hash.as_deref()) else {
            if matches!(old.kind, crate::model::InstructionFileKind::Observed)
                || join.resolution.project_root_status != ProjectRootStatus::Known
            {
                continue;
            }
            return Some((
                old.path.clone(),
                old_hash.to_owned(),
                "<missing>".to_owned(),
            ));
        };
        if old_hash != new_hash {
            return Some((old.path.clone(), old_hash.to_owned(), new_hash.to_owned()));
        }
    }
    if snapshot
        .effective_chain_hash
        .as_deref()
        .zip(join.resolution.effective_chain_hash.as_deref())
        .is_some_and(|(old_hash, new_hash)| old_hash != new_hash)
    {
        let path = join
            .nearest_path
            .clone()
            .or_else(|| join.resolution.chain.last().map(|file| file.path.clone()))?;
        return Some((
            path,
            snapshot.effective_chain_hash.clone()?,
            join.resolution.effective_chain_hash.clone()?,
        ));
    }
    None
}

fn overscoped_path(join: &InstructionJoin, finding: &Finding) -> Option<PathBuf> {
    let target = finding.affected_paths.first()?;
    let target = resolve_instruction_path(join, target)?;
    let nested = join
        .resolution
        .chain
        .iter()
        .filter(|file| file.scope == InstructionScope::ProjectNested)
        .filter(|file| target.starts_with(file.path.parent().unwrap_or(file.path.as_path())))
        .collect::<Vec<_>>();
    if nested.is_empty()
        || nested.iter().any(|file| {
            file.content
                .as_deref()
                .is_some_and(|content| guidance_matches(finding, content))
        })
    {
        return None;
    }
    join.resolution
        .chain
        .iter()
        .rev()
        .filter(|file| file.scope != InstructionScope::ProjectNested)
        .find(|file| {
            file.content
                .as_deref()
                .is_some_and(|content| guidance_matches(finding, content))
        })
        .map(|file| file.path.clone())
}

fn guidance_matches(finding: &Finding, content: &str) -> bool {
    let raw_content = content;
    let content = normalize_guidance(content);
    if content.is_empty() {
        return false;
    }
    let key = normalize_guidance(&finding.key);
    if key.is_empty() {
        return false;
    }
    if guidance_fact_matches(&key, raw_content) {
        return true;
    }
    let parts = key.split('|').collect::<Vec<_>>();
    if let Some(phrase) = parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| {
            part.len() >= 4
                && !matches!(
                    *part,
                    "failure"
                        | "correction"
                        | "missing"
                        | "not_observed"
                        | "loop"
                        | "exec_command"
                        | "unknown_tool"
                        | "event"
                )
                && !part.starts_with("exit_code_")
        })
        .max_by_key(|part| part.len())
    {
        return content.contains(phrase);
    }
    if content.contains(&key) {
        return true;
    }
    let mut terms = key
        .split('|')
        .flat_map(|part| part.split_whitespace())
        .filter(|term| term.len() >= 4)
        .filter(|term| !matches!(*term, "failure" | "correction" | "missing" | "not_observed"))
        .collect::<Vec<_>>();
    terms.sort_by_key(|term| std::cmp::Reverse(term.len()));
    terms.iter().any(|term| content.contains(term))
}

fn guidance_fact_matches(key: &str, content: &str) -> bool {
    content.lines().map(normalize_fact).any(|line| {
        CORRECTION_MARKERS
            .iter()
            .chain(DISCOVERY_MARKERS)
            .any(|marker| {
                let Some(rest) = line.strip_prefix(marker) else {
                    return false;
                };
                let rest = rest.strip_suffix(" instead").unwrap_or(rest).trim();
                !rest.is_empty() && bounded_fingerprint(rest) == key
            })
    })
}

fn current_instruction_is_usable(join: &InstructionJoin) -> bool {
    !join.resolution.truncated
        && !join
            .resolution
            .files
            .iter()
            .any(|file| file.state == InstructionFileState::Unreadable)
        && !join.resolution.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == crate::model::InstructionDiagnosticKind::Unreadable
        })
        && !join.resolution.files.iter().any(|file| {
            matches!(
                file.state,
                InstructionFileState::Unreadable | InstructionFileState::Truncated
            )
        })
        && join
            .resolution
            .chain
            .iter()
            .all(|file| file.state == InstructionFileState::Selected && file.content.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_matches_bounded_facts_and_rejects_unrelated_text() {
        let finding = Finding {
            kind: FindingType::Knowledge,
            severity: FindingSeverity::Medium,
            confidence: FindingConfidence::Medium,
            scope: FindingScope::Global,
            key: normalize_fact("/fixture/project/src/lib.rs"),
            summary: String::new(),
            evidence: Vec::new(),
            occurrences: 2,
            distinct_sessions: 2,
            affected_paths: Vec::new(),
            observed_commands: Vec::new(),
            sequence: Vec::new(),
            suggested_action: String::new(),
            limitations: Vec::new(),
            verification_status: None,
        };
        assert!(guidance_matches(
            &finding,
            "This project uses /fixture/project/src/lib.rs."
        ));
        assert!(!guidance_matches(
            &finding,
            "Use /fixture/project/src/main.rs."
        ));
    }
}
