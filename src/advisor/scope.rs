//! Evidence-based instruction scope recommendation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::analysis::{Finding, FindingConfidence, FindingScope, FindingType};
use crate::model::{
    CanonicalData, InstructionFile, InstructionFileState, InstructionScope, ProjectRootStatus,
    normalize_path,
};

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

pub(super) fn stored_file<'a>(data: &'a CanonicalData, path: &Path) -> Option<&'a InstructionFile> {
    data.instruction_joins
        .iter()
        .flat_map(|join| join.resolution.files.iter())
        .find(|file| file.path == path && usable_file(file))
}

pub(super) fn file_hash(data: &CanonicalData, path: &Path) -> Option<String> {
    stored_file(data, path).and_then(|file| {
        file.content_hash.clone().or_else(|| {
            file.content
                .as_deref()
                .map(|content| crate::instructions::content_hash(content.as_bytes()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::test_support::{data_with_join, file, finding, join, source};
    use crate::analysis::{
        EvidenceRef, EvidenceRole, FindingConfidence, FindingScope, FindingType,
    };
    use crate::model::InstructionScope;
    use std::path::PathBuf;

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
}
