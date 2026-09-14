//! Doctor report and proposal summary presentation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::analysis::{
    EvidenceRef, EvidenceRole, Finding, FindingScope, VerificationStatus, bounded_excerpt,
    sort_findings,
};
use crate::model::{CanonicalData, DiagnosticKind, SourceKind, SourceRef};
use crate::period::{
    PeriodCoverage, ReportingPeriod, SourceKey, Timestamp, event_timestamp, record_timestamps,
};
use crate::store::{FreshnessState, StoreFreshness};

use super::diff::{DiffBatch, RenderedDiff, SkippedProposal};
use super::proposal::{
    MAX_PROPOSAL_TEXT_BYTES, MAX_REPORT_EVIDENCE, Proposal, bounded_evidence, heuristic_for,
};

const DEFAULT_REPORT_EXCERPT_BYTES: usize = 256;
/// Default maximum number of session rows rendered by a report.
pub const DEFAULT_SESSION_LIMIT: usize = 50;
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
pub struct CoverageLimitation {
    pub kind: String,
    pub source: SourceRef,
    pub message: String,
    pub selected_sessions: usize,
    pub selected_records: usize,
    pub affected_lenses: Vec<String>,
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
    #[serde(default)]
    pub limitations: Vec<CoverageLimitation>,
    #[serde(default)]
    pub limitations_omitted: usize,
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
const PROPOSAL_VERIFICATION: &str =
    "Run the project's documented verification command and inspect the target diff after applying.";

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
    let (sessions, omitted_count) = report_sessions_with_limit(data, DEFAULT_SESSION_LIMIT);
    json_document(
        "sessions",
        serde_json::json!({
            "freshness": freshness_json(freshness),
            "coverage": coverage_json(&coverage),
            "sessions": sessions.into_iter().map(|session| {
                serde_json::json!({
                    "id": session.id,
                    "created_at": session.created_at,
                    "updated_at": session.updated_at,
                    "cwd": session.cwd,
                    "project": session.project,
                })
            }).collect::<Vec<_>>(),
            "omitted_count": omitted_count,
        }),
    )
}

