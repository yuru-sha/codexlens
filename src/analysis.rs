//! Deterministic, local-only lenses over canonical session data.
//!
//! The module deliberately accepts [`CanonicalData`] instead of reopening
//! rollout or instruction files.  That keeps upstream field names at the
//! adapter boundary and makes every lens deterministic for the same input.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{
    CanonicalData, InstructionFile, InstructionJoin, InstructionScope, InstructionSnapshot,
    Message, MessageRole, Record, RecordKind, SourceRef, ToolCall, ToolOutcome, ToolResult,
};

pub const DEFAULT_REWORK_WINDOW_SECONDS: i64 = 10 * 60;
pub const DEFAULT_EXCERPT_BYTES: usize = 512;
pub const DEFAULT_MIN_OCCURRENCES: usize = 2;
pub const DEFAULT_MIN_SESSIONS: usize = 2;

pub const CORRECTION_MARKERS: &[&str] = &[
    "please use ",
    "use ",
    "this project uses ",
    "this repo uses ",
    "this repository uses ",
    "the project uses ",
    "the repo uses ",
    "the repository uses ",
    "this project requires ",
    "the project requires ",
    "remember that ",
    "note that ",
    "do not ",
    "don't ",
    "never ",
];

pub const DISCOVERY_MARKERS: &[&str] = &[
    "this project uses ",
    "this repo uses ",
    "this repository uses ",
    "the project uses ",
    "the repository uses ",
    "this project requires ",
    "the project requires ",
    "remember that ",
    "note that ",
];

const DEFAULT_MAX_EVIDENCE: usize = 12;
const LONG_FACT_BYTES: usize = 240;

/// Options shared by all lenses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisOptions {
    pub rework_window_seconds: i64,
    pub excerpt_max_bytes: usize,
    /// Exclusions are caller-supplied project evidence, never a repository
    /// hard-coded list.
    pub excluded_path_prefixes: Vec<String>,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self {
            rework_window_seconds: DEFAULT_REWORK_WINDOW_SECONDS,
            excerpt_max_bytes: DEFAULT_EXCERPT_BYTES,
            excluded_path_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingType {
    Failure,
    Correction,
    Rework,
    Stuck,
    Verification,
    Knowledge,
    Gap,
    Overscoped,
    Duplicate,
    Stale,
    Truncated,
}

impl FindingType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Correction => "correction",
            Self::Rework => "rework",
            Self::Stuck => "stuck",
            Self::Verification => "verification",
            Self::Knowledge => "knowledge",
            Self::Gap => "gap",
            Self::Overscoped => "overscoped",
            Self::Duplicate => "duplicate",
            Self::Stale => "stale",
            Self::Truncated => "truncated",
        }
    }
}

pub type Severity = FindingSeverity;
pub type Confidence = FindingConfidence;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingScope {
    Global,
    Project(PathBuf),
    Instruction(PathBuf),
    Path(String),
}

impl FindingScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project(_) => "project",
            Self::Instruction(_) => "instruction",
            Self::Path(_) => "path",
        }
    }
}

impl std::fmt::Display for FindingScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => formatter.write_str("global"),
            Self::Project(path) => write!(formatter, "project:{}", path.display()),
            Self::Instruction(path) => write!(formatter, "instruction:{}", path.display()),
            Self::Path(path) => write!(formatter, "path:{path}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FindingSeverity {
    Low,
    Medium,
    High,
}

impl FindingSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FindingConfidence {
    Low,
    Medium,
    High,
}

impl FindingConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationStatus {
    Missing,
    NotObserved,
}

impl VerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotObserved => "not_observed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceRole {
    Observation,
    PrecedingAction,
    FileOperation,
    VerificationCommand,
    InstructionSnapshot,
    InstructionFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub session_id: Option<String>,
    pub source: SourceRef,
    pub role: EvidenceRole,
    pub excerpt: Option<String>,
}

/// A bounded, explainable lens result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: FindingType,
    pub severity: FindingSeverity,
    pub confidence: FindingConfidence,
    pub scope: FindingScope,
    pub key: String,
    pub summary: String,
    pub evidence: Vec<EvidenceRef>,
    pub occurrences: usize,
    pub distinct_sessions: usize,
    pub affected_paths: Vec<String>,
    pub observed_commands: Vec<String>,
    pub sequence: Vec<String>,
    pub suggested_action: String,
    pub limitations: Vec<String>,
    pub verification_status: Option<VerificationStatus>,
}

impl Finding {
    fn sort_key(&self) -> (&str, &str) {
        (self.kind.as_str(), &self.key)
    }
}

/// Run all Phase 3 lenses and return stable output.
pub fn analyze(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut findings = recurring_findings(data, options);
    let recurring = findings.clone();
    findings.extend(analyze_instructions(data, &recurring, options));
    sort_findings(&mut findings);
    findings
}

/// Convenience wrapper using the deterministic defaults.
pub fn analyze_default(data: &CanonicalData) -> Vec<Finding> {
    analyze(data, &AnalysisOptions::default())
}

pub fn failures(data: &CanonicalData) -> Vec<Finding> {
    analyze_failures(data, &AnalysisOptions::default())
}

pub fn corrections(data: &CanonicalData) -> Vec<Finding> {
    analyze_corrections(data, &AnalysisOptions::default())
}

pub fn rework(data: &CanonicalData) -> Vec<Finding> {
    analyze_rework(data, &AnalysisOptions::default())
}

pub fn verification(data: &CanonicalData) -> Vec<Finding> {
    analyze_verification(data, &AnalysisOptions::default())
}

pub fn knowledge(data: &CanonicalData) -> Vec<Finding> {
    analyze_knowledge(data, &AnalysisOptions::default())
}

pub fn stuck(data: &CanonicalData) -> Vec<Finding> {
    rework(data)
}

pub fn rediscovery(data: &CanonicalData) -> Vec<Finding> {
    knowledge(data)
}

