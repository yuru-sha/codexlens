//! Validated, transactional proposal application.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::analysis::{FindingScope, bounded_excerpt};
use crate::instructions::content_hash;
use crate::model::{
    CanonicalData, InstructionFileKind, InstructionFileState, InstructionJoin, InstructionScope,
};

use super::diff::{RenderedDiff, prepare_changes, render_diff};
use super::proposal::{Proposal, ProposalAction};

const MAX_MESSAGE_BYTES: usize = 256;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("proposal batch rejected: {0}")]
    InvalidBatch(String),
    #[error("optimize --apply requires explicit confirmation")]
    ConfirmationRequired,
    #[error("backup creation failed: {message}; backups retained at {backup_dir}")]
    BackupFailed { message: String, backup_dir: String },
    #[error(
        "apply failed for {path}: {message}; recovery: restored; backups retained at {backup_dir}"
    )]
    RolledBack {
        path: String,
        message: String,
        backup_dir: String,
    },
    #[error(
        "apply failed for {path}: {message}; recovery failed: {recovery}; backups retained at {backup_dir}"
    )]
    RecoveryFailed {
        path: String,
        message: String,
        recovery: String,
        backup_dir: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    NotNeeded,
}

impl std::fmt::Display for RecoveryStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNeeded => formatter.write_str("not needed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub changed_files: Vec<PathBuf>,
    pub backup_dir: PathBuf,
    pub recovery: RecoveryStatus,
}

#[derive(Debug)]
pub struct ApplyPlan {
    changes: Vec<PlannedChange>,
    write_set: Vec<PathBuf>,
}

impl ApplyPlan {
    pub fn write_set(&self) -> &[PathBuf] {
        &self.write_set
    }

    pub fn apply(self, confirmed: bool) -> Result<ApplyReport, ApplyError> {
        if !confirmed {
            return Err(ApplyError::ConfirmationRequired);
        }
        apply_plan(self, None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedChange {
    path: PathBuf,
    before: String,
    after: String,
    expected_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct ScopeContext {
    root: PathBuf,
    selected: BTreeSet<PathBuf>,
    project: bool,
}

#[derive(Debug)]
struct Backup {
    path: PathBuf,
    backup_path: PathBuf,
}

/// Validate the complete reviewed batch without writing any target.
pub fn prepare_apply(
    data: &CanonicalData,
    batch: &[RenderedDiff],
) -> Result<ApplyPlan, ApplyError> {
    if batch.is_empty() {
        return Err(ApplyError::InvalidBatch(
            "no applicable proposals".to_owned(),
        ));
    }

    let mut changes = Vec::new();
    let mut write_set = BTreeMap::<PathBuf, PathBuf>::new();
    for rendered in batch {
        let proposal = &rendered.proposal;
        let paths = validate_proposal_scope(data, proposal)?;
        let prepared = prepare_changes(proposal)
            .map_err(|error| invalid_batch(format!("proposal diff is invalid: {error}")))?;
        let rendered_again = render_diff(proposal)
            .map_err(|error| invalid_batch(format!("proposal diff is invalid: {error}")))?;
        if rendered_again != rendered.diff || rendered.diff.is_empty() {
            return Err(invalid_batch(
                "reviewed patch does not match the validated proposal",
            ));
        }

        let expected_paths = proposal_paths(proposal);
        let actual_paths = prepared
            .iter()
            .map(|change| change.path.clone())
            .collect::<BTreeSet<_>>();
        if actual_paths != expected_paths {
            return Err(invalid_batch(
                "proposal write set does not match its action",
            ));
        }

        for change in prepared {
            let canonical = paths
                .get(&change.path)
                .ok_or_else(|| invalid_batch("proposal contains an unvalidated write path"))?
                .clone();
            if write_set
                .insert(canonical.clone(), change.path.clone())
                .is_some()
            {
                return Err(invalid_batch(format!(
                    "conflicting proposals share {}",
                    path_label(&canonical)
                )));
            }
            let expected_hash = expected_hash(proposal, &change.path)
                .ok_or_else(|| invalid_batch("proposal is missing an expected file hash"))?;
            if content_hash(change.before.as_bytes()) != expected_hash {
                return Err(invalid_batch(format!(
                    "proposal hash does not match {}",
                    path_label(&change.path)
                )));
            }
            changes.push(PlannedChange {
                path: canonical,
                before: change.before,
                after: change.after,
                expected_hash: expected_hash.to_owned(),
            });
        }
    }

    if changes.iter().all(|change| change.before == change.after) {
        return Err(invalid_batch("proposal batch contains no changes"));
    }
    let mut write_set = write_set.into_keys().collect::<Vec<_>>();
    write_set.sort();
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ApplyPlan { changes, write_set })
}

/// Validate scope before reading proposal targets, then prepare the reviewed patch.
pub fn prepare_apply_proposals(
    data: &CanonicalData,
    proposals: &[Proposal],
) -> Result<ApplyPlan, ApplyError> {
    if proposals.is_empty() {
        return Err(ApplyError::InvalidBatch(
            "no applicable proposals".to_owned(),
        ));
    }
    let mut rendered = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        validate_proposal_scope(data, proposal)?;
        let diff = render_diff(proposal)
            .map_err(|error| invalid_batch(format!("proposal diff is invalid: {error}")))?;
        if diff.is_empty() {
            return Err(invalid_batch("proposal batch contains a no-op"));
        }
        rendered.push(RenderedDiff {
            proposal: proposal.clone(),
            diff,
        });
    }
    prepare_apply(data, &rendered)
}

fn proposal_paths(proposal: &Proposal) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::from([proposal.target_path.clone()]);
    if matches!(
        proposal.action,
        ProposalAction::MoveToDocs | ProposalAction::SplitScope
    ) {
        if let Some(source) = &proposal.source_path {
            paths.insert(source.clone());
        }
    }
    paths
}

fn expected_hash<'a>(proposal: &'a Proposal, path: &Path) -> Option<&'a str> {
    if path == proposal.target_path {
        proposal.expected_target_hash.as_deref()
    } else if proposal.source_path.as_deref() == Some(path) {
        proposal.expected_source_hash.as_deref()
    } else {
        None
    }
}