pub fn render_json_sessions_with_period(
    data: &CanonicalData,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    period: &PeriodCoverage,
) -> Result<String, serde_json::Error> {
    let (sessions, omitted_count) = report_sessions_with_limit(data, DEFAULT_SESSION_LIMIT);
    json_document(
        "sessions",
        serde_json::json!({
            "freshness": freshness_json(freshness),
            "coverage": coverage_json_with_period(coverage, period),
            "sessions": sessions.into_iter().map(|session| {
                serde_json::json!({
                    "id": session.id,
                    "created_at": session.created_at,
                    "updated_at": session.updated_at,
                    "cwd": session.cwd,
                    "project": session.project,
                })
            }).collect::<Vec<_>>(),
            "omitted_count": omitted_count,
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
    let mut value = serde_json::json!({
        "scope": coverage.scope,
        "status": coverage.status,
        "activity_start": coverage.activity_start,
        "activity_end": coverage.activity_end,
        "valid_activity_timestamps": coverage.valid_activity_timestamps,
        "missing_activity_timestamps": coverage.missing_activity_timestamps,
        "invalid_activity_timestamps": coverage.invalid_activity_timestamps,
        "session_count": coverage.session_count,
        "record_count": coverage.record_count,
    });
    let limitations = coverage_limitations_json(coverage);
    value["limitations"] = limitations["limitations"].clone();
    value["limitations_omitted"] = limitations["limitations_omitted"].clone();
    value
}

fn coverage_limitation_json(limitation: &CoverageLimitation) -> serde_json::Value {
    serde_json::json!({
        "kind": limitation.kind,
        "source": source_json(&limitation.source),
        "message": bounded_excerpt(&limitation.message, MAX_PROPOSAL_TEXT_BYTES),
        "selected_sessions": limitation.selected_sessions,
        "selected_records": limitation.selected_records,
        "affected_lenses": bounded_json_strings(&limitation.affected_lenses),
    })
}

pub fn coverage_limitations_json(coverage: &ReportCoverage) -> serde_json::Value {
    serde_json::json!({
        "limitations": coverage
            .limitations
            .iter()
            .map(coverage_limitation_json)
            .collect::<Vec<_>>(),
        "limitations_omitted": coverage.limitations_omitted,
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
        "verification": PROPOSAL_VERIFICATION,
    })
}

pub fn doctor(
    data: &CanonicalData,
    findings: &[Finding],
    freshness: StoreFreshness,
    options: &DoctorOptions,
) -> DoctorReport {
    let coverage = report_coverage(data);
    doctor_with_coverage(data, findings, freshness, options, &coverage)
}

/// Build a doctor report from coverage computed by the caller.
///
/// Reusing this coverage keeps the report period and rendered coverage aligned
/// without scanning the canonical data a second time.
pub fn doctor_with_coverage(
    data: &CanonicalData,
    findings: &[Finding],
    freshness: StoreFreshness,
    options: &DoctorOptions,
    coverage: &ReportCoverage,
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
        period_start: coverage.activity_start.clone(),
        period_end: coverage.activity_end.clone(),
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

/// Return bounded session summaries and the number omitted by the bound.
pub fn report_sessions_with_limit(
    data: &CanonicalData,
    limit: usize,
) -> (Vec<SessionSummary>, usize) {
    let summaries = report_sessions(data);
    let omitted_count = summaries.len().saturating_sub(limit);
    (summaries.into_iter().take(limit).collect(), omitted_count)
}

pub fn report_coverage(data: &CanonicalData) -> ReportCoverage {
    report_coverage_filtered(data, None, data)
}

pub fn report_coverage_with_period(
    data: &CanonicalData,
    period: &ReportingPeriod,
) -> ReportCoverage {
    report_coverage_filtered(data, Some(period), data)
}

/// Compute period coverage from the unprojected data while attributing each
/// limitation to the data that the report will actually analyze.
pub fn report_coverage_for_period_selection(
    data: &CanonicalData,
    period: &ReportingPeriod,
    selected_data: &CanonicalData,
) -> ReportCoverage {
    report_coverage_filtered(data, Some(period), selected_data)
}

fn report_coverage_filtered(
    data: &CanonicalData,
    period: Option<&ReportingPeriod>,
    impact_data: &CanonicalData,
) -> ReportCoverage {
    let record_times = record_timestamps(data);
    let mut observations = CoverageObservations::new(period);

    for session in &data.sessions {
        observations.observe_timestamp(
            session.created_at.as_deref(),
            &session.provenance,
            "session.created_at",
            "missing_activity_timestamp",
        );
        observations.observe_timestamp(
            session.updated_at.as_deref(),
            &session.provenance,
            "session.updated_at",
            "missing_activity_timestamp",
        );
    }
    for turn in &data.turns {
        observations.observe_timestamp(
            turn.started_at.as_deref(),
            &turn.provenance,
            "turn.started_at",
            "missing_lifecycle_timestamp",
        );
        observations.observe_timestamp(
            turn.completed_at.as_deref(),
            &turn.provenance,
            "turn.completed_at",
            "missing_lifecycle_timestamp",
        );
        for event in &turn.lifecycle {
            observations.observe_event_timestamp(
                event.timestamp.as_deref(),
                &event.provenance,
                &record_times,
                &format!("turn.lifecycle.{}", event.kind),
                "missing_lifecycle_timestamp",
            );
        }
    }
    for record in &data.records {
        observations.observe_timestamp(
            record.timestamp.as_deref(),
            &record.provenance,
            "record.timestamp",
            "missing_activity_timestamp",
        );
    }
    for message in &data.messages {
        observations.observe_event_timestamp(
            message.timestamp.as_deref(),
            &message.provenance,
            &record_times,
            "message.timestamp",
            "missing_activity_timestamp",
        );
    }
    for operation in &data.file_operations {
        observations.observe_event_timestamp(
            operation.timestamp.as_deref(),
            &operation.provenance,
            &record_times,
            "file_operation.timestamp",
            "missing_activity_timestamp",
        );
    }
    for usage in &data.token_usage {
        observations.observe_event_timestamp(
            usage.timestamp.as_deref(),
            &usage.provenance,
            &record_times,
            "token_usage.timestamp",
            "missing_activity_timestamp",
        );
    }
    for diagnostic in &data.diagnostics {
        observations.add_diagnostic(diagnostic);
    }

    let CoverageObservations {
        mut timestamps,
        missing_activity_timestamps,
        invalid_activity_timestamps,
        mut limitations,
        limitations_omitted,
        period: _,
    } = observations;
    limitations.sort_by(compare_limitations);
    limitations.truncate(MAX_REPORT_EVIDENCE);
    let limitations: Vec<CoverageLimitation> = if limitations.is_empty() {
        Vec::new()
    } else {
        let source_impacts = source_impact_index(impact_data, &limitations);
        limitations
            .into_iter()
            .map(|limitation| {
                let (selected_sessions, selected_records) =
                    source_impact(&source_impacts, limitation.source);
                CoverageLimitation {
                    kind: limitation.kind.to_owned(),
                    source: limitation.source.clone(),
                    message: limitation.message,
                    selected_sessions,
                    selected_records,
                    affected_lenses: limitation
                        .affected_lenses
                        .iter()
                        .map(|lens| (*lens).to_owned())
                        .collect(),
                }
            })
            .collect()
    };
    timestamps.sort();
    let (activity_start, activity_end) = match (timestamps.first(), timestamps.last()) {
        (Some(start), Some(end)) => (Some(start.1.clone()), Some(end.1.clone())),
        _ => (None, None),
    };
    let valid_activity_timestamps = timestamps.len();
    let session_count = session_count(data);
    let record_count = data.records.len();
    let has_limitations = !limitations.is_empty() || limitations_omitted > 0;
    let status = if session_count == 0
        && record_count == 0
        && valid_activity_timestamps == 0
        && missing_activity_timestamps == 0
        && invalid_activity_timestamps == 0
        && !has_limitations
    {
        "empty"
    } else if valid_activity_timestamps == 0
        || missing_activity_timestamps > 0
        || invalid_activity_timestamps > 0
        || has_limitations
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
        limitations,
        limitations_omitted,
    }
}

struct CoverageObservations<'period, 'source> {
    period: Option<&'period ReportingPeriod>,
    timestamps: Vec<(Timestamp, String)>,
    missing_activity_timestamps: usize,
    invalid_activity_timestamps: usize,
    limitations: Vec<PendingCoverageLimitation<'source>>,
    limitations_omitted: usize,
}

struct PendingCoverageLimitation<'source> {
    kind: &'static str,
    source: &'source SourceRef,
    message: String,
    affected_lenses: &'static [&'static str],
}

impl<'period, 'source> CoverageObservations<'period, 'source> {
    fn new(period: Option<&'period ReportingPeriod>) -> Self {
        Self {
            period,
            timestamps: Vec::new(),
            missing_activity_timestamps: 0,
            invalid_activity_timestamps: 0,
            limitations: Vec::new(),
            limitations_omitted: 0,
        }
    }

    fn observe_timestamp(
        &mut self,
        timestamp: Option<&str>,
        source: &'source SourceRef,
        field: &str,
        missing_kind: &'static str,
    ) {
        let Some(timestamp) = timestamp else {
            self.missing_activity_timestamps += 1;
            self.add_limitation(
                missing_kind,
                source,
                format!("missing {field} timestamp"),
                timestamp_lenses(),
            );
            return;
        };
        let Some(parsed) = Timestamp::parse(timestamp) else {
            self.invalid_activity_timestamps += 1;
            self.add_limitation(
                "invalid_timestamp",
                source,
                format!("{field} timestamp is invalid"),
                timestamp_lenses(),
            );
            return;
        };
        if self.period.is_some_and(|period| !period.contains(parsed)) {
            return;
        }
        self.timestamps.push((parsed, timestamp.to_owned()));
    }

    fn observe_event_timestamp(
        &mut self,
        value: Option<&str>,
        source: &'source SourceRef,
        record_times: &HashMap<SourceKey, Option<Timestamp>>,
        field: &str,
        missing_kind: &'static str,
    ) {
        if let Some(value) = value {
            self.observe_timestamp(Some(value), source, field, missing_kind);
        } else if let Some(timestamp) = event_timestamp(None, source, record_times) {
            let formatted = timestamp.format();
            self.observe_timestamp(Some(&formatted), source, field, missing_kind);
        } else {
            self.observe_timestamp(None, source, field, missing_kind);
        }
    }

    fn add_diagnostic(&mut self, diagnostic: &'source crate::model::CanonicalDiagnostic) {
        self.add_limitation(
            diagnostic.kind.as_str(),
            &diagnostic.source,
            diagnostic.message.clone(),
            diagnostic_lenses(diagnostic.kind),
        );
    }

    fn add_limitation(
        &mut self,
        kind: &'static str,
        source: &'source SourceRef,
        message: String,
        affected_lenses: &'static [&'static str],
    ) {
        self.limitations.push(PendingCoverageLimitation {
            kind,
            source,
            message,
            affected_lenses,
        });
        if self.limitations.len() > MAX_REPORT_EVIDENCE {
            self.limitations.sort_by(compare_limitations);
            self.limitations.pop();
            self.limitations_omitted += 1;
        }
    }
}