/// Run the instructions lens against already-produced non-instruction
/// findings.  [`instructions`] is the default convenience wrapper.
pub fn analyze_instructions(
    data: &CanonicalData,
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

pub fn instructions(data: &CanonicalData) -> Vec<Finding> {
    let options = AnalysisOptions::default();
    let recurring = recurring_findings(data, &options);
    analyze_instructions(data, &recurring, &options)
}

fn recurring_findings(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    [
        analyze_failures(data, options),
        analyze_corrections(data, options),
        analyze_rework(data, options),
        analyze_verification(data, options),
        analyze_knowledge(data, options),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|left, right| {
        right
            .severity
            .cmp(&left.severity)
            .then_with(|| right.confidence.cmp(&left.confidence))
            .then_with(|| right.distinct_sessions.cmp(&left.distinct_sessions))
            .then_with(|| right.occurrences.cmp(&left.occurrences))
            .then_with(|| left.sort_key().cmp(&right.sort_key()))
            .then_with(|| left.scope.to_string().cmp(&right.scope.to_string()))
    });
}

#[derive(Debug, Clone)]
struct Position {
    timestamp: Option<i64>,
    sequence: Option<usize>,
    source: SourceRef,
}

#[derive(Debug, Clone)]
struct Activity {
    session_id: String,
    turn_id: Option<String>,
    position: Position,
    description: String,
    source: SourceRef,
    path: Option<String>,
    kind: ActivityKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActivityKind {
    Edit,
    Failure,
    Action,
    Verification,
}

#[derive(Debug, Clone)]
struct FailureEvent {
    session_id: String,
    turn_id: Option<String>,
    key: String,
    tool: String,
    family: String,
    category: String,
    structured: bool,
    description: String,
    position: Position,
    source: SourceRef,
}

#[derive(Debug, Clone)]
struct CorrectionEvent {
    session_id: String,
    key: String,
    text: String,
    message: Message,
    preceding: Activity,
}

#[derive(Debug, Clone)]
struct EditEvent {
    session_id: String,
    turn_id: Option<String>,
    path: String,
    operation: String,
    position: Position,
    source: SourceRef,
}

#[derive(Debug, Clone)]
struct VerificationEvent {
    session_id: String,
    turn_id: Option<String>,
    command: String,
    kind: String,
    position: Position,
    source: SourceRef,
}

#[derive(Debug, Clone)]
struct FactEvent {
    session_id: String,
    key: String,
    text: String,
    source: SourceRef,
    excerpt: String,
    role: EvidenceRole,
}

pub fn analyze_failures(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut grouped: BTreeMap<String, Vec<FailureEvent>> = BTreeMap::new();
    for event in failure_events(data) {
        grouped.entry(event.key.clone()).or_default().push(event);
    }

    let mut findings = grouped
        .into_iter()
        .filter_map(|(key, mut events)| {
            events.sort_by(|left, right| compare_positions(&left.position, &right.position));
            let sessions = distinct_sessions(events.iter().map(|event| event.session_id.as_str()));
            if events.len() < DEFAULT_MIN_OCCURRENCES || sessions.len() < DEFAULT_MIN_SESSIONS {
                return None;
            }
            let first = events.first()?;
            let scope = majority_scope(data, events.iter().map(|event| event.session_id.as_str()));
            let structured = events.iter().all(|event| event.structured);
            let severity = if events.len() >= 3 {
                FindingSeverity::High
            } else {
                FindingSeverity::Medium
            };
            let confidence = if structured {
                FindingConfidence::High
            } else {
                FindingConfidence::Medium
            };
            let mut evidence = Vec::new();
            for event in &events {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(event.session_id.clone()),
                        event.source.clone(),
                        EvidenceRole::Observation,
                        Some(&event.description),
                        options,
                    ),
                );
            }
            Some(Finding {
                kind: FindingType::Failure,
                severity,
                confidence,
                scope,
                key,
                summary: format!(
                    "Repeated failure for {} {} ({}) observed {} times across {} sessions",
                    first.tool,
                    first.family,
                    first.category,
                    events.len(),
                    sessions.len()
                ),
                evidence,
                occurrences: events.len(),
                distinct_sessions: sessions.len(),
                affected_paths: Vec::new(),
                observed_commands: vec![first.family.clone()],
                sequence: Vec::new(),
                suggested_action: format!(
                    "Document the prerequisite or preferred command for {} in the applicable instructions",
                    first.family
                ),
                limitations: vec![
                    "This is recurring observational evidence, not proof that the command is always incorrect".to_owned(),
                ],
                verification_status: None,
            })
        })
        .collect::<Vec<_>>();
    sort_findings(&mut findings);
    findings
}

pub fn analyze_corrections(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
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
                    .map(|event| event.preceding.description.clone())
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
    sort_findings(&mut findings);
    findings
}

pub fn analyze_rework(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let edits = edit_events(data, options);
    let failures = failure_activities(data);
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
                "{} edits to {} occurred within the ten-minute rework window",
                count, path
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
                .map(|(_, edit)| format!("edit {}", edit.path))
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
                sequence.push(activity.description.clone());
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
                .map(|activity| activity.description.clone())
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
    sort_findings(&mut findings);
    findings
}