fn validate_proposal_scope(
    data: &CanonicalData,
    proposal: &Proposal,
) -> Result<BTreeMap<PathBuf, PathBuf>, ApplyError> {
    if proposal.source_path.is_some()
        && !matches!(
            proposal.action,
            ProposalAction::MoveToDocs | ProposalAction::SplitScope
        )
    {
        return Err(invalid_batch(
            "source_path is only allowed for move_to_docs or split_scope",
        ));
    }
    if matches!(proposal.target_scope, FindingScope::Path(_)) {
        return Err(invalid_batch("path-scoped proposals cannot be applied"));
    }
    let target = canonical_file(&proposal.target_path)?;
    if let FindingScope::Instruction(path) = &proposal.target_scope {
        let scoped = canonical_file(path)?;
        if scoped != target {
            return Err(invalid_batch(
                "target path does not match instruction scope",
            ));
        }
    }
    if let FindingScope::Project(path) = &proposal.target_scope {
        let scoped = canonical_directory(path)?;
        if scoped != target && !target.starts_with(&scoped) {
            return Err(invalid_batch("target path is outside project scope"));
        }
    }

    let session_ids = proposal
        .evidence
        .iter()
        .filter_map(|evidence| evidence.session_id.as_deref())
        .collect::<BTreeSet<_>>();
    if session_ids.is_empty() {
        return Err(invalid_batch(
            "proposal evidence has no session-bound instruction resolution",
        ));
    }

    let mut paths = BTreeMap::new();
    let mut common_root = None;
    for session_id in session_ids {
        let join = unique_join(data, session_id)?;
        let target_context = target_context(join, proposal, &target)?;
        if common_root
            .as_ref()
            .is_some_and(|root: &PathBuf| root != &target_context.root)
        {
            return Err(invalid_batch("proposal resolves to ambiguous roots"));
        }
        common_root = Some(target_context.root.clone());
        validate_target(&target_context, &target, proposal.action)?;
        paths.insert(proposal.target_path.clone(), target.clone());

        if let Some(source_path) = &proposal.source_path {
            let source = canonical_file(source_path)?;
            let source_context = selected_context(join, &source)?;
            if !source.starts_with(&source_context.root) {
                return Err(invalid_batch("source is outside the resolved root"));
            }
            if source_context.root != target_context.root {
                return Err(invalid_batch(
                    "source and target are not under one resolved root",
                ));
            }
            if source == target {
                return Err(invalid_batch("source and target must be distinct"));
            }
            paths.insert(source_path.clone(), source);
        }
    }

    Ok(paths)
}