fn compare_limitations(
    left: &PendingCoverageLimitation<'_>,
    right: &PendingCoverageLimitation<'_>,
) -> std::cmp::Ordering {
    limitation_priority(left.kind)
        .cmp(&limitation_priority(right.kind))
        .then_with(|| left.kind.cmp(right.kind))
        .then_with(|| left.source.path.cmp(&right.source.path))
        .then_with(|| left.source.line.cmp(&right.source.line))
        .then_with(|| left.message.cmp(&right.message))
}

const TIMESTAMP_LENSES: &[&str] = &["corrections", "rework", "stuck", "verification", "usage"];
const METADATA_LENSES: &[&str] = &["inventory", "overhead", "usage", "waste", "instructions"];
const SOURCE_LENSES: &[&str] = &[
    "failures",
    "corrections",
    "rework",
    "stuck",
    "verification",
    "knowledge",
    "instructions",
    "inventory",
    "overhead",
    "usage",
    "waste",
    "prompts",
];

fn timestamp_lenses() -> &'static [&'static str] {
    TIMESTAMP_LENSES
}

fn diagnostic_lenses(kind: DiagnosticKind) -> &'static [&'static str] {
    match kind {
        DiagnosticKind::MetadataConflict => METADATA_LENSES,
        DiagnosticKind::MalformedJson
        | DiagnosticKind::OversizedLine
        | DiagnosticKind::Unreadable
        | DiagnosticKind::StateSchemaMismatch
        | DiagnosticKind::StateQuery
        | DiagnosticKind::UnsupportedReader => SOURCE_LENSES,
    }
}

