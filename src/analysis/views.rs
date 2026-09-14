//! Typed, deterministic product views over canonical data.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{
    CanonicalData, InstructionSnapshotAccuracy, InstructionSnapshotSource, SourceRef, Surface,
    SurfaceKind, SurfaceLoadMode, SurfaceScope, SurfaceUsageState,
};

use super::{
    AnalysisOptions, EvidenceRef, EvidenceRole, Finding, FindingConfidence, FindingScope,
    FindingSeverity, FindingType, analyze_failures, analyze_rework, bounded_excerpt, evidence_for,
    majority_scope, normalize_fact, push_evidence, redact_sensitive,
};

pub const MAX_VIEW_EVIDENCE: usize = 3;
pub const HEAVY_STARTUP_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewOpportunity {
    pub id: String,
    pub title: String,
    pub scope: FindingScope,
    pub target: String,
    pub impact: String,
    pub severity: FindingSeverity,
    pub confidence: FindingConfidence,
    pub occurrences: usize,
    pub distinct_sessions: usize,
    pub action: String,
    pub evidence: Vec<EvidenceRef>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryReport {
    pub measure: String,
    pub rows: Vec<InventoryRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryRow {
    pub id: String,
    pub scope: FindingScope,
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
pub enum UsageKind {
    Tool,
    Skill,
    Model,
    Surface,
}

impl UsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Skill => "skill",
            Self::Model => "model",
            Self::Surface => "surface",
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
            InventoryRow {
                id: surface.id.clone(),
                scope: surface_scope(&surface.scope),
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
        let aggregate = aggregates
            .entry((UsageKind::Tool, name.clone(), String::new()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Tool, name));
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
        let aggregate = aggregates
            .entry((UsageKind::Tool, name.clone(), String::new()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Tool, name));
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
        let aggregate = aggregates
            .entry((UsageKind::Model, model.clone(), String::new()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Model, model.clone()));
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
        let aggregate = aggregates
            .entry((UsageKind::Model, model.to_owned(), String::new()))
            .or_insert_with(|| UsageAggregate::new(UsageKind::Model, model.to_owned()));
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
        measure: "Observed tool, Skill, model, and configured-surface effort ranked by counts, tokens, duration, and coverage".to_owned(),
        coverage,
        rows,
    }
}

pub fn prompts(data: &CanonicalData) -> PromptReport {
    let options = AnalysisOptions::default();
    let mut grouped = BTreeMap::<PromptClass, PromptAggregate>::new();
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
        let aggregate = grouped.entry(class).or_default();
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
            .map(|(class, aggregate)| PromptRow {
                class,
                scope: majority_scope(data, aggregate.sessions.iter().map(String::as_str)),
                occurrences: aggregate.occurrences,
                distinct_sessions: aggregate.sessions.len(),
                verdict: prompt_verdict(class).to_owned(),
                evidence: bounded_evidence(aggregate.evidence),
                limitations: vec!["Classification uses user role, bounded markers, and punctuation; intent is not inferred".to_owned()],
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
            let action = if finding.observed_commands.is_empty() {
                finding.suggested_action.clone()
            } else {
                format!(
                    "Fix the recurring {category} prerequisite for {}",
                    finding_target(&finding)
                )
            };
            let opportunity = finding_opportunity(&finding, finding_target(&finding), action);
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
                .unwrap_or_else(|| finding_target(&finding));
            let opportunity = finding_opportunity(
                &finding,
                path.clone(),
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
        let bytes = row.startup_bytes.unwrap_or_default();
        let impact = if row.usage_state == SurfaceUsageState::Unused {
            format!(
                "No observed use for a configured {} surface; startup estimate is {bytes} bytes",
                row.kind.as_str()
            )
        } else {
            format!(
                "{} bytes are estimated at startup for a configured {} surface",
                bytes,
                row.kind.as_str()
            )
        };
        let severity = if bytes >= HEAVY_STARTUP_BYTES.saturating_mul(2) {
            FindingSeverity::High
        } else {
            FindingSeverity::Medium
        };
        let confidence = match row.usage_state {
            SurfaceUsageState::Unused | SurfaceUsageState::Used => FindingConfidence::High,
            SurfaceUsageState::Rare => FindingConfidence::Medium,
            SurfaceUsageState::Unknown => FindingConfidence::Low,
        };
        let mut limitations = row.limitations;
        ensure_limitations(&mut limitations);
        opportunities.push(ViewOpportunity {
            id: format!("surface:{}", row.id),
            title: format!("Actionable configuration surface: {}", row.name),
            scope: row.scope,
            target,
            impact,
            severity,
            confidence,
            occurrences: row.observed_uses,
            distinct_sessions: row.observed_sessions,
            action,
            evidence: bounded_evidence(row.evidence),
            limitations,
        });
    }
    for row in failures_with_options(data, options).rows {
        let mut opportunity = row.opportunity;
        opportunity.action = format!(
            "Fix the recurring {} prerequisite at {}",
            row.category, opportunity.target
        );
        opportunities.push(opportunity);
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
    occurrences: usize,
    sessions: BTreeSet<String>,
    evidence: Vec<EvidenceRef>,
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
    if limitations.is_empty() {
        limitations.push("Result is based on the selected canonical evidence".to_owned());
    }
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

fn finding_target(finding: &Finding) -> String {
    if let Some(path) = finding.affected_paths.first() {
        return path.clone();
    }
    match &finding.scope {
        FindingScope::Global => "AGENTS.md".to_owned(),
        FindingScope::Project(path) => path.join("AGENTS.md").to_string_lossy().into_owned(),
        FindingScope::Instruction(path) => path.to_string_lossy().into_owned(),
        FindingScope::Path(path) => path.clone(),
    }
}

fn finding_opportunity(finding: &Finding, target: String, action: String) -> ViewOpportunity {
    let target = bounded_excerpt(&target, 512);
    let action = bounded_excerpt(&action, 512);
    let mut limitations = finding.limitations.clone();
    ensure_limitations(&mut limitations);
    ViewOpportunity {
        id: format!("{}:{}", finding.kind.as_str(), finding.key),
        title: bounded_excerpt(&finding.summary, 256),
        scope: finding.scope.clone(),
        target,
        impact: bounded_excerpt(&finding.summary, 512),
        severity: finding.severity,
        confidence: finding.confidence,
        occurrences: finding.occurrences,
        distinct_sessions: finding.distinct_sessions,
        action,
        evidence: bounded_evidence(finding.evidence.clone()),
        limitations,
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
    use std::path::Path;

    use crate::model::{
        CanonicalData, InstructionSnapshot, InstructionSnapshotAccuracy, InstructionSnapshotSource,
        OutcomeSource, Surface, SurfaceKind, SurfaceLoadMode, SurfaceScope, SurfaceUsageState,
        TokenUsage, ToolCall, ToolOutcome, ToolResult,
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
        assert!(!wrapper_rows.is_empty());
        assert!(wrapper_rows.iter().all(|row| {
            row.opportunity
                .action
                .contains("no shell-command prerequisite")
        }));
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