fn unique_join<'a>(
    data: &'a CanonicalData,
    session_id: &str,
) -> Result<&'a InstructionJoin, ApplyError> {
    let mut matches = data
        .instruction_joins
        .iter()
        .filter(|join| join.session_id == session_id);
    let Some(join) = matches.next() else {
        return Err(invalid_batch(
            "proposal session has no instruction resolution",
        ));
    };
    if matches.next().is_some() {
        return Err(invalid_batch(
            "proposal session has ambiguous instruction resolutions",
        ));
    }
    Ok(join)
}

fn target_context(
    join: &InstructionJoin,
    proposal: &Proposal,
    target: &Path,
) -> Result<ScopeContext, ApplyError> {
    match &proposal.target_scope {
        FindingScope::Global => context_for_kind(join, InstructionScope::Global),
        FindingScope::Project(root) => {
            let context = context_for_kind(join, InstructionScope::ProjectRoot)?;
            let requested = canonical_directory(root)?;
            if requested != context.root {
                return Err(invalid_batch(
                    "proposal project scope is not the resolved root",
                ));
            }
            Ok(context)
        }
        FindingScope::Instruction(_) => selected_context(join, target),
        FindingScope::Path(_) => Err(invalid_batch("path-scoped proposals cannot be applied")),
    }
}

fn selected_context(join: &InstructionJoin, path: &Path) -> Result<ScopeContext, ApplyError> {
    let mut kinds = Vec::new();
    for file in selected_files(join) {
        if canonical_file(&file.path)? == path && !kinds.contains(&file.scope) {
            kinds.push(file.scope);
        }
    }
    if kinds.len() != 1 {
        return Err(invalid_batch(
            "path is not one unambiguous selected instruction file",
        ));
    }
    context_for_kind(join, kinds[0])
}

fn context_for_kind(
    join: &InstructionJoin,
    kind: InstructionScope,
) -> Result<ScopeContext, ApplyError> {
    match kind {
        InstructionScope::Global => {
            if !join.resolution.diagnostics.is_empty() {
                return Err(invalid_batch(
                    "global instruction resolution is unavailable",
                ));
            }
            let files = selected_files(join)
                .filter(|file| {
                    file.scope == InstructionScope::Global
                        && matches!(
                            file.kind,
                            InstructionFileKind::Override | InstructionFileKind::Standard
                        )
                        && file
                            .path
                            .file_name()
                            .is_some_and(|name| name == "AGENTS.override.md" || name == "AGENTS.md")
                })
                .collect::<Vec<_>>();
            if files.len() != 1 {
                return Err(invalid_batch(
                    "global instruction resolution is missing or ambiguous",
                ));
            }
            let selected = [canonical_file(&files[0].path)?].into_iter().collect();
            let root_path = files[0]
                .path
                .parent()
                .ok_or_else(|| invalid_batch("global instruction root is unavailable"))?;
            Ok(ScopeContext {
                root: canonical_directory(root_path)?,
                selected,
                project: false,
            })
        }
        InstructionScope::ProjectRoot | InstructionScope::ProjectNested => {
            if join.resolution.project_root_status != crate::model::ProjectRootStatus::Known {
                return Err(invalid_batch("project instruction root is unavailable"));
            }
            if join.resolution.diagnostics.iter().any(|diagnostic| {
                diagnostic.kind != crate::model::InstructionDiagnosticKind::GlobalScopeUnavailable
            }) || join.resolution.truncated
            {
                return Err(invalid_batch(
                    "project instruction resolution is unavailable",
                ));
            }
            let root = join
                .resolution
                .project_root
                .as_deref()
                .ok_or_else(|| invalid_batch("project instruction root is unavailable"))?;
            let selected = selected_files(join)
                .filter(|file| {
                    matches!(
                        file.scope,
                        InstructionScope::ProjectRoot | InstructionScope::ProjectNested
                    )
                })
                .map(|file| canonical_file(&file.path))
                .collect::<Result<BTreeSet<_>, _>>()?;
            Ok(ScopeContext {
                root: canonical_directory(root)?,
                selected,
                project: true,
            })
        }
    }
}