fn limitation_priority(kind: &str) -> u8 {
    match kind {
        "missing_activity_timestamp" | "missing_lifecycle_timestamp" | "invalid_timestamp" => 1,
        _ => 0,
    }
}

#[derive(Default)]
struct SourceImpact {
    sessions: BTreeSet<String>,
    records: usize,
}

type SourceImpactIndex = HashMap<(u8, std::path::PathBuf), SourceImpact>;

fn source_impact_index(
    data: &CanonicalData,
    limitations: &[PendingCoverageLimitation<'_>],
) -> SourceImpactIndex {
    let mut index = SourceImpactIndex::new();
    let mut target_paths = HashMap::<u8, HashSet<std::path::PathBuf>>::new();
    for limitation in limitations {
        let kind = source_kind_key(limitation.source.kind);
        target_paths
            .entry(kind)
            .or_default()
            .insert(limitation.source.path.clone());
        index
            .entry((kind, limitation.source.path.clone()))
            .or_default();
    }
    for session in &data.sessions {
        add_source_impact(
            &mut index,
            &target_paths,
            &session.provenance,
            Some(&session.id),
            false,
        );
    }
    for turn in &data.turns {
        add_source_impact(
            &mut index,
            &target_paths,
            &turn.provenance,
            turn.session_id.as_deref(),
            false,
        );
    }
    for record in &data.records {
        add_source_impact(
            &mut index,
            &target_paths,
            &record.provenance,
            record.session_id.as_deref(),
            true,
        );
    }
    for message in &data.messages {
        add_source_impact(
            &mut index,
            &target_paths,
            &message.provenance,
            message.session_id.as_deref(),
            false,
        );
    }
    for call in &data.tool_calls {
        add_source_impact(
            &mut index,
            &target_paths,
            &call.provenance,
            call.session_id.as_deref(),
            false,
        );
    }
    for result in &data.tool_results {
        add_source_impact(
            &mut index,
            &target_paths,
            &result.provenance,
            result.session_id.as_deref(),
            false,
        );
    }
    for operation in &data.file_operations {
        add_source_impact(
            &mut index,
            &target_paths,
            &operation.provenance,
            operation.session_id.as_deref(),
            false,
        );
    }
    for usage in &data.token_usage {
        add_source_impact(
            &mut index,
            &target_paths,
            &usage.provenance,
            usage.session_id.as_deref(),
            false,
        );
    }
    for snapshot in &data.instruction_snapshots {
        add_source_impact(
            &mut index,
            &target_paths,
            &snapshot.provenance,
            snapshot.session_id.as_deref(),
            false,
        );
    }
    for join in &data.instruction_joins {
        add_source_impact(
            &mut index,
            &target_paths,
            &join.provenance,
            Some(&join.session_id),
            false,
        );
    }
    index
}

fn add_source_impact(
    index: &mut SourceImpactIndex,
    target_paths: &HashMap<u8, HashSet<std::path::PathBuf>>,
    source: &SourceRef,
    session_id: Option<&str>,
    is_record: bool,
) {
    let kind = source_kind_key(source.kind);
    if !target_paths
        .get(&kind)
        .is_some_and(|paths| paths.contains(&source.path))
    {
        return;
    }
    let Some(impact) = index.get_mut(&(kind, source.path.clone())) else {
        return;
    };
    if let Some(session_id) = session_id {
        impact.sessions.insert(session_id.to_owned());
    }
    if is_record {
        impact.records += 1;
    }
}

fn source_impact(index: &SourceImpactIndex, source: &SourceRef) -> (usize, usize) {
    index
        .get(&(source_kind_key(source.kind), source.path.clone()))
        .map_or((0, 0), |impact| (impact.sessions.len(), impact.records))
}

fn source_kind_key(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Rollout => 0,
        SourceKind::State => 1,
    }
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
    let mut output = format!(
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
    );
    output.push_str(&render_coverage_limitations(coverage));
    output
}

