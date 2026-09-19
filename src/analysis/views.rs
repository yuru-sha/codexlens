//! Typed, deterministic product views over canonical data.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{
    CanonicalData, InstructionFileState, InstructionScope, InstructionSnapshotAccuracy,
    InstructionSnapshotSource, SourceRef, Surface, SurfaceKind, SurfaceLoadMode, SurfaceScope,
    SurfaceUsageState,
};

use super::{
    AnalysisOptions, EvidenceRef, EvidenceRole, Finding, FindingConfidence, FindingScope,
    FindingSeverity, FindingType, analyze_failures, analyze_rework, bounded_excerpt, evidence_for,
    majority_project, majority_scope, normalize_fact, push_evidence, redact_sensitive,
};

pub const MAX_VIEW_EVIDENCE: usize = 3;
pub const MAX_VIEW_LIMITATIONS: usize = 3;
pub const HEAVY_STARTUP_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ViewOpportunity {
    pub id: String,
    pub title: String,
    pub scope: FindingScope,
    pub owner: String,
    pub target: String,
    pub impact: String,
    pub severity: FindingSeverity,
    pub confidence: FindingConfidence,
    pub occurrences: usize,
    pub distinct_sessions: usize,
    pub action: String,
    pub follow_up: String,
    pub evidence: Vec<EvidenceRef>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryReport {
    pub measure: String,
    pub rows: Vec<InventoryRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InventoryRow {
    pub id: String,
    pub scope: FindingScope,
    pub owner: String,
    pub kind: SurfaceKind,
    pub name: String,
    pub path: Option<PathBuf>,
    pub load_mode: SurfaceLoadMode,
    pub static_bytes: Option<usize>,
    pub startup_bytes: Option<usize>,
    pub usage_state: SurfaceUsageState,
    pub observed_uses: usize,
    pub observed_sessions: usize,
    pub action: Option<String>,
    pub evidence: Vec<EvidenceRef>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverheadReport {
    pub measure: String,
    pub rows: Vec<OverheadRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OverheadRow {
    pub scope: FindingScope,
    pub project: Option<PathBuf>,
    pub session_count: usize,
    pub observed_min_startup_bytes: Option<usize>,
    pub readable_startup_bytes: Option<usize>,
    pub residual_bytes: Option<usize>,
    pub unknown_cost: bool,
    pub evidence: Vec<EvidenceRef>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum UsageKind {
    Tool,
    Skill,
    Model,
    Surface,
    Prompt,
    Subagent,
}

impl UsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Skill => "skill",
            Self::Model => "model",
            Self::Surface => "surface",
            Self::Prompt => "prompt",
            Self::Subagent => "subagent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCoverage {
    pub total_sessions: usize,
    pub observed_sessions: usize,
    pub known_events: usize,
    pub total_events: usize,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub measure: String,
    pub coverage: UsageCoverage,
    pub rows: Vec<UsageRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageRow {
    pub kind: UsageKind,
    pub name: String,
    pub scope: FindingScope,
    pub usage_state: Option<SurfaceUsageState>,
    pub occurrences: usize,
    pub distinct_sessions: usize,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub duration_ms: u64,
    pub duration_observations: usize,
    pub coverage: UsageCoverage,
    pub evidence: Vec<EvidenceRef>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PromptClass {
    Steer,
    Correct,
    Question,
    Instruct,
}

impl PromptClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Correct => "correct",
            Self::Question => "question",
            Self::Instruct => "instruct",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptReport {
    pub measure: String,
    pub rows: Vec<PromptRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PromptRow {
    pub class: PromptClass,
    pub scope: FindingScope,
    pub occurrences: usize,
    pub distinct_sessions: usize,
    pub verdict: String,
    pub evidence: Vec<EvidenceRef>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureReport {
    pub measure: String,
    pub rows: Vec<FailureRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FailureRow {
    pub category: String,
    pub tool: String,
    pub command_family: String,
    pub opportunity: ViewOpportunity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StuckReport {
    pub measure: String,
    pub rows: Vec<StuckRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StuckRow {
    pub path: String,
    pub session_id: Option<String>,
    pub sequence: Vec<String>,
    pub observed_commands: Vec<String>,
    pub opportunity: ViewOpportunity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WasteReport {
    pub measure: String,
    pub opportunities: Vec<ViewOpportunity>,
}

/// Combine typed waste opportunities with the other actionable findings used
/// by the doctor view, keeping one bounded opportunity per routed target.
pub fn doctor_opportunities(
    data: &CanonicalData,
    findings: &[Finding],
    waste: &WasteReport,
    overhead: &OverheadReport,
) -> Vec<ViewOpportunity> {
    let mut by_target = BTreeMap::<(String, String), ViewOpportunity>::new();
    let waste_keys = waste
        .opportunities
        .iter()
        .map(doctor_opportunity_key)
        .collect::<BTreeSet<_>>();
    for opportunity in &waste.opportunities {
        merge_doctor_opportunity(&mut by_target, opportunity.clone());
    }
    for row in &overhead.rows {
        let Some(opportunity) = overhead_opportunity(data, row) else {
            continue;
        };
        merge_doctor_opportunity(&mut by_target, opportunity);
    }
    for finding in findings {
        if finding.kind == FindingType::Failure {
            let (_, _, category) = failure_key_parts(&finding.key);
            if transient_failure_category(&category) {
                continue;
            }
        }
        if finding.kind == FindingType::Gap
            && gap_base_id(finding)
                .is_some_and(|id| waste_keys.contains(&(id, finding.scope.to_string())))
        {
            continue;
        }
        let Some(opportunity) = opportunity_from_finding(data, finding) else {
            continue;
        };
        if waste_keys.contains(&doctor_opportunity_key(&opportunity)) {
            continue;
        }
        merge_doctor_opportunity(&mut by_target, opportunity);
    }
    let mut opportunities = by_target.into_values().collect::<Vec<_>>();
    opportunities.sort_by(opportunity_order);
    opportunities
}

fn overhead_opportunity(data: &CanonicalData, row: &OverheadRow) -> Option<ViewOpportunity> {
    let readable = row
        .readable_startup_bytes
        .filter(|bytes| *bytes > HEAVY_STARTUP_BYTES)?;
    if row.unknown_cost || row.evidence.is_empty() {
        return None;
    }
    let exact_target = overhead_target(data, &row.scope, row.session_count);
    let target = exact_target
        .clone()
        .unwrap_or_else(|| scope_target(&row.scope));
    let confidence = if exact_target.is_some() {
        FindingConfidence::High
    } else {
        FindingConfidence::Medium
    };
    Some(ViewOpportunity {
        id: format!("overhead:{}", row.scope),
        title: format!("Reduce startup context overhead for {}", row.scope),
        scope: row.scope.clone(),
        owner: owner_for_scope(&row.scope),
        target: target.clone(),
        impact: format!(
            "{readable} bytes of user-controlled always-on configuration are included across {} session(s)",
            row.session_count
        ),
        severity: if readable >= HEAVY_STARTUP_BYTES.saturating_mul(2) {
            FindingSeverity::High
        } else {
            FindingSeverity::Medium
        },
        confidence,
        occurrences: row.session_count,
        distinct_sessions: row.session_count,
        action: format!("Review and slim always-on configuration at {target}"),
        follow_up: follow_up_for("overhead", &row.scope),
        evidence: bounded_evidence(row.evidence.clone()),
        limitations: {
            let mut limitations = row.limitations.clone();
            if exact_target.is_none() {
                limitations.push(
                    "An exact instruction target was not selected because scope evidence was missing or ambiguous; optimize will report this as skipped"
                        .to_owned(),
                );
            }
            ensure_limitations(&mut limitations);
            limitations
        },
    })
}

fn gap_base_id(finding: &Finding) -> Option<String> {
    let (kind, key) = finding.key.split_once('|')?;
    Some(format!("{kind}:{key}"))
}

fn opportunity_from_finding(data: &CanonicalData, finding: &Finding) -> Option<ViewOpportunity> {
    if finding.evidence.is_empty() || finding.suggested_action.trim().is_empty() {
        return None;
    }
    Some(finding_opportunity(
        data,
        finding,
        finding_target(data, finding),
        finding.suggested_action.clone(),
    ))
}

fn doctor_opportunity_key(opportunity: &ViewOpportunity) -> (String, String) {
    (opportunity.id.clone(), opportunity.scope.to_string())
}

fn merge_doctor_opportunity(
    by_target: &mut BTreeMap<(String, String), ViewOpportunity>,
    opportunity: ViewOpportunity,
) {
    let key = doctor_opportunity_key(&opportunity);
    let Some(existing) = by_target.get_mut(&key) else {
        by_target.insert(key, opportunity);
        return;
    };
    existing.occurrences = existing.occurrences.saturating_add(opportunity.occurrences);
    existing.distinct_sessions = existing
        .distinct_sessions
        .saturating_add(opportunity.distinct_sessions);
    for evidence in opportunity.evidence {
        add_evidence(&mut existing.evidence, evidence);
    }
    existing.limitations.extend(opportunity.limitations);
    ensure_limitations(&mut existing.limitations);
}

pub fn inventory(data: &CanonicalData) -> InventoryReport {
    let options = AnalysisOptions::default();
    let heavy_threshold = heavy_startup_threshold(data);
    let mut rows = data
        .surfaces
        .iter()
        .map(|surface| {
            let action = surface_action(surface, heavy_threshold);
            let mut limitations = surface.limitations.clone();
            if surface.usage_state == SurfaceUsageState::Unknown {
                limitations
                    .push("Usage evidence is unavailable; unused was not inferred".to_owned());
            }
            ensure_limitations(&mut limitations);
            let scope = surface_scope(&surface.scope);
            InventoryRow {
                id: surface.id.clone(),
                owner: owner_for_scope(&scope),
                scope,
                kind: surface.kind,
                name: surface.name.clone(),
                path: surface.path.clone(),
                load_mode: surface.load_mode,
                static_bytes: surface.static_bytes,
                startup_bytes: surface.startup_bytes,
                usage_state: surface.usage_state,
                observed_uses: surface.observed_uses,
                observed_sessions: surface.observed_sessions,
                action,
                evidence: surface_evidence(data, surface, &options),
                limitations,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.scope
            .to_string()
            .cmp(&right.scope.to_string())
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    InventoryReport {
        measure: "Configured surfaces joined with observed use, scope, and startup or on-demand estimates".to_owned(),
        rows,
    }
}

pub fn overhead(data: &CanonicalData) -> OverheadReport {
    let options = AnalysisOptions::default();
    let mut global_readable = 0usize;
    let mut global_unknown = false;
    let mut global_evidence = Vec::new();
    let mut project_readable = BTreeMap::<PathBuf, usize>::new();
    let mut project_unknown = BTreeSet::<PathBuf>::new();
    let mut project_evidence = BTreeMap::<PathBuf, Vec<EvidenceRef>>::new();

    for surface in &data.surfaces {
        if !is_always_on(surface) {
            continue;
        }
        let is_unknown = surface.enabled != Some(true) || surface.startup_bytes.is_none();
        let bytes = surface.startup_bytes.unwrap_or(0);
        match &surface.scope {
            SurfaceScope::Global => {
                global_readable = global_readable.saturating_add(bytes);
                global_unknown |= is_unknown;
                extend_evidence(
                    &mut global_evidence,
                    surface_evidence(data, surface, &options),
                );
            }
            SurfaceScope::Project(path) | SurfaceScope::Nested(path) => {
                let path = path.clone();
                *project_readable.entry(path.clone()).or_default() = project_readable
                    .get(&path)
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(bytes);
                if is_unknown {
                    project_unknown.insert(path.clone());
                }
                let evidence = project_evidence.entry(path).or_default();
                extend_evidence(evidence, surface_evidence(data, surface, &options));
            }
        }
    }

    let mut snapshot_min = BTreeMap::<String, usize>::new();
    for snapshot in &data.instruction_snapshots {
        let Some(session_id) = snapshot.session_id.as_ref() else {
            continue;
        };
        if snapshot.source == InstructionSnapshotSource::Unavailable
            || snapshot.accuracy == InstructionSnapshotAccuracy::Unavailable
            || snapshot.truncated
        {
            continue;
        }
        snapshot_min
            .entry(session_id.clone())
            .and_modify(|current| *current = (*current).min(snapshot.byte_count))
            .or_insert(snapshot.byte_count);
    }

    let mut sessions_by_project = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for session in &data.sessions {
        if let Some(project) = session_project(data, &session.id) {
            sessions_by_project
                .entry(project)
                .or_default()
                .insert(session.id.clone());
        }
    }
    for session_id in snapshot_min.keys() {
        if let Some(project) = session_project(data, session_id) {
            sessions_by_project
                .entry(project)
                .or_default()
                .insert(session_id.clone());
        }
    }
    for project in project_readable.keys() {
        sessions_by_project.entry(project.clone()).or_default();
    }

    let mut all_sessions = data
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<BTreeSet<_>>();
    all_sessions.extend(snapshot_min.keys().cloned());
    let mut rows = Vec::new();
    rows.push(build_overhead_row(
        data,
        FindingScope::Global,
        None,
        &all_sessions,
        global_readable,
        global_unknown,
        &global_evidence,
        &snapshot_min,
    ));
    for (project, sessions) in sessions_by_project {
        let mut evidence = global_evidence.clone();
        if let Some(project_evidence) = project_evidence.get(&project) {
            extend_evidence(&mut evidence, project_evidence.clone());
        }
        let readable = global_readable
            .saturating_add(project_readable.get(&project).copied().unwrap_or_default());
        let unknown = global_unknown || project_unknown.contains(&project);
        rows.push(build_overhead_row(
            data,
            FindingScope::Project(project.clone()),
            Some(project),
            &sessions,
            readable,
            unknown,
            &evidence,
            &snapshot_min,
        ));
    }
    OverheadReport {
        measure: "Minimum observed session-start context reconciled with readable always-on surfaces; residual is unattributed cost".to_owned(),
        rows,
    }
}

pub fn usage(data: &CanonicalData) -> UsageReport {
    let options = AnalysisOptions::default();
    let coverage = usage_coverage(data);
    let mut aggregates = BTreeMap::<(UsageKind, String, String), UsageAggregate>::new();
    let mut call_names = HashMap::<(String, String, String), String>::new();
    let mut call_names_by_id = HashMap::<String, String>::new();
    let mut ambiguous_call_ids = BTreeSet::new();

    for call in &data.tool_calls {
        let Some(session_id) = call.session_id.as_ref() else {
            continue;
        };
        let name = canonical_tool_name(call.tool_name.as_deref().unwrap_or("unknown_tool"));
        if let Some(call_id) = call.call_id.as_ref() {
            call_names.insert(
                (
                    call_id.clone(),
                    call.session_id.clone().unwrap_or_default(),
                    call.turn_id.clone().unwrap_or_default(),
                ),
                name.clone(),
            );
            if let Some(previous) = call_names_by_id.get(call_id) {
                if previous != &name {
                    ambiguous_call_ids.insert(call_id.clone());
                }
            } else {
                call_names_by_id.insert(call_id.clone(), name.clone());
            }
        }
        let scope = majority_scope(data, std::iter::once(session_id.as_str()));
        let aggregate = aggregates
            .entry((UsageKind::Tool, name.clone(), scope.to_string()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Tool, name));
        aggregate.scope = Some(scope);
        aggregate.occurrences = aggregate.occurrences.saturating_add(1);
        aggregate.sessions.insert(session_id.clone());
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                Some(session_id.clone()),
                call.provenance.clone(),
                EvidenceRole::Observation,
                call.tool_name.as_deref(),
                &options,
            ),
        );
    }

    for result in &data.tool_results {
        if result.is_duplicate {
            continue;
        }
        let Some(session_id) = result.session_id.as_ref() else {
            continue;
        };
        let (name, matched_call) = result
            .call_id
            .as_ref()
            .and_then(|call_id| {
                call_names
                    .get(&(
                        call_id.clone(),
                        result.session_id.clone().unwrap_or_default(),
                        result.turn_id.clone().unwrap_or_default(),
                    ))
                    .or_else(|| {
                        (!ambiguous_call_ids.contains(call_id))
                            .then(|| call_names_by_id.get(call_id))
                            .flatten()
                    })
            })
            .map_or_else(
                || ("unknown_tool".to_owned(), false),
                |name| (name.clone(), true),
            );
        let scope = majority_scope(data, std::iter::once(session_id.as_str()));
        let aggregate = aggregates
            .entry((UsageKind::Tool, name.clone(), scope.to_string()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Tool, name));
        aggregate.scope = Some(scope);
        if !matched_call {
            aggregate.occurrences = aggregate.occurrences.saturating_add(1);
        }
        aggregate.sessions.insert(session_id.clone());
        if let Some(duration) = result.duration_ms.filter(|duration| *duration >= 0) {
            aggregate.duration_ms = aggregate
                .duration_ms
                .saturating_add(u64::try_from(duration).unwrap_or_default());
            aggregate.duration_observations = aggregate.duration_observations.saturating_add(1);
        }
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                Some(session_id.clone()),
                result.provenance.clone(),
                EvidenceRole::Observation,
                result.command.as_deref(),
                &options,
            ),
        );
    }

    let mut token_seen = BTreeSet::new();
    for usage in &data.token_usage {
        let Some(session_id) = usage.session_id.as_ref() else {
            continue;
        };
        let Some(model) = data
            .sessions
            .iter()
            .find(|session| session.id == *session_id)
            .and_then(|session| session.model.clone())
        else {
            continue;
        };
        let key = (
            session_id.clone(),
            usage.turn_id.clone(),
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
        );
        if !token_seen.insert(key) {
            continue;
        }
        let scope = majority_scope(data, std::iter::once(session_id.as_str()));
        let aggregate = aggregates
            .entry((UsageKind::Model, model.clone(), scope.to_string()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Model, model.clone()));
        aggregate.scope = Some(scope);
        aggregate.sessions.insert(session_id.clone());
        aggregate.input_tokens = aggregate
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or_default());
        aggregate.cached_input_tokens = aggregate
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens.unwrap_or_default());
        aggregate.output_tokens = aggregate
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or_default());
        aggregate.reasoning_output_tokens = aggregate
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens.unwrap_or_default());
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                Some(session_id.clone()),
                usage.provenance.clone(),
                EvidenceRole::Observation,
                Some(&format!("model {model}")),
                &options,
            ),
        );
    }
    for session in &data.sessions {
        let Some(model) = session.model.as_deref() else {
            continue;
        };
        let scope = majority_scope(data, std::iter::once(session.id.as_str()));
        let aggregate = aggregates
            .entry((UsageKind::Model, model.to_owned(), scope.to_string()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Model, model.to_owned()));
        aggregate.scope = Some(scope);
        aggregate.occurrences = aggregate.occurrences.saturating_add(1);
        aggregate.sessions.insert(session.id.clone());
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                Some(session.id.clone()),
                session.provenance.clone(),
                EvidenceRole::Observation,
                Some(model),
                &options,
            ),
        );
    }

    for row in prompts(data).rows {
        let scope_key = row.scope.to_string();
        let aggregate = aggregates
            .entry((UsageKind::Prompt, row.class.as_str().to_owned(), scope_key))
            .or_insert_with(|| {
                UsageAggregate::new(UsageKind::Prompt, row.class.as_str().to_owned())
            });
        aggregate.scope = Some(row.scope);
        aggregate.occurrences = aggregate.occurrences.saturating_add(row.occurrences);
        aggregate.reported_sessions = aggregate.reported_sessions.max(row.distinct_sessions);
        extend_evidence(&mut aggregate.evidence, row.evidence);
        aggregate.limitations.extend(row.limitations);
    }

    for session in data
        .sessions
        .iter()
        .filter(|session| session.parent_id.is_some())
    {
        let scope = majority_scope(data, std::iter::once(session.id.as_str()));
        let name = session
            .originator
            .as_deref()
            .or(session.model.as_deref())
            .unwrap_or("unknown_subagent")
            .to_owned();
        let aggregate = aggregates
            .entry((UsageKind::Subagent, name.clone(), scope.to_string()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Subagent, name));
        aggregate.scope = Some(scope);
        aggregate.occurrences = aggregate.occurrences.saturating_add(1);
        aggregate.sessions.insert(session.id.clone());
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                Some(session.id.clone()),
                session.provenance.clone(),
                EvidenceRole::Observation,
                Some("subagent session"),
                &options,
            ),
        );
    }

    let mut subagent_token_seen = BTreeSet::new();
    for usage in &data.token_usage {
        let Some(session_id) = usage.session_id.as_ref() else {
            continue;
        };
        let Some(session) = data
            .sessions
            .iter()
            .find(|session| session.id == *session_id && session.parent_id.is_some())
        else {
            continue;
        };
        let key = (
            session_id.clone(),
            usage.turn_id.clone(),
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
        );
        if !subagent_token_seen.insert(key) {
            continue;
        }
        let scope = majority_scope(data, std::iter::once(session.id.as_str()));
        let name = session
            .originator
            .as_deref()
            .or(session.model.as_deref())
            .unwrap_or("unknown_subagent")
            .to_owned();
        let aggregate = aggregates
            .entry((UsageKind::Subagent, name.clone(), scope.to_string()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Subagent, name));
        aggregate.scope = Some(scope);
        aggregate.input_tokens = aggregate
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or_default());
        aggregate.cached_input_tokens = aggregate
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens.unwrap_or_default());
        aggregate.output_tokens = aggregate
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or_default());
        aggregate.reasoning_output_tokens = aggregate
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens.unwrap_or_default());
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                Some(session_id.clone()),
                usage.provenance.clone(),
                EvidenceRole::Observation,
                Some("subagent token usage"),
                &options,
            ),
        );
    }

    for surface in &data.surfaces {
        let kind = if surface.kind == SurfaceKind::Skill {
            UsageKind::Skill
        } else {
            UsageKind::Surface
        };
        let scope = surface_scope(&surface.scope);
        let scope_key = scope.to_string();
        let aggregate = aggregates
            .entry((kind, surface.name.clone(), scope_key))
            .or_insert_with(|| UsageAggregate::new(kind, surface.name.clone()));
        aggregate.scope = Some(scope);
        aggregate.usage_state = Some(surface.usage_state);
        aggregate.occurrences = aggregate.occurrences.saturating_add(surface.observed_uses);
        aggregate.reported_sessions = aggregate.reported_sessions.max(surface.observed_sessions);
        extend_evidence(
            &mut aggregate.evidence,
            surface_evidence(data, surface, &options),
        );
    }

    let mut rows = aggregates
        .into_values()
        .filter_map(|aggregate| {
            if aggregate.evidence.is_empty() {
                return None;
            }
            let scope = aggregate.scope.unwrap_or_else(|| {
                majority_scope(data, aggregate.sessions.iter().map(String::as_str))
            });
            let mut limitations = aggregate.limitations;
            if aggregate.kind == UsageKind::Surface || aggregate.kind == UsageKind::Skill {
                limitations.push(
                    "Token and duration totals are not attributable to a configured surface"
                        .to_owned(),
                );
            }
            if aggregate.duration_observations == 0 && aggregate.kind == UsageKind::Tool {
                limitations.push("No structured duration was recorded for this tool".to_owned());
            }
            ensure_limitations(&mut limitations);
            Some(UsageRow {
                kind: aggregate.kind,
                name: aggregate.name,
                scope,
                usage_state: aggregate.usage_state,
                occurrences: aggregate.occurrences,
                distinct_sessions: aggregate.sessions.len().max(aggregate.reported_sessions),
                input_tokens: aggregate.input_tokens,
                cached_input_tokens: aggregate.cached_input_tokens,
                output_tokens: aggregate.output_tokens,
                reasoning_output_tokens: aggregate.reasoning_output_tokens,
                duration_ms: aggregate.duration_ms,
                duration_observations: aggregate.duration_observations,
                coverage: coverage.clone(),
                evidence: bounded_evidence(aggregate.evidence),
                limitations,
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then_with(|| right.output_tokens.cmp(&left.output_tokens))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.scope.to_string().cmp(&right.scope.to_string()))
    });
    UsageReport {
        measure: "Observed tool, Skill, model, prompt, subagent, and configured-surface effort ranked by counts, tokens, duration, and coverage".to_owned(),
        coverage,
        rows,
    }
}

