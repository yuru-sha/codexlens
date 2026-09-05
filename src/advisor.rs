//! Advisory reports and read-only instruction proposals.
//!
//! `doctor` consumes canonical data and stored instruction snapshots.
//! `optimize --diff` reads recommended instruction files to render a diff and
//! never writes them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::analysis::{
    EvidenceRef, Finding, FindingConfidence, FindingScope, FindingType, VerificationStatus,
    bounded_excerpt, sort_findings,
};
use crate::model::{
    CanonicalData, InstructionFile, InstructionFileState, InstructionScope, ProjectRootStatus,
    SourceRef, normalize_path,
};
use crate::store::StoreFreshness;

const MAX_PROPOSAL_TEXT_BYTES: usize = 512;
const MAX_REPORT_EVIDENCE: usize = 12;
const DEFAULT_REPORT_EXCERPT_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalAction {
    Add,
    /// Reserved for caller-supplied replacements with an approved old value.
    Modify,
    Remove,
    MoveToDocs,
    SplitScope,
}

impl ProposalAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Modify => "modify",
            Self::Remove => "remove",
            Self::MoveToDocs => "move_to_docs",
            Self::SplitScope => "split_scope",
        }
    }
}

/// A bounded, review-only change suggestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub target_scope: FindingScope,
    pub target_path: PathBuf,
    pub action: ProposalAction,
    pub observed_problem: String,
    pub evidence_count: usize,
    pub distinct_sessions: usize,
    pub confidence: FindingConfidence,
    pub evidence: Vec<EvidenceRef>,
    pub proposed_text: Option<String>,
    pub existing_text: Option<String>,
    pub source_path: Option<PathBuf>,
    pub expected_target_hash: Option<String>,
    pub expected_source_hash: Option<String>,
    pub target_rationale: String,
    pub limitations: Vec<String>,
    pub review_reminder: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProposalError {
    #[error("proposal has no evidence")]
    MissingEvidence,
    #[error("proposal target path is empty")]
    EmptyTarget,
    #[error("proposal observed problem is empty")]
    MissingObservedProblem,
    #[error("proposal target rationale is empty")]
    MissingTargetRationale,
    #[error("proposal review reminder is empty")]
    MissingReviewReminder,
    #[error("proposal action {action} requires {field}")]
    MissingActionField {
        action: &'static str,
        field: &'static str,
    },
    #[error("proposal text exceeds {MAX_PROPOSAL_TEXT_BYTES} bytes")]
    TextTooLong,
    #[error("proposal has more than {MAX_REPORT_EVIDENCE} evidence references")]
    TooMuchEvidence,
    #[error("proposal evidence excerpts must be bounded and redacted")]
    UnboundedEvidence,
}

impl Proposal {
    pub fn validate(&self) -> Result<(), ProposalError> {
        if self.evidence_count == 0 || self.evidence.is_empty() {
            return Err(ProposalError::MissingEvidence);
        }
        if self.evidence.len() > MAX_REPORT_EVIDENCE {
            return Err(ProposalError::TooMuchEvidence);
        }
        if self.evidence.iter().any(|evidence| {
            evidence
                .excerpt
                .as_deref()
                .is_some_and(|excerpt| bounded_excerpt(excerpt, MAX_PROPOSAL_TEXT_BYTES) != excerpt)
        }) {
            return Err(ProposalError::UnboundedEvidence);
        }
        if self.target_path.as_os_str().is_empty() {
            return Err(ProposalError::EmptyTarget);
        }
        if self.observed_problem.trim().is_empty() {
            return Err(ProposalError::MissingObservedProblem);
        }
        if self.target_rationale.trim().is_empty() {
            return Err(ProposalError::MissingTargetRationale);
        }
        if self.review_reminder.trim().is_empty() {
            return Err(ProposalError::MissingReviewReminder);
        }
        if self
            .proposed_text
            .as_deref()
            .is_some_and(|text| text.len() > MAX_PROPOSAL_TEXT_BYTES)
            || self
                .existing_text
                .as_deref()
                .is_some_and(|text| text.len() > MAX_PROPOSAL_TEXT_BYTES)
        {
            return Err(ProposalError::TextTooLong);
        }
        match self.action {
            ProposalAction::Add => required_text(self.proposed_text.as_deref(), "proposed_text"),
            ProposalAction::Modify => {
                required_text(self.existing_text.as_deref(), "existing_text")?;
                required_text(self.proposed_text.as_deref(), "proposed_text")
            }
            ProposalAction::Remove => required_text(self.existing_text.as_deref(), "existing_text"),
            ProposalAction::MoveToDocs | ProposalAction::SplitScope => {
                if self.source_path.is_none() {
                    return Err(ProposalError::MissingActionField {
                        action: self.action.as_str(),
                        field: "source_path",
                    });
                }
                required_text(self.existing_text.as_deref(), "existing_text")?;
                required_text(self.proposed_text.as_deref(), "proposed_text")
            }
        }
    }