fn selected_files(join: &InstructionJoin) -> impl Iterator<Item = &crate::model::InstructionFile> {
    join.resolution
        .files
        .iter()
        .filter(|file| file.state == InstructionFileState::Selected)
}

fn validate_target(
    context: &ScopeContext,
    target: &Path,
    action: ProposalAction,
) -> Result<(), ApplyError> {
    if !target.starts_with(&context.root) {
        return Err(invalid_batch("target is outside the resolved root"));
    }
    if context.selected.contains(target) {
        return Ok(());
    }
    if matches!(
        action,
        ProposalAction::MoveToDocs | ProposalAction::SplitScope
    ) && context.project
        && permitted_documentation_target(target, &context.root)
    {
        return Ok(());
    }
    Err(invalid_batch(
        "target is outside the selected instruction write set",
    ))
}

fn permitted_documentation_target(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative == Path::new("README.md") {
        return true;
    }
    let mut components = relative.components();
    matches!(components.next(), Some(Component::Normal(name)) if name == "docs")
        && path.extension().is_some_and(|extension| extension == "md")
}

fn canonical_file(path: &Path) -> Result<PathBuf, ApplyError> {
    canonical_existing(path, ExistingKind::File)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ApplyError> {
    canonical_existing(path, ExistingKind::Directory)
}

fn canonical_existing(path: &Path, expected: ExistingKind) -> Result<PathBuf, ApplyError> {
    if !path.is_absolute() {
        return Err(invalid_batch(format!(
            "relative path is not allowed: {}",
            path_label(path)
        )));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(invalid_batch(format!(
            "raw parent traversal is not allowed: {}",
            path_label(path)
        )));
    }
    let components = path.components().collect::<Vec<_>>();
    let mut current = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        if current.as_os_str().is_empty() {
            continue;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            invalid_batch(format!(
                "path is unavailable at {}: {}",
                path_label(&current),
                bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES)
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_batch(format!(
                "symlink path component is not allowed: {}",
                path_label(&current)
            )));
        }
        let last = index + 1 == components.len();
        if last {
            let valid = match expected {
                ExistingKind::File => metadata.is_file(),
                ExistingKind::Directory => metadata.is_dir(),
            };
            if !valid {
                return Err(invalid_batch(format!(
                    "path is not an existing regular {}: {}",
                    match expected {
                        ExistingKind::File => "file",
                        ExistingKind::Directory => "directory",
                    },
                    path_label(path)
                )));
            }
        } else if !metadata.is_dir() {
            return Err(invalid_batch(format!(
                "path component is not a directory: {}",
                path_label(&current)
            )));
        }
    }
    fs::canonicalize(path).map_err(|error| {
        invalid_batch(format!(
            "path cannot be canonicalized: {}: {}",
            path_label(path),
            bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES)
        ))
    })
}