pub fn prompts(data: &CanonicalData) -> PromptReport {
    let options = AnalysisOptions::default();
    let mut grouped = BTreeMap::<(PromptClass, String), PromptAggregate>::new();
    for message in &data.messages {
        if !matches!(message.role, Some(crate::model::MessageRole::User)) {
            continue;
        }
        let Some(content) = message
            .content
            .as_deref()
            .filter(|content| !content.trim().is_empty())
        else {
            continue;
        };
        let class = classify_prompt(content);
        let (scope, unknown_attribution) = match message.session_id.as_deref() {
            Some(session_id) if data.sessions.iter().any(|session| session.id == session_id) => {
                (majority_scope(data, std::iter::once(session_id)), false)
            }
            _ => (FindingScope::Path("unknown_prompt_scope".to_owned()), true),
        };
        let aggregate = grouped
            .entry((class, scope.to_string()))
            .or_insert_with(|| PromptAggregate {
                scope: Some(scope),
                ..PromptAggregate::default()
            });
        if unknown_attribution {
            aggregate.limitations.push(
                "Prompt could not be attributed to a known session, global scope, or project"
                    .to_owned(),
            );
        }
        aggregate.occurrences = aggregate.occurrences.saturating_add(1);
        if let Some(session_id) = message.session_id.as_ref() {
            aggregate.sessions.insert(session_id.clone());
        }
        add_evidence(
            &mut aggregate.evidence,
            evidence_for(
                message.session_id.clone(),
                message.provenance.clone(),
                EvidenceRole::Observation,
                Some(content),
                &options,
            ),
        );
    }
    PromptReport {
        measure: "User prompts classified by bounded lexical role into steer, correct, question, and instruct".to_owned(),
        rows: grouped
            .into_iter()
            .map(|((class, _), aggregate)| {
                let mut limitations = aggregate.limitations;
                limitations.push("Classification uses user role, bounded markers, and punctuation; intent is not inferred".to_owned());
                ensure_limitations(&mut limitations);
                PromptRow {
                    class,
                    scope: aggregate.scope.unwrap_or(FindingScope::Global),
                    occurrences: aggregate.occurrences,
                    distinct_sessions: aggregate.sessions.len(),
                    verdict: prompt_verdict(class).to_owned(),
                    evidence: bounded_evidence(aggregate.evidence),
                    limitations,
                }
            })
            .collect(),
    }
}