pub fn analyze_verification(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let edits = edit_events(data, options);
    let verifications = verification_events(data);
    let mut groups: BTreeMap<(String, Option<String>), Vec<EditEvent>> = BTreeMap::new();
    for edit in edits {
        groups
            .entry((edit.session_id.clone(), edit.turn_id.clone()))
            .or_default()
            .push(edit);
    }

    let mut findings = Vec::new();
    for ((session_id, turn_id), mut changes) in groups {
        changes.sort_by(|left, right| compare_positions(&left.position, &right.position));
        let mut checks = verifications
            .iter()
            .filter(|check| {
                check.session_id == session_id && (turn_id.is_none() || check.turn_id == turn_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        checks.sort_by(|left, right| compare_positions(&left.position, &right.position));

        let mut pending = Vec::new();
        let mut observed_before_pending = Vec::new();
        let mut change_and_check = changes
            .iter()
            .map(|change| Activity {
                session_id: change.session_id.clone(),
                turn_id: change.turn_id.clone(),
                position: change.position.clone(),
                description: format!("{} {}", change.operation, change.path),
                source: change.source.clone(),
                path: Some(change.path.clone()),
                kind: ActivityKind::Edit,
            })
            .collect::<Vec<_>>();
        change_and_check.extend(checks.iter().map(|check| Activity {
            session_id: check.session_id.clone(),
            turn_id: check.turn_id.clone(),
            position: check.position.clone(),
            description: format!("{} ({})", check.command, check.kind),
            source: check.source.clone(),
            path: None,
            kind: ActivityKind::Verification,
        }));
        change_and_check.sort_by(compare_activity_positions);

        for activity in change_and_check {
            match activity.kind {
                ActivityKind::Edit => pending.push(activity),
                ActivityKind::Verification => {
                    if pending.is_empty() {
                        observed_before_pending.push(activity);
                    } else {
                        pending.clear();
                    }
                }
                ActivityKind::Action => {}
                ActivityKind::Failure => {}
            }
        }
        if pending.is_empty() {
            continue;
        }

        let complete = turn_is_complete(data, &session_id, turn_id.as_deref());
        let status = if complete {
            VerificationStatus::Missing
        } else {
            VerificationStatus::NotObserved
        };
        let paths = pending
            .iter()
            .filter_map(|activity| activity.path.clone())
            .collect::<BTreeSet<_>>();
        let paths = paths.into_iter().collect::<Vec<_>>();
        let scope = if let Some(path) = paths.first() {
            path_scope(data, &[session_id.as_str()], path)
        } else {
            majority_scope(data, [session_id.as_str()])
        };
        let mut evidence = Vec::new();
        for activity in &pending {
            push_evidence(
                &mut evidence,
                evidence_for(
                    Some(session_id.clone()),
                    activity.source.clone(),
                    EvidenceRole::FileOperation,
                    Some(&activity.description),
                    options,
                ),
            );
        }
        for check in observed_before_pending.iter().rev().take(2).rev() {
            push_evidence(
                &mut evidence,
                evidence_for(
                    Some(session_id.clone()),
                    check.source.clone(),
                    EvidenceRole::VerificationCommand,
                    Some(&check.description),
                    options,
                ),
            );
        }
        let key = format!("{}|{}", paths.join(","), status.as_str());
        let summary = match status {
            VerificationStatus::Missing => format!(
                "No recognized verification command followed the latest changes to {} before the turn completed",
                paths.join(", ")
            ),
            VerificationStatus::NotObserved => format!(
                "Verification was not observed after changes to {} before the incomplete rollout ended",
                paths.join(", ")
            ),
        };
        findings.push(Finding {
            kind: FindingType::Verification,
            severity: if complete {
                FindingSeverity::Medium
            } else {
                FindingSeverity::Low
            },
            confidence: if complete {
                FindingConfidence::Medium
            } else {
                FindingConfidence::Low
            },
            scope,
            key,
            summary,
            evidence,
            occurrences: pending.len(),
            distinct_sessions: 1,
            affected_paths: paths,
            observed_commands: observed_before_pending
                .iter()
                .map(|activity| activity.description.clone())
                .collect(),
            sequence: pending
                .iter()
                .map(|activity| activity.description.clone())
                .collect(),
            suggested_action: "If this change batch is expected to be complete, run and record an observed project verification command".to_owned(),
            limitations: vec![match status {
                VerificationStatus::Missing => {
                    "Only the documented command allowlist was recognized; absence is not proof that an unrecognized command was not run".to_owned()
                }
                VerificationStatus::NotObserved => {
                    "The rollout ended before completion was observed, so this is not proof that verification was not run".to_owned()
                }
            }],
            verification_status: Some(status),
        });
    }
    sort_findings(&mut findings);
    findings
}

pub fn analyze_knowledge(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut facts = correction_facts(data, options);
    facts.extend(discovery_facts(data, options));
    let mut grouped: BTreeMap<String, Vec<FactEvent>> = BTreeMap::new();
    for fact in facts {
        grouped.entry(fact.key.clone()).or_default().push(fact);
    }

    let mut findings = grouped
        .into_iter()
        .filter_map(|(key, mut facts)| {
            facts.sort_by(|left, right| compare_sources(&left.source, &right.source));
            let sessions = distinct_sessions(facts.iter().map(|fact| fact.session_id.as_str()));
            if facts.len() < DEFAULT_MIN_OCCURRENCES || sessions.len() < DEFAULT_MIN_SESSIONS {
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
                    snapshot.session_id.as_deref() == Some(session)
                        && snapshot.content.is_some()
                        && snapshot.source
                            != crate::model::InstructionSnapshotSource::Unavailable
                })
            });
            let mut limitations = vec![
                "This candidate uses repeated bounded lexical facts; it does not perform semantic summarization".to_owned(),
            ];
            if missing_snapshot {
                limitations.push(
                    "An instruction snapshot was unavailable for at least one supporting session, so destination comparison is inconclusive".to_owned(),
                );
            }
            let suggested_action = if long {
                format!(
                    "Add a short index/link in {target}; keep the detailed fact in a scoped project documentation page"
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
    sort_findings(&mut findings);
    findings
}

fn failure_events(data: &CanonicalData) -> Vec<FailureEvent> {
    let mut events = Vec::new();
    for result in &data.tool_results {
        if result.is_duplicate || !result_is_failed(result) {
            continue;
        }
        let Some(session_id) = result.session_id.clone() else {
            continue;
        };
        let call = matching_call(data, result);
        let tool = call
            .and_then(|call| call.tool_name.as_deref())
            .map(normalize_tool)
            .unwrap_or_else(|| "unknown_tool".to_owned());
        let command = result
            .command
            .as_deref()
            .or_else(|| call.and_then(|call| call.command.as_deref()))
            .or_else(|| call.and_then(|call| call.input_summary.as_deref()))
            .unwrap_or_default();
        let family = command_family(command);
        let category = failure_category(result);
        let key = format!("{tool}|{family}|{category}");
        let description = failure_description(&tool, &family, &category, result);
        events.push(FailureEvent {
            session_id,
            turn_id: result.turn_id.clone(),
            key,
            tool,
            family,
            category,
            structured: result.exit_code.is_some_and(|code| code != 0)
                || result.status.as_deref().is_some_and(status_is_failed),
            description,
            position: position_for_source(data, &result.provenance, None),
            source: result.provenance.clone(),
        });
    }
    for record in &data.records {
        if !is_explicit_error_record(record) {
            continue;
        }
        let Some(session_id) = record.session_id.clone() else {
            continue;
        };
        let category = record
            .original_nested_type
            .as_deref()
            .map(normalize_fragment)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "error".to_owned());
        let key = format!("event|event|{category}");
        events.push(FailureEvent {
            session_id,
            turn_id: record.turn_id.clone(),
            key,
            tool: "event".to_owned(),
            family: "event".to_owned(),
            category,
            structured: true,
            description: "explicit error event".to_owned(),
            position: position_for_source(data, &record.provenance, record.timestamp.as_deref()),
            source: record.provenance.clone(),
        });
    }
    events
}

fn failure_activities(data: &CanonicalData) -> Vec<Activity> {
    failure_events(data)
        .into_iter()
        .map(|event| Activity {
            session_id: event.session_id,
            turn_id: event.turn_id,
            position: event.position,
            description: event.description,
            source: event.source,
            path: None,
            kind: ActivityKind::Failure,
        })
        .collect()
}

fn result_is_failed(result: &ToolResult) -> bool {
    if let Some(exit_code) = result.exit_code {
        return exit_code != 0;
    }
    result.outcome == ToolOutcome::Failed || result.status.as_deref().is_some_and(status_is_failed)
}

fn status_is_failed(status: &str) -> bool {
    matches!(
        normalize_fragment(status).as_str(),
        "failed" | "failure" | "error" | "cancelled" | "canceled" | "timeout" | "timed_out"
    )
}

fn failure_category(result: &ToolResult) -> String {
    if let Some(code) = result.exit_code.filter(|code| *code != 0) {
        return match code {
            126 => "permission_denied".to_owned(),
            127 => "command_not_found".to_owned(),
            _ => format!("exit_code_{code}"),
        };
    }
    if let Some(status) = result
        .status
        .as_deref()
        .filter(|status| status_is_failed(status))
    {
        let status = normalize_fragment(status);
        return match status.as_str() {
            "cancelled" | "canceled" => "cancelled".to_owned(),
            "timeout" | "timed_out" => "timeout".to_owned(),
            _ => "failed_status".to_owned(),
        };
    }
    let output = combined_result_output(result);
    let normalized = normalize_fragment(&output);
    for (marker, category) in [
        ("permission denied", "permission_denied"),
        ("command not found", "command_not_found"),
        ("no such file", "missing_file"),
        ("timed out", "timeout"),
        ("timeout", "timeout"),
        ("parse error", "parse_error"),
        ("syntax error", "syntax_error"),
    ] {
        if normalized.contains(marker) {
            return category.to_owned();
        }
    }
    "output_error".to_owned()
}

fn failure_description(tool: &str, family: &str, category: &str, result: &ToolResult) -> String {
    let output = combined_result_output(result);
    if output.is_empty() {
        format!("{tool} {family} -> {category}")
    } else {
        format!("{tool} {family} -> {category}: {output}")
    }
}

fn combined_result_output(result: &ToolResult) -> String {
    [result.stderr.as_deref(), result.stdout.as_deref()]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn matching_call<'a>(data: &'a CanonicalData, result: &ToolResult) -> Option<&'a ToolCall> {
    data.tool_calls.iter().find(|call| {
        call.call_id == result.call_id
            && call.call_id.is_some()
            && context_matches(call.session_id.as_deref(), result.session_id.as_deref())
            && context_matches(call.turn_id.as_deref(), result.turn_id.as_deref())
    })
}

fn context_matches(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn is_explicit_error_record(record: &Record) -> bool {
    let nested = record.original_nested_type.as_deref().unwrap_or_default();
    matches!(nested, "error" | "stream_error" | "exec_error")
        || matches!(record.kind, RecordKind::Unknown { .. })
            && matches!(nested, "error" | "stream_error" | "exec_error")
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
    actions.sort_by(compare_activity_positions);

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
    let normalized = normalize_fact(trimmed);
    for marker in CORRECTION_MARKERS {
        if let Some(rest) = normalized.strip_prefix(marker) {
            let rest = rest.strip_suffix(" instead").unwrap_or(rest).trim();
            if !rest.is_empty() {
                return Some(rest.to_owned());
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

fn normalize_fact(text: &str) -> String {
    normalize_fragment(text)
        .trim_matches(|character: char| ".,;:!?`\"'".contains(character))
        .to_owned()
}

fn correction_facts(data: &CanonicalData, options: &AnalysisOptions) -> Vec<FactEvent> {
    correction_events(data)
        .into_iter()
        .map(|event| FactEvent {
            session_id: event.session_id,
            key: event.key,
            text: event.text,
            source: event.message.provenance.clone(),
            excerpt: event.message.content.unwrap_or_default(),
            role: EvidenceRole::Observation,
        })
        .map(|fact| FactEvent {
            excerpt: bounded_excerpt(&fact.excerpt, options.excerpt_max_bytes),
            ..fact
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
            key: fact.clone(),
            text: fact,
            source: message.provenance.clone(),
            excerpt: bounded_excerpt(content, options.excerpt_max_bytes),
            role: EvidenceRole::Observation,
        });
    }
    facts
}

fn edit_events(data: &CanonicalData, options: &AnalysisOptions) -> Vec<EditEvent> {
    data.file_operations
        .iter()
        .filter_map(|operation| {
            let session_id = operation.session_id.clone()?;
            if !is_edit_operation(&operation.operation) {
                return None;
            }
            let path = normalize_path(&operation.path);
            if path.is_empty() || path_is_excluded(&path, options) {
                return None;
            }
            Some(EditEvent {
                session_id,
                turn_id: operation.turn_id.clone(),
                path,
                operation: normalize_fragment(&operation.operation),
                position: position_for_source(
                    data,
                    &operation.provenance,
                    operation.timestamp.as_deref(),
                ),
                source: operation.provenance.clone(),
            })
        })
        .collect()
}

fn is_edit_operation(operation: &str) -> bool {
    matches!(
        normalize_fragment(operation).as_str(),
        "edit"
            | "write"
            | "write_file"
            | "modify"
            | "modified"
            | "patch"
            | "apply_patch"
            | "create"
            | "update"
            | "replace"
            | "save"
            | "save_file"
            | "delete"
            | "remove"
    )
}

fn verification_events(data: &CanonicalData) -> Vec<VerificationEvent> {
    let mut events = Vec::new();
    let mut call_ids = HashSet::new();
    for call in &data.tool_calls {
        let Some(session_id) = call.session_id.clone() else {
            continue;
        };
        let command = call
            .command
            .as_deref()
            .or(call.input_summary.as_deref())
            .unwrap_or_default();
        let Some(kind) = verification_kind(command) else {
            continue;
        };
        if let Some(call_id) = call.call_id.as_ref() {
            call_ids.insert((session_id.clone(), call_id.clone()));
        }
        events.push(VerificationEvent {
            session_id,
            turn_id: call.turn_id.clone(),
            command: bounded_excerpt(command, DEFAULT_EXCERPT_BYTES),
            kind,
            position: position_for_source(data, &call.provenance, None),
            source: call.provenance.clone(),
        });
    }
    for result in &data.tool_results {
        let Some(session_id) = result.session_id.clone() else {
            continue;
        };
        if result
            .call_id
            .as_ref()
            .is_some_and(|call_id| call_ids.contains(&(session_id.clone(), call_id.clone())))
        {
            continue;
        }
        let command = result.command.as_deref().unwrap_or_default();
        let Some(kind) = verification_kind(command) else {
            continue;
        };
        events.push(VerificationEvent {
            session_id,
            turn_id: result.turn_id.clone(),
            command: bounded_excerpt(command, DEFAULT_EXCERPT_BYTES),
            kind,
            position: position_for_source(data, &result.provenance, None),
            source: result.provenance.clone(),
        });
    }
    events
}

fn verification_kind(command: &str) -> Option<String> {
    let tokens = command_tokens(command);
    let lower = tokens
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let first = lower.first()?.as_str();
    let kind = match first {
        "cargo" => lower.get(1).and_then(|value| match value.as_str() {
            "test" | "nextest" => Some("test"),
            "fmt" => Some("format"),
            "clippy" | "check" => Some("check"),
            "build" => Some("build"),
            _ => None,
        }),
        "go" => lower.get(1).and_then(|value| match value.as_str() {
            "test" => Some("test"),
            "fmt" => Some("format"),
            "vet" => Some("lint"),
            "build" => Some("build"),
            _ => None,
        }),
        "pytest" => Some("test"),
        "ruff" => Some(if lower.get(1).is_some_and(|value| value == "format") {
            "format"
        } else {
            "lint"
        }),
        "mypy" | "eslint" => Some("lint"),
        "prettier" => Some("format"),
        "python" | "python3" if lower.get(1).is_some_and(|value| value == "-m") => lower
            .get(2)
            .and_then(|value| (value == "pytest").then_some("test")),
        "npm" | "pnpm" | "yarn" | "bun" => command_manager_kind(&lower),
        "make" | "just" => lower.iter().skip(1).find_map(|value| target_kind(value)),
        "git" if lower.get(1).is_some_and(|value| value == "diff") => lower
            .iter()
            .any(|value| value == "--check")
            .then_some("check"),
        _ => lower
            .first()
            .and_then(|value| Path::new(value).file_stem().and_then(|stem| stem.to_str()))
            .and_then(target_kind),
    }?;
    Some(kind.to_owned())
}

pub fn classify_verification_command(command: &str) -> Option<String> {
    verification_kind(command)
}

fn command_manager_kind(tokens: &[String]) -> Option<&'static str> {
    let mut values = tokens.iter().skip(1);
    let subcommand = values.next()?;
    let target = if subcommand == "run" {
        values.next().map(String::as_str).unwrap_or_default()
    } else {
        subcommand.as_str()
    };
    target_kind(target)
}

fn target_kind(target: &str) -> Option<&'static str> {
    match target {
        "test" | "tests" => Some("test"),
        "lint" | "check" | "vet" => Some("lint"),
        "format" | "fmt" => Some("format"),
        "build" | "compile" => Some("build"),
        "verify" => Some("check"),
        _ => None,
    }
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
    activities.extend(
        failures
            .iter()
            .filter(|failure| {
                failure.session_id
                    == edits
                        .first()
                        .map(|edit| edit.session_id.as_str())
                        .unwrap_or_default()
            })
            .cloned(),
    );
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

fn has_kind_transition(activities: &[Activity]) -> bool {
    activities
        .windows(2)
        .any(|window| window[0].kind != window[1].kind)
}

fn turn_is_complete(data: &CanonicalData, session_id: &str, turn_id: Option<&str>) -> bool {
    if let Some(turn_id) = turn_id {
        if data.turns.iter().any(|turn| {
            turn.session_id.as_deref() == Some(session_id)
                && turn.id == turn_id
                && (turn.completed_at.is_some()
                    || turn.lifecycle.iter().any(|event| {
                        matches!(event.kind.as_str(), "turn_complete" | "turn_aborted")
                    }))
        }) {
            return true;
        }
    }
    data.records.iter().any(|record| {
        record.session_id.as_deref() == Some(session_id)
            && turn_id.is_none_or(|id| record.turn_id.as_deref() == Some(id))
            && matches!(
                record.original_nested_type.as_deref(),
                Some("turn_complete" | "turn_aborted" | "task_complete")
            )
    })
}

fn instruction_join_findings(
    data: &CanonicalData,
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
        let snapshots = session_ids
            .iter()
            .flat_map(|session_id| {
                data.instruction_snapshots
                    .iter()
                    .filter(move |snapshot| snapshot.session_id.as_deref() == Some(session_id))
            })
            .collect::<Vec<_>>();
        let available_snapshots = snapshots
            .iter()
            .filter(|snapshot| snapshot.content.is_some() && !snapshot_is_unavailable(snapshot))
            .collect::<Vec<_>>();
        if available_snapshots.is_empty() {
            continue;
        }
        let historical_match = available_snapshots.iter().any(|snapshot| {
            guidance_matches(finding, snapshot.content.as_deref().unwrap_or_default())
        });
        if !historical_match {
            let mut evidence = finding.evidence.clone();
            for snapshot in &available_snapshots {
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
                    &join.session_id == session_id && join.resolution.effective_content.is_some()
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
                scope: finding.scope.clone(),
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
            let Some(snapshot) = snapshots.iter().find(|snapshot| {
                snapshot.session_id.as_deref() == Some(session_id)
                    && snapshot.content.is_some()
                    && !snapshot_is_unavailable(snapshot)
            }) else {
                continue;
            };
            if let Some((path, old_hash, new_hash)) = stale_file(join, snapshot) {
                let mut evidence = finding.evidence.clone();
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
                        join.provenance.clone(),
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

            if let Some(path) = overscoped_path(join, finding) {
                let mut evidence = finding.evidence.clone();
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(session_id.clone()),
                        join.provenance.clone(),
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
    findings
}

fn instruction_truncation_findings(
    data: &CanonicalData,
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
                join.provenance.clone(),
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

fn instruction_duplicate_findings(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let mut findings = Vec::new();
    for join in &data.instruction_joins {
        let mut contents: BTreeMap<String, Vec<&InstructionFile>> = BTreeMap::new();
        for file in &join.resolution.chain {
            let Some(content) = file.content.as_deref() else {
                continue;
            };
            let normalized = normalize_guidance(content);
            if !normalized.is_empty() {
                contents.entry(normalized).or_default().push(file);
            }
        }
        for (normalized, files) in contents {
            if files.len() < 2 {
                continue;
            }
            let mut evidence = Vec::new();
            for file in &files {
                push_evidence(
                    &mut evidence,
                    evidence_for(
                        Some(join.session_id.clone()),
                        join.provenance.clone(),
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
                key: format!("{}|{}", join.session_id, normalized),
                summary: format!(
                    "Equivalent normalized guidance is loaded from {} instruction files",
                    files.len()
                ),
                evidence,
                occurrences: files.len(),
                distinct_sessions: 1,
                affected_paths: Vec::new(),
                observed_commands: Vec::new(),
                sequence: files.iter().map(|file| file.path.display().to_string()).collect(),
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
    if let (Some(old_hash), Some(new_hash)) = (
        snapshot.effective_chain_hash.as_deref(),
        join.resolution.effective_chain_hash.as_deref(),
    ) {
        if old_hash != new_hash {
            let path = join
                .nearest_path
                .clone()
                .or_else(|| join.resolution.chain.last().map(|file| file.path.clone()))?;
            return Some((path, old_hash.to_owned(), new_hash.to_owned()));
        }
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
    None
}

fn overscoped_path(join: &InstructionJoin, finding: &Finding) -> Option<PathBuf> {
    let target = finding.affected_paths.first()?;
    let target = Path::new(target);
    if !target.is_absolute() {
        return None;
    }
    let nested = join
        .resolution
        .chain
        .iter()
        .filter(|file| file.scope == InstructionScope::ProjectNested)
        .any(|file| target.starts_with(file.path.parent().unwrap_or(file.path.as_path())));
    if !nested {
        return None;
    }
    join.resolution
        .chain
        .iter()
        .find(|file| {
            file.scope == InstructionScope::ProjectRoot
                && file
                    .content
                    .as_deref()
                    .is_some_and(|content| guidance_matches(finding, content))
        })
        .map(|file| file.path.clone())
}

fn guidance_matches(finding: &Finding, content: &str) -> bool {
    let content = normalize_guidance(content);
    if content.is_empty() {
        return false;
    }
    let key = normalize_guidance(&finding.key);
    if key.is_empty() {
        return false;
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

fn snapshot_is_unavailable(snapshot: &InstructionSnapshot) -> bool {
    snapshot.source == crate::model::InstructionSnapshotSource::Unavailable
        || snapshot.accuracy == crate::model::InstructionSnapshotAccuracy::Unavailable
}

fn evidence_sessions(finding: &Finding) -> Vec<String> {
    let mut sessions = finding
        .evidence
        .iter()
        .filter_map(|evidence| evidence.session_id.clone())
        .collect::<Vec<_>>();
    sessions.sort();
    sessions.dedup();
    sessions
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

fn path_scope(data: &CanonicalData, sessions: &[&str], path: &str) -> FindingScope {
    let nearest = sessions
        .iter()
        .filter_map(|session_id| nearest_instruction_path(data, session_id, path))
        .collect::<Vec<_>>();
    if let Some(path) = majority_path(nearest) {
        return FindingScope::Instruction(path);
    }
    if let Some(project) = majority_project(data, sessions.iter().copied()) {
        return FindingScope::Project(project);
    }
    FindingScope::Path(path.to_owned())
}

fn nearest_instruction_path(data: &CanonicalData, session_id: &str, path: &str) -> Option<PathBuf> {
    let join = data
        .instruction_joins
        .iter()
        .find(|join| join.session_id == session_id)?;
    let nearest = join.nearest_path.as_ref()?;
    let target = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        join.project_root.as_ref()?.join(path)
    };
    target
        .starts_with(nearest.parent().unwrap_or(nearest.as_path()))
        .then(|| nearest.clone())
}

fn majority_scope<'a, I>(data: &CanonicalData, sessions: I) -> FindingScope
where
    I: IntoIterator<Item = &'a str>,
{
    majority_project(data, sessions)
        .map(FindingScope::Project)
        .unwrap_or(FindingScope::Global)
}

fn majority_project<'a, I>(data: &CanonicalData, sessions: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut total = 0;
    for session_id in sessions {
        let Some(session) = data
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            continue;
        };
        let Some(project) = session
            .project
            .as_deref()
            .or(session.cwd.as_deref())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        total += 1;
        *counts
            .entry(PathBuf::from(normalize_path(project)))
            .or_default() += 1;
    }
    let (project, count) = counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))?;
    (count * 2 > total).then_some(project)
}

fn majority_path<I>(paths: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut total = 0;
    for path in paths {
        total += 1;
        *counts.entry(path).or_default() += 1;
    }
    let (path, count) = counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))?;
    (count * 2 > total).then_some(path)
}

fn distinct_sessions<'a, I>(sessions: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = &'a str>,
{
    sessions.into_iter().map(str::to_owned).collect()
}

fn position_for_message(data: &CanonicalData, message: &Message) -> Position {
    position_for_source(data, &message.provenance, message.timestamp.as_deref())
}

fn position_for_source(
    data: &CanonicalData,
    source: &SourceRef,
    timestamp: Option<&str>,
) -> Position {
    let record = data.records.iter().find(|record| {
        record.provenance.path == source.path && record.provenance.line == source.line
    });
    Position {
        timestamp: timestamp.and_then(parse_timestamp).or_else(|| {
            record.and_then(|record| record.timestamp.as_deref().and_then(parse_timestamp))
        }),
        sequence: record.map(|record| record.sequence),
        source: source.clone(),
    }
}

fn compare_activity_positions(left: &Activity, right: &Activity) -> Ordering {
    compare_positions(&left.position, &right.position)
        .then_with(|| left.turn_id.cmp(&right.turn_id))
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.description.cmp(&right.description))
}

fn compare_positions(left: &Position, right: &Position) -> Ordering {
    left.timestamp
        .zip(right.timestamp)
        .map(|(left, right)| left.cmp(&right))
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.sequence.cmp(&right.sequence))
        .then_with(|| left.source.path.cmp(&right.source.path))
        .then_with(|| left.source.line.cmp(&right.source.line))
}

fn compare_sources(left: &SourceRef, right: &SourceRef) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.line.cmp(&right.line))
}

fn command_family(command: &str) -> String {
    let mut tokens = command_tokens(command);
    if tokens.is_empty() {
        return "unknown_command".to_owned();
    }
    strip_command_wrappers(&mut tokens);
    if tokens.is_empty() {
        return "unknown_command".to_owned();
    }
    let executable = Path::new(&tokens[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&tokens[0])
        .to_ascii_lowercase();
    let mut family = vec![executable];
    for token in tokens.iter().skip(1) {
        if token.starts_with('-')
            || looks_volatile(token)
            || token.contains('/')
            || token.contains('\\')
        {
            continue;
        }
        family.push(normalize_fragment(token));
        if family.len() == 3 {
            break;
        }
    }
    family.join(" ")
}

fn command_tokens(command: &str) -> Vec<String> {
    if let Ok(value) = serde_json::from_str::<Value>(command) {
        if let Some(tokens) = value.as_array() {
            return tokens
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        if let Some(object) = value.as_object() {
            for key in ["cmd", "command", "argv", "args"] {
                if let Some(value) = object.get(key) {
                    let nested = if let Some(value) = value.as_str() {
                        command_tokens(value)
                    } else if let Some(values) = value.as_array() {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    } else {
                        Vec::new()
                    };
                    if !nested.is_empty() {
                        return nested;
                    }
                }
            }
        }
    }
    command
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|character| character == '\'' || character == '"')
                .to_owned()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

fn strip_command_wrappers(tokens: &mut Vec<String>) {
    loop {
        let Some(first) = tokens.first().map(String::as_str) else {
            return;
        };
        if matches!(first, "env" | "sudo" | "command") {
            tokens.remove(0);
            continue;
        }
        if matches!(first, "sh" | "bash" | "zsh" | "dash") {
            if let Some(index) = tokens
                .iter()
                .position(|token| token == "-c" || token == "-lc")
            {
                let nested = tokens.get(index + 1..).unwrap_or_default().join(" ");
                *tokens = command_tokens(&nested);
                continue;
            }
        }
        return;
    }
}

fn normalize_tool(tool: &str) -> String {
    let normalized = normalize_fragment(tool);
    match normalized.as_str() {
        "shell" | "exec" | "exec_command" => "exec_command".to_owned(),
        "" => "unknown_tool".to_owned(),
        _ => normalized,
    }
}

fn normalize_fragment(value: &str) -> String {
    value
        .split_whitespace()
        .map(|token| {
            let lower = token.to_ascii_lowercase();
            if lower.starts_with("file://") || lower.contains('/') || lower.contains('\\') {
                "<path>".to_owned()
            } else if looks_volatile(&lower) {
                "<id>".to_owned()
            } else {
                lower
                    .trim_matches(|character: char| "()[]{}<>,;:!?`\"'".contains(character))
                    .to_owned()
            }
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_guidance(value: &str) -> String {
    value
        .split_whitespace()
        .map(|token| token.to_ascii_lowercase())
        .map(|token| {
            token
                .trim_matches(|character: char| "`*_#>-. ,;:!?()[]{}\"'".contains(character))
                .to_owned()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_volatile(value: &str) -> bool {
    let value = value.trim_matches(|character: char| "()[]{}<>,;:!?`\"'".contains(character));
    let has_digit = value.chars().any(|character| character.is_ascii_digit());
    (value.len() >= 8 && value.chars().all(|character| character.is_ascii_hexdigit()))
        || (value.split('-').count() >= 4 && value.len() >= 16)
        || (value.len() >= 8 && value.chars().all(|character| character.is_ascii_digit()))
        || (has_digit && value.len() >= 12 && value.split('-').count() >= 3)
}

fn normalize_path(path: &str) -> String {
    let path = path
        .strip_prefix("file://")
        .unwrap_or(path)
        .replace('\\', "/");
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            value => components.push(value),
        }
    }
    let prefix = if path.starts_with('/') { "/" } else { "" };
    format!("{prefix}{}", components.join("/"))
}

fn path_is_excluded(path: &str, options: &AnalysisOptions) -> bool {
    options.excluded_path_prefixes.iter().any(|prefix| {
        let prefix = normalize_path(prefix);
        path == prefix || path.starts_with(&format!("{prefix}/"))
    })
}

fn evidence_for(
    session_id: Option<String>,
    source: SourceRef,
    role: EvidenceRole,
    excerpt: Option<&str>,
    options: &AnalysisOptions,
) -> EvidenceRef {
    EvidenceRef {
        session_id,
        source,
        role,
        excerpt: excerpt.map(|value| bounded_excerpt(value, options.excerpt_max_bytes)),
    }
}

fn push_evidence(target: &mut Vec<EvidenceRef>, evidence: EvidenceRef) {
    if target.iter().any(|existing| {
        existing.session_id == evidence.session_id
            && existing.source.path == evidence.source.path
            && existing.source.line == evidence.source.line
            && existing.role == evidence.role
    }) {
        return;
    }
    if target.len() < DEFAULT_MAX_EVIDENCE {
        target.push(evidence);
    }
}

fn limit_evidence(mut evidence: Vec<EvidenceRef>) -> Vec<EvidenceRef> {
    evidence.truncate(DEFAULT_MAX_EVIDENCE);
    evidence
}

fn bounded_excerpt(value: &str, max_bytes: usize) -> String {
    let mut redacted = value.to_owned();
    for marker in ["token=", "password=", "secret=", "api_key="] {
        redacted = redact_assignment(&redacted, marker);
    }
    let max_bytes = max_bytes.max(3);
    if redacted.len() <= max_bytes {
        return redacted;
    }
    let mut end = max_bytes - 3;
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &redacted[..end])
}

fn redact_assignment(value: &str, marker: &str) -> String {
    let Some(start) = value.to_ascii_lowercase().find(marker) else {
        return value.to_owned();
    };
    let value_start = start + marker.len();
    let end = value[value_start..]
        .find(char::is_whitespace)
        .map_or(value.len(), |offset| value_start + offset);
    format!("{}[redacted]{}", &value[..value_start], &value[end..])
}

fn parse_timestamp(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.len() < 20 {
        return None;
    }
    let year = parse_digits(value, 0, 4)?;
    let month = parse_digits(value, 5, 2)?;
    let day = parse_digits(value, 8, 2)?;
    let hour = parse_digits(value, 11, 2)?;
    let minute = parse_digits(value, 14, 2)?;
    let second = parse_digits(value, 17, 2)?;
    if value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T') && value.as_bytes().get(10) != Some(&b't')
        || value.as_bytes().get(13) != Some(&b':')
        || value.as_bytes().get(16) != Some(&b':')
        || month == 0
        || month > 12
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let suffix = value
        .get(19..)
        .unwrap_or_default()
        .trim_start_matches(|character: char| character == '.' || character.is_ascii_digit());
    let offset = if suffix.eq_ignore_ascii_case("z") {
        0
    } else if suffix.len() >= 6 && (suffix.starts_with('+') || suffix.starts_with('-')) {
        let sign = if suffix.starts_with('+') { 1 } else { -1 };
        let hours = parse_digits(suffix, 1, 2)?;
        let minutes = if suffix.as_bytes().get(3) == Some(&b':') {
            parse_digits(suffix, 4, 2)?
        } else {
            parse_digits(suffix, 3, 2)?
        };
        sign * (hours * 3600 + minutes * 60)
    } else {
        return None;
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

fn parse_digits(value: &str, start: usize, length: usize) -> Option<i64> {
    value.get(start..start + length)?.parse().ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::instructions::snapshot_from_rollout;
    use crate::model::{
        InstructionFileKind, InstructionFileState, InstructionResolution, ProjectRootStatus,
    };
    use crate::normalize::normalize_rollout;
    use crate::rollout::{PlainJsonlReader, parse_rollout_reader};

    fn fixture_data() -> CanonicalData {
        let parsed = parse_rollout_reader(
            Path::new("fixture-analysis.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/analysis/lenses.jsonl"
            ))),
        );
        normalize_rollout(&parsed)
    }

    #[test]
    fn all_phase_three_lenses_rank_synthetic_evidence_deterministically() {
        let data = fixture_data();
        assert_eq!(data.file_operations.len(), 4);

        let findings = analyze_default(&data);
        for kind in [
            FindingType::Failure,
            FindingType::Correction,
            FindingType::Rework,
            FindingType::Stuck,
            FindingType::Verification,
            FindingType::Knowledge,
            FindingType::Gap,
        ] {
            assert!(
                findings.iter().any(|finding| finding.kind == kind),
                "missing {kind:?}"
            );
        }
        let failure = findings
            .iter()
            .find(|finding| finding.kind == FindingType::Failure)
            .unwrap();
        assert_eq!(failure.occurrences, 2);
        assert_eq!(failure.distinct_sessions, 2);
        assert_eq!(
            failure.scope,
            FindingScope::Project(PathBuf::from("/fixture/project"))
        );
        assert!(
            failure
                .evidence
                .iter()
                .all(|evidence| evidence.source.line.is_some())
        );
        let first_output = serde_json::to_string(&findings).unwrap();
        let second_output = serde_json::to_string(&analyze_default(&data)).unwrap();
        assert_eq!(first_output, second_output);
    }

    #[test]
    fn failures_prefer_structured_results_and_do_not_report_one_session() {
        let data = fixture_data();
        let options = AnalysisOptions::default();
        let findings = analyze_failures(&data, &options);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].confidence, FindingConfidence::High);
        assert!(findings[0].key.contains("exit_code_1"));

        let mut one = data.clone();
        one.sessions.pop();
        for item in one.tool_results.iter_mut() {
            item.session_id = Some("fixture-only-session".to_owned());
        }
        assert!(analyze_failures(&one, &options).is_empty());

        let structured = parse_rollout_reader(
            Path::new("structured.jsonl"),
            PlainJsonlReader::new(Cursor::new(include_bytes!(
                "../tests/fixtures/rollout/structured-outcome.jsonl"
            ))),
        );
        assert!(analyze_failures(&normalize_rollout(&structured), &options).is_empty());

        let mut normalized_paths = fixture_data();
        normalized_paths.tool_results[0].command =
            Some(r#"["cargo","test","/volatile/first/path","fixture-id-001"]"#.to_owned());
        normalized_paths.tool_results[1].command =
            Some(r#"["cargo","test","/volatile/second/path","fixture-id-002"]"#.to_owned());
        let duplicate = normalized_paths.tool_results[0].clone();
        normalized_paths.tool_results.push(ToolResult {
            is_duplicate: true,
            ..duplicate
        });
        normalized_paths.sessions[1].project = Some("/fixture/other-project".to_owned());
        let findings = analyze_failures(&normalized_paths, &options);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].occurrences, 2);
        assert_eq!(findings[0].scope, FindingScope::Global);
    }

    #[test]
    fn correction_questions_and_new_requirements_are_not_markers() {
        assert_eq!(correction_fact("How should this be tested?"), None);
        assert_eq!(correction_fact("Implement the new parser."), None);
        assert_eq!(
            correction_fact("Use cargo test instead."),
            Some("cargo test".to_owned())
        );
        let data = fixture_data();
        let findings = analyze_corrections(&data, &AnalysisOptions::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].occurrences, 2);
        assert!(
            findings[0]
                .evidence
                .iter()
                .any(|evidence| evidence.role == EvidenceRole::PrecedingAction)
        );
    }

    #[test]
    fn verification_requires_completion_and_recognizes_observed_commands() {
        let mut data = fixture_data();
        assert!(
            analyze_verification(&data, &AnalysisOptions::default())
                .iter()
                .all(|finding| finding.verification_status == Some(VerificationStatus::Missing))
        );

        data.tool_calls.push(ToolCall {
            id: None,
            call_id: Some("fixture-analysis-check".to_owned()),
            session_id: Some("fixture-analysis-session-b".to_owned()),
            turn_id: Some("fixture-analysis-turn-b".to_owned()),
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: Some("/fixture/project".to_owned()),
            status: None,
            provenance: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 16),
        });
        assert!(
            analyze_verification(&data, &AnalysisOptions::default())
                .iter()
                .filter(|finding| {
                    finding.evidence.iter().any(|evidence| {
                        evidence.session_id.as_deref() == Some("fixture-analysis-session-b")
                    })
                })
                .all(|finding| finding.verification_status != Some(VerificationStatus::Missing))
        );

        let mut incomplete = fixture_data();
        incomplete
            .records
            .retain(|record| record.original_nested_type.as_deref() != Some("turn_complete"));
        for turn in &mut incomplete.turns {
            turn.completed_at = None;
            turn.lifecycle
                .retain(|event| !matches!(event.kind.as_str(), "turn_complete" | "turn_aborted"));
        }
        let findings = analyze_verification(&incomplete, &AnalysisOptions::default());
        assert!(findings
            .iter()
            .all(|finding| finding.verification_status == Some(VerificationStatus::NotObserved)));
    }

    #[test]
    fn generated_paths_are_only_excluded_when_the_caller_supplies_evidence() {
        let mut data = fixture_data();
        for operation in &mut data.file_operations {
            operation.path = "/fixture/generated/output.rs".to_owned();
        }
        assert!(!analyze_rework(&data, &AnalysisOptions::default()).is_empty());
        let options = AnalysisOptions {
            excluded_path_prefixes: vec!["/fixture/generated".to_owned()],
            ..AnalysisOptions::default()
        };
        assert!(analyze_rework(&data, &options).is_empty());
    }

    #[test]
    fn instruction_join_emits_only_documented_categories_and_is_inconclusive_without_snapshot() {
        let mut data = fixture_data();
        let current = "Run cargo lint.".to_owned();
        let root = InstructionFile {
            path: PathBuf::from("/fixture/project/AGENTS.md"),
            scope: InstructionScope::ProjectRoot,
            kind: InstructionFileKind::Standard,
            state: InstructionFileState::Selected,
            chain_position: Some(0),
            content: Some(current.clone()),
            content_hash: Some(crate::instructions::content_hash(current.as_bytes())),
            byte_count: current.len(),
            diagnostic: None,
        };
        let nested = InstructionFile {
            path: PathBuf::from("/fixture/project/src/AGENTS.md"),
            scope: InstructionScope::ProjectNested,
            kind: InstructionFileKind::Standard,
            state: InstructionFileState::Selected,
            chain_position: Some(1),
            content: Some(current.clone()),
            content_hash: Some(crate::instructions::content_hash(current.as_bytes())),
            byte_count: current.len(),
            diagnostic: None,
        };
        let resolution = InstructionResolution {
            project_root: Some(PathBuf::from("/fixture/project")),
            cwd: Some(PathBuf::from("/fixture/project/src")),
            project_root_status: ProjectRootStatus::Known,
            files: vec![root.clone(), nested.clone()],
            chain: vec![root, nested],
            effective_content: Some(format!("{current}\n\n{current}")),
            effective_chain_hash: Some(crate::instructions::content_hash(
                format!("{current}\n\n{current}").as_bytes(),
            )),
            byte_count: current.len() * 2 + 2,
            truncated: true,
            diagnostics: Vec::new(),
        };
        data.instruction_joins = data
            .sessions
            .iter()
            .map(|session| InstructionJoin {
                session_id: session.id.clone(),
                cwd: Some(PathBuf::from("/fixture/project/src")),
                project_root: Some(PathBuf::from("/fixture/project")),
                project_root_status: ProjectRootStatus::Known,
                resolution: resolution.clone(),
                nearest_path: Some(PathBuf::from("/fixture/project/src/AGENTS.md")),
                nearest_scope: Some(InstructionScope::ProjectNested),
                provenance: session.provenance.clone(),
            })
            .collect();
        let old = snapshot_from_rollout(
            Some("fixture-analysis-session-a".to_owned()),
            Some("fixture-analysis-turn-a".to_owned()),
            Some("Run cargo test."),
            SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 2),
        );
        let mut old_b = old.clone();
        old_b.session_id = Some("fixture-analysis-session-b".to_owned());
        data.instruction_snapshots = vec![old, old_b];
        let base = Finding {
            kind: FindingType::Failure,
            severity: FindingSeverity::Medium,
            confidence: FindingConfidence::High,
            scope: FindingScope::Project(PathBuf::from("/fixture/project")),
            key: "exec_command|cargo lint|exit_code_1".to_owned(),
            summary: "synthetic recurring evidence".to_owned(),
            evidence: data
                .sessions
                .iter()
                .map(|session| EvidenceRef {
                    session_id: Some(session.id.clone()),
                    source: session.provenance.clone(),
                    role: EvidenceRole::Observation,
                    excerpt: None,
                })
                .collect(),
            occurrences: 2,
            distinct_sessions: 2,
            affected_paths: vec!["/fixture/project/src/lib.rs".to_owned()],
            observed_commands: vec!["cargo lint".to_owned()],
            sequence: Vec::new(),
            suggested_action: "synthetic".to_owned(),
            limitations: Vec::new(),
            verification_status: None,
        };
        let findings = analyze_instructions(&data, &[base], &AnalysisOptions::default());
        for kind in [
            FindingType::Gap,
            FindingType::Overscoped,
            FindingType::Duplicate,
            FindingType::Stale,
            FindingType::Truncated,
        ] {
            assert!(
                findings.iter().any(|finding| finding.kind == kind),
                "missing {kind:?}"
            );
        }

        data.instruction_snapshots.clear();
        let findings = analyze_instructions(&data, &[], &AnalysisOptions::default());
        assert!(
            findings
                .iter()
                .all(|finding| { !matches!(finding.kind, FindingType::Gap | FindingType::Stale) })
        );
    }
}