    /// Build the smallest useful proposal from a deterministic finding.
    pub fn from_finding(data: &CanonicalData, finding: &Finding) -> Option<Self> {
        let recommendation = recommend_scope(data, finding)?;
        if finding.evidence.is_empty()
            || (finding.kind == FindingType::Verification
                && finding.verification_status == Some(VerificationStatus::NotObserved))
        {
            return None;
        }

        let (action, mut proposed_text) = proposal_advice(finding)?;
        let mut target_path = recommendation.target_path.clone();
        let mut source_path = None;
        let mut existing_text = None;
        let mut expected_target_hash = file_hash(data, &target_path);
        let mut expected_source_hash = None;
        let mut target_scope = recommendation.target_scope.clone();
        let mut target_rationale = recommendation.rationale.clone();

        match action {
            ProposalAction::Add => {}
            ProposalAction::Remove => {
                let file = stored_file(data, &target_path)?;
                existing_text = instruction_anchor(file.content.as_deref()?, finding);
                existing_text.as_ref()?;
                proposed_text = None;
            }
            ProposalAction::MoveToDocs => {
                let source = stored_file(data, &target_path)?;
                let source_content = instruction_anchor(source.content.as_deref()?, finding)?;
                let observed_fact = observed_fact(finding)?;
                source_path = Some(target_path.clone());
                expected_source_hash = source.content_hash.clone();
                let project = project_for_scope(&recommendation.target_scope)?;
                target_path = project.join("docs/knowledge.md");
                target_scope = FindingScope::Project(project);
                expected_target_hash = file_hash(data, &target_path);
                existing_text = Some(source_content.clone());
                proposed_text = Some(observed_fact);
                target_rationale = format!(
                    "The repeated fact is routed to the project docs page {} while the instruction file keeps a short link",
                    target_path.display()
                );
            }
            ProposalAction::SplitScope => {
                let broad = match &finding.scope {
                    FindingScope::Instruction(path) => path.clone(),
                    _ => return None,
                };
                let source = stored_file(data, &broad)?;
                source_path = Some(broad);
                expected_source_hash = source.content_hash.clone();
                let anchor = instruction_anchor(source.content.as_deref()?, finding)?;
                existing_text = Some(anchor.clone());
                proposed_text = Some(anchor);
                existing_text.as_ref()?;
            }
            ProposalAction::Modify => return None,
        }

        let evidence = bounded_evidence(&finding.evidence, MAX_PROPOSAL_TEXT_BYTES);
        let mut limitations = finding.limitations.clone();
        limitations.extend(recommendation.limitations.clone());
        let proposal = Self {
            target_scope,
            target_path,
            action,
            observed_problem: bounded_excerpt(
                &format!(
                    "{} Target rationale: {}",
                    finding.summary, recommendation.rationale
                ),
                MAX_PROPOSAL_TEXT_BYTES,
            ),
            evidence_count: finding.occurrences,
            distinct_sessions: finding.distinct_sessions,
            confidence: recommendation.confidence,
            evidence,
            proposed_text,
            existing_text,
            source_path,
            expected_target_hash,
            expected_source_hash,
            target_rationale,
            limitations,
            review_reminder: "Review the evidence and diff before applying this proposal; codexlens does not apply it automatically".to_owned(),
        };
        proposal.validate().ok().map(|_| proposal)
    }
}

fn required_text(text: Option<&str>, field: &'static str) -> Result<(), ProposalError> {
    text.filter(|text| !text.trim().is_empty())
        .map(|_| ())
        .ok_or(ProposalError::MissingActionField {
            action: "proposal",
            field,
        })
}

fn bounded_evidence(evidence: &[EvidenceRef], max_bytes: usize) -> Vec<EvidenceRef> {
    evidence
        .iter()
        .take(MAX_REPORT_EVIDENCE)
        .cloned()
        .map(|mut evidence| {
            evidence.excerpt = evidence
                .excerpt
                .as_deref()
                .map(|excerpt| bounded_excerpt(excerpt, max_bytes));
            evidence
        })
        .collect()
}

fn proposal_advice(finding: &Finding) -> Option<(ProposalAction, Option<String>)> {
    let (action, text) = match finding.kind {
        FindingType::Failure => (
            ProposalAction::Add,
            finding
                .observed_commands
                .first()
                .map(|command| format!("Before running {command}, verify the documented prerequisite.")),
        ),
        FindingType::Correction | FindingType::Knowledge | FindingType::Gap => (
            if finding.kind == FindingType::Knowledge && finding.suggested_action.contains("docs/") {
                ProposalAction::MoveToDocs
            } else {
                ProposalAction::Add
            },
            Some(finding.suggested_action.clone()),
        ),
        FindingType::Rework | FindingType::Stuck => (
            ProposalAction::Add,
            finding
                .affected_paths
                .first()
                .map(|path| format!("Review the scoped edit and verification guidance for {path}.")),
        ),
        FindingType::Verification => (
            ProposalAction::Add,
            Some(
                "After changing files, run the recognized project verification command and record its result."
                    .to_owned(),
            ),
        ),
        FindingType::Overscoped => (
            ProposalAction::SplitScope,
            Some(
                "Keep this path-specific guidance in the nearest applicable instruction scope."
                    .to_owned(),
            ),
        ),
        FindingType::Duplicate => (ProposalAction::Remove, None),
        // ponytail: findings contain observed text, not an approved replacement pair;
        // keep modify for explicitly constructed proposals until that contract exists.
        FindingType::Stale | FindingType::Truncated => return None,
    };
    Some((
        action,
        text.map(|text| bounded_excerpt(&text, MAX_PROPOSAL_TEXT_BYTES)),
    ))
}

fn observed_fact(finding: &Finding) -> Option<String> {
    finding
        .evidence
        .iter()
        .filter_map(|evidence| evidence.excerpt.as_deref())
        .map(str::trim)
        .find(|excerpt| !excerpt.is_empty())
        .map(|excerpt| bounded_excerpt(excerpt, MAX_PROPOSAL_TEXT_BYTES))
}

fn instruction_anchor(content: &str, finding: &Finding) -> Option<String> {
    let lines = content
        .split_inclusive('\n')
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let mut needles = finding
        .observed_commands
        .iter()
        .map(|command| command.to_ascii_lowercase())
        .filter(|command| !command.is_empty())
        .collect::<Vec<_>>();
    needles.extend(
        finding
            .key
            .split('|')
            .map(str::trim)
            .filter(|part| part.len() >= 4)
            .map(str::to_ascii_lowercase),
    );
    needles.extend(
        finding
            .evidence
            .iter()
            .filter_map(|evidence| evidence.excerpt.as_deref())
            .map(str::trim)
            .filter(|excerpt| excerpt.len() >= 4)
            .map(str::to_ascii_lowercase),
    );
    let matches = lines
        .into_iter()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            needles.iter().any(|needle| lower.contains(needle))
        })
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRecommendation {
    pub target_scope: FindingScope,
    pub target_path: PathBuf,
    pub confidence: FindingConfidence,
    pub rationale: String,
    pub limitations: Vec<String>,
}

