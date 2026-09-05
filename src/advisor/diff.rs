//! Read-only proposal diff rendering and skipped-result handling.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::analysis::FindingConfidence;

use super::proposal::{Proposal, ProposalAction, ProposalError};

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
                ProposalAction::MoveToDocs => move_to_docs_link(
                    source_path,
                    &proposal.target_path,
                    proposal.existing_text.as_deref().unwrap_or_default(),
                ),
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

fn move_to_docs_link(source_path: &Path, target_path: &Path, existing_text: &str) -> String {
    let source_dir = source_path.parent().unwrap_or_else(|| Path::new("."));
    let link = relative_path(source_dir, target_path);
    let line_ending = if existing_text.ends_with("\r\n") {
        "\r\n"
    } else if existing_text.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    format!("See [the detailed fact]({}).{line_ending}", link.display())
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in &from[common..] {
        relative.push("..");
    }
    for component in &to[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    }
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
    use crate::advisor::test_support::{proposal, temp_file};
    use crate::analysis::FindingConfidence;
    use std::path::Path;

    #[test]
    fn diff_renderer_supports_actions_and_never_writes() {
        let target = temp_file("AGENTS.md");
        let source_path = temp_file("source.md");
        std::fs::write(&target, "old guidance\n").unwrap();
        std::fs::write(&source_path, "move this\nkeep next\n").unwrap();
        let before_target = std::fs::read_to_string(&target).unwrap();
        let old_target_hash = crate::instructions::content_hash(b"old guidance\n");
        let mut add = proposal(&target, ProposalAction::Add);
        add.expected_target_hash = Some(old_target_hash.clone());
        assert!(render_diff(&add).unwrap().contains("+new guidance"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before_target);
        let mut no_op = proposal(&target, ProposalAction::Add);
        no_op.proposed_text = Some("old guidance".to_owned());
        no_op.expected_target_hash = Some(old_target_hash);
        assert!(render_diff(&no_op).unwrap().is_empty());
        std::fs::write(&target, "old guidance\nsecond line\n").unwrap();
        let second_line_target_hash =
            crate::instructions::content_hash(b"old guidance\nsecond line\n");
        let mut block_no_op = proposal(&target, ProposalAction::Add);
        block_no_op.proposed_text = Some("old guidance\nsecond line".to_owned());
        block_no_op.expected_target_hash = Some(second_line_target_hash.clone());
        assert!(render_diff(&block_no_op).unwrap().is_empty());
        let mut missing = proposal(&temp_file("missing.md"), ProposalAction::Add);
        missing.expected_target_hash = Some("missing-baseline".to_owned());
        assert!(matches!(
            render_diff(&missing),
            Err(DiffError::MissingTarget(_))
        ));

        let mut modify = proposal(&target, ProposalAction::Modify);
        modify.expected_target_hash = Some(second_line_target_hash.clone());
        assert!(render_diff(&modify).unwrap().contains("+new guidance"));
        let mut remove = proposal(&target, ProposalAction::Remove);
        remove.expected_target_hash = Some(second_line_target_hash.clone());
        assert!(render_diff(&remove).unwrap().contains("-old guidance"));

        let docs = temp_file("docs.md");
        std::fs::write(&docs, "docs\n").unwrap();
        let mut move_proposal = proposal(&docs, ProposalAction::MoveToDocs);
        move_proposal.source_path = Some(source_path.clone());
        move_proposal.existing_text = Some("move this\n".to_owned());
        move_proposal.proposed_text = Some("move this\n".to_owned());
        move_proposal.expected_source_hash =
            Some(crate::instructions::content_hash(b"move this\nkeep next\n"));
        move_proposal.expected_target_hash = Some(crate::instructions::content_hash(b"docs\n"));
        let move_diff = render_diff(&move_proposal).unwrap();
        let docs_name = docs.file_name().unwrap().to_string_lossy();
        assert!(move_diff.contains(&format!(
            "+See [the detailed fact]({docs_name}).\n+keep next"
        )));

        let mut missing_target_baseline = move_proposal.clone();
        missing_target_baseline.expected_target_hash = None;
        assert!(matches!(
            render_diff(&missing_target_baseline),
            Err(DiffError::InvalidProposal(
                ProposalError::MissingActionField {
                    action: "move_to_docs",
                    field: "expected_target_hash"
                }
            ))
        ));
        let mut missing_source_baseline = move_proposal.clone();
        missing_source_baseline.expected_source_hash = None;
        assert!(matches!(
            render_diff(&missing_source_baseline),
            Err(DiffError::InvalidProposal(
                ProposalError::MissingActionField {
                    action: "move_to_docs",
                    field: "expected_source_hash"
                }
            ))
        ));
        let mut changed_source = move_proposal.clone();
        changed_source.expected_source_hash = Some(crate::instructions::content_hash(b"other\n"));
        assert!(matches!(
            render_diff(&changed_source),
            Err(DiffError::ChangedSource(path)) if path == source_path
        ));
        let mut changed_target = move_proposal.clone();
        changed_target.expected_target_hash = Some(crate::instructions::content_hash(b"other\n"));
        assert!(matches!(
            render_diff(&changed_target),
            Err(DiffError::ChangedTarget(path)) if path == docs
        ));

        let mut split = proposal(&target, ProposalAction::SplitScope);
        split.source_path = Some(source_path.clone());
        split.existing_text = Some("move this\n".to_owned());
        split.expected_target_hash = Some(second_line_target_hash);
        split.expected_source_hash =
            Some(crate::instructions::content_hash(b"move this\nkeep next\n"));
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
    fn move_to_docs_link_is_relative_and_keeps_line_ending() {
        assert_eq!(
            move_to_docs_link(
                Path::new("/work/docs/project/AGENTS.md"),
                Path::new("/work/docs/project/docs/knowledge.md"),
                "old\n",
            ),
            "See [the detailed fact](docs/knowledge.md).\n"
        );
        assert_eq!(
            move_to_docs_link(
                Path::new("/work/project/src/AGENTS.md"),
                Path::new("/work/project/docs/knowledge.md"),
                "old\r\n",
            ),
            "See [the detailed fact](../docs/knowledge.md).\r\n"
        );
    }
}
