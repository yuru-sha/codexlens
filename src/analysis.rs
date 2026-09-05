//! Deterministic, local-only lenses over canonical session data.
//!
//! The module deliberately accepts [`CanonicalData`] instead of reopening
//! rollout or instruction files.  That keeps upstream field names at the
//! adapter boundary and makes every lens deterministic for the same input.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::model::{
    CanonicalData, InstructionJoin, InstructionSnapshot, Message, MessageRole, SourceRef, ToolCall,
    ToolResult, normalize_path,
};
use serde::{Deserialize, Serialize};

mod failure;
mod instructions;
mod knowledge;
mod verification;

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
    "this project requires ",
    "the project requires ",
    "remember that ",
    "note that ",
];

const DEFAULT_MAX_EVIDENCE: usize = 12;
const MAX_FACT_KEY_BYTES: usize = 128;
const MISSING_SNAPSHOT_LIMITATION: &str = "An instruction snapshot was unavailable for at least one supporting session, so instruction comparison is inconclusive";

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
        .into_iter()
        .filter(|finding| finding.kind == FindingType::Stuck)
        .collect()
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
    instructions::analyze(data, recurring_findings, options)
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
            .then_with(|| normalize_fragment(&left.key).cmp(&normalize_fragment(&right.key)))
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

pub fn analyze_failures(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    failure::analyze(data, options)
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

pub fn analyze_rework(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    let edits = edit_events(data, options);
    let failures = failure::activities(data);
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
                "{} edits to {} occurred within the {} rework window",
                count,
                path,
                rework_window_label(options.rework_window_seconds)
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
                .map(|(_, edit)| {
                    bounded_excerpt(
                        &format!("edit {}", edit.path),
                        options.excerpt_max_bytes,
                    )
                })
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
                sequence.push(bounded_excerpt(
                    &activity.description,
                    options.excerpt_max_bytes,
                ));
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
                .map(|activity| bounded_excerpt(&activity.description, options.excerpt_max_bytes))
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
    annotate_snapshot_limitations(data, &mut findings);
    sort_findings(&mut findings);
    findings
}

pub fn analyze_verification(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    verification::analyze(data, options)
}

pub fn analyze_knowledge(data: &CanonicalData, options: &AnalysisOptions) -> Vec<Finding> {
    knowledge::analyze(data, options)
}

fn matching_call<'a>(data: &'a CanonicalData, result: &ToolResult) -> Option<&'a ToolCall> {
    let candidates = data
        .tool_calls
        .iter()
        .filter(|call| {
            call.call_id == result.call_id
                && call.call_id.is_some()
                && call_result_context_matches(
                    call.session_id.as_deref(),
                    result.session_id.as_deref(),
                )
                && call_result_context_matches(call.turn_id.as_deref(), result.turn_id.as_deref())
        })
        .collect::<Vec<_>>();
    let exact = candidates
        .iter()
        .copied()
        .filter(|call| {
            context_matches(call.session_id.as_deref(), result.session_id.as_deref())
                && context_matches(call.turn_id.as_deref(), result.turn_id.as_deref())
        })
        .collect::<Vec<_>>();
    if let Some(call) = exact.first().copied() {
        return Some(call);
    }
    (candidates.len() == 1)
        .then(|| candidates.into_iter().next())
        .flatten()
}

fn context_matches(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        _ => false,
    }
}