/// Select a target without reading the current filesystem.
pub fn recommend_scope(data: &CanonicalData, finding: &Finding) -> Option<ScopeRecommendation> {
    let session_ids = evidence_sessions(finding);
    if session_ids.is_empty() || session_ids.len() < finding.distinct_sessions {
        return None;
    }

    let mut limitations = Vec::new();
    let evidence_complete = session_ids.len() == finding.distinct_sessions;
    let base_confidence = if evidence_complete {
        finding.confidence
    } else {
        lower_confidence(finding.confidence)
    };
    let mut scope_confidence = base_confidence;

    if let FindingScope::Instruction(path) = &finding.scope {
        if finding.kind != FindingType::Overscoped {
            return Some(ScopeRecommendation {
                target_scope: finding.scope.clone(),
                target_path: path.clone(),
                confidence: base_confidence,
                rationale: format!(
                    "The stored finding already identifies the applicable instruction file {}",
                    path.display()
                ),
                limitations,
            });
        }
    }

    if let Some(path) = finding.affected_paths.first() {
        let mut nearest_paths = BTreeMap::new();
        for session in &session_ids {
            if let Some(target) = nearest_path_for(data, session, path) {
                *nearest_paths.entry(target).or_insert(0) += 1;
            }
        }
        if let Some(target) = strict_majority(nearest_paths, session_ids.len()) {
            let target_scope = FindingScope::Instruction(target.clone());
            let rationale = format!(
                "Path-specific evidence for {} is routed to the nearest applicable instruction file {}",
                path,
                target.display()
            );
            if finding.kind == FindingType::Overscoped {
                limitations.push(
                    "The split-scope target is inferred from stored path and instruction-chain evidence".to_owned(),
                );
            }
            return Some(ScopeRecommendation {
                target_scope,
                target_path: target,
                confidence: base_confidence,
                rationale,
                limitations,
            });
        }
        limitations.push(
            "No single nearest instruction path had a strict majority across supporting sessions"
                .to_owned(),
        );
        scope_confidence = lower_confidence(scope_confidence);
    }

    let project = majority_project(data, &session_ids);
    let requested_global = matches!(finding.scope, FindingScope::Global);
    if requested_global || project.is_none() {
        let target = majority_global_path(data, &session_ids)?;
        let rationale = if requested_global {
            format!(
                "The finding is cross-project/global, so the majority global instruction file {} is selected",
                target.display()
            )
        } else {
            format!(
                "No project has a strict majority, so the cross-project finding uses global instructions at {}",
                target.display()
            )
        };
        return Some(ScopeRecommendation {
            target_scope: FindingScope::Global,
            target_path: target,
            confidence: scope_confidence,
            rationale,
            limitations,
        });
    }
    let project = project?;
    if !project_scope_known(data, &session_ids) {
        return None;
    }
    let target = majority_project_instruction_path(data, &session_ids)
        .unwrap_or_else(|| project.join("AGENTS.md"));
    if stored_file(data, &target).is_none() {
        limitations.push(
            "No existing project instruction file was observed; the conventional root path is only a candidate".to_owned(),
        );
    }
    let target_scope = if matches!(finding.scope, FindingScope::Instruction(_)) {
        FindingScope::Instruction(target.clone())
    } else {
        FindingScope::Project(project.clone())
    };
    let rationale = format!(
        "A strict project majority selects {} as the project instruction target",
        target.display()
    );
    Some(ScopeRecommendation {
        target_scope,
        target_path: target.clone(),
        confidence: if stored_file(data, &target).is_some() {
            scope_confidence
        } else {
            lower_confidence(scope_confidence)
        },
        rationale,
        limitations,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalPlan {
    pub proposals: Vec<Proposal>,
    pub skipped: Vec<SkippedProposal>,
}

pub fn proposals_for_findings(data: &CanonicalData, findings: &[Finding]) -> ProposalPlan {
    let mut plan = ProposalPlan {
        proposals: Vec::new(),
        skipped: Vec::new(),
    };
    for finding in findings {
        match Proposal::from_finding(data, finding) {
            Some(proposal) if proposal.confidence == FindingConfidence::High => {
                plan.proposals.push(proposal);
            }
            Some(proposal) => plan.skipped.push(SkippedProposal {
                target_path: proposal.target_path,
                reason: format!(
                    "proposal confidence is {}; optimize --diff requires high-confidence proposals",
                    proposal.confidence.as_str()
                ),
            }),
            None => plan.skipped.push(SkippedProposal {
                target_path: target_path_for_finding(data, finding),
                reason: proposal_skip_reason(data, finding),
            }),
        }
    }
    plan.proposals.sort_by(|left, right| {
        left.target_path
            .cmp(&right.target_path)
            .then_with(|| left.action.as_str().cmp(right.action.as_str()))
            .then_with(|| left.observed_problem.cmp(&right.observed_problem))
    });
    plan.skipped.sort_by(|left, right| {
        left.target_path
            .cmp(&right.target_path)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    plan
}

fn target_path_for_finding(data: &CanonicalData, finding: &Finding) -> PathBuf {
    recommend_scope(data, finding)
        .map(|recommendation| recommendation.target_path)
        .unwrap_or_else(|| match &finding.scope {
            FindingScope::Global => PathBuf::from("AGENTS.md"),
            FindingScope::Project(path) => path.join("AGENTS.md"),
            FindingScope::Instruction(path) => path.clone(),
            FindingScope::Path(path) => PathBuf::from(path),
        })
}

fn proposal_skip_reason(data: &CanonicalData, finding: &Finding) -> String {
    if matches!(finding.kind, FindingType::Stale | FindingType::Truncated) {
        return format!(
            "finding type {} is unsupported by optimize --diff; no deterministic replacement text is available",
            finding.kind.as_str()
        );
    }
    if finding.evidence.is_empty() || finding.occurrences == 0 || finding.distinct_sessions == 0 {
        return "finding has incomplete evidence; no safe proposal can be built".to_owned();
    }
    if finding.kind == FindingType::Verification
        && finding.verification_status == Some(VerificationStatus::NotObserved)
    {
        return "verification was not observed; the proposal is incomplete".to_owned();
    }
    if recommend_scope(data, finding).is_none() {
        return "scope recommendation is unavailable or ambiguous".to_owned();
    }
    "no safe proposal could be built from the stored evidence".to_owned()
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

fn lower_confidence(confidence: FindingConfidence) -> FindingConfidence {
    match confidence {
        FindingConfidence::High => FindingConfidence::Medium,
        FindingConfidence::Medium => FindingConfidence::Low,
        FindingConfidence::Low => FindingConfidence::Low,
    }
}

fn majority_project(data: &CanonicalData, sessions: &[String]) -> Option<PathBuf> {
    let mut counts = BTreeMap::<PathBuf, usize>::new();
    for session_id in sessions {
        let session = data
            .sessions
            .iter()
            .find(|session| session.id == *session_id)?;
        let project = session
            .project
            .as_deref()
            .or(session.cwd.as_deref())
            .filter(|project| !project.is_empty())?;
        *counts
            .entry(PathBuf::from(normalize_path(project)))
            .or_default() += 1;
    }
    strict_majority(counts, sessions.len())
}

fn majority_global_path(data: &CanonicalData, sessions: &[String]) -> Option<PathBuf> {
    let mut counts = BTreeMap::<PathBuf, usize>::new();
    for session_id in sessions {
        let Some(join) = data
            .instruction_joins
            .iter()
            .find(|join| join.session_id == *session_id)
        else {
            continue;
        };
        let Some(path) = join
            .resolution
            .chain
            .iter()
            .find(|file| file.scope == InstructionScope::Global && usable_file(file))
            .map(|file| file.path.clone())
        else {
            continue;
        };
        *counts.entry(path).or_default() += 1;
    }
    strict_majority(counts, sessions.len())
}

fn majority_project_instruction_path(data: &CanonicalData, sessions: &[String]) -> Option<PathBuf> {
    let mut counts = BTreeMap::<PathBuf, usize>::new();
    for session_id in sessions {
        let Some(join) = data
            .instruction_joins
            .iter()
            .find(|join| join.session_id == *session_id)
        else {
            continue;
        };
        let Some(path) = join
            .resolution
            .chain
            .iter()
            .find(|file| file.scope == InstructionScope::ProjectRoot && usable_file(file))
            .map(|file| file.path.clone())
        else {
            continue;
        };
        *counts.entry(path).or_default() += 1;
    }
    strict_majority(counts, sessions.len())
}

fn nearest_path_for(data: &CanonicalData, session_id: &str, path: &str) -> Option<PathBuf> {
    let join = data
        .instruction_joins
        .iter()
        .find(|join| join.session_id == session_id)?;
    let target = resolve_path(join.cwd.as_deref(), join.project_root.as_deref(), path)?;
    join.resolution
        .chain
        .iter()
        .filter(|file| file.scope == InstructionScope::ProjectNested && usable_file(file))
        .filter(|file| target.starts_with(file.path.parent().unwrap_or(file.path.as_path())))
        .max_by_key(|file| file.path.components().count())
        .map(|file| file.path.clone())
}

fn resolve_path(cwd: Option<&Path>, project: Option<&Path>, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(normalize_path(path));
    if path.is_absolute() {
        return Some(path);
    }
    let base = cwd.or(project)?;
    Some(PathBuf::from(normalize_path(
        &base.join(path).to_string_lossy(),
    )))
}

fn strict_majority<T: Ord>(counts: BTreeMap<T, usize>, total: usize) -> Option<T> {
    let (value, count) = counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))?;
    (count * 2 > total).then_some(value)
}

fn project_for_scope(scope: &FindingScope) -> Option<PathBuf> {
    match scope {
        FindingScope::Project(path) => Some(path.clone()),
        FindingScope::Instruction(path) => path.parent().map(Path::to_path_buf),
        _ => None,
    }
}

fn project_scope_known(data: &CanonicalData, sessions: &[String]) -> bool {
    sessions.iter().all(|session_id| {
        data.instruction_joins
            .iter()
            .find(|join| join.session_id == *session_id)
            .is_some_and(|join| join.project_root_status == ProjectRootStatus::Known)
    })
}

fn usable_file(file: &InstructionFile) -> bool {
    matches!(
        file.state,
        InstructionFileState::Selected | InstructionFileState::Truncated
    ) && file.content.is_some()
}

fn stored_file<'a>(data: &'a CanonicalData, path: &Path) -> Option<&'a InstructionFile> {
    data.instruction_joins
        .iter()
        .flat_map(|join| join.resolution.files.iter())
        .find(|file| file.path == path && usable_file(file))
}

fn file_hash(data: &CanonicalData, path: &Path) -> Option<String> {
    stored_file(data, path).and_then(|file| {
        file.content_hash.clone().or_else(|| {
            file.content
                .as_deref()
                .map(|content| crate::instructions::content_hash(content.as_bytes()))
        })
    })
}

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
                heuristic: heuristic_for(&sanitized.kind).to_owned(),
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

fn heuristic_for(kind: &FindingType) -> &'static str {
    match kind {
        FindingType::Failure => "repeated failed tool outcome",
        FindingType::Correction => "repeated explicit correction marker",
        FindingType::Rework => "repeated file operation within the short rework window",
        FindingType::Stuck => "failure and edit burst within one short window",
        FindingType::Verification => "recognized verification command after a change",
        FindingType::Knowledge => "repeated bounded lexical fact across sessions",
        FindingType::Gap => "repeated evidence without matching instruction text",
        FindingType::Overscoped => {
            "path-specific evidence covered only by broader instruction scope"
        }
        FindingType::Duplicate => "equivalent normalized guidance in multiple scopes",
        FindingType::Stale => "current instruction hash differs from stored snapshot",
        FindingType::Truncated => "instruction chain reached the configured byte limit",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedDiff {
    pub proposal: Proposal,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedProposal {
    pub target_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffBatch {
    pub rendered: Vec<RenderedDiff>,
    pub skipped: Vec<SkippedProposal>,
}

#[derive(Debug, Error)]
pub enum DiffError {
    #[error("invalid proposal: {0}")]
    InvalidProposal(#[from] ProposalError),
    #[error("target file is missing: {0}")]
    MissingTarget(PathBuf),
    #[error("source file is missing: {0}")]
    MissingSource(PathBuf),
    #[error("target file cannot be read: {path}: {message}")]
    UnreadableTarget { path: PathBuf, message: String },
    #[error("source file cannot be read: {path}: {message}")]
    UnreadableSource { path: PathBuf, message: String },
    #[error("target file changed since proposal creation: {0}")]
    ChangedTarget(PathBuf),
    #[error("source file changed since proposal creation: {0}")]
    ChangedSource(PathBuf),
    #[error("proposal anchor is missing or ambiguous in {path}")]
    InvalidAnchor { path: PathBuf },
    #[error("proposal paths must be different: {0}")]
    SamePath(PathBuf),
}

pub fn render_diff(proposal: &Proposal) -> Result<String, DiffError> {
    proposal.validate()?;
    match proposal.action {
        ProposalAction::Add => {
            let current = read_target(&proposal.target_path)?;
            check_hash(
                &proposal.target_path,
                &current,
                proposal.expected_target_hash.as_deref(),
                false,
            )?;
            let updated = append_text(
                &current,
                proposal.proposed_text.as_deref().unwrap_or_default(),
            );
            Ok(unified_diff(&proposal.target_path, &current, &updated))
        }
        ProposalAction::Modify => {
            let current = read_target(&proposal.target_path)?;
            check_hash(
                &proposal.target_path,
                &current,
                proposal.expected_target_hash.as_deref(),
                false,
            )?;
            let updated = replace_once(
                &current,
                proposal.existing_text.as_deref().unwrap_or_default(),
                proposal.proposed_text.as_deref().unwrap_or_default(),
                &proposal.target_path,
            )?;
            Ok(unified_diff(&proposal.target_path, &current, &updated))
        }
        ProposalAction::Remove => {
            let current = read_target(&proposal.target_path)?;
            check_hash(
                &proposal.target_path,
                &current,
                proposal.expected_target_hash.as_deref(),
                false,
            )?;
            let updated = replace_once(
                &current,
                proposal.existing_text.as_deref().unwrap_or_default(),
                "",
                &proposal.target_path,
            )?;
            Ok(unified_diff(&proposal.target_path, &current, &updated))
        }
        ProposalAction::MoveToDocs | ProposalAction::SplitScope => {
            let source_path = proposal.source_path.as_ref().ok_or_else(|| {
                DiffError::InvalidProposal(ProposalError::MissingActionField {
                    action: proposal.action.as_str(),
                    field: "source_path",
                })
            })?;
            if source_path == &proposal.target_path {
                return Err(DiffError::SamePath(source_path.clone()));
            }
            let source = read_source(source_path)?;
            let target = read_target(&proposal.target_path)?;
            check_hash(
                source_path,
                &source,
                proposal.expected_source_hash.as_deref(),
                true,
            )?;
            check_hash(
                &proposal.target_path,
                &target,
                proposal.expected_target_hash.as_deref(),
                false,
            )?;
            let source_replacement = match proposal.action {
                ProposalAction::MoveToDocs => move_to_docs_link(&proposal.target_path),
                ProposalAction::SplitScope => String::new(),
                _ => unreachable!("source update is only used for move/split actions"),
            };
            let source_updated = replace_once(
                &source,
                proposal.existing_text.as_deref().unwrap_or_default(),
                &source_replacement,
                source_path,
            )?;
            let target_updated = append_text(
                &target,
                proposal.proposed_text.as_deref().unwrap_or_default(),
            );
            let mut output = unified_diff(source_path, &source, &source_updated);
            output.push_str(&unified_diff(
                &proposal.target_path,
                &target,
                &target_updated,
            ));
            Ok(output)
        }
    }
}

pub fn render_diffs(proposals: &[Proposal]) -> DiffBatch {
    let mut ordered = proposals.to_vec();
    ordered.sort_by(|left, right| {
        left.target_path
            .cmp(&right.target_path)
            .then_with(|| left.action.as_str().cmp(right.action.as_str()))
            .then_with(|| left.observed_problem.cmp(&right.observed_problem))
    });
    let mut path_counts = BTreeMap::<PathBuf, usize>::new();
    for proposal in &ordered {
        if proposal.confidence != FindingConfidence::High {
            continue;
        }
        *path_counts.entry(proposal.target_path.clone()).or_default() += 1;
        if let Some(source) = &proposal.source_path {
            *path_counts.entry(source.clone()).or_default() += 1;
        }
    }
    let mut result = DiffBatch {
        rendered: Vec::new(),
        skipped: Vec::new(),
    };
    for proposal in ordered {
        if proposal.confidence != FindingConfidence::High {
            result.skipped.push(SkippedProposal {
                target_path: proposal.target_path,
                reason: format!(
                    "proposal confidence is {}; optimize --diff requires high-confidence proposals",
                    proposal.confidence.as_str()
                ),
            });
            continue;
        }
        let conflict = path_counts
            .get(&proposal.target_path)
            .is_some_and(|count| *count > 1)
            || proposal
                .source_path
                .as_ref()
                .is_some_and(|path| path_counts.get(path).is_some_and(|count| *count > 1));
        if conflict {
            result.skipped.push(SkippedProposal {
                target_path: proposal.target_path,
                reason: "conflicting proposals share a target or source path".to_owned(),
            });
            continue;
        }
        match render_diff(&proposal) {
            Ok(diff) if diff.is_empty() => result.skipped.push(SkippedProposal {
                target_path: proposal.target_path,
                reason: "proposal is a no-op for the current target".to_owned(),
            }),
            Ok(diff) => result.rendered.push(RenderedDiff { proposal, diff }),
            Err(error) => result.skipped.push(SkippedProposal {
                target_path: proposal.target_path,
                reason: error.to_string(),
            }),
        }
    }
    result
}

pub fn render_proposal_summary(rendered: &RenderedDiff) -> String {
    let proposal = &rendered.proposal;
    let mut output = format!(
        "Proposal {} {}\nObserved: {}\nEvidence: {} occurrences across {} sessions\nConfidence: {}\nTarget: {}\n",
        proposal.action.as_str(),
        proposal.target_path.display(),
        proposal.observed_problem,
        proposal.evidence_count,
        proposal.distinct_sessions,
        proposal.confidence.as_str(),
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

fn read_target(path: &Path) -> Result<String, DiffError> {
    if !path.is_file() {
        return Err(DiffError::MissingTarget(path.to_path_buf()));
    }
    fs::read_to_string(path).map_err(|error| DiffError::UnreadableTarget {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn read_source(path: &Path) -> Result<String, DiffError> {
    if !path.is_file() {
        return Err(DiffError::MissingSource(path.to_path_buf()));
    }
    fs::read_to_string(path).map_err(|error| DiffError::UnreadableSource {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn check_hash(
    path: &Path,
    content: &str,
    expected: Option<&str>,
    source: bool,
) -> Result<(), DiffError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = crate::instructions::content_hash(content.as_bytes());
    if actual == expected {
        return Ok(());
    }
    if source {
        Err(DiffError::ChangedSource(path.to_path_buf()))
    } else {
        Err(DiffError::ChangedTarget(path.to_path_buf()))
    }
}

fn append_text(content: &str, text: &str) -> String {
    if text.trim().is_empty() || contains_text_block(content, text) {
        return content.to_owned();
    }
    let mut updated = content.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() && !updated.ends_with("\n\n") {
        updated.push('\n');
    }
    updated.push_str(text);
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated
}

fn contains_text_block(content: &str, text: &str) -> bool {
    let needle = normalized_lines(text);
    !needle.is_empty()
        && normalized_lines(content)
            .windows(needle.len())
            .any(|window| window == needle)
}

fn normalized_lines(content: &str) -> Vec<&str> {
    let mut lines = content.split('\n').map(str::trim_end).collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn move_to_docs_link(path: &Path) -> String {
    let path = path.to_string_lossy().into_owned();
    let link = path
        .find("docs/")
        .map_or_else(|| path.clone(), |index| path[index..].to_owned());
    format!("See [the detailed fact]({link}).")
}

fn replace_once(content: &str, old: &str, new: &str, path: &Path) -> Result<String, DiffError> {
    if old.is_empty() {
        return Err(DiffError::InvalidAnchor {
            path: path.to_path_buf(),
        });
    }
    let mut matches = content.match_indices(old);
    let Some((start, _)) = matches.next() else {
        return Err(DiffError::InvalidAnchor {
            path: path.to_path_buf(),
        });
    };
    if matches.next().is_some() {
        return Err(DiffError::InvalidAnchor {
            path: path.to_path_buf(),
        });
    }
    let end = start + old.len();
    let mut updated = String::with_capacity(content.len() + new.len().saturating_sub(old.len()));
    updated.push_str(&content[..start]);
    updated.push_str(new);
    updated.push_str(&content[end..]);
    Ok(updated)
}

// ponytail: whole-file hunk keeps the renderer dependency-free; switch to an
// LCS/context diff if instruction files become too large for review.
fn unified_diff(path: &Path, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let old_lines = diff_lines(old);
    let new_lines = diff_lines(new);
    let mut output = format!(
        "--- a/{}\n+++ b/{}\n@@ -{},{} +{},{} @@\n",
        path.display(),
        path.display(),
        if old_lines.is_empty() { 0 } else { 1 },
        old_lines.len(),
        if new_lines.is_empty() { 0 } else { 1 },
        new_lines.len()
    );
    for line in old_lines {
        output.push('-');
        output.push_str(&line);
        output.push('\n');
    }
    for line in new_lines {
        output.push('+');
        output.push_str(&line);
        output.push('\n');
    }
    output
}

fn diff_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = content.split('\n').map(str::to_owned).collect::<Vec<_>>();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{EvidenceRole, FindingSeverity};
    use crate::model::{InstructionFileKind, InstructionResolution, ProjectRootStatus, SourceKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn source(line: usize) -> SourceRef {
        SourceRef::rollout(PathBuf::from("fixture.jsonl"), line)
    }

    fn finding(scope: FindingScope, kind: FindingType, path: Option<&str>) -> Finding {
        Finding {
            kind,
            severity: FindingSeverity::High,
            confidence: FindingConfidence::High,
            scope,
            key: "fixture fact".to_owned(),
            summary: "Repeated synthetic evidence".to_owned(),
            evidence: vec![EvidenceRef {
                session_id: Some("session".to_owned()),
                source: source(1),
                role: EvidenceRole::Observation,
                excerpt: Some("secret=hidden synthetic evidence".to_owned()),
            }],
            occurrences: 2,
            distinct_sessions: 1,
            affected_paths: path.into_iter().map(str::to_owned).collect(),
            observed_commands: vec!["cargo test".to_owned()],
            sequence: Vec::new(),
            suggested_action: "Add the concise synthetic fact".to_owned(),
            limitations: vec!["synthetic limitation".to_owned()],
            verification_status: None,
        }
    }

    fn file(path: &str, scope: InstructionScope, content: &str) -> InstructionFile {
        InstructionFile {
            path: PathBuf::from(path),
            scope,
            kind: InstructionFileKind::Standard,
            state: InstructionFileState::Selected,
            chain_position: Some(0),
            content: Some(content.to_owned()),
            content_hash: Some(crate::instructions::content_hash(content.as_bytes())),
            byte_count: content.len(),
            diagnostic: None,
        }
    }

    fn join(
        session: &str,
        project: &str,
        files: Vec<InstructionFile>,
    ) -> crate::model::InstructionJoin {
        let chain = files.clone();
        crate::model::InstructionJoin {
            session_id: session.to_owned(),
            cwd: Some(PathBuf::from(format!("{project}/src"))),
            project_root: Some(PathBuf::from(project)),
            project_root_status: ProjectRootStatus::Known,
            nearest_path: chain.last().map(|file| file.path.clone()),
            nearest_scope: chain.last().map(|file| file.scope),
            resolution: InstructionResolution {
                project_root: Some(PathBuf::from(project)),
                cwd: Some(PathBuf::from(format!("{project}/src"))),
                project_root_status: ProjectRootStatus::Known,
                files: files.clone(),
                chain,
                effective_content: Some(
                    files
                        .iter()
                        .filter_map(|file| file.content.clone())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ),
                effective_chain_hash: None,
                byte_count: 0,
                truncated: false,
                diagnostics: Vec::new(),
            },
            provenance: SourceRef::state(PathBuf::from("state.sqlite")),
        }
    }

    fn data_with_join(files: Vec<InstructionFile>) -> CanonicalData {
        CanonicalData {
            sessions: vec![crate::model::Session {
                id: "session".to_owned(),
                created_at: Some("2026-01-01T00:00:00Z".to_owned()),
                updated_at: Some("2026-01-01T00:01:00Z".to_owned()),
                cwd: Some("/fixture/project/src".to_owned()),
                project: Some("/fixture/project".to_owned()),
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
                provenance: source(1),
            }],
            instruction_joins: vec![join("session", "/fixture/project", files)],
            ..CanonicalData::default()
        }
    }

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "codexlens-advisor-{}-{}-{name}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn proposal(path: &Path, action: ProposalAction) -> Proposal {
        let mut value = Proposal {
            target_scope: FindingScope::Instruction(path.to_path_buf()),
            target_path: path.to_path_buf(),
            action,
            observed_problem: "synthetic problem".to_owned(),
            evidence_count: 1,
            distinct_sessions: 1,
            confidence: FindingConfidence::High,
            evidence: vec![EvidenceRef {
                session_id: Some("session".to_owned()),
                source: source(1),
                role: EvidenceRole::Observation,
                excerpt: Some("synthetic".to_owned()),
            }],
            proposed_text: Some("new guidance".to_owned()),
            existing_text: Some("old guidance".to_owned()),
            source_path: None,
            expected_target_hash: None,
            expected_source_hash: None,
            target_rationale: "nearest instruction file".to_owned(),
            limitations: vec!["review".to_owned()],
            review_reminder: "review before applying".to_owned(),
        };
        match action {
            ProposalAction::Add => value.existing_text = None,
            ProposalAction::Remove => value.proposed_text = None,
            ProposalAction::Modify => {}
            ProposalAction::MoveToDocs | ProposalAction::SplitScope => {
                value.source_path = Some(path.with_extension("source"));
            }
        }
        value
    }

    #[test]
    fn proposal_actions_round_trip_deterministically() {
        let path = PathBuf::from("/fixture/project/AGENTS.md");
        for action in [
            ProposalAction::Add,
            ProposalAction::Modify,
            ProposalAction::Remove,
            ProposalAction::MoveToDocs,
            ProposalAction::SplitScope,
        ] {
            let value = proposal(&path, action);
            let first = serde_json::to_string(&value).unwrap();
            let second = serde_json::to_string(&value).unwrap();
            assert_eq!(first, second);
            assert_eq!(serde_json::from_str::<Proposal>(&first).unwrap(), value);
        }
    }

    #[test]
    fn scope_recommendation_prefers_nested_and_global_majority() {
        let root = file(
            "/fixture/project/AGENTS.md",
            InstructionScope::ProjectRoot,
            "root",
        );
        let nested = file(
            "/fixture/project/src/AGENTS.md",
            InstructionScope::ProjectNested,
            "nested",
        );
        let data = data_with_join(vec![root, nested]);
        let recommendation = recommend_scope(
            &data,
            &finding(
                FindingScope::Project(PathBuf::from("/fixture/project")),
                FindingType::Rework,
                Some("src/lib.rs"),
            ),
        )
        .unwrap();
        assert_eq!(
            recommendation.target_path,
            PathBuf::from("/fixture/project/src/AGENTS.md")
        );
        assert!(recommendation.rationale.contains("nearest"));

        let global = file(
            "/fixture/.codex/AGENTS.md",
            InstructionScope::Global,
            "global",
        );
        let mut cross = data_with_join(vec![global]);
        cross.sessions[0].project = Some("/fixture/one".to_owned());
        let global_recommendation = recommend_scope(
            &cross,
            &finding(FindingScope::Global, FindingType::Correction, None),
        )
        .unwrap();
        assert_eq!(global_recommendation.target_scope, FindingScope::Global);
    }

    #[test]
    fn ambiguous_scope_is_suppressed() {
        let mut data = data_with_join(vec![file(
            "/fixture/project/AGENTS.md",
            InstructionScope::ProjectRoot,
            "root",
        )]);
        data.sessions.push(crate::model::Session {
            id: "other".to_owned(),
            project: Some("/fixture/other".to_owned()),
            cwd: Some("/fixture/other".to_owned()),
            provenance: source(2),
            created_at: None,
            updated_at: None,
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
        });
        let mut value = finding(FindingScope::Global, FindingType::Correction, None);
        value.evidence.push(EvidenceRef {
            session_id: Some("other".to_owned()),
            source: source(2),
            role: EvidenceRole::Observation,
            excerpt: None,
        });
        value.distinct_sessions = 2;
        assert!(recommend_scope(&data, &value).is_none());
    }

    #[test]
    fn ambiguous_path_fallback_lowers_confidence() {
        let root = file(
            "/fixture/project/AGENTS.md",
            InstructionScope::ProjectRoot,
            "root",
        );
        let first_nested = file(
            "/fixture/project/src/AGENTS.md",
            InstructionScope::ProjectNested,
            "first",
        );
        let second_root = file(
            "/fixture/project/AGENTS.md",
            InstructionScope::ProjectRoot,
            "root",
        );
        let second_nested = file(
            "/fixture/project/other/AGENTS.md",
            InstructionScope::ProjectNested,
            "second",
        );
        let mut data = data_with_join(vec![root, first_nested]);
        data.sessions.push(crate::model::Session {
            id: "other".to_owned(),
            created_at: None,
            updated_at: None,
            cwd: Some("/fixture/project/other".to_owned()),
            project: Some("/fixture/project".to_owned()),
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
            provenance: source(2),
        });
        let mut second = join(
            "other",
            "/fixture/project",
            vec![second_root, second_nested],
        );
        second.cwd = Some(PathBuf::from("/fixture/project/other"));
        second.resolution.cwd = second.cwd.clone();
        data.instruction_joins.push(second);

        let mut value = finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Rework,
            Some("file.rs"),
        );
        value.evidence.push(EvidenceRef {
            session_id: Some("other".to_owned()),
            source: source(2),
            role: EvidenceRole::Observation,
            excerpt: None,
        });
        value.distinct_sessions = 2;
        let recommendation = recommend_scope(&data, &value).unwrap();
        assert_eq!(recommendation.confidence, FindingConfidence::Medium);
        assert!(
            recommendation
                .limitations
                .iter()
                .any(|limitation| limitation.contains("strict majority"))
        );
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
    fn proposal_validation_rejects_unbounded_evidence() {
        let path = PathBuf::from("/fixture/project/AGENTS.md");
        let mut value = proposal(&path, ProposalAction::Add);
        value.evidence[0].excerpt = Some("secret=hidden".to_owned());
        assert_eq!(value.validate(), Err(ProposalError::UnboundedEvidence));
    }

    #[test]
    fn diff_renderer_supports_actions_and_never_writes() {
        let target = temp_file("AGENTS.md");
        let source_path = temp_file("source.md");
        std::fs::write(&target, "old guidance\n").unwrap();
        std::fs::write(&source_path, "move this\n").unwrap();
        let before_target = std::fs::read_to_string(&target).unwrap();
        let add = proposal(&target, ProposalAction::Add);
        assert!(render_diff(&add).unwrap().contains("+new guidance"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before_target);
        let mut no_op = proposal(&target, ProposalAction::Add);
        no_op.proposed_text = Some("old guidance".to_owned());
        assert!(render_diff(&no_op).unwrap().is_empty());
        std::fs::write(&target, "old guidance\nsecond line\n").unwrap();
        let mut block_no_op = proposal(&target, ProposalAction::Add);
        block_no_op.proposed_text = Some("old guidance\nsecond line".to_owned());
        assert!(render_diff(&block_no_op).unwrap().is_empty());
        let missing = proposal(&temp_file("missing.md"), ProposalAction::Add);
        assert!(matches!(
            render_diff(&missing),
            Err(DiffError::MissingTarget(_))
        ));

        let mut modify = proposal(&target, ProposalAction::Modify);
        assert!(render_diff(&modify).unwrap().contains("+new guidance"));
        let remove = proposal(&target, ProposalAction::Remove);
        assert!(render_diff(&remove).unwrap().contains("-old guidance"));

        let docs = temp_file("docs.md");
        std::fs::write(&docs, "docs\n").unwrap();
        let mut move_proposal = proposal(&docs, ProposalAction::MoveToDocs);
        move_proposal.source_path = Some(source_path.clone());
        move_proposal.existing_text = Some("move this\n".to_owned());
        move_proposal.proposed_text = Some("move this\n".to_owned());
        let move_diff = render_diff(&move_proposal).unwrap();
        assert!(move_diff.contains("+move this"));
        assert!(move_diff.contains("+See [the detailed fact]("));

        let mut split = proposal(&target, ProposalAction::SplitScope);
        split.source_path = Some(source_path.clone());
        split.existing_text = Some("move this\n".to_owned());
        assert!(render_diff(&split).unwrap().contains("+new guidance"));

        modify.expected_target_hash = Some("changed".to_owned());
        assert!(matches!(
            render_diff(&modify),
            Err(DiffError::ChangedTarget(_))
        ));
        let mut low_confidence = proposal(&target, ProposalAction::Add);
        low_confidence.confidence = FindingConfidence::Medium;
        let low_batch = render_diffs(&[low_confidence]);
        assert!(low_batch.rendered.is_empty());
        assert!(low_batch.skipped[0].reason.contains("high-confidence"));
        let conflict = render_diffs(&[add.clone(), add]);
        assert_eq!(conflict.rendered.len(), 0);
        assert_eq!(conflict.skipped.len(), 2);

        let _ = std::fs::remove_file(target);
        let _ = std::fs::remove_file(source_path);
        let _ = std::fs::remove_file(docs);
    }

    #[test]
    fn finding_to_proposal_bounds_and_keeps_evidence_reference() {
        let data = data_with_join(vec![file(
            "/fixture/project/AGENTS.md",
            InstructionScope::ProjectRoot,
            "root",
        )]);
        let value = finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Correction,
            None,
        );
        let proposal = Proposal::from_finding(&data, &value).unwrap();
        assert_eq!(proposal.evidence_count, 2);
        assert_eq!(proposal.evidence[0].source.kind, SourceKind::Rollout);
        assert!(proposal.proposed_text.unwrap().len() <= MAX_PROPOSAL_TEXT_BYTES);
    }

    #[test]
    fn move_to_docs_uses_observed_fact_and_verified_anchor() {
        let path = "/fixture/project/AGENTS.md";
        let data = data_with_join(vec![file(
            path,
            InstructionScope::ProjectRoot,
            "observed fact\nunrelated line\n",
        )]);
        let mut value = finding(
            FindingScope::Project(PathBuf::from("/fixture/project")),
            FindingType::Knowledge,
            None,
        );
        value.key = "observed fact".to_owned();
        value.evidence[0].excerpt = Some("observed fact".to_owned());
        value.suggested_action = "Keep the detailed fact in docs/knowledge.md".to_owned();

        let proposal = Proposal::from_finding(&data, &value).unwrap();
        assert_eq!(proposal.existing_text.as_deref(), Some("observed fact\n"));
        assert_eq!(proposal.proposed_text.as_deref(), Some("observed fact"));

        let unrelated = data_with_join(vec![file(
            path,
            InstructionScope::ProjectRoot,
            "unrelated line\n",
        )]);
        assert!(Proposal::from_finding(&unrelated, &value).is_none());
    }

    #[test]
    fn proposal_plan_preserves_unsupported_findings_as_skipped() {
        let path = PathBuf::from("/fixture/project/AGENTS.md");
        let data = data_with_join(vec![file(
            path.to_str().unwrap(),
            InstructionScope::ProjectRoot,
            "root",
        )]);
        let value = finding(
            FindingScope::Instruction(path.clone()),
            FindingType::Stale,
            None,
        );

        let plan = proposals_for_findings(&data, &[value]);
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].target_path, path);
        assert!(plan.skipped[0].reason.contains("unsupported"));
    }
}