pub fn render_coverage_limitations(coverage: &ReportCoverage) -> String {
    if coverage.limitations.is_empty() && coverage.limitations_omitted == 0 {
        return "Limitations: none\n".to_owned();
    }
    let mut output = String::from("Limitations:\n");
    for limitation in &coverage.limitations {
        output.push_str(&format!(
            "Limitation {} at {}: {} (selected sessions: {}; selected records: {}; affected lenses: {})\n",
            limitation.kind,
            bounded_excerpt(&source_label(&limitation.source), MAX_PROPOSAL_TEXT_BYTES),
            bounded_excerpt(&limitation.message, MAX_PROPOSAL_TEXT_BYTES),
            limitation.selected_sessions,
            limitation.selected_records,
            limitation.affected_lenses.join(", "),
        ));
    }
    if coverage.limitations_omitted > 0 {
        output.push_str(&format!(
            "Limitations omitted: {} additional limitation(s)\n",
            coverage.limitations_omitted
        ));
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
        "Proposal {} {}\nObserved: {}\nEvidence: {} occurrences across {} sessions\nConfidence: {}\nHeuristic: {}\nTarget: {}\nVerification: {}\n",
        proposal.action.as_str(),
        proposal.target_path.display(),
        proposal.observed_problem,
        proposal.evidence_count,
        proposal.distinct_sessions,
        proposal.confidence.as_str(),
        proposal.heuristic,
        proposal.target_rationale,
        PROPOSAL_VERIFICATION,
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
    use crate::advisor::test_support::{data_with_join, finding, proposal, source};
    use crate::advisor::{ProposalAction, RenderedDiff};
    use crate::analysis::{FindingScope, FindingType, VerificationStatus};
    use crate::model::{
        CanonicalDiagnostic, DiagnosticKind, Record, RecordKind, Session, SourceRef, Turn,
        TurnLifecycleEvent,
    };
    use crate::period::{ReportingPeriod, select_report_data};
    use crate::store::StoreFreshness;
    use std::path::PathBuf;

    fn record_with_timestamp(session_id: &str, timestamp: Option<&str>) -> Record {
        Record {
            session_id: Some(session_id.to_owned()),
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
    fn doctor_with_coverage_uses_precomputed_coverage() {
        let coverage = ReportCoverage {
            scope: "selected_store".to_owned(),
            status: "observed".to_owned(),
            activity_start: Some("precomputed-start".to_owned()),
            activity_end: Some("precomputed-end".to_owned()),
            valid_activity_timestamps: 2,
            missing_activity_timestamps: 0,
            invalid_activity_timestamps: 0,
            session_count: 1,
            record_count: 2,
            limitations: Vec::new(),
            limitations_omitted: 0,
        };

        let report = doctor_with_coverage(
            &CanonicalData::default(),
            &[],
            StoreFreshness::recorded(1, None),
            &DoctorOptions::default(),
            &coverage,
        );

        assert_eq!(report.period_start.as_deref(), Some("precomputed-start"));
        assert_eq!(report.period_end.as_deref(), Some("precomputed-end"));
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
                record_with_timestamp("session-a", Some("2026-01-01T00:00:00Z")),
                record_with_timestamp("session-a", None),
                record_with_timestamp("session-a", Some("also-not-a-timestamp")),
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
            records: vec![record_with_timestamp(
                "session-a",
                Some("2026-01-01T00:00:00Z"),
            )],
            ..CanonicalData::default()
        };
        let summaries = report_sessions(&record_only);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "session-a");
        assert!(summaries[0].created_at.is_none());

        let bounded = CanonicalData {
            records: (0..51)
                .map(|index| {
                    record_with_timestamp(
                        &format!("session-{index:02}"),
                        Some("2026-01-01T00:00:00Z"),
                    )
                })
                .collect(),
            ..CanonicalData::default()
        };
        let (sessions, omitted_count) = report_sessions_with_limit(&bounded, DEFAULT_SESSION_LIMIT);
        assert_eq!(sessions.len(), DEFAULT_SESSION_LIMIT);
        assert_eq!(omitted_count, 1);
        let document: serde_json::Value = serde_json::from_str(
            &render_json_sessions(&bounded, &StoreFreshness::recorded(51, None)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            document["data"]["sessions"].as_array().unwrap().len(),
            DEFAULT_SESSION_LIMIT
        );
        assert_eq!(document["data"]["omitted_count"], 1);
    }

    #[test]
    fn coverage_surfaces_bounded_limitations_with_source_and_lens_impact() {
        let mut data = data_with_join(Vec::new());
        data.records = vec![
            record_with_timestamp("session", Some("2026-01-01T00:00:00Z")),
            record_with_timestamp("session", Some("not-a-timestamp")),
        ];
        data.turns.push(Turn {
            id: "turn".to_owned(),
            session_id: Some("session".to_owned()),
            started_at: None,
            completed_at: Some("2026-01-01T00:00:01Z".to_owned()),
            cwd: None,
            model: None,
            reasoning_effort: None,
            sequence: 1,
            lifecycle: vec![TurnLifecycleEvent {
                kind: "turn_started".to_owned(),
                timestamp: None,
                sequence: 1,
                provenance: source(3),
            }],
            provenance: source(2),
        });
        data.diagnostics = vec![
            CanonicalDiagnostic {
                kind: DiagnosticKind::OversizedLine,
                source: source(4),
                message: "synthetic oversized line".to_owned(),
            },
            CanonicalDiagnostic {
                kind: DiagnosticKind::Unreadable,
                source: SourceRef::rollout("unreadable.jsonl".into(), 1),
                message: "synthetic unreadable source".to_owned(),
            },
            CanonicalDiagnostic {
                kind: DiagnosticKind::MetadataConflict,
                source: SourceRef::state("state.sqlite".into()),
                message: "synthetic metadata conflict".to_owned(),
            },
        ];

        let coverage = report_coverage(&data);
        assert_eq!(coverage.status, "partial");
        let kinds = coverage
            .limitations
            .iter()
            .map(|limitation| limitation.kind.as_str())
            .collect::<BTreeSet<_>>();
        for kind in [
            "missing_lifecycle_timestamp",
            "invalid_timestamp",
            "oversized_line",
            "unreadable",
            "metadata_conflict",
        ] {
            assert!(kinds.contains(kind), "missing limitation kind {kind}");
        }
        let invalid = coverage
            .limitations
            .iter()
            .find(|limitation| limitation.kind == "invalid_timestamp")
            .unwrap();
        assert_eq!(invalid.source.line, Some(1));
        assert_eq!(invalid.selected_sessions, 1);
        assert_eq!(invalid.selected_records, 2);
        assert!(invalid.affected_lenses.iter().any(|lens| lens == "rework"));
        assert!(
            render_report_metadata(&coverage, &StoreFreshness::recorded(1, None))
                .contains("Limitations:\n")
        );

        let document: serde_json::Value = serde_json::from_str(
            &render_json_sessions(&data, &StoreFreshness::recorded(1, None)).unwrap(),
        )
        .unwrap();
        let limitations = document["data"]["coverage"]["limitations"]
            .as_array()
            .unwrap();
        assert!(limitations.iter().any(|limitation| {
            limitation["kind"] == "oversized_line"
                && limitation["source"]["line"] == 4
                && limitation["selected_sessions"] == 1
                && limitation["selected_records"] == 2
        }));
    }

    #[test]
    fn complete_and_empty_coverage_have_no_limitations() {
        let complete = report_coverage(&CanonicalData {
            records: vec![record_with_timestamp(
                "session",
                Some("2026-01-01T00:00:00Z"),
            )],
            ..CanonicalData::default()
        });
        assert_eq!(complete.status, "observed");
        assert!(complete.limitations.is_empty());

        let empty = report_coverage(&CanonicalData::default());
        assert_eq!(empty.status, "empty");
        assert!(empty.limitations.is_empty());
    }

    #[test]
    fn coverage_orders_timestamps_by_full_precision_and_offset() {
        let data = CanonicalData {
            records: vec![
                record_with_timestamp("session", Some("2026-01-01T00:00:00.900Z")),
                record_with_timestamp("session", Some("2026-01-01T01:00:00.100+01:00")),
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
                records: vec![record_with_timestamp("session", Some(timestamp))],
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
                record_with_timestamp("session", Some("2026-01-01T00:00:00.100Z")),
                record_with_timestamp("session", Some("2026-01-01T01:00:00.100+01:00")),
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
    fn doctor_period_fields_match_valid_activity_coverage() {
        let data = CanonicalData {
            sessions: vec![Session {
                id: "session".to_owned(),
                created_at: Some("2026-01-03T09:00:00+09:00".to_owned()),
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
            records: vec![
                record_with_timestamp("session", Some("2026-01-03T00:30:00.123456789Z")),
                record_with_timestamp("session", Some("not-a-timestamp")),
            ],
            ..CanonicalData::default()
        };

        let report = doctor(
            &data,
            &[],
            StoreFreshness::recorded(1, Some("2099-01-01T00:00:00Z".to_owned())),
            &DoctorOptions::default(),
        );

        let coverage = report_coverage(&data);
        assert_eq!(
            coverage.activity_start.as_deref(),
            Some("2026-01-03T09:00:00+09:00")
        );
        assert_eq!(
            coverage.activity_end.as_deref(),
            Some("2026-01-03T00:30:00.123456789Z")
        );
        assert_eq!(report.period_start, coverage.activity_start);
        assert_eq!(report.period_end, coverage.activity_end);
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
