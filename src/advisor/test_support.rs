//! Shared synthetic fixtures for advisor module tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::analysis::{
    EvidenceRef, EvidenceRole, Finding, FindingConfidence, FindingScope, FindingSeverity,
    FindingType,
};
use crate::model::{
    CanonicalData, InstructionFile, InstructionFileKind, InstructionFileState,
    InstructionResolution, InstructionScope, ProjectRootStatus, SourceRef,
};

use super::proposal::{Proposal, ProposalAction};
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) fn source(line: usize) -> SourceRef {
    SourceRef::rollout(PathBuf::from("fixture.jsonl"), line)
}

pub(crate) fn finding(scope: FindingScope, kind: FindingType, path: Option<&str>) -> Finding {
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

pub(crate) fn file(path: &str, scope: InstructionScope, content: &str) -> InstructionFile {
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

pub(crate) fn join(
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

pub(crate) fn data_with_join(files: Vec<InstructionFile>) -> CanonicalData {
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

pub(crate) fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "codexlens-advisor-{}-{}-{name}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(crate) fn proposal(path: &Path, action: ProposalAction) -> Proposal {
    let mut value = Proposal {
        target_scope: FindingScope::Instruction(path.to_path_buf()),
        target_path: path.to_path_buf(),
        action,
        observed_problem: "synthetic problem".to_owned(),
        evidence_count: 1,
        distinct_sessions: 1,
        confidence: FindingConfidence::High,
        heuristic: "synthetic heuristic".to_owned(),
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
