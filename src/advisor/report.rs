//! Doctor report and proposal summary presentation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::analysis::{
    EvidenceRef, EvidenceRole, Finding, FindingScope, VerificationStatus, bounded_excerpt,
    sort_findings,
};
use crate::model::{CanonicalData, SourceKind, SourceRef};
use crate::period::{PeriodCoverage, ReportingPeriod, Timestamp};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportCoverage {
    pub scope: String,
    pub status: String,
    pub activity_start: Option<String>,
    pub activity_end: Option<String>,
    pub valid_activity_timestamps: usize,
    pub missing_activity_timestamps: usize,
    pub invalid_activity_timestamps: usize,
    pub session_count: usize,
    pub record_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub cwd: Option<String>,
    pub project: Option<String>,
}

const JSON_SCHEMA_VERSION: u32 = 1;

pub fn render_json_finding_report(
    command: &str,
    report: &DoctorReport,
) -> Result<String, serde_json::Error> {
    render_json_finding_report_inner(command, report, None, None)
}

pub fn render_json_finding_report_with_coverage(
    command: &str,
    report: &DoctorReport,
    coverage: &ReportCoverage,
) -> Result<String, serde_json::Error> {
    render_json_finding_report_inner(command, report, Some(coverage), None)
}

pub fn render_json_finding_report_with_period(
    command: &str,
    report: &DoctorReport,
    coverage: &ReportCoverage,
    period: &PeriodCoverage,
) -> Result<String, serde_json::Error> {
    render_json_finding_report_inner(command, report, Some(coverage), Some(period))
}

fn render_json_finding_report_inner(
    command: &str,
    report: &DoctorReport,
    coverage: Option<&ReportCoverage>,
    period: Option<&PeriodCoverage>,
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
    let mut data = serde_json::json!({
        "period_start": report.period_start,
        "period_end": report.period_end,
        "session_count": report.session_count,
        "freshness": freshness_json(&report.freshness),
        "finding_counts": report.finding_counts,
        "groups": groups,
    });
    if let Some(coverage) = coverage {
        data["coverage"] = period.map_or_else(
            || coverage_json(coverage),
            |period| coverage_json_with_period(coverage, period),
        );
    }
    json_document(command, data)
}

