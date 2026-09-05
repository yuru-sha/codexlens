//! Doctor report and proposal summary presentation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::analysis::{Finding, FindingScope, bounded_excerpt, sort_findings};
use crate::model::{CanonicalData, SourceRef};
use crate::store::StoreFreshness;

use super::diff::RenderedDiff;
use super::proposal::{MAX_PROPOSAL_TEXT_BYTES, bounded_evidence, heuristic_for};

const DEFAULT_REPORT_EXCERPT_BYTES: usize = 256;

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
    finding.summary = bounded_excerpt(&finding.summary, excerpt_max_bytes);
    finding.suggested_action = bounded_excerpt(&finding.suggested_action, excerpt_max_bytes);
    finding.observed_commands = finding
        .observed_commands
        .iter()
        .map(|command| bounded_excerpt(command, excerpt_max_bytes))
        .collect();
    finding.sequence = finding
        .sequence
        .iter()
        .map(|entry| bounded_excerpt(entry, excerpt_max_bytes))
        .collect();
    finding.limitations = finding
        .limitations
        .iter()
        .map(|limitation| bounded_excerpt(limitation, excerpt_max_bytes))
        .collect();
    finding.evidence = bounded_evidence(&finding.evidence, excerpt_max_bytes);
    finding
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
    use crate::advisor::test_support::finding;
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
}