pub fn failures(data: &CanonicalData) -> FailureReport {
    failures_with_options(data, &AnalysisOptions::default())
}

pub fn failures_with_options(data: &CanonicalData, options: &AnalysisOptions) -> FailureReport {
    let mut rows = analyze_failures(data, options)
        .into_iter()
        .filter_map(|finding| {
            let (tool, command_family, category) = failure_key_parts(&finding.key);
            if transient_failure_category(&category) {
                return None;
            }
            let target = finding_target(data, &finding);
            let action = if finding.observed_commands.is_empty() {
                finding.suggested_action.clone()
            } else {
                format!("Fix the recurring {category} prerequisite for {}", target)
            };
            let opportunity = finding_opportunity(data, &finding, target, action);
            if opportunity.evidence.is_empty() {
                return None;
            }
            Some(FailureRow {
                category,
                tool,
                command_family,
                opportunity,
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| opportunity_order(&left.opportunity, &right.opportunity));
    FailureReport {
        measure: "Recurring non-transient failures grouped by normalized category, command family, and originating tool".to_owned(),
        rows,
    }
}

pub fn stuck(data: &CanonicalData) -> StuckReport {
    stuck_with_options(data, &AnalysisOptions::default())
}

pub fn stuck_with_options(data: &CanonicalData, options: &AnalysisOptions) -> StuckReport {
    let mut rows = analyze_rework(data, options)
        .into_iter()
        .filter(|finding| finding.kind == FindingType::Stuck)
        .filter_map(|finding| {
            let path = finding
                .affected_paths
                .first()
                .cloned()
                .unwrap_or_else(|| finding_target(data, &finding));
            let target = finding_target(data, &finding);
            let opportunity = finding_opportunity(
                data,
                &finding,
                target,
                format!("Fix the failure/edit loop at {path}; verify the next change before repeating it"),
            );
            let row = StuckRow {
                path,
                session_id: finding
                    .evidence
                    .iter()
                    .find_map(|evidence| evidence.session_id.clone()),
                sequence: finding.sequence.clone(),
                observed_commands: finding.observed_commands.clone(),
                opportunity,
            };
            (!row.opportunity.evidence.is_empty()).then_some(row)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| opportunity_order(&left.opportunity, &right.opportunity));
    StuckReport {
        measure:
            "Short-window session and path sequences showing an edit burst or failure/edit loop"
                .to_owned(),
        rows,
    }
}

pub fn waste(data: &CanonicalData) -> WasteReport {
    waste_with_options(data, &AnalysisOptions::default())
}

pub fn waste_with_options(data: &CanonicalData, options: &AnalysisOptions) -> WasteReport {
    let inventory = inventory(data);
    let mut opportunities = Vec::new();
    for row in inventory.rows {
        let Some(action) = row.action else {
            continue;
        };
        if row.evidence.is_empty() {
            continue;
        }
        let target = row
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| row.name.clone());
        let impact = match (row.usage_state, row.startup_bytes) {
            (SurfaceUsageState::Unused, Some(bytes)) => format!(
                "No observed use for a configured {} surface; startup estimate is {bytes} bytes",
                row.kind.as_str()
            ),
            (SurfaceUsageState::Unused, None) => format!(
                "No observed use for a configured {} surface; startup estimate is unknown",
                row.kind.as_str()
            ),
            (_, Some(bytes)) => format!(
                "{bytes} bytes are estimated at startup for a configured {} surface",
                row.kind.as_str()
            ),
            (_, None) => format!(
                "Startup estimate is unknown for a configured {} surface",
                row.kind.as_str()
            ),
        };
        let severity = row
            .startup_bytes
            .filter(|bytes| *bytes >= HEAVY_STARTUP_BYTES.saturating_mul(2))
            .map_or(FindingSeverity::Medium, |_| FindingSeverity::High);
        let confidence = match row.usage_state {
            SurfaceUsageState::Unused | SurfaceUsageState::Used => FindingConfidence::High,
            SurfaceUsageState::Rare => FindingConfidence::Medium,
            SurfaceUsageState::Unknown => FindingConfidence::Low,
        };
        let mut limitations = row.limitations;
        if row.startup_bytes.is_none() {
            limitations.push("startup estimate is unavailable; cost was not quantified".to_owned());
        }
        ensure_limitations(&mut limitations);
        opportunities.push(ViewOpportunity {
            id: format!("surface:{}", row.id),
            title: format!("Actionable configuration surface: {}", row.name),
            owner: owner_for_scope(&row.scope),
            scope: row.scope.clone(),
            target,
            impact,
            severity,
            confidence,
            occurrences: row.observed_uses,
            distinct_sessions: row.observed_sessions,
            action,
            follow_up: follow_up_for_evidence(
                data,
                "inventory",
                &row.scope,
                row.evidence
                    .iter()
                    .filter_map(|evidence| evidence.session_id.as_deref()),
            ),
            evidence: bounded_evidence(row.evidence),
            limitations,
        });
    }
    for row in failures_with_options(data, options).rows {
        opportunities.push(row.opportunity);
    }
    for row in stuck_with_options(data, options).rows {
        opportunities.push(row.opportunity);
    }
    opportunities.sort_by(opportunity_order);
    WasteReport {
        measure: "Ranked actionable union of unused or heavy surfaces, recurring failures, and stuck sequences".to_owned(),
        opportunities,
    }
}

#[derive(Debug, Default)]
struct PromptAggregate {
    scope: Option<FindingScope>,
    occurrences: usize,
    sessions: BTreeSet<String>,
    evidence: Vec<EvidenceRef>,
    limitations: Vec<String>,
}

#[derive(Debug)]
struct UsageAggregate {
    kind: UsageKind,
    name: String,
    scope: Option<FindingScope>,
    usage_state: Option<SurfaceUsageState>,
    occurrences: usize,
    reported_sessions: usize,
    sessions: BTreeSet<String>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    duration_ms: u64,
    duration_observations: usize,
    evidence: Vec<EvidenceRef>,
    limitations: Vec<String>,
}

impl UsageAggregate {
    fn new(kind: UsageKind, name: String) -> Self {
        Self {
            kind,
            name,
            scope: None,
            usage_state: None,
            occurrences: 0,
            reported_sessions: 0,
            sessions: BTreeSet::new(),
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            duration_ms: 0,
            duration_observations: 0,
            evidence: Vec::new(),
            limitations: Vec::new(),
        }
    }
}

fn surface_scope(scope: &SurfaceScope) -> FindingScope {
    match scope {
        SurfaceScope::Global => FindingScope::Global,
        SurfaceScope::Project(path) => FindingScope::Project(path.clone()),
        SurfaceScope::Nested(path) => FindingScope::Instruction(path.clone()),
    }
}

fn surface_action(surface: &Surface, heavy_threshold: usize) -> Option<String> {
    if surface.enabled != Some(true) || surface.usage_state == SurfaceUsageState::Unknown {
        return None;
    }
    let target = surface_target(surface);
    if surface.usage_state == SurfaceUsageState::Unused {
        return Some(format!("Remove {target} if it is no longer needed"));
    }
    if is_heavy(surface, heavy_threshold) {
        return Some(format!("Slim {target} or re-scope it away from startup"));
    }
    None
}

fn is_heavy(surface: &Surface, threshold: usize) -> bool {
    surface.load_mode == SurfaceLoadMode::StartupFull
        && surface.startup_bytes.is_some_and(|bytes| bytes > threshold)
}

fn heavy_startup_threshold(data: &CanonicalData) -> usize {
    let mut estimates = data
        .surfaces
        .iter()
        .filter(|surface| surface.load_mode == SurfaceLoadMode::StartupFull)
        .filter_map(|surface| surface.startup_bytes)
        .collect::<Vec<_>>();
    if estimates.is_empty() {
        return HEAVY_STARTUP_BYTES;
    }
    estimates.sort_unstable();
    let index = (estimates.len() * 9).div_ceil(10).saturating_sub(1);
    estimates[index].max(HEAVY_STARTUP_BYTES)
}

fn is_always_on(surface: &Surface) -> bool {
    matches!(
        surface.load_mode,
        SurfaceLoadMode::StartupFull | SurfaceLoadMode::StartupDescription
    )
}

fn surface_target(surface: &Surface) -> String {
    let Some(path) = surface.path.as_ref() else {
        return bounded_excerpt(&surface.name, 512);
    };
    let target = if matches!(
        surface.kind,
        SurfaceKind::Config | SurfaceKind::McpServer | SurfaceKind::Plugin | SurfaceKind::Hook
    ) {
        format!(
            "{} {} declaration in {}",
            surface.kind.as_str(),
            surface.name,
            path.display()
        )
    } else {
        path.to_string_lossy().into_owned()
    };
    bounded_excerpt(&target, 512)
}

fn surface_evidence(
    data: &CanonicalData,
    surface: &Surface,
    options: &AnalysisOptions,
) -> Vec<EvidenceRef> {
    let mut evidence = Vec::new();
    if let Some(path) = surface.path.as_ref() {
        add_evidence(
            &mut evidence,
            evidence_for(
                None,
                SourceRef::state(path.clone()),
                EvidenceRole::InstructionFile,
                Some(&surface.name),
                options,
            ),
        );
    }
    for join in &data.instruction_joins {
        let matches = surface.kind == SurfaceKind::Instruction
            && surface.path.as_ref().is_some_and(|path| {
                join.resolution.chain.iter().any(|file| {
                    crate::model::normalize_path(&file.path.to_string_lossy())
                        == crate::model::normalize_path(&path.to_string_lossy())
                })
            });
        if matches {
            add_evidence(
                &mut evidence,
                evidence_for(
                    Some(join.session_id.clone()),
                    join.provenance.clone(),
                    EvidenceRole::InstructionSnapshot,
                    Some(&surface.name),
                    options,
                ),
            );
        }
    }
    for call in &data.tool_calls {
        if surface_name_matches(surface.kind, call.tool_name.as_deref(), &surface.name) {
            add_evidence(
                &mut evidence,
                evidence_for(
                    call.session_id.clone(),
                    call.provenance.clone(),
                    EvidenceRole::Observation,
                    call.tool_name.as_deref(),
                    options,
                ),
            );
        }
    }
    bounded_evidence(evidence)
}

fn surface_name_matches(kind: SurfaceKind, tool: Option<&str>, name: &str) -> bool {
    let Some(tool) = tool.map(str::trim).filter(|tool| !tool.is_empty()) else {
        return false;
    };
    crate::config::tool_matches_surface(kind, tool, name)
}

fn extend_evidence(target: &mut Vec<EvidenceRef>, values: Vec<EvidenceRef>) {
    for value in values {
        add_evidence(target, value);
    }
}

fn add_evidence(target: &mut Vec<EvidenceRef>, evidence: EvidenceRef) {
    push_evidence(target, evidence);
    target.truncate(MAX_VIEW_EVIDENCE);
}

fn bounded_evidence(mut evidence: Vec<EvidenceRef>) -> Vec<EvidenceRef> {
    evidence.truncate(MAX_VIEW_EVIDENCE);
    evidence
}

fn ensure_limitations(limitations: &mut Vec<String>) {
    let mut unique = Vec::new();
    let mut seen = BTreeSet::new();
    let mut omitted = 0usize;
    for limitation in limitations.drain(..) {
        let limitation = bounded_excerpt(&limitation, 512);
        if !seen.insert(limitation.clone()) {
            omitted = omitted.saturating_add(1);
        } else if unique.len() < MAX_VIEW_LIMITATIONS.saturating_sub(1) {
            unique.push(limitation);
        } else {
            omitted = omitted.saturating_add(1);
        }
    }
    if unique.is_empty() {
        unique.push("Result is based on the selected canonical evidence".to_owned());
    }
    if omitted > 0 {
        unique.push(format!("{omitted} additional limitation(s) omitted"));
    }
    *limitations = unique;
}

#[allow(clippy::too_many_arguments)]
fn build_overhead_row(
    data: &CanonicalData,
    scope: FindingScope,
    project: Option<PathBuf>,
    sessions: &BTreeSet<String>,
    readable: usize,
    unknown_surface: bool,
    evidence: &[EvidenceRef],
    snapshot_min: &BTreeMap<String, usize>,
) -> OverheadRow {
    let mut observed = None;
    let mut unknown_snapshot = false;
    for session_id in sessions {
        if let Some(bytes) = snapshot_min.get(session_id) {
            observed = Some(observed.map_or(*bytes, |current: usize| current.min(*bytes)));
        } else if data
            .sessions
            .iter()
            .any(|session| session.id == *session_id)
        {
            unknown_snapshot = true;
        }
    }
    let unknown_cost = unknown_surface || unknown_snapshot || observed.is_none();
    let residual = (!unknown_cost).then(|| observed.unwrap_or_default().saturating_sub(readable));
    let options = AnalysisOptions::default();
    let mut row_evidence = evidence.to_vec();
    for snapshot in &data.instruction_snapshots {
        if snapshot
            .session_id
            .as_ref()
            .is_some_and(|session_id| sessions.contains(session_id))
            && snapshot.source != InstructionSnapshotSource::Unavailable
            && snapshot.accuracy != InstructionSnapshotAccuracy::Unavailable
            && !snapshot.truncated
        {
            add_evidence(
                &mut row_evidence,
                evidence_for(
                    snapshot.session_id.clone(),
                    snapshot.provenance.clone(),
                    EvidenceRole::InstructionSnapshot,
                    Some(&format!(
                        "session-start context: {} bytes",
                        snapshot.byte_count
                    )),
                    &options,
                ),
            );
        }
    }
    let mut limitations = Vec::new();
    if unknown_snapshot || observed.is_none() {
        limitations.push("A usable session-start context snapshot was unavailable for at least one selected session".to_owned());
    }
    if unknown_surface {
        limitations.push(
            "At least one always-on surface had an unknown readable startup estimate".to_owned(),
        );
    }
    if observed.is_some_and(|observed| observed < readable) && !unknown_surface {
        limitations.push("Readable startup estimate exceeds the observed minimum; the sources may cover different instruction chains".to_owned());
    }
    ensure_limitations(&mut limitations);
    OverheadRow {
        scope,
        project,
        session_count: sessions.len(),
        observed_min_startup_bytes: observed,
        readable_startup_bytes: Some(readable),
        residual_bytes: residual,
        unknown_cost,
        evidence: bounded_evidence(row_evidence),
        limitations,
    }
}

fn session_project(data: &CanonicalData, session_id: &str) -> Option<PathBuf> {
    data.sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.project.as_deref().or(session.cwd.as_deref()))
        .filter(|project| !project.is_empty())
        .map(|project| PathBuf::from(crate::model::normalize_path(project)))
}

fn usage_coverage(data: &CanonicalData) -> UsageCoverage {
    let timestamped_sources = data
        .records
        .iter()
        .filter(|record| {
            record
                .timestamp
                .as_deref()
                .and_then(super::Timestamp::parse)
                .is_some()
        })
        .map(|record| (record.provenance.path.clone(), record.provenance.line))
        .collect::<BTreeSet<_>>();
    let mut all_session_ids = data
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<BTreeSet<_>>();
    let mut observed_sessions = BTreeSet::new();
    let total_events = data.messages.len()
        + data.tool_calls.len()
        + data
            .tool_results
            .iter()
            .filter(|result| !result.is_duplicate)
            .count()
        + data.token_usage.len();
    let message_sessions = data
        .messages
        .iter()
        .filter_map(|message| message.session_id.clone());
    let call_sessions = data
        .tool_calls
        .iter()
        .filter_map(|call| call.session_id.clone());
    let result_sessions = data
        .tool_results
        .iter()
        .filter(|result| !result.is_duplicate)
        .filter_map(|result| result.session_id.clone());
    let usage_sessions = data
        .token_usage
        .iter()
        .filter_map(|usage| usage.session_id.clone());
    observed_sessions.extend(message_sessions.clone());
    observed_sessions.extend(call_sessions.clone());
    observed_sessions.extend(result_sessions.clone());
    observed_sessions.extend(usage_sessions.clone());
    all_session_ids.extend(message_sessions);
    all_session_ids.extend(call_sessions);
    all_session_ids.extend(result_sessions);
    all_session_ids.extend(usage_sessions);

    let known_events = data
        .messages
        .iter()
        .filter(|message| {
            event_timestamp_is_known(
                message.timestamp.as_deref(),
                &timestamped_sources,
                &message.provenance,
            )
        })
        .count()
        + data
            .tool_calls
            .iter()
            .filter(|call| source_is_timestamped(&timestamped_sources, &call.provenance))
            .count()
        + data
            .tool_results
            .iter()
            .filter(|result| {
                !result.is_duplicate
                    && source_is_timestamped(&timestamped_sources, &result.provenance)
            })
            .count()
        + data
            .token_usage
            .iter()
            .filter(|usage| {
                event_timestamp_is_known(
                    usage.timestamp.as_deref(),
                    &timestamped_sources,
                    &usage.provenance,
                )
            })
            .count();
    let status = if total_events == 0 {
        "unknown"
    } else if known_events < total_events || !data.diagnostics.is_empty() {
        "partial"
    } else {
        "observed"
    };
    UsageCoverage {
        total_sessions: all_session_ids.len(),
        observed_sessions: observed_sessions.len(),
        known_events,
        total_events,
        status: status.to_owned(),
    }
}

fn source_is_timestamped(
    timestamped_sources: &BTreeSet<(PathBuf, Option<usize>)>,
    source: &SourceRef,
) -> bool {
    timestamped_sources.contains(&(source.path.clone(), source.line))
}

fn event_timestamp_is_known(
    timestamp: Option<&str>,
    timestamped_sources: &BTreeSet<(PathBuf, Option<usize>)>,
    source: &SourceRef,
) -> bool {
    match timestamp {
        Some(timestamp) => super::Timestamp::parse(timestamp).is_some(),
        None => source_is_timestamped(timestamped_sources, source),
    }
}

fn canonical_tool_name(name: &str) -> String {
    let normalized = name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" => "unknown_tool".to_owned(),
        _ => bounded_excerpt(&normalized, 128),
    }
}

fn classify_prompt(content: &str) -> PromptClass {
    let normalized = normalize_fact(&redact_sensitive(content));
    if content.trim_end().ends_with('?') || question_prefix(&normalized) {
        return PromptClass::Question;
    }
    if normalized.starts_with("please use ")
        || (normalized.starts_with("use ") && normalized.ends_with(" instead"))
        || normalized.starts_with("do not ")
        || normalized.starts_with("don't ")
        || normalized.starts_with("never ")
    {
        return PromptClass::Correct;
    }
    if [
        "this project uses ",
        "this repo uses ",
        "this repository uses ",
        "the project uses ",
        "this project requires ",
        "the project requires ",
        "remember that ",
        "note that ",
        "prefer ",
        "keep ",
        "avoid ",
    ]
    .iter()
    .any(|marker| normalized.starts_with(marker))
    {
        return PromptClass::Steer;
    }
    PromptClass::Instruct
}

fn question_prefix(text: &str) -> bool {
    matches!(
        text.split_whitespace().next().unwrap_or_default(),
        "can" | "could" | "would" | "what" | "why" | "how" | "where" | "when" | "which" | "should"
    )
}

fn prompt_verdict(class: PromptClass) -> &'static str {
    match class {
        PromptClass::Steer => "Repeated steering is a candidate for scoped project guidance",
        PromptClass::Correct => {
            "Repeated corrections indicate a rule or prerequisite is not being applied"
        }
        PromptClass::Question => {
            "Repeated questions indicate a discoverability gap, not configuration waste by themselves"
        }
        PromptClass::Instruct => {
            "Direct task instructions describe requested work and are not treated as waste"
        }
    }
}