fn apply_plan(
    plan: ApplyPlan,
    fail_after_writes: Option<usize>,
) -> Result<ApplyReport, ApplyError> {
    for change in &plan.changes {
        let current = canonical_file(&change.path)?;
        if current != change.path {
            return Err(invalid_batch("validated path changed before writing"));
        }
        let content = fs::read_to_string(&change.path).map_err(|error| {
            invalid_batch(format!(
                "validated file cannot be re-read: {}: {}",
                path_label(&change.path),
                bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES)
            ))
        })?;
        if content != change.before || content_hash(content.as_bytes()) != change.expected_hash {
            return Err(invalid_batch(format!(
                "file changed since review: {}",
                path_label(&change.path)
            )));
        }
    }

    let backup_dir = create_backup_dir().map_err(|error| ApplyError::BackupFailed {
        message: bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES),
        backup_dir: path_label(&error.backup_dir),
    })?;
    let backups = match create_backups(&backup_dir, &plan.write_set) {
        Ok(backups) => backups,
        Err(error) => {
            return Err(ApplyError::BackupFailed {
                message: bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES),
                backup_dir: path_label(&backup_dir),
            });
        }
    };
    if let Err(error) = write_manifest(&backup_dir, &backups) {
        return Err(ApplyError::BackupFailed {
            message: bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES),
            backup_dir: path_label(&backup_dir),
        });
    }

    for (index, change) in plan.changes.iter().enumerate() {
        if fail_after_writes == Some(index) {
            return apply_failure(
                &backups[..index],
                &backup_dir,
                &change.path,
                "synthetic write failure",
            );
        }
        if let Err(error) = write_change(change, index) {
            return apply_failure(
                &backups[..=index],
                &backup_dir,
                &change.path,
                &bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES),
            );
        }
    }

    Ok(ApplyReport {
        changed_files: plan.write_set,
        backup_dir,
        recovery: RecoveryStatus::NotNeeded,
    })
}

struct BackupDirectoryError {
    backup_dir: PathBuf,
    source: std::io::Error,
}

impl std::fmt::Display for BackupDirectoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

fn create_backup_dir() -> Result<PathBuf, BackupDirectoryError> {
    let base = std::env::temp_dir().join("codexlens-backups");
    fs::create_dir_all(&base).map_err(|source| BackupDirectoryError {
        backup_dir: base.clone(),
        source,
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for attempt in 0..100 {
        let path = base.join(format!("{}-{nonce}-{attempt}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(BackupDirectoryError {
                    backup_dir: path,
                    source,
                });
            }
        }
    }
    Err(BackupDirectoryError {
        backup_dir: base,
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique backup directory",
        ),
    })
}

fn create_backups(backup_dir: &Path, paths: &[PathBuf]) -> std::io::Result<Vec<Backup>> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let backup_path = backup_dir.join(format!("{index:04}.bak"));
            fs::copy(path, &backup_path)?;
            Ok(Backup {
                path: path.clone(),
                backup_path,
            })
        })
        .collect()
}

fn write_manifest(backup_dir: &Path, backups: &[Backup]) -> std::io::Result<()> {
    let manifest = backups
        .iter()
        .enumerate()
        .map(|(index, backup)| format!("{index:04}\t{}\n", path_label(&backup.path)))
        .collect::<String>();
    fs::write(backup_dir.join("manifest.tsv"), manifest)
}

