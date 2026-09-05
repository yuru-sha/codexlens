//! Proposal modeling, validation, and finding conversion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::analysis::{
    EvidenceRef, Finding, FindingConfidence, FindingScope, FindingType, VerificationStatus,
    bounded_excerpt,
};
use crate::model::CanonicalData;

use super::diff::SkippedProposal;
use super::scope::{file_hash, recommend_scope, stored_file};
pub(super) const MAX_PROPOSAL_TEXT_BYTES: usize = 512;
const MAX_REPORT_EVIDENCE: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalAction {
    Add,
    /// Explicit-only; findings do not carry an approved replacement pair.
    Modify,
    Remove,
    /// Explicit-only; the docs target must have a stored baseline hash.
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
    pub heuristic: String,
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
    #[error("proposal heuristic is empty")]
    MissingHeuristic,
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
        if self.heuristic.trim().is_empty() {
            return Err(ProposalError::MissingHeuristic);
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
        if self.expected_target_hash.is_none() {
            return Err(ProposalError::MissingActionField {
                action: self.action.as_str(),
                field: "expected_target_hash",
            });
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
                if self.expected_source_hash.is_none() {
                    return Err(ProposalError::MissingActionField {
                        action: self.action.as_str(),
                        field: "expected_source_hash",
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
        let target_path = recommendation.target_path.clone();
        let mut source_path = None;
        let mut existing_text = None;
        let expected_target_hash = file_hash(data, &target_path);
        let mut expected_source_hash = None;
        let target_scope = recommendation.target_scope.clone();
        let target_rationale = recommendation.rationale.clone();

        match action {
            ProposalAction::Add => {}
            ProposalAction::Remove => {
                let file = stored_file(data, &target_path)?;
                existing_text = if finding.kind == FindingType::Duplicate {
                    duplicate_anchor(data, &target_path, finding)
                } else {
                    instruction_anchor(file.content.as_deref()?, finding)
                };
                existing_text.as_ref()?;
                proposed_text = None;
            }
            ProposalAction::MoveToDocs => return None,
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
            heuristic: heuristic_for(finding).to_owned(),
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

pub(super) fn bounded_evidence(evidence: &[EvidenceRef], max_bytes: usize) -> Vec<EvidenceRef> {
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
        FindingType::Correction | FindingType::Gap => (
            ProposalAction::Add,
            Some(finding.suggested_action.clone()),
        ),
        FindingType::Knowledge if finding.suggested_action.contains("docs/") => return None,
        FindingType::Knowledge => (ProposalAction::Add, Some(finding.suggested_action.clone())),
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

fn duplicate_anchor(data: &CanonicalData, target_path: &Path, finding: &Finding) -> Option<String> {
    let duplicate_paths = finding
        .evidence
        .iter()
        .filter(|evidence| evidence.role == crate::analysis::EvidenceRole::InstructionFile)
        .map(|evidence| evidence.source.path.clone())
        .collect::<BTreeSet<_>>();
    if duplicate_paths.len() < 2 || !duplicate_paths.contains(target_path) {
        return None;
    }
    let content = stored_file(data, target_path)?.content.as_deref()?;
    (!content.trim().is_empty() && content.len() <= MAX_PROPOSAL_TEXT_BYTES)
        .then(|| content.to_owned())
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
    if finding.kind == FindingType::Knowledge && finding.suggested_action.contains("docs/") {
        return "move_to_docs is explicit-only because the docs target has no stored baseline hash"
            .to_owned();
    }
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

pub(super) fn heuristic_for(finding: &Finding) -> &'static str {
    match (&finding.kind, finding.verification_status) {
        (FindingType::Failure, _) => "repeated failed tool outcome",
        (FindingType::Correction, _) => "repeated explicit correction marker",
        (FindingType::Rework, _) => "repeated file operation within the short rework window",
        (FindingType::Stuck, _) => "failure and edit burst within one short window",
        (FindingType::Verification, Some(VerificationStatus::Missing)) => {
            "absence of a recognized verification command after a change"
        }
        (FindingType::Verification, Some(VerificationStatus::NotObserved)) => {
            "verification outcome was not observed in the available rollout"
        }
        (FindingType::Verification, None) => "recognized verification status heuristic",
        (FindingType::Knowledge, _) => "repeated bounded lexical fact across sessions",
        (FindingType::Gap, _) => "repeated evidence without matching instruction text",
        (FindingType::Overscoped, _) => {
            "path-specific evidence covered only by broader instruction scope"
        }
        (FindingType::Duplicate, _) => "equivalent normalized guidance in multiple scopes",
        (FindingType::Stale, _) => "current instruction hash differs from stored snapshot",
        (FindingType::Truncated, _) => "instruction chain reached the configured byte limit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::test_support::{data_with_join, file, finding, proposal};
    use crate::advisor::{RenderedDiff, render_proposal_summary};
    use crate::analysis::{EvidenceRef, EvidenceRole};
    use crate::model::{InstructionScope, SourceKind, SourceRef};
    use std::path::PathBuf;

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
    fn proposal_validation_rejects_unbounded_evidence() {
        let path = PathBuf::from("/fixture/project/AGENTS.md");
        let mut value = proposal(&path, ProposalAction::Add);
        value.evidence[0].excerpt = Some("secret=hidden".to_owned());
        assert_eq!(value.validate(), Err(ProposalError::UnboundedEvidence));
    }

    #[test]
    fn proposal_validation_requires_target_baseline_for_mutating_actions() {
        let path = PathBuf::from("/fixture/project/AGENTS.md");
        for action in [
            ProposalAction::Add,
            ProposalAction::Modify,
            ProposalAction::Remove,
            ProposalAction::MoveToDocs,
            ProposalAction::SplitScope,
        ] {
            let value = proposal(&path, action);
            assert!(matches!(
                value.validate(),
                Err(ProposalError::MissingActionField {
                    field: "expected_target_hash",
                    ..
                })
            ));
        }
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
        assert!(proposal.proposed_text.as_deref().unwrap().len() <= MAX_PROPOSAL_TEXT_BYTES);
        assert_eq!(proposal.heuristic, "repeated explicit correction marker");
        assert!(
            render_proposal_summary(&RenderedDiff {
                proposal,
                diff: String::new(),
            })
            .contains("Heuristic: repeated explicit correction marker")
        );
    }

    #[test]
    fn knowledge_move_to_docs_is_explicit_only() {
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

        let plan = proposals_for_findings(&data, &[value]);
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.contains("explicit-only"));
    }

    #[test]
    fn duplicate_finding_uses_the_verified_duplicate_file_content() {
        let root = PathBuf::from("/fixture/project/AGENTS.md");
        let nested = PathBuf::from("/fixture/project/src/AGENTS.md");
        let data = data_with_join(vec![
            file(
                root.to_str().unwrap(),
                InstructionScope::ProjectRoot,
                "same guidance\n",
            ),
            file(
                nested.to_str().unwrap(),
                InstructionScope::ProjectNested,
                "same guidance\n",
            ),
        ]);
        let mut value = finding(
            FindingScope::Instruction(nested.clone()),
            FindingType::Duplicate,
            None,
        );
        value.evidence = vec![
            EvidenceRef {
                session_id: Some("session".to_owned()),
                source: SourceRef::state(root),
                role: EvidenceRole::InstructionFile,
                excerpt: None,
            },
            EvidenceRef {
                session_id: Some("session".to_owned()),
                source: SourceRef::state(nested.clone()),
                role: EvidenceRole::InstructionFile,
                excerpt: None,
            },
        ];

        let proposal = Proposal::from_finding(&data, &value).unwrap();
        assert_eq!(proposal.action, ProposalAction::Remove);
        assert_eq!(proposal.target_path, nested);
        assert_eq!(proposal.existing_text.as_deref(), Some("same guidance\n"));
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