fn failure_key_parts(key: &str) -> (String, String, String) {
    let mut parts = key.splitn(3, '|');
    (
        parts.next().unwrap_or("unknown_tool").to_owned(),
        parts.next().unwrap_or("unknown_command").to_owned(),
        parts.next().unwrap_or("unknown_error").to_owned(),
    )
}

fn transient_failure_category(category: &str) -> bool {
    matches!(
        category,
        "cancelled"
            | "canceled"
            | "timeout"
            | "timed_out"
            | "aborted"
            | "interrupted"
            | "exit_code_124"
            | "exit_code_130"
            | "exit_code_143"
    )
}

fn finding_target(data: &CanonicalData, finding: &Finding) -> String {
    if let Some(recommendation) = crate::advisor::recommend_scope(data, finding) {
        return recommendation.target_path.to_string_lossy().into_owned();
    }
    if let Some(path) = finding.affected_paths.first() {
        return path.clone();
    }
    scope_target(&finding.scope)
}

fn scope_target(scope: &FindingScope) -> String {
    match scope {
        FindingScope::Global => "AGENTS.md".to_owned(),
        FindingScope::Project(path) => path.join("AGENTS.md").to_string_lossy().into_owned(),
        FindingScope::Instruction(path) => path.to_string_lossy().into_owned(),
        FindingScope::Path(path) => path.clone(),
    }
}