fn write_change(change: &PlannedChange, index: usize) -> std::io::Result<()> {
    let parent = change
        .path
        .parent()
        .ok_or_else(|| std::io::Error::other("target has no parent directory"))?;
    let temporary = parent.join(format!(
        ".codexlens-apply-{}-{index}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(change.after.as_bytes())?;
        file.sync_all()?;
        let permissions = fs::metadata(&change.path)?.permissions();
        fs::set_permissions(&temporary, permissions)?;
        fs::rename(&temporary, &change.path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn apply_failure(
    backups: &[Backup],
    backup_dir: &Path,
    failed_path: &Path,
    message: &str,
) -> Result<ApplyReport, ApplyError> {
    match restore_backups(backups) {
        Ok(()) => Err(ApplyError::RolledBack {
            path: path_label(failed_path),
            message: bounded_excerpt(message, MAX_MESSAGE_BYTES),
            backup_dir: path_label(backup_dir),
        }),
        Err(error) => Err(ApplyError::RecoveryFailed {
            path: path_label(failed_path),
            message: bounded_excerpt(message, MAX_MESSAGE_BYTES),
            recovery: bounded_excerpt(&error.to_string(), MAX_MESSAGE_BYTES),
            backup_dir: path_label(backup_dir),
        }),
    }
}

fn restore_backups(backups: &[Backup]) -> std::io::Result<()> {
    for backup in backups {
        if canonical_file(&backup.path).map_err(|_| {
            std::io::Error::other(format!(
                "validated path is no longer safe: {}",
                backup.path.display()
            ))
        })? != backup.path
        {
            return Err(std::io::Error::other(
                "validated path changed during recovery",
            ));
        }
        fs::copy(&backup.backup_path, &backup.path)?;
    }
    Ok(())
}

fn invalid_batch(message: impl Into<String>) -> ApplyError {
    ApplyError::InvalidBatch(bounded_excerpt(&message.into(), MAX_MESSAGE_BYTES))
}

fn path_label(path: &Path) -> String {
    bounded_excerpt(&path.display().to_string(), MAX_MESSAGE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::diff::render_diff;
    use crate::advisor::test_support::{data_with_join, file, join, proposal, temp_file};
    use crate::analysis::FindingScope;
    use crate::model::InstructionScope;

    #[test]
    fn multi_proposal_failure_restores_all_files_and_keeps_backups() {
        let root = temp_file("project");
        let first = root.join("AGENTS.md");
        let second = root.join("docs.md");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();
        let first_hash = content_hash(b"first\n");
        let second_hash = content_hash(b"second\n");
        let files = vec![
            file(
                &first.to_string_lossy(),
                InstructionScope::ProjectRoot,
                "first\n",
            ),
            file(
                &second.to_string_lossy(),
                InstructionScope::ProjectRoot,
                "second\n",
            ),
        ];
        let mut data = data_with_join(files);
        data.sessions[0].project = Some(root.to_string_lossy().into_owned());
        data.sessions[0].cwd = Some(root.to_string_lossy().into_owned());
        data.instruction_joins[0] = join(
            "session",
            &root.to_string_lossy(),
            vec![
                file(
                    &first.to_string_lossy(),
                    InstructionScope::ProjectRoot,
                    "first\n",
                ),
                file(
                    &second.to_string_lossy(),
                    InstructionScope::ProjectRoot,
                    "second\n",
                ),
            ],
        );

        let mut first_proposal = proposal(&first, ProposalAction::Add);
        first_proposal.target_scope = FindingScope::Project(root.clone());
        first_proposal.expected_target_hash = Some(first_hash);
        let mut second_proposal = proposal(&second, ProposalAction::Add);
        second_proposal.target_scope = FindingScope::Project(root.clone());
        second_proposal.expected_target_hash = Some(second_hash);
        let first_diff = render_diff(&first_proposal).unwrap();
        let second_diff = render_diff(&second_proposal).unwrap();
        let plan = prepare_apply(
            &data,
            &[
                RenderedDiff {
                    proposal: first_proposal,
                    diff: first_diff,
                },
                RenderedDiff {
                    proposal: second_proposal,
                    diff: second_diff,
                },
            ],
        )
        .unwrap();

        let error = apply_plan(plan, Some(1)).unwrap_err();
        let backup_dir = match error {
            ApplyError::RolledBack { backup_dir, .. } => PathBuf::from(backup_dir),
            other => panic!("unexpected error: {other}"),
        };
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second\n");
        assert!(backup_dir.join("manifest.tsv").is_file());
        assert!(backup_dir.join("0000.bak").is_file());
        assert!(backup_dir.join("0001.bak").is_file());

        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
        let _ = std::fs::remove_dir(root);
        let _ = std::fs::remove_dir_all(backup_dir);
    }

    #[test]
    fn move_to_docs_applies_source_and_target_as_one_write_set() {
        let root = temp_file("move-project");
        let source = root.join("AGENTS.md");
        let docs_dir = root.join("docs");
        let target = docs_dir.join("knowledge.md");
        std::fs::create_dir_all(&docs_dir).unwrap();
        std::fs::write(&source, "move this\nkeep next\n").unwrap();
        std::fs::write(&target, "docs\n").unwrap();
        let mut data = data_with_join(vec![file(
            &source.to_string_lossy(),
            InstructionScope::ProjectRoot,
            "move this\nkeep next\n",
        )]);
        data.sessions[0].project = Some(root.to_string_lossy().into_owned());
        data.sessions[0].cwd = Some(root.to_string_lossy().into_owned());
        data.instruction_joins[0] = join(
            "session",
            &root.to_string_lossy(),
            vec![file(
                &source.to_string_lossy(),
                InstructionScope::ProjectRoot,
                "move this\nkeep next\n",
            )],
        );

        let mut proposal = proposal(&target, ProposalAction::MoveToDocs);
        proposal.target_scope = FindingScope::Project(root.clone());
        proposal.source_path = Some(source.clone());
        proposal.existing_text = Some("move this\n".to_owned());
        proposal.proposed_text = Some("move this\n".to_owned());
        proposal.expected_target_hash = Some(content_hash(b"docs\n"));
        proposal.expected_source_hash = Some(content_hash(b"move this\nkeep next\n"));
        let diff = render_diff(&proposal).unwrap();
        let plan = prepare_apply(&data, &[RenderedDiff { proposal, diff }]).unwrap();
        let report = plan.apply(true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "See [the detailed fact](docs/knowledge.md).\nkeep next\n"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "docs\n\nmove this\n"
        );
        assert_eq!(report.changed_files.len(), 2);
        assert!(report.backup_dir.join("manifest.tsv").is_file());

        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(target);
        let _ = std::fs::remove_dir(docs_dir);
        let _ = std::fs::remove_dir(root);
        let _ = std::fs::remove_dir_all(report.backup_dir);
    }

    #[test]
    fn preflight_rejects_traversal_changed_hash_and_tampered_patch_without_writes() {
        let root = temp_file("preflight-project");
        let target = root.join("AGENTS.md");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&target, "old\n").unwrap();
        let mut data = data_with_join(vec![file(
            &target.to_string_lossy(),
            InstructionScope::ProjectRoot,
            "old\n",
        )]);
        data.sessions[0].project = Some(root.to_string_lossy().into_owned());
        data.sessions[0].cwd = Some(root.to_string_lossy().into_owned());
        data.instruction_joins[0] = join(
            "session",
            &root.to_string_lossy(),
            vec![file(
                &target.to_string_lossy(),
                InstructionScope::ProjectRoot,
                "old\n",
            )],
        );

        let mut traversal = proposal(&root.join("..").join("x.md"), ProposalAction::Add);
        traversal.target_scope = FindingScope::Project(root.clone());
        traversal.expected_target_hash = Some(content_hash(b"old\n"));
        let error = prepare_apply_proposals(&data, &[traversal]).unwrap_err();
        assert!(error.to_string().contains("parent traversal"));
        assert!(error.to_string().len() < 512);

        let mut changed = proposal(&target, ProposalAction::Add);
        changed.target_scope = FindingScope::Project(root.clone());
        changed.expected_target_hash = Some(content_hash(b"old\n"));
        let reviewed = RenderedDiff {
            diff: render_diff(&changed).unwrap(),
            proposal: changed,
        };
        std::fs::write(&target, "changed\n").unwrap();
        assert!(prepare_apply(&data, &[reviewed]).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "changed\n");

        std::fs::write(&target, "old\n").unwrap();
        let mut tampered = proposal(&target, ProposalAction::Add);
        tampered.target_scope = FindingScope::Project(root.clone());
        tampered.expected_target_hash = Some(content_hash(b"old\n"));
        let error = prepare_apply(
            &data,
            &[RenderedDiff {
                proposal: tampered,
                diff: "tampered patch".to_owned(),
            }],
        )
        .unwrap_err();
        assert!(error.to_string().contains("reviewed patch"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");

        let _ = std::fs::remove_file(target);
        let _ = std::fs::remove_dir(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_components_are_rejected_before_writing() {
        use std::os::unix::fs::symlink;

        let root = temp_file("symlink-project");
        let real = root.join("real");
        let link = root.join("link");
        let target = link.join("AGENTS.md");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("AGENTS.md"), "old\n").unwrap();
        symlink(&real, &link).unwrap();
        let mut proposal = proposal(&target, ProposalAction::Add);
        proposal.target_scope = FindingScope::Project(root.clone());
        proposal.expected_target_hash = Some(content_hash(b"old\n"));
        let error = canonical_file(&target).unwrap_err();
        assert!(error.to_string().contains("symlink"));

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(real.join("AGENTS.md"));
        let _ = std::fs::remove_dir(real);
        let _ = std::fs::remove_dir(root);
        let _ = proposal;
    }
}
