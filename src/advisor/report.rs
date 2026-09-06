//! Doctor report and proposal summary presentation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::analysis::{
    EvidenceRef, EvidenceRole, Finding, FindingScope, VerificationStatus, bounded_excerpt,
    sort_findings,
};
use crate::model::{CanonicalData, SourceKind, SourceRef};
use crate::store::{FreshnessState, StoreFreshness};

use super::diff::{DiffBatch, RenderedDiff, SkippedProposal};
use super::proposal::{
    MAX_PROPOSAL_TEXT_BYTES, MAX_REPORT_EVIDENCE, Proposal, bounded_evidence, heuristic_for,
};

const DEFAULT_REPORT_EXCERPT_BYTES: usize = 256;
// ponytail: cap machine diffs at 16 KiB; add a streamed artifact only when consumers need full patches.
const MAX_JSON_DIFF_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorOptions {
    pub max_findings_per_scope: Option<usize>,
    pub excerpt_max_bytes: usize,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            max_findings_per_scope: None,
            excerpt_max_bytes: DEFAULT_REPORT_EXCERPT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorGroup {
    pub scope: FindingScope,
    pub findings: Vec<DoctorFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorFinding {
    pub finding: Finding,
    pub heuristic: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub session_count: usize,
    pub freshness: StoreFreshness,
    pub finding_counts: BTreeMap<String, usize>,
    pub groups: Vec<DoctorGroup>,
}

const JSON_SCHEMA_VERSION: u32 = 1;

pub fn render_json_finding_report(
    command: &str,
    report: &DoctorReport,
) -> Result<String, serde_json::Error> {
    let groups = report
        .groups
        .iter()
        .map(|group| {
            serde_json::json!({
                "scope": scope_json(&group.scope),
                "findings": group
                    .findings
                    .iter()
                    .map(|reported| finding_json(&reported.finding, &reported.heuristic))
                    .collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    json_document(
        command,
        serde_json::json!({
            "period_start": report.period_start,
            "period_end": report.period_end,
            "session_count": report.session_count,
            "freshness": freshness_json(&report.freshness),
            "finding_counts": report.finding_counts,
            "groups": groups,
        }),
    )
}

pub fn render_json_sessions(
    data: &CanonicalData,
    freshness: &StoreFreshness,
) -> Result<String, serde_json::Error> {
    let mut sessions = data.sessions.iter().collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    sessions.dedup_by(|left, right| left.id == right.id);
    json_document(
        "sessions",
        serde_json::json!({
            "freshness": freshness_json(freshness),
            "sessions": sessions.into_iter().map(|session| {
                serde_json::json!({
                    "id": session.id,
                    "created_at": session.created_at,
                    "updated_at": session.updated_at,
                    "cwd": session.cwd,
                    "project": session.project,
                })
            }).collect::<Vec<_>>(),
        }),
    )
}

pub fn render_json_diff(batch: &DiffBatch) -> Result<String, serde_json::Error> {
    let mut rendered = batch.rendered.iter().collect::<Vec<_>>();
    rendered.sort_by(|left, right| {
        left.proposal
            .target_path
            .cmp(&right.proposal.target_path)
            .then_with(|| {
                left.proposal
                    .action
                    .as_str()
                    .cmp(right.proposal.action.as_str())
            })
            .then_with(|| {
                left.proposal
                    .observed_problem
                    .cmp(&right.proposal.observed_problem)
            })
    });
    let mut skipped = batch
        .skipped
        .iter()
        .cloned()
        .map(|skipped| (skipped, None))
        .collect::<Vec<(SkippedProposal, Option<&Proposal>)>>();
    let mut rendered_json = Vec::new();
    for rendered_diff in rendered {
        let redacted_diff = bounded_excerpt(&rendered_diff.diff, usize::MAX);
        if redacted_diff == rendered_diff.diff && redacted_diff.len() <= MAX_JSON_DIFF_BYTES {
            rendered_json.push(rendered_diff_json(rendered_diff, &redacted_diff));
        } else {
            let reason = if redacted_diff != rendered_diff.diff {
                "rendered diff required redaction and was omitted to keep the unified diff applicable"
                    .to_owned()
            } else {
                format!(
                    "rendered diff exceeds the {MAX_JSON_DIFF_BYTES}-byte JSON limit and was omitted"
                )
            };
            skipped.push((
                SkippedProposal {
                    target_path: rendered_diff.proposal.target_path.clone(),
                    reason,
                },
                Some(&rendered_diff.proposal),
            ));
        }
    }
    skipped.sort_by(|left, right| {
        left.0
            .target_path
            .cmp(&right.0.target_path)
            .then_with(|| left.0.reason.cmp(&right.0.reason))
    });
    json_document(
        "optimize_diff",
        serde_json::json!({
            "rendered": rendered_json,
            "skipped": skipped
                .into_iter()
                .map(|(skipped, proposal)| {
                    serde_json::json!({
                        "target_path": skipped.target_path.to_string_lossy(),
                        "reason": bounded_excerpt(&skipped.reason, MAX_PROPOSAL_TEXT_BYTES),
                        "proposal": proposal.map(proposal_json),
                    })
                })
                .collect::<Vec<_>>(),
        }),
    )
}

fn json_document(command: &str, data: serde_json::Value) -> Result<String, serde_json::Error> {
    let mut output = serde_json::to_string(&serde_json::json!({
        "schema_version": JSON_SCHEMA_VERSION,
        "command": command,
        "data": data,
    }))?;
    output.push('\n');
    Ok(output)
}

fn freshness_json(freshness: &StoreFreshness) -> serde_json::Value {
    serde_json::json!({
        "state": match freshness.state {
            FreshnessState::Empty => "empty",
            FreshnessState::Recorded => "recorded",
        },
        "source_count": freshness.source_count,
        "latest_ingested_at": freshness.latest_ingested_at,
    })
}

fn scope_json(scope: &FindingScope) -> serde_json::Value {
    match scope {
        FindingScope::Global => serde_json::json!({"kind": "global"}),
        FindingScope::Project(path) => serde_json::json!({
            "kind": "project",
            "value": path.to_string_lossy(),
        }),
        FindingScope::Instruction(path) => serde_json::json!({
            "kind": "instruction",
            "value": path.to_string_lossy(),
        }),
        FindingScope::Path(path) => serde_json::json!({
            "kind": "path",
            "value": path,
        }),
    }
}

fn finding_json(finding: &Finding, heuristic: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": finding.kind.as_str(),
        "severity": finding.severity.as_str(),
        "confidence": finding.confidence.as_str(),
        "scope": scope_json(&finding.scope),
        "key": finding.key,
        "summary": finding.summary,
        "evidence": finding.evidence.iter().map(evidence_json).collect::<Vec<_>>(),
        "occurrences": finding.occurrences,
        "distinct_sessions": finding.distinct_sessions,
        "affected_paths": finding.affected_paths,
        "observed_commands": finding.observed_commands,
        "sequence": finding.sequence,
        "suggested_action": finding.suggested_action,
        "limitations": finding.limitations,
        "verification_status": finding.verification_status.map(VerificationStatus::as_str),
        "heuristic": heuristic,
    })
}

fn evidence_json(evidence: &EvidenceRef) -> serde_json::Value {
    serde_json::json!({
        "session_id": evidence.session_id,
        "source": source_json(&evidence.source),
        "role": evidence_role(&evidence.role),
        "excerpt": evidence
            .excerpt
            .as_deref()
            .map(|excerpt| bounded_excerpt(excerpt, MAX_PROPOSAL_TEXT_BYTES)),
    })
}

fn source_json(source: &SourceRef) -> serde_json::Value {
    serde_json::json!({
        "kind": match source.kind {
            SourceKind::Rollout => "rollout",
            SourceKind::State => "state",
        },
        "path": source.path.to_string_lossy(),
        "line": source.line,
        "ingested_at": source.ingested_at,
        "parser_schema_version": source.parser_schema_version,
    })
}

fn evidence_role(role: &EvidenceRole) -> &'static str {
    match role {
        EvidenceRole::Observation => "observation",
        EvidenceRole::PrecedingAction => "preceding_action",
        EvidenceRole::FileOperation => "file_operation",
        EvidenceRole::VerificationCommand => "verification_command",
        EvidenceRole::InstructionSnapshot => "instruction_snapshot",
        EvidenceRole::InstructionFile => "instruction_file",
    }
}

fn rendered_diff_json(rendered: &RenderedDiff, diff: &str) -> serde_json::Value {
    serde_json::json!({
        "proposal": proposal_json(&rendered.proposal),
        "diff": diff,
    })
}

fn proposal_json(proposal: &Proposal) -> serde_json::Value {
    serde_json::json!({
        "target_scope": scope_json(&proposal.target_scope),
        "target_path": proposal.target_path.to_string_lossy(),
        "action": proposal.action.as_str(),
        "observed_problem": bounded_excerpt(&proposal.observed_problem, MAX_PROPOSAL_TEXT_BYTES),
        "evidence_count": proposal.evidence_count,
        "distinct_sessions": proposal.distinct_sessions,
        "confidence": proposal.confidence.as_str(),
        "heuristic": proposal.heuristic,
        "evidence": proposal.evidence.iter().map(evidence_json).collect::<Vec<_>>(),
        "proposed_text": proposal
            .proposed_text
            .as_deref()
            .map(|text| bounded_excerpt(text, MAX_PROPOSAL_TEXT_BYTES)),
        "existing_text": proposal
            .existing_text
            .as_deref()
            .map(|text| bounded_excerpt(text, MAX_PROPOSAL_TEXT_BYTES)),
        "source_path": proposal.source_path.as_ref().map(|path| path.to_string_lossy()),
        "expected_target_hash": proposal.expected_target_hash,
        "expected_source_hash": proposal.expected_source_hash,
        "target_rationale": bounded_excerpt(&proposal.target_rationale, MAX_PROPOSAL_TEXT_BYTES),
        "limitations": proposal
            .limitations
            .iter()
            .map(|limitation| bounded_excerpt(limitation, MAX_PROPOSAL_TEXT_BYTES))
            .collect::<Vec<_>>(),
        "review_reminder": bounded_excerpt(&proposal.review_reminder, MAX_PROPOSAL_TEXT_BYTES),
    })
}

pub fn doctor(
    data: &CanonicalData,
    findings: &[Finding],
    freshness: StoreFreshness,
    options: &DoctorOptions,
) -> DoctorReport {
    let mut ranked = findings.to_vec();
    sort_findings(&mut ranked);
    let mut finding_counts = BTreeMap::new();
    for finding in &ranked {
        *finding_counts
            .entry(finding.kind.as_str().to_owned())
            .or_insert(0) += 1;
    }

    let mut grouped = BTreeMap::<(u8, String), DoctorGroup>::new();
    for finding in ranked {
        let scope = finding.scope.clone();
        let key = (scope_rank(&scope), scope.to_string());
        let sanitized = sanitize_finding(finding, options.excerpt_max_bytes);
        grouped
            .entry(key)
            .or_insert_with(|| DoctorGroup {
                scope,
                findings: Vec::new(),
            })
            .findings
            .push(DoctorFinding {
                heuristic: heuristic_for(&sanitized).to_owned(),
                finding: sanitized,
            });
    }
    let mut groups = grouped.into_values().collect::<Vec<_>>();
    if let Some(limit) = options.max_findings_per_scope {
        for group in &mut groups {
            group.findings.truncate(limit);
        }
    }

    DoctorReport {
        period_start: period(data).0,
        period_end: period(data).1,
        session_count: session_count(data),
        freshness,
        finding_counts,
        groups,
    }
}

fn session_count(data: &CanonicalData) -> usize {
    let mut sessions = BTreeSet::new();
    sessions.extend(data.sessions.iter().map(|session| session.id.clone()));
    sessions.extend(data.turns.iter().filter_map(|turn| turn.session_id.clone()));
    sessions.extend(
        data.records
            .iter()
            .filter_map(|record| record.session_id.clone()),
    );
    sessions.extend(
        data.messages
            .iter()
            .filter_map(|message| message.session_id.clone()),
    );
    sessions.extend(
        data.tool_calls
            .iter()
            .filter_map(|call| call.session_id.clone()),
    );
    sessions.extend(
        data.tool_results
            .iter()
            .filter_map(|result| result.session_id.clone()),
    );
    sessions.extend(
        data.file_operations
            .iter()
            .filter_map(|operation| operation.session_id.clone()),
    );
    sessions.extend(
        data.token_usage
            .iter()
            .filter_map(|usage| usage.session_id.clone()),
    );
    sessions.extend(
        data.instruction_snapshots
            .iter()
            .filter_map(|snapshot| snapshot.session_id.clone()),
    );
    sessions.extend(
        data.instruction_joins
            .iter()
            .map(|join| join.session_id.clone()),
    );
    sessions.len()
}

fn scope_rank(scope: &FindingScope) -> u8 {
    match scope {
        FindingScope::Global => 0,
        FindingScope::Project(_) => 1,
        FindingScope::Instruction(_) => 2,
        FindingScope::Path(_) => 3,
    }
}

fn sanitize_finding(mut finding: Finding, excerpt_max_bytes: usize) -> Finding {
    finding.key = bounded_excerpt(&finding.key, excerpt_max_bytes);
    finding.summary = bounded_excerpt(&finding.summary, excerpt_max_bytes);
    finding.suggested_action = bounded_excerpt(&finding.suggested_action, excerpt_max_bytes);
    finding.affected_paths = bounded_strings(&finding.affected_paths, excerpt_max_bytes);
    finding.observed_commands = bounded_strings(&finding.observed_commands, excerpt_max_bytes);
    finding.sequence = bounded_strings(&finding.sequence, excerpt_max_bytes);
    finding.limitations = bounded_strings(&finding.limitations, excerpt_max_bytes);
    finding.evidence = bounded_evidence(&finding.evidence, excerpt_max_bytes);
    finding
}

fn bounded_strings(values: &[String], max_bytes: usize) -> Vec<String> {
    values
        .iter()
        .take(MAX_REPORT_EVIDENCE)
        .map(|value| bounded_excerpt(value, max_bytes))
        .collect()
}

fn period(data: &CanonicalData) -> (Option<String>, Option<String>) {
    let mut values = Vec::new();
    values.extend(data.sessions.iter().flat_map(|session| {
        [session.created_at.as_ref(), session.updated_at.as_ref()]
            .into_iter()
            .flatten()
            .cloned()
    }));
    values.extend(
        data.records
            .iter()
            .filter_map(|record| record.timestamp.clone()),
    );
    values.extend(
        data.messages
            .iter()
            .filter_map(|message| message.timestamp.clone()),
    );
    values.extend(
        data.file_operations
            .iter()
            .filter_map(|operation| operation.timestamp.clone()),
    );
    values.sort();
    (values.first().cloned(), values.last().cloned())
}

pub fn render_doctor(report: &DoctorReport) -> String {
    let mut output = String::new();
    output.push_str("Analyzed period: ");
    match (&report.period_start, &report.period_end) {
        (Some(start), Some(end)) if start == end => output.push_str(start),
        (Some(start), Some(end)) => {
            output.push_str(start);
            output.push_str(" .. ");
            output.push_str(end);
        }
        _ => output.push_str("unknown"),
    }
    output.push('\n');
    output.push_str(&format!("Sessions: {}\n", report.session_count));
    output.push_str(&format!(
        "Store freshness: {} ({} source files)\n",
        report.freshness, report.freshness.source_count
    ));
    output.push_str("Finding counts:");
    if report.finding_counts.is_empty() {
        output.push_str(" none\n");
    } else {
        for (index, (kind, count)) in report.finding_counts.iter().enumerate() {
            if index == 0 {
                output.push(' ');
            } else {
                output.push_str(", ");
            }
            output.push_str(kind);
            output.push('=');
            output.push_str(&count.to_string());
        }
        output.push('\n');
    }
    for group in &report.groups {
        output.push('\n');
        output.push('[');
        output.push_str(&group.scope.to_string());
        output.push_str("]\n");
        for reported in &group.findings {
            let finding = &reported.finding;
            output.push_str(&format!(
                "- {} / {} / {}: {} ({} occurrences, {} sessions)\n",
                finding.kind.as_str(),
                finding.severity.as_str(),
                finding.confidence.as_str(),
                finding.summary,
                finding.occurrences,
                finding.distinct_sessions
            ));
            output.push_str(&format!("  heuristic: {}\n", reported.heuristic));
            output.push_str(&format!("  action: {}\n", finding.suggested_action));
            for evidence in &finding.evidence {
                output.push_str("  evidence: ");
                output.push_str(&source_label(&evidence.source));
                if let Some(excerpt) = &evidence.excerpt {
                    output.push_str(" — ");
                    output.push_str(excerpt);
                }
                output.push('\n');
            }
            for limitation in &finding.limitations {
                output.push_str("  limitation: ");
                output.push_str(limitation);
                output.push('\n');
            }
        }
    }
    output
}

fn source_label(source: &SourceRef) -> String {
    match source.line {
        Some(line) => format!("{}:{line}", source.path.display()),
        None => source.path.display().to_string(),
    }
}

pub fn render_proposal_summary(rendered: &RenderedDiff) -> String {
    let proposal = &rendered.proposal;
    let mut output = format!(
        "Proposal {} {}\nObserved: {}\nEvidence: {} occurrences across {} sessions\nConfidence: {}\nHeuristic: {}\nTarget: {}\n",
        proposal.action.as_str(),
        proposal.target_path.display(),
        proposal.observed_problem,
        proposal.evidence_count,
        proposal.distinct_sessions,
        proposal.confidence.as_str(),
        proposal.heuristic,
        proposal.target_rationale,
    );
    for limitation in &proposal.limitations {
        output.push_str("Limitation: ");
        output.push_str(limitation);
        output.push('\n');
    }
    for evidence in bounded_evidence(&proposal.evidence, MAX_PROPOSAL_TEXT_BYTES) {
        output.push_str("Evidence ref: ");
        output.push_str(&source_label(&evidence.source));
        if let Some(excerpt) = evidence.excerpt {
            output.push_str(" — ");
            output.push_str(&excerpt);
        }
        output.push('\n');
    }
    output.push_str(&proposal.review_reminder);
    output.push('\n');
    output.push_str(&rendered.diff);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::test_support::{finding, proposal};
    use crate::advisor::{ProposalAction, RenderedDiff};
    use crate::analysis::{FindingScope, FindingType, VerificationStatus};
    use crate::store::StoreFreshness;
    use std::path::PathBuf;

    #[test]
    fn doctor_orders_scopes_and_bounds_evidence() {
        let mut value = finding(FindingScope::Global, FindingType::Failure, None);
        value.evidence[0].excerpt = Some("token=secret-value".repeat(100));
        let project = finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Correction,
            None,
        );
        let report = doctor(
            &CanonicalData::default(),
            &[project, value],
            StoreFreshness::recorded(2, Some("100".to_owned())),
            &DoctorOptions::default(),
        );
        assert_eq!(report.groups[0].scope, FindingScope::Global);
        assert_eq!(
            report.groups[1].scope,
            FindingScope::Project(PathBuf::from("/fixture/project"))
        );
        let excerpt = report.groups[0].findings[0].finding.evidence[0]
            .excerpt
            .as_ref()
            .unwrap();
        assert!(excerpt.len() <= DEFAULT_REPORT_EXCERPT_BYTES);
        assert!(excerpt.contains("[redacted]"));
        assert_eq!(
            report.groups[0].findings[0].heuristic,
            "repeated failed tool outcome"
        );
        assert!(render_doctor(&report).contains("heuristic: repeated failed tool outcome"));
        assert_eq!(report.freshness.source_count, 2);
    }

    #[test]
    fn verification_heuristic_describes_status() {
        let mut value = finding(FindingScope::Global, FindingType::Verification, None);
        value.verification_status = Some(VerificationStatus::Missing);
        assert_eq!(
            heuristic_for(&value),
            "absence of a recognized verification command after a change"
        );
        value.verification_status = Some(VerificationStatus::NotObserved);
        assert_eq!(
            heuristic_for(&value),
            "verification outcome was not observed in the available rollout"
        );
    }

    #[test]
    fn json_report_keeps_sensitive_excerpts_redacted_and_bounded() {
        let mut value = finding(FindingScope::Global, FindingType::Failure, None);
        value.summary = "token=summary-secret".to_owned();
        value.evidence[0].excerpt = Some("token=evidence-secret".repeat(100));
        let report = doctor(
            &CanonicalData::default(),
            &[value],
            StoreFreshness::recorded(1, None),
            &DoctorOptions::default(),
        );
        let output = render_json_finding_report("doctor", &report).unwrap();
        assert!(!output.contains("summary-secret"));
        assert!(!output.contains("evidence-secret"));
        assert!(output.contains("[redacted]"));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn json_report_sanitizes_keys_and_bounds_finding_arrays() {
        let mut value = finding(FindingScope::Global, FindingType::Failure, None);
        value.key = "token=key-secret".to_owned();
        value.affected_paths = (0..MAX_REPORT_EVIDENCE + 1)
            .map(|index| format!("path-{index}"))
            .collect();
        value.observed_commands = (0..MAX_REPORT_EVIDENCE + 1)
            .map(|index| format!("command-{index}"))
            .collect();
        value.sequence = (0..MAX_REPORT_EVIDENCE + 1)
            .map(|index| format!("sequence-{index}"))
            .collect();
        value.limitations = (0..MAX_REPORT_EVIDENCE + 1)
            .map(|index| format!("limitation-{index}"))
            .collect();
        let report = doctor(
            &CanonicalData::default(),
            &[value],
            StoreFreshness::recorded(1, None),
            &DoctorOptions::default(),
        );
        let finding = &report.groups[0].findings[0].finding;
        assert!(!finding.key.contains("key-secret"));
        assert_eq!(finding.affected_paths.len(), MAX_REPORT_EVIDENCE);
        assert_eq!(finding.observed_commands.len(), MAX_REPORT_EVIDENCE);
        assert_eq!(finding.sequence.len(), MAX_REPORT_EVIDENCE);
        assert_eq!(finding.limitations.len(), MAX_REPORT_EVIDENCE);

        let output = render_json_finding_report("doctor", &report).unwrap();
        assert!(!output.contains("key-secret"));
    }

    #[test]
    fn json_diff_skips_extended_credential_formats() {
        let diffs = [
            "Authorization: Bearer bearer-secret\n",
            "-----BEGIN PRIVATE KEY-----\nprivate-secret\n-----END PRIVATE KEY-----\n",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature-secret\n",
        ];
        let rendered = diffs
            .iter()
            .enumerate()
            .map(|(index, diff)| {
                let path = PathBuf::from(format!("/fixture/{index}.md"));
                let mut proposal = proposal(&path, ProposalAction::Add);
                proposal.expected_target_hash = Some("hash".to_owned());
                RenderedDiff {
                    proposal,
                    diff: (*diff).to_owned(),
                }
            })
            .collect();
        let output = render_json_diff(&DiffBatch {
            rendered,
            skipped: Vec::new(),
        })
        .unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(
            document["data"]["rendered"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        assert_eq!(
            document["data"]["skipped"].as_array().unwrap().len(),
            diffs.len()
        );
        for secret in ["bearer-secret", "private-secret", "signature-secret"] {
            assert!(!output.contains(secret));
        }
        assert!(
            document["data"]["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("redaction")))
        );
    }
}