pub fn render_json_sessions(
    data: &CanonicalData,
    freshness: &StoreFreshness,
) -> Result<String, serde_json::Error> {
    let coverage = report_coverage(data);
    json_document(
        "sessions",
        serde_json::json!({
            "freshness": freshness_json(freshness),
            "coverage": coverage_json(&coverage),
            "sessions": report_sessions(data).into_iter().map(|session| {
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

pub fn render_json_sessions_with_period(
    data: &CanonicalData,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    period: &PeriodCoverage,
) -> Result<String, serde_json::Error> {
    json_document(
        "sessions",
        serde_json::json!({
            "freshness": freshness_json(freshness),
            "coverage": coverage_json_with_period(coverage, period),
            "sessions": report_sessions(data).into_iter().map(|session| {
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
    render_json_diff_inner(batch, None)
}

pub fn render_json_diff_with_period(
    batch: &DiffBatch,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    period: &PeriodCoverage,
) -> Result<String, serde_json::Error> {
    render_json_diff_inner(batch, Some((freshness, coverage, period)))
}

fn render_json_diff_inner(
    batch: &DiffBatch,
    metadata: Option<(&StoreFreshness, &ReportCoverage, &PeriodCoverage)>,
) -> Result<String, serde_json::Error> {
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
    let mut data = serde_json::json!({
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
    });
    if let Some((freshness, coverage, period)) = metadata {
        data["freshness"] = freshness_json(freshness);
        data["coverage"] = coverage_json_with_period(coverage, period);
    }
    json_document("optimize_diff", data)
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

fn coverage_json(coverage: &ReportCoverage) -> serde_json::Value {
    serde_json::json!({
        "scope": coverage.scope,
        "status": coverage.status,
        "activity_start": coverage.activity_start,
        "activity_end": coverage.activity_end,
        "valid_activity_timestamps": coverage.valid_activity_timestamps,
        "missing_activity_timestamps": coverage.missing_activity_timestamps,
        "invalid_activity_timestamps": coverage.invalid_activity_timestamps,
        "session_count": coverage.session_count,
        "record_count": coverage.record_count,
    })
}

fn coverage_json_with_period(
    coverage: &ReportCoverage,
    period: &PeriodCoverage,
) -> serde_json::Value {
    let mut value = coverage_json(coverage);
    value["requested_start"] = serde_json::json!(period.requested_start);
    value["requested_end"] = serde_json::json!(period.requested_end);
    value["observed_start"] = serde_json::json!(period.observed_start);
    value["observed_end"] = serde_json::json!(period.observed_end);
    value["included_sessions"] = serde_json::json!(period.included_sessions);
    value["included_records"] = serde_json::json!(period.included_records);
    value["excluded_records"] = serde_json::json!(period.excluded_records);
    value["unknown_timestamp_records"] = serde_json::json!(period.unknown_timestamp_records);
    value["unknown_timestamp_events"] = serde_json::json!(period.unknown_timestamp_events);
    value["state"] = serde_json::json!(period.state.as_str());
    value
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
        "key": bounded_excerpt(&finding.key, MAX_PROPOSAL_TEXT_BYTES),
        "summary": finding.summary,
        "evidence": finding.evidence.iter().map(evidence_json).collect::<Vec<_>>(),
        "occurrences": finding.occurrences,
        "distinct_sessions": finding.distinct_sessions,
        "affected_paths": bounded_json_strings(&finding.affected_paths),
        "observed_commands": bounded_json_strings(&finding.observed_commands),
        "sequence": bounded_json_strings(&finding.sequence),
        "suggested_action": finding.suggested_action,
        "limitations": bounded_json_strings(&finding.limitations),
        "verification_status": finding.verification_status.map(VerificationStatus::as_str),
        "heuristic": heuristic,
    })
}

fn bounded_json_strings(values: &[String]) -> Vec<String> {
    values
        .iter()
        .take(MAX_REPORT_EVIDENCE)
        .map(|value| bounded_excerpt(value, MAX_PROPOSAL_TEXT_BYTES))
        .collect()
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

pub fn report_sessions(data: &CanonicalData) -> Vec<SessionSummary> {
    let mut summaries = BTreeMap::<String, SessionSummary>::new();
    for session in &data.sessions {
        summaries
            .entry(session.id.clone())
            .or_insert_with(|| SessionSummary {
                id: session.id.clone(),
                created_at: session.created_at.clone(),
                updated_at: session.updated_at.clone(),
                cwd: session.cwd.clone(),
                project: session.project.clone(),
            });
    }
    for session_id in session_ids(data) {
        summaries
            .entry(session_id.clone())
            .or_insert_with(|| SessionSummary {
                id: session_id,
                created_at: None,
                updated_at: None,
                cwd: None,
                project: None,
            });
    }
    summaries.into_values().collect()
}

pub fn report_coverage(data: &CanonicalData) -> ReportCoverage {
    report_coverage_filtered(data, None)
}

pub fn report_coverage_with_period(
    data: &CanonicalData,
    period: &ReportingPeriod,
) -> ReportCoverage {
    report_coverage_filtered(data, Some(period))
}

fn report_coverage_filtered(
    data: &CanonicalData,
    period: Option<&ReportingPeriod>,
) -> ReportCoverage {
    let mut timestamps = Vec::<(Timestamp, String)>::new();
    let mut missing_activity_timestamps = 0;
    let mut invalid_activity_timestamps = 0;

    for session in &data.sessions {
        for timestamp in [session.created_at.as_deref(), session.updated_at.as_deref()] {
            observe_timestamp(
                timestamp,
                &mut timestamps,
                &mut missing_activity_timestamps,
                &mut invalid_activity_timestamps,
                period,
            );
        }
    }
    for turn in &data.turns {
        for timestamp in [turn.started_at.as_deref(), turn.completed_at.as_deref()] {
            observe_timestamp(
                timestamp,
                &mut timestamps,
                &mut missing_activity_timestamps,
                &mut invalid_activity_timestamps,
                period,
            );
        }
        for event in &turn.lifecycle {
            observe_timestamp(
                event.timestamp.as_deref(),
                &mut timestamps,
                &mut missing_activity_timestamps,
                &mut invalid_activity_timestamps,
                period,
            );
        }
    }
    for timestamp in data
        .records
        .iter()
        .map(|record| record.timestamp.as_deref())
        .chain(
            data.messages
                .iter()
                .map(|message| message.timestamp.as_deref()),
        )
        .chain(
            data.file_operations
                .iter()
                .map(|operation| operation.timestamp.as_deref()),
        )
        .chain(
            data.token_usage
                .iter()
                .map(|usage| usage.timestamp.as_deref()),
        )
    {
        observe_timestamp(
            timestamp,
            &mut timestamps,
            &mut missing_activity_timestamps,
            &mut invalid_activity_timestamps,
            period,
        );
    }

    timestamps.sort();
    let (activity_start, activity_end) = match (timestamps.first(), timestamps.last()) {
        (Some(start), Some(end)) => (Some(start.1.clone()), Some(end.1.clone())),
        _ => (None, None),
    };
    let valid_activity_timestamps = timestamps.len();
    let session_count = session_count(data);
    let record_count = data.records.len();
    let status = if session_count == 0
        && record_count == 0
        && valid_activity_timestamps == 0
        && missing_activity_timestamps == 0
        && invalid_activity_timestamps == 0
    {
        "empty"
    } else if valid_activity_timestamps == 0
        || missing_activity_timestamps > 0
        || invalid_activity_timestamps > 0
    {
        "partial"
    } else {
        "observed"
    };

    ReportCoverage {
        scope: "selected_store".to_owned(),
        status: status.to_owned(),
        activity_start,
        activity_end,
        valid_activity_timestamps,
        missing_activity_timestamps,
        invalid_activity_timestamps,
        session_count,
        record_count,
    }
}

fn observe_timestamp(
    timestamp: Option<&str>,
    valid: &mut Vec<(Timestamp, String)>,
    missing: &mut usize,
    invalid: &mut usize,
    period: Option<&ReportingPeriod>,
) {
    let Some(timestamp) = timestamp else {
        *missing += 1;
        return;
    };
    let Some(parsed) = Timestamp::parse(timestamp) else {
        *invalid += 1;
        return;
    };
    if period.is_some_and(|period| !period.contains(parsed)) {
        return;
    }
    valid.push((parsed, timestamp.to_owned()));
}

fn session_count(data: &CanonicalData) -> usize {
    session_ids(data).len()
}

fn session_ids(data: &CanonicalData) -> BTreeSet<String> {
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
    sessions
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

pub fn render_doctor(report: &DoctorReport) -> String {
    render_doctor_inner(report, None, None)
}

pub fn render_doctor_with_coverage(report: &DoctorReport, coverage: &ReportCoverage) -> String {
    render_doctor_inner(report, Some(coverage), None)
}

pub fn render_doctor_with_period(
    report: &DoctorReport,
    coverage: &ReportCoverage,
    period: &PeriodCoverage,
) -> String {
    render_doctor_inner(report, Some(coverage), Some(period))
}

fn render_doctor_inner(
    report: &DoctorReport,
    coverage: Option<&ReportCoverage>,
    period: Option<&PeriodCoverage>,
) -> String {
    let mut output = String::new();
    if let Some(period) = period {
        output.push_str(&render_period_metadata(period));
    }
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
    if let Some(coverage) = coverage {
        output.push_str(&render_report_metadata(coverage, &report.freshness));
    } else {
        output.push_str(&format!("Sessions: {}\n", report.session_count));
        output.push_str(&format!(
            "Store freshness: {} ({} source files)\n",
            report.freshness, report.freshness.source_count
        ));
    }
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

fn render_period_metadata(period: &PeriodCoverage) -> String {
    let requested = match (&period.requested_start, &period.requested_end) {
        (Some(start), Some(end)) => format!("[{start}, {end})"),
        (Some(start), None) => format!("[{start}, ∞)"),
        (None, Some(end)) => format!("(-∞, {end})"),
        (None, None) => "all".to_owned(),
    };
    format!(
        "Requested period: {requested}\nObserved records: {} (excluded: {}, unknown timestamps: {} records, {} events)\nCoverage: {}\n",
        period.included_records,
        period.excluded_records,
        period.unknown_timestamp_records,
        period.unknown_timestamp_events,
        period.state.as_str(),
    )
}

pub fn render_report_metadata_with_period(
    coverage: &ReportCoverage,
    freshness: &StoreFreshness,
    period: &PeriodCoverage,
) -> String {
    let mut output = render_period_metadata(period);
    output.push_str(&render_report_metadata(coverage, freshness));
    output
}

pub fn render_report_metadata(coverage: &ReportCoverage, freshness: &StoreFreshness) -> String {
    let period = match (&coverage.activity_start, &coverage.activity_end) {
        (Some(start), Some(end)) if start == end => start.clone(),
        (Some(start), Some(end)) => format!("{start} .. {end}"),
        _ => "unknown".to_owned(),
    };
    let scope = coverage.scope.replace('_', " ");
    format!(
        "Coverage: {} ({}; not necessarily all historical activity or current raw inputs; refresh explicitly, archives via --include-archived)\nActivity: {period}\nActivity timestamps: {} valid, {} missing, {} invalid\nSessions: {}\nRecords: {}\nLatest ingestion: {}\nStore freshness: {} ({} source files)\n",
        scope,
        coverage.status,
        coverage.valid_activity_timestamps,
        coverage.missing_activity_timestamps,
        coverage.invalid_activity_timestamps,
        coverage.session_count,
        coverage.record_count,
        freshness.latest_ingested_at.as_deref().unwrap_or("unknown"),
        freshness,
        freshness.source_count,
    )
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
    use crate::model::{Record, RecordKind, Session};
    use crate::period::{ReportingPeriod, select_report_data};
    use crate::store::StoreFreshness;
    use std::path::PathBuf;

    fn record_with_timestamp(timestamp: Option<&str>) -> Record {
        Record {
            session_id: Some("session".to_owned()),
            turn_id: None,
            timestamp: timestamp.map(str::to_owned),
            sequence: 0,
            original_record_type: None,
            original_nested_type: None,
            error_category: None,
            is_error: false,
            is_terminal: false,
            kind: RecordKind::ResponseItem,
            provenance: crate::advisor::test_support::source(1),
        }
    }

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
    fn coverage_counts_valid_missing_and_invalid_activity_without_ingestion_fallback() {
        let session = |id: &str, created_at: Option<&str>, updated_at: Option<&str>| Session {
            id: id.to_owned(),
            created_at: created_at.map(str::to_owned),
            updated_at: updated_at.map(str::to_owned),
            cwd: None,
            project: None,
            model: None,
            provider: None,
            source: None,
            thread_source: None,
            rollout_path: None,
            archive_state: None,
            title: None,
            preview: None,
            parent_id: None,
            cli_version: None,
            originator: None,
            history_mode: None,
            reasoning_effort: None,
            provenance: crate::advisor::test_support::source(1),
        };
        let record = |timestamp: Option<&str>| Record {
            session_id: Some("session-a".to_owned()),
            turn_id: None,
            timestamp: timestamp.map(str::to_owned),
            sequence: 0,
            original_record_type: None,
            original_nested_type: None,
            error_category: None,
            is_error: false,
            is_terminal: false,
            kind: RecordKind::ResponseItem,
            provenance: crate::advisor::test_support::source(1),
        };
        let data = CanonicalData {
            sessions: vec![
                session("session-a", Some("2026-01-02T00:00:00Z"), None),
                session(
                    "session-b",
                    Some("not-a-timestamp"),
                    Some("2026-01-04T00:00:00+09:00"),
                ),
            ],
            records: vec![
                record(Some("2026-01-01T00:00:00Z")),
                record(None),
                record(Some("also-not-a-timestamp")),
            ],
            ..CanonicalData::default()
        };

        let coverage = report_coverage(&data);

        assert_eq!(coverage.status, "partial");
        assert_eq!(
            coverage.activity_start.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            coverage.activity_end.as_deref(),
            Some("2026-01-04T00:00:00+09:00")
        );
        assert_eq!(coverage.valid_activity_timestamps, 3);
        assert_eq!(coverage.missing_activity_timestamps, 2);
        assert_eq!(coverage.invalid_activity_timestamps, 2);
        assert_eq!(coverage.session_count, 2);
        assert_eq!(coverage.record_count, 3);

        let record_only = CanonicalData {
            records: vec![record(Some("2026-01-01T00:00:00Z"))],
            ..CanonicalData::default()
        };
        let summaries = report_sessions(&record_only);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "session-a");
        assert!(summaries[0].created_at.is_none());
    }

    #[test]
    fn coverage_orders_timestamps_by_full_precision_and_offset() {
        let data = CanonicalData {
            records: vec![
                record_with_timestamp(Some("2026-01-01T00:00:00.900Z")),
                record_with_timestamp(Some("2026-01-01T01:00:00.100+01:00")),
            ],
            ..CanonicalData::default()
        };

        let coverage = report_coverage(&data);

        assert_eq!(
            coverage.activity_start.as_deref(),
            Some("2026-01-01T01:00:00.100+01:00")
        );
        assert_eq!(
            coverage.activity_end.as_deref(),
            Some("2026-01-01T00:00:00.900Z")
        );
    }

    #[test]
    fn period_selection_and_coverage_agree_on_invalid_event_timestamps() {
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-01T00:00:00Z"),
            Some("2026-01-02T00:00:00Z"),
        )
        .unwrap()
        .unwrap();

        for timestamp in [
            " 2026-01-01T00:00:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00.1234567890Z",
        ] {
            let data = CanonicalData {
                records: vec![record_with_timestamp(Some(timestamp))],
                ..CanonicalData::default()
            };

            let selected = select_report_data(&data, Some(&period));
            let coverage = report_coverage_with_period(&data, &period);

            assert!(selected.data.records.is_empty(), "{timestamp}");
            assert_eq!(
                selected.coverage.unknown_timestamp_records, 1,
                "{timestamp}"
            );
            assert_eq!(coverage.valid_activity_timestamps, 0, "{timestamp}");
            assert_eq!(coverage.invalid_activity_timestamps, 1, "{timestamp}");
        }
    }

    #[test]
    fn period_selection_normalizes_equivalent_utc_and_offset_timestamps() {
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-01T00:00:00.100Z"),
            Some("2026-01-01T00:00:00.101Z"),
        )
        .unwrap()
        .unwrap();
        let data = CanonicalData {
            records: vec![
                record_with_timestamp(Some("2026-01-01T00:00:00.100Z")),
                record_with_timestamp(Some("2026-01-01T01:00:00.100+01:00")),
            ],
            ..CanonicalData::default()
        };

        let selected = select_report_data(&data, Some(&period));

        assert_eq!(selected.data.records.len(), 2);
        assert_eq!(
            selected.coverage.observed_start.as_deref(),
            Some("2026-01-01T00:00:00.1Z")
        );
        assert_eq!(
            selected.coverage.observed_end.as_deref(),
            Some("2026-01-01T00:00:00.1Z")
        );
    }

    #[test]
    fn period_coverage_ignores_boundary_session_timestamps() {
        let data = CanonicalData {
            sessions: vec![Session {
                id: "session".to_owned(),
                created_at: Some("2026-01-02T00:00:00Z".to_owned()),
                updated_at: Some("2026-01-04T00:00:00Z".to_owned()),
                cwd: None,
                project: None,
                model: None,
                provider: None,
                source: None,
                thread_source: None,
                rollout_path: None,
                archive_state: None,
                title: None,
                preview: None,
                parent_id: None,
                cli_version: None,
                originator: None,
                history_mode: None,
                reasoning_effort: None,
                provenance: crate::advisor::test_support::source(1),
            }],
            records: vec![Record {
                session_id: Some("session".to_owned()),
                turn_id: None,
                timestamp: Some("2026-01-03T12:00:00Z".to_owned()),
                sequence: 0,
                original_record_type: None,
                original_nested_type: None,
                error_category: None,
                is_error: false,
                is_terminal: false,
                kind: RecordKind::ResponseItem,
                provenance: crate::advisor::test_support::source(2),
            }],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap()
        .unwrap();
        let selected = select_report_data(&data, Some(&period));
        assert_eq!(selected.data.sessions.len(), 1);

        let coverage = report_coverage_with_period(&selected.data, &period);

        assert_eq!(
            coverage.activity_start.as_deref(),
            Some("2026-01-03T12:00:00Z")
        );
        assert_eq!(
            coverage.activity_end.as_deref(),
            Some("2026-01-03T12:00:00Z")
        );
        assert_eq!(coverage.valid_activity_timestamps, 1);
    }

    #[test]
    fn legacy_period_fields_remain_separate_from_valid_activity_coverage() {
        let data = CanonicalData {
            sessions: vec![Session {
                id: "session".to_owned(),
                created_at: Some("2026-01-01T00:00:00+09:00".to_owned()),
                updated_at: None,
                cwd: None,
                project: None,
                model: None,
                provider: None,
                source: None,
                thread_source: None,
                rollout_path: None,
                archive_state: None,
                title: None,
                preview: None,
                parent_id: None,
                cli_version: None,
                originator: None,
                history_mode: None,
                reasoning_effort: None,
                provenance: crate::advisor::test_support::source(1),
            }],
            records: vec![Record {
                session_id: Some("session".to_owned()),
                turn_id: None,
                timestamp: Some("2025-12-31T20:00:00Z".to_owned()),
                sequence: 0,
                original_record_type: None,
                original_nested_type: None,
                error_category: None,
                is_error: false,
                is_terminal: false,
                kind: RecordKind::ResponseItem,
                provenance: crate::advisor::test_support::source(1),
            }],
            ..CanonicalData::default()
        };

        let report = doctor(
            &data,
            &[],
            StoreFreshness::recorded(1, Some("2099-01-01T00:00:00Z".to_owned())),
            &DoctorOptions::default(),
        );

        assert_eq!(report.period_start.as_deref(), Some("2025-12-31T20:00:00Z"));
        assert_eq!(
            report.period_end.as_deref(),
            Some("2026-01-01T00:00:00+09:00")
        );
        let coverage = report_coverage(&data);
        assert_eq!(
            coverage.activity_start.as_deref(),
            Some("2026-01-01T00:00:00+09:00")
        );
        assert_eq!(
            coverage.activity_end.as_deref(),
            Some("2025-12-31T20:00:00Z")
        );
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
        assert_eq!(finding.affected_paths.len(), MAX_REPORT_EVIDENCE + 1);
        assert_eq!(finding.observed_commands.len(), MAX_REPORT_EVIDENCE + 1);
        assert_eq!(finding.sequence.len(), MAX_REPORT_EVIDENCE + 1);
        assert_eq!(finding.limitations.len(), MAX_REPORT_EVIDENCE + 1);

        let output = render_json_finding_report("doctor", &report).unwrap();
        assert!(!output.contains("key-secret"));
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        let finding = &document["data"]["groups"][0]["findings"][0];
        for field in [
            "affected_paths",
            "observed_commands",
            "sequence",
            "limitations",
        ] {
            assert_eq!(
                finding[field].as_array().unwrap().len(),
                MAX_REPORT_EVIDENCE
            );
        }
    }

    #[test]
    fn json_diff_skips_extended_credential_formats() {
        let diffs = [
            "Authorization: Bearer bearer-secret\n",
            "Authorization: Bearer: bearer-delimited-secret\n",
            "Authorization: Basic basic-secret\n",
            "Authorization: Token token-auth-secret\n",
            "-----BEGIN PRIVATE KEY-----\nprivate-secret\n-----END PRIVATE KEY-----\n",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature-secret\n",
            "eyJhbGciOiJub25lIn0.e30.short-signature\n",
            "eyJhbGciOiJub25lIn0.a.b.c.jwe-secret\n",
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
        for secret in [
            "bearer-secret",
            "bearer-delimited-secret",
            "basic-secret",
            "token-auth-secret",
            "private-secret",
            "signature-secret",
            "short-signature",
            "jwe-secret",
        ] {
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