fn overhead_target(
    data: &CanonicalData,
    scope: &FindingScope,
    session_count: usize,
) -> Option<String> {
    let target_scope = match scope {
        FindingScope::Global => InstructionScope::Global,
        FindingScope::Project(_) => InstructionScope::ProjectRoot,
        FindingScope::Instruction(path) => return Some(path.to_string_lossy().into_owned()),
        FindingScope::Path(path) => return Some(path.clone()),
    };
    let project = match scope {
        FindingScope::Project(path) => Some(path),
        _ => None,
    };
    let mut paths_by_session = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for join in &data.instruction_joins {
        if project.is_some_and(|project| join.project_root.as_ref() != Some(project)) {
            continue;
        }
        let Some(path) = join
            .resolution
            .chain
            .iter()
            .find(|file| {
                file.scope == target_scope
                    && matches!(
                        file.state,
                        InstructionFileState::Selected | InstructionFileState::Truncated
                    )
                    && file.content.is_some()
            })
            .map(|file| file.path.clone())
        else {
            continue;
        };
        paths_by_session
            .entry(path)
            .or_default()
            .insert(join.session_id.clone());
    }
    let path = paths_by_session
        .into_iter()
        .max_by(|left, right| {
            left.1
                .len()
                .cmp(&right.1.len())
                .then_with(|| right.0.cmp(&left.0))
        })
        .and_then(|(path, sessions)| {
            (session_count > 0 && sessions.len() * 2 > session_count).then_some(path)
        });
    path.map(|path| path.to_string_lossy().into_owned())
}

fn finding_opportunity(
    data: &CanonicalData,
    finding: &Finding,
    target: String,
    action: String,
) -> ViewOpportunity {
    let target = bounded_excerpt(&target, 512);
    let action = bounded_excerpt(&action, 512);
    let mut limitations = finding.limitations.clone();
    ensure_limitations(&mut limitations);
    ViewOpportunity {
        id: format!("{}:{}", finding.kind.as_str(), finding.key),
        title: bounded_excerpt(&finding.summary, 256),
        scope: finding.scope.clone(),
        owner: owner_for_scope(&finding.scope),
        target,
        impact: bounded_excerpt(&finding.summary, 512),
        severity: finding.severity,
        confidence: finding.confidence,
        occurrences: finding.occurrences,
        distinct_sessions: finding.distinct_sessions,
        action,
        follow_up: follow_up_for_finding(data, finding),
        evidence: bounded_evidence(finding.evidence.clone()),
        limitations,
    }
}

fn owner_for_scope(scope: &FindingScope) -> String {
    bounded_excerpt(
        &match scope {
            FindingScope::Global => "global instruction owner".to_owned(),
            FindingScope::Project(path) | FindingScope::Instruction(path) => {
                path.display().to_string()
            }
            FindingScope::Path(path) => path.clone(),
        },
        512,
    )
}

fn follow_up_for(command: &str, scope: &FindingScope) -> String {
    let command = focused_command(command);
    follow_up_for_scope_arg(command, quoted_scope_arg(scope))
}

pub fn follow_up_for_finding(data: &CanonicalData, finding: &Finding) -> String {
    let command = match finding.kind.as_str() {
        "failure" => "failures",
        "stuck" => "stuck",
        "rework" => "rework",
        "correction" => "corrections",
        "gap" => "instructions",
        "verification" => "verification",
        "knowledge" => "knowledge",
        "overscoped" | "duplicate" | "stale" | "truncated" => "instructions",
        _ => "doctor",
    };
    follow_up_for_evidence(
        data,
        command,
        &finding.scope,
        finding
            .evidence
            .iter()
            .filter_map(|evidence| evidence.session_id.as_deref()),
    )
}