fn call_result_context_matches(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn same_call(left: &ToolCall, right: &ToolCall) -> bool {
    left.call_id == right.call_id
        && left.session_id == right.session_id
        && left.turn_id == right.turn_id
        && left.provenance == right.provenance
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

fn normalize_fact(text: &str) -> String {
    text.split_whitespace()
        .map(normalize_fact_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_fact_token(value: &str) -> String {
    let lower = value.to_ascii_lowercase().replace('\\', "/");
    let token = lower
        .trim_matches(|character: char| ".,;:!?`\"'".contains(character))
        .to_owned();
    if token.starts_with("file://") || token.starts_with('/') {
        let path = normalize_path(&token);
        return format!(
            "<path:{}>",
            crate::instructions::content_hash(path.as_bytes())
        );
    }
    if token.contains('/') {
        return normalize_path(&token);
    }
    if looks_volatile(&token) {
        "<id>".to_owned()
    } else {
        token
    }
}

fn bounded_fingerprint(value: &str) -> String {
    let redacted = redact_sensitive(value);
    let normalized = normalize_fact(&redacted);
    if normalized.len() <= MAX_FACT_KEY_BYTES {
        return normalized;
    }
    let hash = crate::instructions::content_hash(normalized.as_bytes());
    let suffix = format!("~{hash}");
    format!(
        "{}{}",
        truncate_bytes(&normalized, MAX_FACT_KEY_BYTES.saturating_sub(suffix.len())),
        suffix
    )
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

fn verification_kind(command: &str) -> Option<String> {
    let mut tokens = command_tokens(command);
    strip_command_wrappers(&mut tokens);
    let lower = tokens
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if lower
        .iter()
        .any(|token| matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return None;
    }
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
        "ruff" => lower.get(1).and_then(|value| match value.as_str() {
            "check" => Some("lint"),
            "format" => Some("format"),
            _ => None,
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
        _ => None,
    }?;
    Some(kind.to_owned())
}

fn call_has_observed_result(data: &CanonicalData, call: &ToolCall) -> bool {
    if let Some(call_id) = call.call_id.as_ref() {
        return data.tool_results.iter().any(|result| {
            !result.is_duplicate
                && result.call_id.as_ref() == Some(call_id)
                && matching_call(data, result).is_some_and(|candidate| same_call(candidate, call))
        });
    }

    let no_id_calls = data
        .tool_calls
        .iter()
        .filter(|candidate| {
            candidate.call_id.is_none()
                && context_matches(candidate.session_id.as_deref(), call.session_id.as_deref())
                && context_matches(candidate.turn_id.as_deref(), call.turn_id.as_deref())
        })
        .count();
    if no_id_calls != 1 {
        return false;
    }
    data.tool_results
        .iter()
        .filter(|result| {
            !result.is_duplicate
                && result.call_id.is_none()
                && context_matches(result.session_id.as_deref(), call.session_id.as_deref())
                && context_matches(result.turn_id.as_deref(), call.turn_id.as_deref())
        })
        .count()
        == 1
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
        "test" => Some("test"),
        "lint" | "check" | "vet" => Some("lint"),
        "format" | "fmt" => Some("format"),
        "build" => Some("build"),
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

    let edit_turn_ids = edits
        .iter()
        .filter_map(|edit| edit.turn_id.as_deref())
        .collect::<HashSet<_>>();
    let relevant_failures = failures.iter().filter(|failure| {
        failure.session_id
            == edits
                .first()
                .map(|edit| edit.session_id.as_str())
                .unwrap_or_default()
            && failure
                .turn_id
                .as_deref()
                .is_none_or(|turn_id| edit_turn_ids.contains(turn_id))
    });
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
    activities.extend(relevant_failures.cloned());
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

fn rework_window_label(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds % 60 == 0 {
        format!("{}-minute", seconds / 60)
    } else {
        format!("{seconds}-second")
    }
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
                && turn.completed_at.is_some()
        }) {
            return true;
        }
    }
    data.records.iter().any(|record| {
        record.session_id.as_deref() == Some(session_id)
            && context_matches(record.turn_id.as_deref(), turn_id)
            && record.is_terminal
    })
}

fn snapshot_for_evidence<'a>(
    data: &'a CanonicalData,
    evidence: &EvidenceRef,
) -> Option<&'a InstructionSnapshot> {
    let session_id = evidence.session_id.as_deref()?;
    let mut snapshots = data.instruction_snapshots.iter().filter(|snapshot| {
        snapshot.session_id.as_deref() == Some(session_id) && snapshot_is_usable(snapshot)
    });
    let exact = snapshots.clone().find(|snapshot| {
        snapshot.provenance.path == evidence.source.path
            && snapshot.provenance.line == evidence.source.line
    });
    if exact.is_some() {
        return exact;
    }
    let turn_id = data.records.iter().find_map(|record| {
        (record.provenance.path == evidence.source.path
            && record.provenance.line == evidence.source.line)
            .then_some(record.turn_id.as_deref())
            .flatten()
    })?;
    snapshots.find(|snapshot| snapshot.turn_id.as_deref() == Some(turn_id))
}

fn snapshot_is_unavailable(snapshot: &InstructionSnapshot) -> bool {
    snapshot.source == crate::model::InstructionSnapshotSource::Unavailable
        || snapshot.accuracy == crate::model::InstructionSnapshotAccuracy::Unavailable
}

fn snapshot_is_usable(snapshot: &InstructionSnapshot) -> bool {
    snapshot.content.is_some() && !snapshot_is_unavailable(snapshot) && !snapshot.truncated
}

fn resolve_instruction_path(join: &InstructionJoin, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(normalize_path(path));
    if path.is_absolute() {
        return Some(path);
    }
    let base = join.cwd.as_ref().or(join.project_root.as_ref())?;
    Some(PathBuf::from(normalize_path(
        &base.join(path).to_string_lossy(),
    )))
}

fn annotate_snapshot_limitations(data: &CanonicalData, findings: &mut [Finding]) {
    for finding in findings {
        let sessions = evidence_sessions(finding);
        let incomplete_evidence = sessions.len() < finding.distinct_sessions;
        let missing_snapshot = finding.evidence.iter().any(|evidence| {
            evidence.session_id.is_some() && snapshot_for_evidence(data, evidence).is_none()
        });
        if (incomplete_evidence || missing_snapshot)
            && !finding
                .limitations
                .iter()
                .any(|limitation| limitation == MISSING_SNAPSHOT_LIMITATION)
        {
            finding
                .limitations
                .push(MISSING_SNAPSHOT_LIMITATION.to_owned());
        }
    }
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

fn instruction_scope(data: &CanonicalData, finding: &Finding, sessions: &[String]) -> FindingScope {
    let session_ids = sessions.iter().map(String::as_str).collect::<Vec<_>>();
    if let Some(path) = finding.affected_paths.first() {
        return path_scope(data, &session_ids, path);
    }
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
    majority_path(nearest)
        .map(FindingScope::Instruction)
        .unwrap_or_else(|| finding.scope.clone())
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
    let target = resolve_instruction_path(join, path)?;
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
    let mut tokens = command_tokens(&redact_sensitive(command));
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
        let normalized = token.to_ascii_lowercase();
        if token.starts_with('-') || !SAFE_COMMAND_WORDS.contains(&normalized.as_str()) {
            continue;
        }
        family.push(normalized);
        if family.len() == 3 {
            break;
        }
    }
    family.join(" ")
}

const SAFE_COMMAND_WORDS: &[&str] = &[
    "build",
    "check",
    "clippy",
    "diff",
    "eslint",
    "fmt",
    "format",
    "lint",
    "mypy",
    "nextest",
    "prettier",
    "pytest",
    "run",
    "test",
    "typecheck",
    "vet",
];

fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            token.push(character);
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                token.push(character);
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(character);
        }
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn strip_command_wrappers(tokens: &mut Vec<String>) {
    loop {
        let Some(first) = tokens.first().map(String::as_str) else {
            return;
        };
        if first == "env" {
            tokens.remove(0);
            strip_wrapper_arguments(tokens, &["-C", "--chdir", "-u", "--unset"]);
            continue;
        }
        if first == "sudo" {
            tokens.remove(0);
            strip_wrapper_arguments(tokens, &["-C", "--chdir", "-u", "--user", "-g", "--group"]);
            continue;
        }
        if first == "command" {
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

fn strip_wrapper_arguments(tokens: &mut Vec<String>, options_with_values: &[&str]) {
    loop {
        let Some(first) = tokens.first() else {
            return;
        };
        if first == "--" {
            tokens.remove(0);
            return;
        }
        if first.contains('=') {
            tokens.remove(0);
            continue;
        }
        if !first.starts_with('-') {
            return;
        }
        let option = tokens.remove(0);
        if options_with_values.contains(&option.as_str()) && !tokens.is_empty() {
            tokens.remove(0);
        }
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
        .map(normalize_guidance_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_guidance_token(value: &str) -> String {
    normalize_fact_token(value)
}

fn looks_volatile(value: &str) -> bool {
    let value = value.trim_matches(|character: char| "()[]{}<>,;:!?`\"'".contains(character));
    let has_digit = value.chars().any(|character| character.is_ascii_digit());
    (value.len() >= 8 && value.chars().all(|character| character.is_ascii_hexdigit()))
        || (value.split('-').count() >= 4 && value.len() >= 16)
        || (value.len() >= 8 && value.chars().all(|character| character.is_ascii_digit()))
        || (has_digit && value.len() >= 12 && value.split('-').count() >= 3)
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

fn reserve_evidence_slots(evidence: &[EvidenceRef], slots: usize) -> Vec<EvidenceRef> {
    let mut evidence = evidence.to_vec();
    evidence.truncate(DEFAULT_MAX_EVIDENCE.saturating_sub(slots));
    evidence
}

pub(crate) fn bounded_excerpt(value: &str, max_bytes: usize) -> String {
    let redacted = redact_sensitive(value);
    let max_bytes = max_bytes.max(3);
    if redacted.len() <= max_bytes {
        return redacted;
    }
    let truncated = truncate_bytes(&redacted, max_bytes - 3);
    format!("{truncated}...")
}

fn redact_sensitive(value: &str) -> String {
    let mut redacted = value.to_owned();
    for marker in ["token=", "password=", "secret=", "api_key="] {
        redacted = redact_assignment(&redacted, marker);
    }
    for name in [
        "token",
        "password",
        "secret",
        "api_key",
        "api-key",
        "access_token",
        "refresh_token",
        "client_secret",
        "secret_key",
        "private_key",
    ] {
        redacted = redact_named_value(&redacted, name);
    }
    redacted
}

fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    let end = value.len().min(max_bytes);
    let mut end = end;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn redact_assignment(value: &str, marker: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let mut cursor = 0;
    let mut redacted = String::with_capacity(value.len());
    while let Some(offset) = lower.get(cursor..).and_then(|rest| rest.find(marker)) {
        let start = cursor + offset;
        let value_start = start + marker.len();
        let suffix = &value[value_start..];
        let end = match suffix.chars().next() {
            Some(delimiter @ ('\'' | '"')) => suffix[1..]
                .find(delimiter)
                .map_or(value.len(), |offset| value_start + offset + 2),
            _ => suffix
                .find(char::is_whitespace)
                .map_or(value.len(), |offset| value_start + offset),
        };
        redacted.push_str(&value[cursor..value_start]);
        redacted.push_str("[redacted]");
        cursor = end;
    }
    redacted.push_str(&value[cursor..]);
    redacted
}

fn redact_named_value(value: &str, name: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let mut cursor = 0;
    let mut redacted = String::with_capacity(value.len());
    while let Some(offset) = lower.get(cursor..).and_then(|rest| rest.find(name)) {
        let start = cursor + offset;
        let before = value[..start].chars().next_back();
        let after_name = start + name.len();
        let after = value.get(after_name..).unwrap_or_default();
        if before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            || after
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            cursor = after_name;
            continue;
        }

        let mut delimiter_start = after_name;
        if value
            .get(delimiter_start..)
            .is_some_and(|rest| rest.starts_with(['\'', '"']))
        {
            delimiter_start += 1;
        }
        let whitespace_start = delimiter_start;
        while value
            .get(delimiter_start..)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
        {
            delimiter_start += value[delimiter_start..]
                .chars()
                .next()
                .expect("whitespace was present")
                .len_utf8();
        }
        let delimiter = value
            .get(delimiter_start..)
            .and_then(|rest| rest.chars().next());
        let cli_flag = value[..start].ends_with("--") && delimiter_start > whitespace_start;
        if !matches!(delimiter, Some('=') | Some(':')) && !cli_flag {
            cursor = after_name;
            continue;
        }

        let value_start = if cli_flag {
            delimiter_start
        } else {
            delimiter_start + delimiter.expect("assignment delimiter").len_utf8()
        };
        let mut value_start = value_start;
        while value
            .get(value_start..)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
        {
            value_start += value[value_start..]
                .chars()
                .next()
                .expect("whitespace was present")
                .len_utf8();
        }
        if value_start >= value.len() {
            cursor = value_start;
            continue;
        }
        let quoted = value[value_start..]
            .chars()
            .next()
            .filter(|character| matches!(character, '\'' | '"'));
        let content_start = value_start + quoted.map_or(0, char::len_utf8);
        let end = secret_value_end(value, content_start, quoted);
        redacted.push_str(&value[cursor..content_start]);
        redacted.push_str("[redacted]");
        if let Some(delimiter) = quoted {
            redacted.push(delimiter);
        }
        cursor = end;
    }
    redacted.push_str(&value[cursor..]);
    redacted
}

fn secret_value_end(value: &str, start: usize, quoted: Option<char>) -> usize {
    if let Some(delimiter) = quoted {
        let mut escaped = false;
        for (offset, character) in value[start..].char_indices() {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                return start + offset;
            }
        }
        return value.len();
    }
    let Some(first) = value.as_bytes().get(start).copied() else {
        return start;
    };
    if matches!(first, b'{' | b'[') {
        let mut depth = 0usize;
        let mut quoted = None;
        let mut escaped = false;
        for (offset, byte) in value[start..].bytes().enumerate() {
            if let Some(delimiter) = quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == delimiter {
                    quoted = None;
                }
                continue;
            }
            if matches!(byte, b'\'' | b'"') {
                quoted = Some(byte);
            } else if matches!(byte, b'{' | b'[') {
                depth += 1;
            } else if matches!(byte, b'}' | b']') {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return start + offset + 1;
                }
            }
        }
        return value.len();
    }
    value[start..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace() || matches!(character, ',' | '}' | ']'))
        .map_or(value.len(), |(offset, _)| start + offset)
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
    let suffix = value.get(19..).unwrap_or_default();
    let suffix = if let Some(fraction) = suffix.strip_prefix('.') {
        let fraction_len = fraction
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if fraction_len == 0 {
            return None;
        }
        &fraction[fraction_len..]
    } else {
        suffix
    };
    let offset = if suffix.eq_ignore_ascii_case("z") {
        0
    } else if suffix.len() == 6
        && (suffix.starts_with('+') || suffix.starts_with('-'))
        && suffix.as_bytes().get(3) == Some(&b':')
    {
        let sign = if suffix.starts_with('+') { 1 } else { -1 };
        let hours = parse_digits(suffix, 1, 2)?;
        let minutes = parse_digits(suffix, 4, 2)?;
        if hours > 23 || minutes > 59 {
            return None;
        }
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
        FileOperation, InstructionFile, InstructionFileKind, InstructionFileState,
        InstructionResolution, InstructionScope, OutcomeSource, ProjectRootStatus, Record,
        RecordKind, ToolOutcome,
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
        let knowledge = findings
            .iter()
            .find(|finding| finding.kind == FindingType::Knowledge)
            .unwrap();
        assert_eq!(knowledge.occurrences, 2);
        assert_eq!(knowledge.distinct_sessions, 2);
        assert!(knowledge.key.len() <= MAX_FACT_KEY_BYTES);
        assert!(!knowledge.evidence.is_empty());
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
            Some("cargo test /volatile/first/path fixture-id-001".to_owned());
        normalized_paths.tool_results[1].command =
            Some("cargo test /volatile/second/path fixture-id-002".to_owned());
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

        let mut without_snapshots = fixture_data();
        without_snapshots.instruction_snapshots.clear();
        assert!(
            analyze_failures(&without_snapshots, &options)[0]
                .limitations
                .iter()
                .any(|limitation| limitation == MISSING_SNAPSHOT_LIMITATION)
        );

        let mut missing_one_turn = fixture_data();
        missing_one_turn.instruction_snapshots.retain(|snapshot| {
            snapshot.session_id.as_deref() == Some("fixture-analysis-session-a")
        });
        missing_one_turn
            .instruction_snapshots
            .push(snapshot_from_rollout(
                Some("fixture-analysis-session-b".to_owned()),
                Some("fixture-analysis-unrelated-turn".to_owned()),
                Some("Run cargo build."),
                SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 99),
            ));
        assert!(
            analyze_failures(&missing_one_turn, &options)[0]
                .limitations
                .iter()
                .any(|limitation| limitation == MISSING_SNAPSHOT_LIMITATION)
        );
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
    fn bounded_fingerprints_redact_all_values_and_keep_marker_contract() {
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
        assert_eq!(
            classify_verification_command("env cargo test"),
            Some("test".to_owned())
        );
        assert_eq!(
            classify_verification_command("env FOO=bar cargo test"),
            Some("test".to_owned())
        );
        assert_eq!(
            classify_verification_command("sudo -u runner cargo test"),
            Some("test".to_owned())
        );
        assert_eq!(
            classify_verification_command("bash -lc \"cargo test\""),
            Some("test".to_owned())
        );
        assert_eq!(classify_verification_command("custom-test"), None);
        assert_eq!(classify_verification_command("cargo tests"), None);
        assert_eq!(classify_verification_command("pytest --version"), None);
        assert_eq!(classify_verification_command("mypy --help"), None);
        assert_eq!(classify_verification_command("ruff"), None);
        assert_eq!(classify_verification_command("ruff --help"), None);
        assert_eq!(
            classify_verification_command("ruff check src"),
            Some("lint".to_owned())
        );
        let excerpt = bounded_excerpt("token=first token=second", 512);
        assert_eq!(excerpt, "token=[redacted] token=[redacted]");
        assert_eq!(
            bounded_excerpt(r#"cargo test token="a b""#, 512),
            r#"cargo test token=[redacted]"#
        );
        let structured = bounded_excerpt(
            r#"tool {"token": "json-secret", "password": "json-password"}"#,
            512,
        );
        assert!(!structured.contains("json-secret"));
        assert!(!structured.contains("json-password"));
        let compound = bounded_excerpt(
            r#"tool {"access_token": "access-secret", "client_secret": "client-secret"}"#,
            512,
        );
        assert!(!compound.contains("access-secret"));
        assert!(!compound.contains("client-secret"));
        let cli = bounded_excerpt("tool --token cli-secret --api-key cli-key", 512);
        assert!(!cli.contains("cli-secret"));
        assert!(!cli.contains("cli-key"));
        assert_eq!(
            command_family(r#"cargo test token="a b" fixture-id-001"#),
            "cargo test"
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

        let mut cross_turn = fixture_data();
        cross_turn.records.extend([
            Record {
                session_id: Some("fixture-analysis-session-a".to_owned()),
                turn_id: Some("fixture-analysis-turn-a-2".to_owned()),
                timestamp: Some("2026-01-03T00:06:00.000Z".to_owned()),
                sequence: 17,
                original_record_type: Some("response_item".to_owned()),
                original_nested_type: Some("custom_tool_call".to_owned()),
                error_category: None,
                is_error: false,
                is_terminal: false,
                kind: RecordKind::ResponseItem,
                provenance: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 17),
            },
            Record {
                session_id: Some("fixture-analysis-session-a".to_owned()),
                turn_id: Some("fixture-analysis-turn-a-2".to_owned()),
                timestamp: Some("2026-01-03T00:07:00.000Z".to_owned()),
                sequence: 18,
                original_record_type: Some("event_msg".to_owned()),
                original_nested_type: Some("exec_command_end".to_owned()),
                error_category: None,
                is_error: false,
                is_terminal: false,
                kind: RecordKind::EventMessage,
                provenance: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 18),
            },
        ]);
        cross_turn.tool_calls.push(ToolCall {
            id: None,
            call_id: Some("fixture-analysis-cross-turn-check".to_owned()),
            session_id: Some("fixture-analysis-session-a".to_owned()),
            turn_id: Some("fixture-analysis-turn-a-2".to_owned()),
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: Some("/fixture/project".to_owned()),
            status: None,
            provenance: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 17),
        });
        cross_turn.tool_results.push(ToolResult {
            id: None,
            call_id: Some("fixture-analysis-cross-turn-check".to_owned()),
            session_id: Some("fixture-analysis-session-a".to_owned()),
            turn_id: Some("fixture-analysis-turn-a-2".to_owned()),
            command: Some("cargo test".to_owned()),
            cwd: Some("/fixture/project".to_owned()),
            stdout: None,
            stderr: None,
            duration_ms: None,
            exit_code: Some(0),
            status: Some("completed".to_owned()),
            outcome: ToolOutcome::Succeeded,
            outcome_source: OutcomeSource::ExitCode,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 18),
        });
        let cross_turn_findings = analyze_verification(&cross_turn, &AnalysisOptions::default());
        assert!(
            cross_turn_findings.iter().all(|finding| {
                !finding.evidence.iter().any(|evidence| {
                    evidence.session_id.as_deref() == Some("fixture-analysis-session-a")
                })
            }),
            "a later-turn verification must cover the preceding edit batch"
        );

        let mut incomplete_later_turn = cross_turn.clone();
        incomplete_later_turn.tool_results.retain(|result| {
            result.call_id.as_deref() != Some("fixture-analysis-cross-turn-check")
        });
        let incomplete_later_turn_findings =
            analyze_verification(&incomplete_later_turn, &AnalysisOptions::default());
        assert!(
            incomplete_later_turn_findings.iter().any(|finding| {
                finding.verification_status == Some(VerificationStatus::NotObserved)
                    && finding.evidence.iter().any(|evidence| {
                        evidence.session_id.as_deref() == Some("fixture-analysis-session-a")
                    })
            }),
            "an incomplete later-turn verification must preserve not_observed"
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
                .all(|finding| {
                    finding.verification_status == Some(VerificationStatus::NotObserved)
                })
        );
        data.tool_results.push(ToolResult {
            id: None,
            call_id: Some("fixture-analysis-check".to_owned()),
            session_id: Some("fixture-analysis-session-b".to_owned()),
            turn_id: Some("fixture-analysis-turn-b".to_owned()),
            command: Some("cargo test".to_owned()),
            cwd: Some("/fixture/project".to_owned()),
            stdout: None,
            stderr: None,
            duration_ms: None,
            exit_code: Some(0),
            status: Some("completed".to_owned()),
            outcome: ToolOutcome::Succeeded,
            outcome_source: OutcomeSource::ExitCode,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 17),
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
            .retain(|record| !matches!(record.provenance.line, Some(8 | 16)));
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
    fn missing_context_does_not_match_another_turn() {
        let data = CanonicalData {
            file_operations: vec![FileOperation {
                session_id: Some("fixture-session".to_owned()),
                turn_id: None,
                path: "src/lib.rs".to_owned(),
                operation: "edit".to_owned(),
                timestamp: None,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 1),
            }],
            records: vec![Record {
                session_id: Some("fixture-session".to_owned()),
                turn_id: Some("other-turn".to_owned()),
                timestamp: None,
                sequence: 2,
                original_record_type: Some("event_msg".to_owned()),
                original_nested_type: Some("turn_complete".to_owned()),
                error_category: None,
                is_error: false,
                is_terminal: true,
                kind: RecordKind::EventMessage,
                provenance: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 2),
            }],
            ..CanonicalData::default()
        };
        let findings = analyze_verification(&data, &AnalysisOptions::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].verification_status,
            Some(VerificationStatus::NotObserved)
        );
        assert!(!context_matches(None, Some("other-turn")));
    }

    #[test]
    fn timestamps_require_a_valid_rfc3339_offset() {
        assert!(parse_timestamp("2026-01-01T00:00:00Z").is_some());
        assert!(parse_timestamp("2026-01-01T00:00:00.123+09:00").is_some());
        assert!(parse_timestamp("2026-01-01T00:00:00+0900").is_none());
        assert!(parse_timestamp("2026-01-01T00:00:00+09:00junk").is_none());
        assert!(parse_timestamp("2026-01-01T00:00:00+99:99").is_none());
    }

    #[test]
    fn stuck_wrapper_filters_rework_and_window_summary_uses_options() {
        let data = fixture_data();
        assert!(
            !stuck(&data).is_empty()
                && stuck(&data)
                    .iter()
                    .all(|finding| finding.kind == FindingType::Stuck)
        );
        let options = AnalysisOptions {
            rework_window_seconds: 180,
            ..AnalysisOptions::default()
        };
        let findings = analyze_rework(&data, &options);
        assert!(
            findings
                .iter()
                .filter(|finding| finding.kind == FindingType::Rework)
                .all(|finding| finding.summary.contains("3-minute rework window"))
        );
    }

    #[test]
    fn rework_requires_edits_inside_the_configured_window() {
        let data = fixture_data();
        let options = AnalysisOptions {
            rework_window_seconds: 30,
            ..AnalysisOptions::default()
        };

        assert!(analyze_rework(&data, &options).is_empty());
    }

    #[test]
    fn unrelated_failures_do_not_create_a_file_loop() {
        let mut data = fixture_data();
        for result in &mut data.tool_results {
            if failure::is_failed(result) {
                result.turn_id = Some("unrelated-turn".to_owned());
            }
        }

        assert!(
            analyze_rework(&data, &AnalysisOptions::default())
                .iter()
                .all(|finding| finding.kind == FindingType::Rework)
        );
    }

    #[test]
    fn missing_turn_ids_still_allow_same_window_stuck_detection() {
        let mut data = fixture_data();
        for operation in &mut data.file_operations {
            operation.turn_id = None;
        }
        for result in &mut data.tool_results {
            if failure::is_failed(result) {
                result.turn_id = None;
            }
        }

        assert!(
            analyze_rework(&data, &AnalysisOptions::default())
                .iter()
                .any(|finding| finding.kind == FindingType::Stuck)
        );
    }

    #[test]
    fn finding_order_uses_normalized_key_before_occurrences() {
        let finding = |key: &str, occurrences| Finding {
            kind: FindingType::Failure,
            severity: FindingSeverity::Medium,
            confidence: FindingConfidence::Medium,
            scope: FindingScope::Global,
            key: key.to_owned(),
            summary: String::new(),
            evidence: vec![EvidenceRef {
                session_id: Some("session".to_owned()),
                source: SourceRef::rollout(PathBuf::from("fixture.jsonl"), 1),
                role: EvidenceRole::Observation,
                excerpt: None,
            }],
            occurrences,
            distinct_sessions: 1,
            affected_paths: Vec::new(),
            observed_commands: Vec::new(),
            sequence: Vec::new(),
            suggested_action: String::new(),
            limitations: Vec::new(),
            verification_status: None,
        };
        let mut findings = vec![finding("z", 2), finding("a", 1)];
        sort_findings(&mut findings);
        assert_eq!(findings[0].key, "a");
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
            truncated: false,
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
        old_b.turn_id = Some("fixture-analysis-turn-b".to_owned());
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
                .enumerate()
                .map(|(index, session)| EvidenceRef {
                    session_id: Some(session.id.clone()),
                    source: SourceRef::rollout(
                        PathBuf::from("fixture-analysis.jsonl"),
                        if index == 0 { 5 } else { 14 },
                    ),
                    role: EvidenceRole::Observation,
                    excerpt: None,
                })
                .collect(),
            occurrences: 2,
            distinct_sessions: 2,
            affected_paths: vec!["src/lib.rs".to_owned()],
            observed_commands: vec!["cargo lint".to_owned()],
            sequence: Vec::new(),
            suggested_action: "synthetic".to_owned(),
            limitations: Vec::new(),
            verification_status: None,
        };
        assert!(snapshot_for_evidence(&data, &base.evidence[0]).is_some());
        assert!(snapshot_for_evidence(&data, &base.evidence[1]).is_some());

        let mut matching = data.instruction_snapshots[0].clone();
        matching.turn_id = Some("fixture-analysis-turn-a-2".to_owned());
        matching.provenance = SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 6);
        matching.content = Some(current.clone());
        matching.content_hash = Some(crate::instructions::content_hash(current.as_bytes()));
        matching.byte_count = current.len();
        let mut cross_turn_data = data.clone();
        cross_turn_data.instruction_snapshots.push(matching);
        let mut cross_turn = base.clone();
        cross_turn.evidence = vec![
            base.evidence[0].clone(),
            EvidenceRef {
                session_id: Some("fixture-analysis-session-a".to_owned()),
                source: SourceRef::rollout(PathBuf::from("fixture-analysis.jsonl"), 6),
                role: EvidenceRole::Observation,
                excerpt: None,
            },
        ];
        cross_turn.occurrences = 2;
        cross_turn.distinct_sessions = 1;
        let cross_turn_findings = analyze_instructions(
            &cross_turn_data,
            std::slice::from_ref(&cross_turn),
            &AnalysisOptions::default(),
        );
        assert!(
            cross_turn_findings
                .iter()
                .any(|finding| finding.kind == FindingType::Gap),
            "a matching snapshot from another turn must not hide a mismatch"
        );

        let findings = analyze_instructions(
            &data,
            std::slice::from_ref(&base),
            &AnalysisOptions::default(),
        );
        for kind in [FindingType::Gap, FindingType::Duplicate, FindingType::Stale] {
            assert!(
                findings.iter().any(|finding| finding.kind == kind),
                "missing {kind:?}"
            );
        }
        assert!(
            findings
                .iter()
                .all(|finding| finding.kind != FindingType::Truncated)
        );
        let gap = findings
            .iter()
            .find(|finding| finding.kind == FindingType::Gap)
            .unwrap();
        assert_eq!(
            gap.scope,
            FindingScope::Instruction(PathBuf::from("/fixture/project/src/AGENTS.md"))
        );
        let duplicate = findings
            .iter()
            .find(|finding| finding.kind == FindingType::Duplicate)
            .unwrap();
        assert_eq!(duplicate.evidence.len(), 2);
        assert!(
            duplicate
                .evidence
                .iter()
                .all(|evidence| evidence.source.kind == crate::model::SourceKind::State)
        );

        let mut truncated_current = data.clone();
        for join in &mut truncated_current.instruction_joins {
            join.resolution.truncated = true;
        }
        let findings = analyze_instructions(
            &truncated_current,
            std::slice::from_ref(&base),
            &AnalysisOptions::default(),
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.kind == FindingType::Truncated)
        );
        assert!(findings.iter().all(|finding| {
            !matches!(
                finding.kind,
                FindingType::Overscoped | FindingType::Duplicate | FindingType::Stale
            )
        }));

        let mut missing_session = data.clone();
        missing_session.instruction_snapshots.pop();
        let findings = analyze_instructions(
            &missing_session,
            std::slice::from_ref(&base),
            &AnalysisOptions::default(),
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.kind != FindingType::Gap)
        );

        data.instruction_snapshots.clear();
        let findings = analyze_instructions(&data, &[], &AnalysisOptions::default());
        assert!(
            findings
                .iter()
                .all(|finding| { !matches!(finding.kind, FindingType::Gap | FindingType::Stale) })
        );

        let mut capped = base.clone();
        capped.distinct_sessions = 3;
        let findings = analyze_instructions(
            &missing_session,
            std::slice::from_ref(&capped),
            &AnalysisOptions::default(),
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.kind != FindingType::Gap)
        );

        let mut global_data = missing_session.clone();
        let global = InstructionFile {
            path: PathBuf::from("/fixture/codex/AGENTS.md"),
            scope: InstructionScope::Global,
            kind: InstructionFileKind::Standard,
            state: InstructionFileState::Selected,
            chain_position: Some(0),
            content: Some("Run cargo lint.".to_owned()),
            content_hash: Some(crate::instructions::content_hash(b"Run cargo lint.")),
            byte_count: 15,
            diagnostic: None,
        };
        for join in &mut global_data.instruction_joins {
            let nested = join
                .resolution
                .chain
                .iter()
                .find(|file| file.scope == InstructionScope::ProjectNested)
                .cloned()
                .unwrap();
            join.resolution.chain = vec![
                global.clone(),
                InstructionFile {
                    content: Some("Local path notes.".to_owned()),
                    ..nested
                },
            ];
        }
        let findings = analyze_instructions(
            &global_data,
            std::slice::from_ref(&base),
            &AnalysisOptions::default(),
        );
        assert_eq!(
            findings
                .iter()
                .find(|finding| finding.kind == FindingType::Overscoped)
                .map(|finding| &finding.scope),
            Some(&FindingScope::Instruction(global.path))
        );

        let mut truncated_snapshot = data.clone();
        for snapshot in &mut truncated_snapshot.instruction_snapshots {
            snapshot.truncated = true;
        }
        let findings = analyze_instructions(
            &truncated_snapshot,
            std::slice::from_ref(&base),
            &AnalysisOptions::default(),
        );
        assert!(
            findings
                .iter()
                .all(|finding| !matches!(finding.kind, FindingType::Gap | FindingType::Stale))
        );
    }
}