fn follow_up_for_evidence<'a, I>(
    data: &CanonicalData,
    command: &str,
    scope: &FindingScope,
    session_ids: I,
) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    if let FindingScope::Instruction(_) = scope {
        let session_ids = session_ids.into_iter().collect::<BTreeSet<_>>();
        if let Some(project) = majority_project(data, session_ids) {
            return follow_up_for(command, &FindingScope::Project(project));
        }
        return follow_up_for_scope_arg(focused_command(command), "projects".to_owned());
    }
    follow_up_for(command, scope)
}

fn follow_up_for_scope_arg(command: &str, scope_arg: String) -> String {
    let follow_up = format!("codexlens {command} --scope {scope_arg}");
    if follow_up.len() <= 512 {
        follow_up
    } else {
        format!("codexlens {command} --scope projects")
    }
}

fn focused_command(command: &str) -> &str {
    match command {
        "failure" => "failures",
        "correction" => "corrections",
        "gap" => "instructions",
        _ => command,
    }
}

fn quoted_scope_arg(scope: &FindingScope) -> String {
    let value = match scope {
        FindingScope::Global => "global".to_owned(),
        FindingScope::Project(path) => format!("project:{}", path.display()),
        FindingScope::Instruction(path) => format!(
            "project:{}",
            path.parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .display()
        ),
        FindingScope::Path(_) => "all".to_owned(),
    };
    shell_quote(&value)
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn opportunity_order(left: &ViewOpportunity, right: &ViewOpportunity) -> std::cmp::Ordering {
    right
        .severity
        .cmp(&left.severity)
        .then_with(|| right.confidence.cmp(&left.confidence))
        .then_with(|| right.distinct_sessions.cmp(&left.distinct_sessions))
        .then_with(|| right.occurrences.cmp(&left.occurrences))
        .then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    use crate::model::{
        CanonicalData, InstructionScope, InstructionSnapshot, InstructionSnapshotAccuracy,
        InstructionSnapshotSource, Message, OutcomeSource, Surface, SurfaceKind, SurfaceLoadMode,
        SurfaceScope, SurfaceUsageState, TokenUsage, ToolCall, ToolOutcome, ToolResult,
    };
    use crate::normalize::normalize_rollout;
    use crate::rollout::{PlainJsonlReader, parse_rollout_reader};

    fn view_fixture() -> CanonicalData {
        let parsed = parse_rollout_reader(
            Path::new("views.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../../tests/fixtures/analysis/views.jsonl"
            ))),
        );
        normalize_rollout(&parsed)
    }

    #[allow(clippy::too_many_arguments)]
    fn surface(
        id: &str,
        kind: SurfaceKind,
        name: &str,
        path: &str,
        scope: SurfaceScope,
        load_mode: SurfaceLoadMode,
        startup_bytes: Option<usize>,
        usage_state: SurfaceUsageState,
        observed_uses: usize,
    ) -> Surface {
        Surface {
            id: id.to_owned(),
            kind,
            name: name.to_owned(),
            path: Some(path.into()),
            scope,
            enabled: Some(true),
            load_mode,
            static_bytes: startup_bytes,
            startup_bytes,
            observed_uses,
            observed_sessions: usize::from(observed_uses > 0),
            usage_state,
            limitations: Vec::new(),
        }
    }

    #[test]
    fn inventory_overhead_and_waste_are_actionable_and_bounded() {
        let mut data = view_fixture();
        data.surfaces = vec![
            surface(
                "unused-rule",
                SurfaceKind::Rule,
                "unused.rules",
                "/fixture/project/unused.rules",
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::StartupFull,
                Some(2048),
                SurfaceUsageState::Unused,
                0,
            ),
            surface(
                "heavy-rule",
                SurfaceKind::Rule,
                "heavy.rules",
                "/fixture/project/heavy.rules",
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::StartupFull,
                Some(8192),
                SurfaceUsageState::Used,
                2,
            ),
            surface(
                "unknown-skill",
                SurfaceKind::Skill,
                "unknown",
                "/fixture/project/unknown/SKILL.md",
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::Unknown,
                None,
                SurfaceUsageState::Unknown,
                0,
            ),
            surface(
                "rare-skill",
                SurfaceKind::Skill,
                "rare",
                "/fixture/project/rare/SKILL.md",
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::OnDemand,
                Some(128),
                SurfaceUsageState::Rare,
                1,
            ),
        ];
        for index in 0..8 {
            let id = format!("baseline-rule-{index}");
            let path = format!("/fixture/project/{id}.rules");
            data.surfaces.push(surface(
                &id,
                SurfaceKind::Rule,
                &id,
                &path,
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::StartupFull,
                Some(0),
                SurfaceUsageState::Used,
                2,
            ));
        }
        data.instruction_snapshots = ["a", "b"]
            .into_iter()
            .enumerate()
            .map(|(index, suffix)| InstructionSnapshot {
                session_id: Some(format!("view-session-{suffix}")),
                turn_id: Some(format!("view-turn-{suffix}")),
                source: InstructionSnapshotSource::Rollout,
                accuracy: InstructionSnapshotAccuracy::Observed,
                content: Some("synthetic context".to_owned()),
                content_hash: None,
                byte_count: 12_000,
                chain: Vec::new(),
                effective_chain_hash: None,
                truncated: false,
                provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 9 + index),
            })
            .collect();

        let inventory = inventory(&data);
        let unused = inventory
            .rows
            .iter()
            .find(|row| row.id == "unused-rule")
            .expect("unused row");
        assert_eq!(unused.usage_state, SurfaceUsageState::Unused);
        assert!(
            unused
                .action
                .as_deref()
                .is_some_and(|action| action.contains("Remove"))
        );
        let heavy = inventory
            .rows
            .iter()
            .find(|row| row.id == "heavy-rule")
            .expect("heavy row");
        assert!(
            heavy
                .action
                .as_deref()
                .is_some_and(|action| action.contains("Slim"))
        );
        assert!(
            inventory
                .rows
                .iter()
                .find(|row| row.id == "unknown-skill")
                .is_some_and(|row| row.action.is_none())
        );

        let overhead = overhead(&data);
        let project = overhead
            .rows
            .iter()
            .find(|row| row.scope == FindingScope::Project("/fixture/project".into()))
            .expect("project overhead");
        assert_eq!(project.observed_min_startup_bytes, Some(12_000));
        assert_eq!(project.readable_startup_bytes, Some(10_240));
        assert_eq!(project.residual_bytes, Some(1_760));

        let waste_report = waste(&data);
        let doctor_report = doctor_opportunities(&data, &[], &waste_report, &overhead);
        let overhead_opportunity = doctor_report
            .iter()
            .find(|opportunity| opportunity.id == "overhead:project:/fixture/project")
            .expect("known user-controlled overhead opportunity");
        assert!(overhead_opportunity.action.contains("Review and slim"));
        assert_eq!(overhead_opportunity.distinct_sessions, 2);
        assert!(waste_report.opportunities.iter().any(|opportunity| {
            opportunity.target == "/fixture/project/unused.rules"
                && opportunity.action.contains("Remove")
        }));
        assert!(waste_report.opportunities.iter().any(|opportunity| {
            opportunity.target == "/fixture/project/heavy.rules"
                && opportunity.action.contains("Slim")
        }));
        assert!(waste_report.opportunities.iter().all(|opportunity| {
            !opportunity.target.is_empty()
                && !opportunity.action.is_empty()
                && !opportunity.evidence.is_empty()
                && opportunity.evidence.len() <= 3
                && !opportunity.limitations.is_empty()
        }));
        let usage_states = usage(&data)
            .rows
            .iter()
            .filter_map(|row| row.usage_state)
            .collect::<Vec<_>>();
        for state in [
            SurfaceUsageState::Unused,
            SurfaceUsageState::Rare,
            SurfaceUsageState::Used,
            SurfaceUsageState::Unknown,
        ] {
            assert!(usage_states.contains(&state));
        }
        assert_eq!(
            serde_json::to_vec(&waste_report).unwrap(),
            serde_json::to_vec(&waste(&data.clone())).unwrap()
        );
        let mut reordered = data.clone();
        reordered.surfaces.reverse();
        assert_eq!(
            serde_json::to_vec(&waste_report).unwrap(),
            serde_json::to_vec(&waste(&reordered)).unwrap()
        );
    }

    #[test]
    fn inventory_heavy_excludes_the_percentile_boundary() {
        let mut data = view_fixture();
        data.surfaces = vec![
            surface(
                "lower-rule",
                SurfaceKind::Rule,
                "lower.rules",
                "/fixture/project/lower.rules",
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::StartupFull,
                Some(2048),
                SurfaceUsageState::Used,
                1,
            ),
            surface(
                "percentile-rule",
                SurfaceKind::Rule,
                "percentile.rules",
                "/fixture/project/percentile.rules",
                SurfaceScope::Project("/fixture/project".into()),
                SurfaceLoadMode::StartupFull,
                Some(8192),
                SurfaceUsageState::Used,
                1,
            ),
        ];

        let row = inventory(&data)
            .rows
            .into_iter()
            .find(|row| row.id == "percentile-rule")
            .expect("percentile row");
        assert!(row.action.is_none());
    }

    #[test]
    fn overhead_opportunity_uses_the_stored_project_instruction_path() {
        let target = PathBuf::from("/fixture/project/project-instructions.md");
        let mut data =
            crate::advisor::test_support::data_with_join(vec![crate::advisor::test_support::file(
                target.to_str().unwrap(),
                InstructionScope::ProjectRoot,
                "project guidance\n",
            )]);
        data.surfaces = vec![surface(
            "heavy-rule",
            SurfaceKind::Rule,
            "heavy.rules",
            "/fixture/project/heavy.rules",
            SurfaceScope::Project("/fixture/project".into()),
            SurfaceLoadMode::StartupFull,
            Some(8_192),
            SurfaceUsageState::Used,
            1,
        )];
        data.instruction_snapshots = vec![InstructionSnapshot {
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            source: InstructionSnapshotSource::Rollout,
            accuracy: InstructionSnapshotAccuracy::Observed,
            content: Some("project guidance\n".to_owned()),
            content_hash: None,
            byte_count: 8_192,
            chain: Vec::new(),
            effective_chain_hash: None,
            truncated: false,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 1),
        }];

        let overhead = overhead(&data);
        let opportunity = doctor_opportunities(&data, &[], &waste(&data), &overhead)
            .into_iter()
            .find(|opportunity| opportunity.id == "overhead:project:/fixture/project")
            .expect("project overhead opportunity");

        assert_eq!(opportunity.target, target.display().to_string());

        let mut ambiguous = data.clone();
        let mut missing_join_session = ambiguous.sessions[0].clone();
        missing_join_session.id = "missing-join-session".to_owned();
        ambiguous.sessions.push(missing_join_session);
        let snapshot = |session_id: &str| InstructionSnapshot {
            session_id: Some(session_id.to_owned()),
            turn_id: Some("turn".to_owned()),
            source: InstructionSnapshotSource::Rollout,
            accuracy: InstructionSnapshotAccuracy::Observed,
            content: Some("project guidance\n".to_owned()),
            content_hash: None,
            byte_count: 8_192,
            chain: Vec::new(),
            effective_chain_hash: None,
            truncated: false,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 1),
        };
        ambiguous.instruction_snapshots =
            vec![snapshot("session"), snapshot("missing-join-session")];
        let ambiguous_overhead = super::overhead(&ambiguous);
        let ambiguous_opportunity =
            doctor_opportunities(&ambiguous, &[], &waste(&ambiguous), &ambiguous_overhead)
                .into_iter()
                .find(|opportunity| opportunity.id == "overhead:project:/fixture/project")
                .expect("ambiguous project overhead opportunity");
        assert_eq!(
            ambiguous_opportunity.target, "/fixture/project/AGENTS.md",
            "a missing join must not establish a strict-majority target"
        );
        assert!(
            ambiguous_opportunity
                .limitations
                .iter()
                .any(|limitation| limitation.contains("exact instruction target"))
        );
        assert_eq!(ambiguous_opportunity.confidence, FindingConfidence::Medium);
    }

    #[test]
    fn surface_matching_uses_canonical_identifier_forms() {
        assert!(surface_name_matches(
            SurfaceKind::Skill,
            Some("skill/deploy"),
            "deploy"
        ));
        for tool in ["docs/search", "docs::search", "mcp__docs__search"] {
            assert!(surface_name_matches(
                SurfaceKind::McpServer,
                Some(tool),
                "docs"
            ));
        }
        assert!(!surface_name_matches(
            SurfaceKind::McpServer,
            Some("other/search"),
            "docs"
        ));
    }

    #[test]
    fn wrapper_tool_names_are_not_merged_into_shell_usage() {
        assert_eq!(canonical_tool_name("exec_command"), "exec_command");
        assert_eq!(canonical_tool_name("exec"), "exec");
        assert_eq!(canonical_tool_name("js"), "js");
        assert_eq!(canonical_tool_name("wait"), "wait");
    }

    #[test]
    fn failure_view_does_not_recommend_shell_prerequisites_for_wrappers() {
        let parsed = parse_rollout_reader(
            Path::new("wrapper-tools.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../../tests/fixtures/rollout/wrapper-tools.jsonl"
            ))),
        );
        let report = failures(&normalize_rollout(&parsed));
        let wrapper_rows = report
            .rows
            .iter()
            .filter(|row| row.command_family == "no_canonical_command")
            .collect::<Vec<_>>();
        assert!(wrapper_rows.is_empty());
    }

    #[test]
    fn waste_preserves_no_canonical_tool_action() {
        let mut data = view_fixture();
        for (index, session_id) in ["view-session-a", "view-session-b"].into_iter().enumerate() {
            data.tool_calls.push(ToolCall {
                id: Some(format!("patch-{index}")),
                call_id: Some(format!("patch-call-{index}")),
                session_id: Some(session_id.to_owned()),
                turn_id: Some(format!("patch-turn-{index}")),
                tool_name: Some("apply_patch".to_owned()),
                input_summary: Some("synthetic patch".to_owned()),
                command: None,
                cwd: Some("/fixture/project".to_owned()),
                status: None,
                provenance: SourceRef::rollout("views.jsonl".into(), 100 + index),
            });
            data.tool_results.push(ToolResult {
                id: Some(format!("patch-result-{index}")),
                call_id: Some(format!("patch-call-{index}")),
                session_id: Some(session_id.to_owned()),
                turn_id: Some(format!("patch-turn-{index}")),
                command: None,
                cwd: Some("/fixture/project".to_owned()),
                stdout: None,
                stderr: Some("synthetic patch failure".to_owned()),
                duration_ms: None,
                exit_code: Some(1),
                status: Some("failed".to_owned()),
                outcome: ToolOutcome::Failed,
                outcome_source: OutcomeSource::ExitCode,
                matched_call: true,
                deduplication_key: None,
                equivalent_to: None,
                is_duplicate: false,
                provenance: SourceRef::rollout("views.jsonl".into(), 110 + index),
            });
        }

        let opportunity = waste(&data)
            .opportunities
            .into_iter()
            .find(|opportunity| {
                opportunity.id == "failure:apply_patch|no_canonical_command|exit_code_1"
            })
            .expect("non-shell failure opportunity");
        assert!(
            opportunity
                .action
                .contains("no shell-command prerequisite was inferred")
        );
        assert!(!opportunity.action.contains("prerequisite at"));
    }

    #[test]
    fn prompts_and_usage_have_distinct_classified_rows() {
        let mut data = view_fixture();
        data.tool_calls = vec![ToolCall {
            id: Some("tool-a".to_owned()),
            call_id: Some("call-a".to_owned()),
            session_id: Some("view-session-a".to_owned()),
            turn_id: Some("view-turn-a".to_owned()),
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: Some("/fixture/project".to_owned()),
            status: None,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 10),
        }];
        data.tool_results = vec![ToolResult {
            id: Some("result-a".to_owned()),
            call_id: Some("call-a".to_owned()),
            session_id: Some("view-session-a".to_owned()),
            turn_id: Some("view-turn-a".to_owned()),
            command: Some("cargo test".to_owned()),
            cwd: Some("/fixture/project".to_owned()),
            stdout: Some("ok".to_owned()),
            stderr: None,
            duration_ms: Some(120),
            exit_code: Some(0),
            status: Some("completed".to_owned()),
            outcome: ToolOutcome::Succeeded,
            outcome_source: OutcomeSource::ExitCode,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 11),
        }];
        data.token_usage = vec![TokenUsage {
            session_id: Some("view-session-a".to_owned()),
            turn_id: Some("view-turn-a".to_owned()),
            timestamp: Some("2026-01-05T00:00:06.000Z".to_owned()),
            input_tokens: Some(100),
            cached_input_tokens: Some(10),
            output_tokens: Some(20),
            reasoning_output_tokens: Some(5),
            sequence: 1,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 12),
        }];

        let prompts = prompts(&data);
        assert_eq!(
            prompts.rows.iter().map(|row| row.class).collect::<Vec<_>>(),
            vec![
                PromptClass::Steer,
                PromptClass::Correct,
                PromptClass::Question,
                PromptClass::Instruct,
            ]
        );
        assert!(prompts.rows.iter().all(|row| row.evidence.len() <= 3));
        assert!(
            prompts
                .rows
                .iter()
                .all(|row| row.scope == FindingScope::Project("/fixture/project".into()))
        );

        let usage = usage(&data);
        let tool = usage
            .rows
            .iter()
            .find(|row| row.kind == UsageKind::Tool && row.name == "exec_command")
            .expect("tool usage");
        assert_eq!(tool.occurrences, 1);
        assert_eq!(tool.duration_ms, 120);
        assert_eq!(tool.scope, FindingScope::Project("/fixture/project".into()));
        let model = usage
            .rows
            .iter()
            .find(|row| row.kind == UsageKind::Model)
            .expect("model usage");
        assert_eq!(model.input_tokens, 100);
        assert!(usage.rows.iter().all(|row| !row.evidence.is_empty()));
    }

    #[test]
    fn waste_does_not_turn_an_unknown_startup_estimate_into_zero() {
        let mut data = view_fixture();
        data.surfaces = vec![surface(
            "unknown-startup",
            SurfaceKind::Rule,
            "unknown.rules",
            "/fixture/project/unknown.rules",
            SurfaceScope::Project("/fixture/project".into()),
            SurfaceLoadMode::PathConditional,
            None,
            SurfaceUsageState::Unused,
            0,
        )];

        let opportunity = waste(&data)
            .opportunities
            .into_iter()
            .find(|opportunity| opportunity.id == "surface:unknown-startup")
            .expect("unknown startup opportunity");
        assert!(opportunity.impact.contains("unknown"));
        assert!(!opportunity.impact.contains("0 bytes"));
        assert!(
            opportunity
                .limitations
                .iter()
                .any(|limitation| limitation.contains("startup estimate"))
        );
    }

    #[test]
    fn prompts_keep_global_and_project_rows_for_the_same_class() {
        let mut data = view_fixture();
        let global_session = data.sessions[0].id.clone();
        let project_session = data.sessions[1].id.clone();
        data.sessions[0].cwd = None;
        data.sessions[0].project = None;
        let mut global_prompt = data.messages[0].clone();
        global_prompt.session_id = Some(global_session);
        let mut project_prompt = Message {
            session_id: Some(project_session.clone()),
            ..global_prompt.clone()
        };
        project_prompt.provenance.line = Some(99);
        data.messages = vec![global_prompt, project_prompt];

        let rows = prompts(&data).rows;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| {
            row.class == PromptClass::Question && row.scope == FindingScope::Global
        }));
        assert!(rows.iter().any(|row| {
            row.class == PromptClass::Question
                && row.scope == FindingScope::Project("/fixture/project".into())
        }));
    }

    #[test]
    fn prompts_keep_unknown_session_scope_explicit() {
        let mut data = view_fixture();
        data.messages = (0..12)
            .map(|index| {
                let mut prompt = data.messages[0].clone();
                prompt.session_id = None;
                prompt.provenance.line = Some(index + 1);
                prompt
            })
            .collect();

        let row = prompts(&data).rows.into_iter().next().expect("prompt row");
        assert_eq!(row.scope, FindingScope::Path("unknown_prompt_scope".into()));
        assert!(row.limitations.len() <= 3);
        assert!(
            row.limitations
                .iter()
                .any(|limitation| limitation.contains("could not be attributed"))
        );
        assert!(
            row.limitations
                .iter()
                .any(|limitation| limitation.contains("omitted"))
        );
    }

    #[test]
    fn follow_up_commands_quote_paths_and_use_focused_commands() {
        let project = FindingScope::Project(PathBuf::from("/fixture/project with space"));
        assert_eq!(
            follow_up_for("failure", &project),
            "codexlens failures --scope 'project:/fixture/project with space'"
        );
        let instruction =
            FindingScope::Instruction(PathBuf::from("/fixture/project with space/AGENTS.md"));
        assert_eq!(
            follow_up_for("gap", &instruction),
            "codexlens instructions --scope 'project:/fixture/project with space'"
        );
    }

    #[test]
    fn finding_follow_up_uses_the_session_project_for_nested_instruction_scope() {
        let data = view_fixture();
        let finding = Finding {
            kind: FindingType::Failure,
            severity: FindingSeverity::Medium,
            confidence: FindingConfidence::High,
            scope: FindingScope::Instruction(PathBuf::from("/fixture/project/nested/AGENTS.md")),
            key: "synthetic-nested-instruction".to_owned(),
            summary: "synthetic nested instruction finding".to_owned(),
            evidence: vec![EvidenceRef {
                session_id: Some("view-session-a".to_owned()),
                source: SourceRef::rollout(PathBuf::from("views.jsonl"), 1),
                role: EvidenceRole::Observation,
                excerpt: None,
            }],
            occurrences: 1,
            distinct_sessions: 1,
            affected_paths: Vec::new(),
            observed_commands: Vec::new(),
            sequence: Vec::new(),
            suggested_action: "synthetic action".to_owned(),
            limitations: Vec::new(),
            verification_status: None,
        };

        assert_eq!(
            follow_up_for_finding(&data, &finding),
            "codexlens failures --scope project:/fixture/project"
        );
    }

    #[test]
    fn doctor_merges_same_actionable_finding_across_sessions() {
        let data = view_fixture();
        let scope = FindingScope::Project(PathBuf::from("/fixture/project"));
        let mut first = crate::advisor::test_support::finding(
            scope.clone(),
            FindingType::Rework,
            Some("src/lib.rs"),
        );
        first.key = "shared-loop".to_owned();
        first.evidence[0].session_id = Some("view-session-a".to_owned());

        let mut second = first.clone();
        second.evidence[0].session_id = Some("view-session-b".to_owned());
        second.evidence[0].source = SourceRef::rollout("views.jsonl".into(), 2);
        second.occurrences = 3;

        let report = doctor_opportunities(
            &data,
            &[first, second],
            &WasteReport {
                measure: "synthetic".to_owned(),
                opportunities: Vec::new(),
            },
            &OverheadReport {
                measure: "synthetic".to_owned(),
                rows: Vec::new(),
            },
        );
        let opportunity = report
            .iter()
            .find(|opportunity| opportunity.id == "rework:shared-loop")
            .expect("merged rework opportunity");
        assert_eq!(opportunity.scope, scope);
        assert_eq!(opportunity.occurrences, 5);
        assert_eq!(opportunity.distinct_sessions, 2);
        assert_eq!(opportunity.evidence.len(), 2);
    }

    #[test]
    fn doctor_keeps_transient_failures_out_of_actionable_findings() {
        let data = view_fixture();
        let mut finding = crate::advisor::test_support::finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Failure,
            Some("src/lib.rs"),
        );
        finding.key = "exec_command|cargo test|timeout".to_owned();

        let report = doctor_opportunities(
            &data,
            &[finding],
            &WasteReport {
                measure: "synthetic".to_owned(),
                opportunities: Vec::new(),
            },
            &OverheadReport {
                measure: "synthetic".to_owned(),
                rows: Vec::new(),
            },
        );
        assert!(report.is_empty());
    }

    #[test]
    fn doctor_keeps_gap_scope_and_opportunity_keys_distinct() {
        let data = view_fixture();
        let mut waste_finding =
            crate::advisor::test_support::finding(FindingScope::Global, FindingType::Failure, None);
        waste_finding.key = "shared".to_owned();
        let waste_opportunity = finding_opportunity(
            &data,
            &waste_finding,
            "AGENTS.md".to_owned(),
            waste_finding.suggested_action.clone(),
        );
        let mut gap = crate::advisor::test_support::finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Gap,
            Some("src/lib.rs"),
        );
        gap.key = "failure|shared".to_owned();
        let waste = WasteReport {
            measure: "synthetic".to_owned(),
            opportunities: vec![waste_opportunity.clone()],
        };
        let empty_overhead = OverheadReport {
            measure: "synthetic".to_owned(),
            rows: Vec::new(),
        };

        let different_scope =
            doctor_opportunities(&data, std::slice::from_ref(&gap), &waste, &empty_overhead);
        assert!(
            different_scope
                .iter()
                .any(|opportunity| opportunity.id == "gap:failure|shared")
        );

        gap.scope = FindingScope::Global;
        let same_scope =
            doctor_opportunities(&data, std::slice::from_ref(&gap), &waste, &empty_overhead);
        assert!(
            !same_scope
                .iter()
                .any(|opportunity| opportunity.id == "gap:failure|shared")
        );

        let mut left = crate::advisor::test_support::finding(
            FindingScope::Path("y|path:z".to_owned()),
            FindingType::Rework,
            Some("src/lib.rs"),
        );
        left.key = "x".to_owned();
        let mut right = crate::advisor::test_support::finding(
            FindingScope::Path("z".to_owned()),
            FindingType::Rework,
            Some("src/lib.rs"),
        );
        right.key = "x|path:y".to_owned();
        let distinct_keys = doctor_opportunities(
            &data,
            &[left, right],
            &WasteReport {
                measure: "synthetic".to_owned(),
                opportunities: Vec::new(),
            },
            &empty_overhead,
        );
        assert_eq!(
            distinct_keys
                .iter()
                .filter(|opportunity| opportunity.id.starts_with("rework:"))
                .count(),
            2
        );
    }

    #[test]
    fn finding_target_prefers_the_evidence_resolved_instruction_file() {
        let data =
            crate::advisor::test_support::data_with_join(vec![crate::advisor::test_support::file(
                "/fixture/project/AGENTS.md",
                InstructionScope::ProjectRoot,
                "Synthetic project guidance.",
            )]);
        let finding = crate::advisor::test_support::finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Failure,
            Some("/fixture/project/src/lib.rs"),
        );

        assert_eq!(
            finding_target(&data, &finding),
            "/fixture/project/AGENTS.md"
        );
    }

    #[test]
    fn usage_reports_prompt_and_subagent_signals() {
        let mut data = view_fixture();
        let parent_id = data.sessions[0].id.clone();
        let mut child = data.sessions[0].clone();
        child.id = "view-subagent".to_owned();
        child.parent_id = Some(parent_id);
        child.originator = Some("synthetic-subagent".to_owned());
        data.sessions.push(child);
        data.token_usage.push(TokenUsage {
            session_id: Some("view-subagent".to_owned()),
            turn_id: Some("view-subagent-turn".to_owned()),
            timestamp: Some("2026-01-05T00:00:06.000Z".to_owned()),
            input_tokens: Some(7),
            cached_input_tokens: Some(2),
            output_tokens: Some(3),
            reasoning_output_tokens: Some(1),
            sequence: 99,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 99),
        });

        let rows = usage(&data).rows;
        let prompts = rows
            .iter()
            .filter(|row| row.kind == UsageKind::Prompt)
            .collect::<Vec<_>>();
        assert!(!prompts.is_empty(), "prompt usage signal is missing");
        assert!(prompts.iter().any(|row| row.name == "question"));
        let subagent = rows.iter().find(|row| row.kind == UsageKind::Subagent);
        assert!(subagent.is_some(), "subagent usage signal is missing");
        assert!(
            subagent
                .is_some_and(|row| { row.name == "synthetic-subagent" && row.output_tokens == 3 })
        );
    }

    #[test]
    fn usage_keeps_the_same_tool_and_model_separate_by_scope() {
        let mut data = view_fixture();
        let global_session = data.sessions[0].id.clone();
        let project_session = data.sessions[1].id.clone();
        data.sessions[0].cwd = None;
        data.sessions[0].project = None;
        data.tool_calls = vec![ToolCall {
            id: Some("scope-tool-global".to_owned()),
            call_id: Some("scope-call-global".to_owned()),
            session_id: Some(global_session.clone()),
            turn_id: Some("view-turn-a".to_owned()),
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: None,
            status: None,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 98),
        }];
        let mut project_call = data.tool_calls[0].clone();
        project_call.session_id = Some(project_session);
        project_call.provenance.line = Some(100);
        data.tool_calls.push(project_call);
        data.tool_calls[0].session_id = Some(global_session);

        let rows = usage(&data)
            .rows
            .into_iter()
            .filter(|row| row.kind == UsageKind::Tool && row.name == "exec_command")
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.scope == FindingScope::Global));
        assert!(
            rows.iter()
                .any(|row| { row.scope == FindingScope::Project("/fixture/project".into()) })
        );
    }

    #[test]
    fn failure_and_stuck_views_keep_normalized_and_sequence_contracts() {
        let parsed = parse_rollout_reader(
            Path::new("lenses.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../../tests/fixtures/analysis/lenses.jsonl"
            ))),
        );
        let data = normalize_rollout(&parsed);

        let failures = failures(&data);
        assert!(failures.rows.iter().any(|row| {
            row.tool == "unknown_tool"
                && row.category == "exit_code_1"
                && row.opportunity.scope == FindingScope::Project("/fixture/project".into())
        }));
        assert!(failures.rows.iter().all(|row| {
            row.opportunity.evidence.len() <= 3
                && !row.opportunity.target.is_empty()
                && !row.opportunity.action.is_empty()
        }));

        let stuck = stuck(&data);
        assert!(
            stuck
                .rows
                .iter()
                .any(|row| { row.path == "src/lib.rs" && !row.sequence.is_empty() })
        );
    }

    #[test]
    fn usage_coverage_rejects_explicit_invalid_timestamps() {
        let mut data = view_fixture();
        for message in &mut data.messages {
            message.timestamp = Some("not-a-timestamp".to_owned());
        }
        data.token_usage = vec![TokenUsage {
            session_id: Some("view-session-a".to_owned()),
            turn_id: Some("view-turn-a".to_owned()),
            timestamp: Some("not-a-timestamp".to_owned()),
            input_tokens: Some(1),
            cached_input_tokens: None,
            output_tokens: Some(1),
            reasoning_output_tokens: None,
            sequence: 1,
            provenance: crate::model::SourceRef::rollout("views.jsonl".into(), 12),
        }];

        let coverage = usage(&data).coverage;
        assert_eq!(coverage.status, "partial");
        assert!(coverage.known_events < coverage.total_events);
    }
}
