use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::config::{ConfigDiagnostic, ConfigReadResult, InstructionConfig, load_config_at};
use crate::model::{
    InstructionDiagnostic, InstructionDiagnosticKind, InstructionFile, InstructionFileKind,
    InstructionFileState, InstructionJoin, InstructionResolution, InstructionScope,
    InstructionSnapshot, InstructionSnapshotAccuracy, InstructionSnapshotEntry,
    InstructionSnapshotSource, ProjectRootStatus, Session, SourceRef,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstructionCaptureOptions {
    pub codex_home: Option<PathBuf>,
    pub config: InstructionConfig,
    config_diagnostics: Vec<ConfigDiagnostic>,
}

impl InstructionCaptureOptions {
    pub fn from_codex_home(
        codex_home: &Path,
        explicit_config: Option<&Path>,
    ) -> (Self, ConfigReadResult) {
        let loaded = load_config_at(codex_home, explicit_config);
        (
            Self {
                codex_home: Some(codex_home.to_path_buf()),
                config: loaded.config.clone(),
                config_diagnostics: loaded.diagnostics.clone(),
            },
            loaded,
        )
    }

    pub fn resolver(&self) -> InstructionResolver {
        let mut resolver = InstructionResolver::new(self.codex_home.clone(), self.config.clone());
        resolver.config_diagnostics = self.config_diagnostics.clone();
        resolver
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionResolver {
    codex_home: Option<PathBuf>,
    config: InstructionConfig,
    config_diagnostics: Vec<ConfigDiagnostic>,
}

impl Default for InstructionResolver {
    fn default() -> Self {
        Self::new(None, InstructionConfig::default())
    }
}

impl InstructionResolver {
    pub fn new(codex_home: Option<PathBuf>, config: InstructionConfig) -> Self {
        Self {
            codex_home,
            config,
            config_diagnostics: Vec::new(),
        }
    }

    pub fn config(&self) -> &InstructionConfig {
        &self.config
    }

    pub fn from_codex_home(
        codex_home: &Path,
        explicit_config: Option<&Path>,
    ) -> (Self, ConfigReadResult) {
        let (capture, loaded) =
            InstructionCaptureOptions::from_codex_home(codex_home, explicit_config);
        (capture.resolver(), loaded)
    }

    pub fn resolve(
        &self,
        project_root_hint: Option<&Path>,
        cwd: Option<&Path>,
    ) -> InstructionResolution {
        let mut files = Vec::new();
        let mut chain = Vec::new();
        let mut diagnostics = Vec::new();
        let max_bytes = self.config.project_doc_max_bytes.max(1);

        for diagnostic in &self.config_diagnostics {
            let line = diagnostic
                .line
                .map_or_else(String::new, |line| format!("line {line}: "));
            diagnostics.push(InstructionDiagnostic {
                path: Some(diagnostic.path.clone()),
                kind: InstructionDiagnosticKind::Config,
                message: bounded_message(&format!(
                    "{}{}: {}",
                    diagnostic.kind.as_str(),
                    line,
                    diagnostic.message
                )),
            });
        }

        if let Some(codex_home) = self.codex_home.as_deref() {
            if codex_home.is_absolute() {
                select_global(
                    codex_home,
                    max_bytes,
                    &mut files,
                    &mut chain,
                    &mut diagnostics,
                );
            } else {
                diagnostics.push(InstructionDiagnostic {
                    path: Some(codex_home.to_path_buf()),
                    kind: InstructionDiagnosticKind::RelativePath,
                    message: "Codex home must be an absolute path".to_owned(),
                });
            }
        } else {
            diagnostics.push(InstructionDiagnostic {
                path: None,
                kind: InstructionDiagnosticKind::GlobalScopeUnavailable,
                message: "Codex home was not provided; global instructions are unavailable"
                    .to_owned(),
            });
        }

        let (project_root, project_root_status) =
            project_root(project_root_hint, cwd, &mut diagnostics);
        if project_root_status == ProjectRootStatus::Known {
            if let Some(root) = project_root.as_deref() {
                let directories = project_directories(root, cwd);
                for directory in directories {
                    select_project_directory(
                        &directory,
                        if directory.as_path() == root {
                            InstructionScope::ProjectRoot
                        } else {
                            InstructionScope::ProjectNested
                        },
                        &self.config,
                        &mut files,
                        &mut chain,
                        &mut diagnostics,
                    );
                }
            }
        }

        let truncated = chain
            .iter()
            .any(|file: &InstructionFile| file.state == InstructionFileState::Truncated);
        let (effective_content, effective_chain_hash, byte_count, chain_truncated) =
            effective_chain(&chain, max_bytes);
        let truncated = truncated || chain_truncated;
        if truncated
            && !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == InstructionDiagnosticKind::Truncated)
        {
            diagnostics.push(InstructionDiagnostic {
                path: chain
                    .iter()
                    .find(|file| file.state == InstructionFileState::Truncated)
                    .map(|file| file.path.clone()),
                kind: InstructionDiagnosticKind::Truncated,
                message: "instruction chain reached the configured byte limit".to_owned(),
            });
        }

        InstructionResolution {
            project_root,
            cwd: cwd.map(Path::to_path_buf),
            project_root_status,
            files,
            chain,
            effective_content,
            effective_chain_hash,
            byte_count,
            truncated,
            diagnostics,
        }
    }

    pub fn resolve_session(&self, session: &Session) -> InstructionResolution {
        let project_root = session.project.as_deref().map(Path::new);
        let cwd = session.cwd.as_deref().map(Path::new);
        self.resolve(project_root, cwd)
    }
}

pub fn resolve_instructions(
    codex_home: Option<&Path>,
    project_root: Option<&Path>,
    cwd: Option<&Path>,
    config: &InstructionConfig,
) -> InstructionResolution {
    InstructionResolver::new(codex_home.map(Path::to_path_buf), config.clone())
        .resolve(project_root, cwd)
}

pub fn join_session(session: &Session, resolver: &InstructionResolver) -> InstructionJoin {
    let resolution = resolver.resolve_session(session);
    let nearest = resolution.chain.last();
    InstructionJoin {
        session_id: session.id.clone(),
        cwd: session.cwd.as_deref().map(PathBuf::from),
        project_root: resolution.project_root.clone(),
        project_root_status: resolution.project_root_status,
        nearest_path: nearest.map(|file| file.path.clone()),
        nearest_scope: nearest.map(|file| file.scope),
        resolution,
        provenance: session.provenance.clone(),
    }
}

pub fn join_sessions(sessions: &[Session], resolver: &InstructionResolver) -> Vec<InstructionJoin> {
    let mut joins = sessions
        .iter()
        .map(|session| join_session(session, resolver))
        .collect::<Vec<_>>();
    joins.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    joins
}

pub fn snapshot_from_rollout(
    session_id: Option<String>,
    turn_id: Option<String>,
    content: Option<&str>,
    provenance: SourceRef,
) -> InstructionSnapshot {
    let (content, content_hash, byte_count, chain, effective_chain_hash) = match content {
        Some(content) => {
            let hash = content_hash(content.as_bytes());
            (
                Some(content.to_owned()),
                Some(hash.clone()),
                content.len(),
                vec![InstructionSnapshotEntry {
                    path: provenance.path.clone(),
                    scope: None,
                    kind: InstructionFileKind::Observed,
                    state: InstructionFileState::Selected,
                    chain_position: 0,
                    content_hash: Some(hash.clone()),
                    byte_count: content.len(),
                }],
                Some(hash),
            )
        }
        None => (None, None, 0, Vec::new(), None),
    };
    InstructionSnapshot {
        session_id,
        turn_id,
        source: if content.is_some() {
            InstructionSnapshotSource::Rollout
        } else {
            InstructionSnapshotSource::Unavailable
        },
        accuracy: if content.is_some() {
            InstructionSnapshotAccuracy::Observed
        } else {
            InstructionSnapshotAccuracy::Unavailable
        },
        content,
        content_hash,
        byte_count,
        chain,
        effective_chain_hash,
        truncated: false,
        provenance,
    }
}

pub(crate) fn snapshot_entries(files: &[InstructionFile]) -> Vec<InstructionSnapshotEntry> {
    files
        .iter()
        .enumerate()
        .map(|(chain_position, file)| InstructionSnapshotEntry {
            path: file.path.clone(),
            scope: Some(file.scope),
            kind: file.kind.clone(),
            state: file.state,
            chain_position,
            content_hash: file.content_hash.clone(),
            byte_count: file.byte_count,
        })
        .collect()
}

pub fn snapshot_from_resolution(
    session_id: Option<String>,
    turn_id: Option<String>,
    resolution: &InstructionResolution,
    provenance: SourceRef,
) -> InstructionSnapshot {
    let Some(content) = resolution.effective_content.as_deref() else {
        return unavailable_snapshot(session_id, turn_id, provenance);
    };
    let chain = snapshot_entries(&resolution.chain);
    InstructionSnapshot {
        session_id,
        turn_id,
        source: InstructionSnapshotSource::FilesystemAtIngest,
        accuracy: InstructionSnapshotAccuracy::Reconstructed,
        content: Some(content.to_owned()),
        content_hash: Some(content_hash(content.as_bytes())),
        byte_count: content.len(),
        chain,
        effective_chain_hash: resolution.effective_chain_hash.clone(),
        truncated: resolution.truncated,
        provenance,
    }
}

pub fn unavailable_snapshot(
    session_id: Option<String>,
    turn_id: Option<String>,
    provenance: SourceRef,
) -> InstructionSnapshot {
    InstructionSnapshot {
        session_id,
        turn_id,
        source: InstructionSnapshotSource::Unavailable,
        accuracy: InstructionSnapshotAccuracy::Unavailable,
        content: None,
        content_hash: None,
        byte_count: 0,
        chain: Vec::new(),
        effective_chain_hash: None,
        truncated: false,
        provenance,
    }
}

pub fn content_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn select_global(
    codex_home: &Path,
    max_bytes: usize,
    files: &mut Vec<InstructionFile>,
    chain: &mut Vec<InstructionFile>,
    diagnostics: &mut Vec<InstructionDiagnostic>,
) {
    select_candidates(
        [
            ("AGENTS.override.md", InstructionFileKind::Override),
            ("AGENTS.md", InstructionFileKind::Standard),
        ]
        .into_iter()
        .map(|(name, kind)| (codex_home.join(name), kind, InstructionScope::Global))
        .collect(),
        max_bytes,
        files,
        chain,
        diagnostics,
    );
}

fn select_project_directory(
    directory: &Path,
    scope: InstructionScope,
    config: &InstructionConfig,
    files: &mut Vec<InstructionFile>,
    chain: &mut Vec<InstructionFile>,
    diagnostics: &mut Vec<InstructionDiagnostic>,
) {
    let mut candidates = vec![
        (
            directory.join("AGENTS.override.md"),
            InstructionFileKind::Override,
            scope,
        ),
        (
            directory.join("AGENTS.md"),
            InstructionFileKind::Standard,
            scope,
        ),
    ];
    candidates.extend(
        config
            .project_doc_fallback_filenames
            .iter()
            .filter(|name| *name != "AGENTS.override.md" && *name != "AGENTS.md")
            .map(|name| {
                (
                    directory.join(name),
                    InstructionFileKind::Fallback(name.clone()),
                    scope,
                )
            }),
    );
    select_candidates(
        candidates,
        config.project_doc_max_bytes.max(1),
        files,
        chain,
        diagnostics,
    );
}

fn select_candidates(
    candidates: Vec<(PathBuf, InstructionFileKind, InstructionScope)>,
    max_bytes: usize,
    files: &mut Vec<InstructionFile>,
    chain: &mut Vec<InstructionFile>,
    diagnostics: &mut Vec<InstructionDiagnostic>,
) {
    for (path, kind, scope) in candidates {
        let mut file = read_candidate(&path, scope, kind, max_bytes, diagnostics);
        let selected = matches!(
            file.state,
            InstructionFileState::Selected | InstructionFileState::Truncated
        );
        if selected {
            file.chain_position = Some(chain.len());
            if file.state == InstructionFileState::Truncated {
                diagnostics.push(InstructionDiagnostic {
                    path: Some(file.path.clone()),
                    kind: InstructionDiagnosticKind::Truncated,
                    message: "instruction file reached the configured byte limit".to_owned(),
                });
            }
            chain.push(file.clone());
        }
        files.push(file);
        if selected {
            break;
        }
    }
}

fn read_candidate(
    path: &Path,
    scope: InstructionScope,
    kind: InstructionFileKind,
    max_bytes: usize,
    diagnostics: &mut Vec<InstructionDiagnostic>,
) -> InstructionFile {
    let base = |state, content, content_hash, byte_count, diagnostic| InstructionFile {
        path: path.to_path_buf(),
        scope,
        kind: kind.clone(),
        state,
        chain_position: None,
        content,
        content_hash,
        byte_count,
        diagnostic,
    };
    let mut unreadable = |message: String, byte_count: usize| {
        diagnostics.push(InstructionDiagnostic {
            path: Some(path.to_path_buf()),
            kind: InstructionDiagnosticKind::Unreadable,
            message: message.clone(),
        });
        base(
            InstructionFileState::Unreadable,
            None,
            None,
            byte_count,
            Some(message),
        )
    };
    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return unreadable("instruction path is not a regular file".to_owned(), 0);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return base(InstructionFileState::Missing, None, None, 0, None);
        }
        Err(error) => {
            return unreadable(bounded_message(&error.to_string()), 0);
        }
    };
    let byte_count = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return unreadable(bounded_message(&error.to_string()), byte_count);
        }
    };
    let read_limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::new();
    if let Err(error) = file
        .by_ref()
        .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
    {
        return unreadable(bounded_message(&error.to_string()), byte_count);
    }
    if byte_count == 0 {
        return base(InstructionFileState::Empty, None, None, 0, None);
    }
    let truncated = bytes.len() > max_bytes;
    if truncated && truncate_utf8(&mut bytes, max_bytes).is_err() {
        return unreadable("instruction file is not valid UTF-8".to_owned(), byte_count);
    }
    let content = match String::from_utf8(bytes.clone()) {
        Ok(content) => content,
        Err(_) => {
            return unreadable("instruction file is not valid UTF-8".to_owned(), byte_count);
        }
    };
    let hash = content_hash(&bytes);
    base(
        if truncated {
            InstructionFileState::Truncated
        } else {
            InstructionFileState::Selected
        },
        Some(content),
        Some(hash),
        byte_count,
        None,
    )
}

fn project_root(
    hint: Option<&Path>,
    cwd: Option<&Path>,
    diagnostics: &mut Vec<InstructionDiagnostic>,
) -> (Option<PathBuf>, ProjectRootStatus) {
    let root = if let Some(hint) = hint {
        if !hint.is_absolute() {
            diagnostics.push(InstructionDiagnostic {
                path: Some(hint.to_path_buf()),
                kind: InstructionDiagnosticKind::RelativePath,
                message: "project root must be an absolute path".to_owned(),
            });
            return (None, ProjectRootStatus::Unavailable);
        }
        match fs::metadata(hint) {
            Ok(metadata) if metadata.is_dir() => hint.to_path_buf(),
            Ok(_) => {
                diagnostics.push(InstructionDiagnostic {
                    path: Some(hint.to_path_buf()),
                    kind: InstructionDiagnosticKind::ProjectRootNotDirectory,
                    message: "project root is not a directory".to_owned(),
                });
                return (None, ProjectRootStatus::Missing);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                diagnostics.push(InstructionDiagnostic {
                    path: Some(hint.to_path_buf()),
                    kind: InstructionDiagnosticKind::MissingProjectRoot,
                    message: "project root does not exist".to_owned(),
                });
                return (None, ProjectRootStatus::Missing);
            }
            Err(error) => {
                diagnostics.push(InstructionDiagnostic {
                    path: Some(hint.to_path_buf()),
                    kind: InstructionDiagnosticKind::MissingProjectRoot,
                    message: bounded_message(&error.to_string()),
                });
                return (None, ProjectRootStatus::Missing);
            }
        }
    } else {
        let Some(cwd) = cwd else {
            return (None, ProjectRootStatus::Unavailable);
        };
        if !cwd.is_absolute() {
            diagnostics.push(InstructionDiagnostic {
                path: Some(cwd.to_path_buf()),
                kind: InstructionDiagnosticKind::RelativePath,
                message: "cwd must be an absolute path".to_owned(),
            });
            return (None, ProjectRootStatus::Unavailable);
        }
        if !validate_cwd(cwd, diagnostics) {
            return (None, ProjectRootStatus::Unavailable);
        }
        let Some(root) = discover_project_root(cwd) else {
            return (None, ProjectRootStatus::Unavailable);
        };
        root
    };

    if let Some(cwd) = cwd {
        if !cwd.is_absolute() {
            diagnostics.push(InstructionDiagnostic {
                path: Some(cwd.to_path_buf()),
                kind: InstructionDiagnosticKind::RelativePath,
                message: "cwd must be an absolute path".to_owned(),
            });
            return (Some(root), ProjectRootStatus::Conflict);
        }
        if !validate_cwd(cwd, diagnostics) {
            return (Some(root), ProjectRootStatus::Conflict);
        }
        if !cwd.starts_with(&root) {
            diagnostics.push(InstructionDiagnostic {
                path: Some(cwd.to_path_buf()),
                kind: InstructionDiagnosticKind::CwdOutsideProjectRoot,
                message: "cwd is outside the configured project root".to_owned(),
            });
            return (Some(root), ProjectRootStatus::Conflict);
        }
    }
    (Some(root), ProjectRootStatus::Known)
}

fn validate_cwd(cwd: &Path, diagnostics: &mut Vec<InstructionDiagnostic>) -> bool {
    match fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => {
            diagnostics.push(InstructionDiagnostic {
                path: Some(cwd.to_path_buf()),
                kind: InstructionDiagnosticKind::MissingCwd,
                message: "cwd is not a directory".to_owned(),
            });
            false
        }
        Err(error) => {
            diagnostics.push(InstructionDiagnostic {
                path: Some(cwd.to_path_buf()),
                kind: InstructionDiagnosticKind::MissingCwd,
                message: bounded_message(&error.to_string()),
            });
            false
        }
    }
}

fn discover_project_root(cwd: &Path) -> Option<PathBuf> {
    if !cwd.is_dir() {
        return None;
    }
    let mut current = cwd;
    loop {
        if [".git", ".hg", ".svn"]
            .iter()
            .any(|marker| current.join(marker).exists())
        {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn project_directories(root: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let target = cwd.filter(|cwd| cwd.starts_with(root)).unwrap_or(root);
    let mut directories = Vec::new();
    let mut current = Some(target);
    while let Some(directory) = current {
        directories.push(directory.to_path_buf());
        if directory == root {
            break;
        }
        current = directory.parent();
    }
    directories.reverse();
    directories
}

fn effective_chain(
    chain: &[InstructionFile],
    max_bytes: usize,
) -> (Option<String>, Option<String>, usize, bool) {
    let contents = chain
        .iter()
        .filter_map(|file| file.content.as_deref())
        .collect::<Vec<_>>();
    if contents.is_empty() {
        return (None, None, 0, false);
    }
    let merged = contents.join("\n\n");
    let mut bytes = merged.into_bytes();
    let truncated = bytes.len() > max_bytes;
    if truncated {
        truncate_utf8(&mut bytes, max_bytes).expect("merged instruction content is valid UTF-8");
    }
    let content = String::from_utf8(bytes).expect("instruction content is valid UTF-8");
    let hash = content_hash(content.as_bytes());
    let byte_count = content.len();
    (Some(content), Some(hash), byte_count, truncated)
}

fn truncate_utf8(bytes: &mut Vec<u8>, max_bytes: usize) -> Result<(), ()> {
    bytes.truncate(max_bytes);
    if let Err(error) = std::str::from_utf8(bytes) {
        if error.error_len().is_none() {
            bytes.truncate(error.valid_up_to());
        } else {
            return Err(());
        }
    }
    Ok(())
}

fn bounded_message(message: &str) -> String {
    const MAX_BYTES: usize = 512;
    if message.len() <= MAX_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "codexlens-instructions-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn resolves_global_nested_and_fallback_precedence() {
        let temp = TempDir::new();
        let home = temp.0.join("codex");
        let root = temp.0.join("project");
        let nested = root.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();
        write(&home.join("AGENTS.override.md"), "global override");
        write(&home.join("AGENTS.md"), "global standard");
        write(&root.join("AGENTS.md"), "root standard");
        write(&root.join("src").join("PROJECT.md"), "nested fallback");
        write(&nested.join("AGENTS.override.md"), "deep override");

        let resolver = InstructionResolver::new(
            Some(home),
            InstructionConfig {
                project_doc_fallback_filenames: vec!["PROJECT.md".to_owned()],
                project_doc_max_bytes: 32 * 1024,
            },
        );
        let result = resolver.resolve(Some(&root), Some(&nested));

        assert_eq!(
            result
                .chain
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>(),
            vec![
                temp.0.join("codex/AGENTS.override.md"),
                root.join("AGENTS.md"),
                root.join("src/PROJECT.md"),
                nested.join("AGENTS.override.md"),
            ]
        );
        assert_eq!(result.chain[0].scope, InstructionScope::Global);
        assert_eq!(result.chain[1].scope, InstructionScope::ProjectRoot);
        assert_eq!(result.chain[2].scope, InstructionScope::ProjectNested);
        assert_eq!(result.chain[3].chain_position, Some(3));
        assert!(result.effective_chain_hash.is_some());
    }

    #[test]
    fn records_empty_missing_and_truncated_states() {
        let temp = TempDir::new();
        let root = temp.0.join("project");
        fs::create_dir_all(&root).unwrap();
        write(&root.join("AGENTS.override.md"), "");
        write(&root.join("AGENTS.md"), "123456");
        let resolver = InstructionResolver::new(
            None,
            InstructionConfig {
                project_doc_fallback_filenames: Vec::new(),
                project_doc_max_bytes: 3,
            },
        );

        let result = resolver.resolve(Some(&root), Some(&root));

        assert_eq!(result.files[0].state, InstructionFileState::Empty);
        assert_eq!(result.files[1].state, InstructionFileState::Truncated);
        assert!(result.truncated);
        assert!(result.files[1].chain_position.is_some());
        assert!(result.files.iter().any(|file| {
            file.path.ends_with("AGENTS.override.md") && file.state == InstructionFileState::Empty
        }));
    }

    #[test]
    fn truncation_does_not_split_utf8_instruction_text() {
        let temp = TempDir::new();
        let root = temp.0.join("project");
        fs::create_dir_all(&root).unwrap();
        write(&root.join("AGENTS.md"), "あいう");
        let resolver = InstructionResolver::new(
            None,
            InstructionConfig {
                project_doc_fallback_filenames: Vec::new(),
                project_doc_max_bytes: 4,
            },
        );

        let result = resolver.resolve(Some(&root), Some(&root));

        assert_eq!(result.chain[0].state, InstructionFileState::Truncated);
        assert_eq!(result.chain[0].content.as_deref(), Some("あ"));
        assert_eq!(result.byte_count, "あ".len());
        assert!(result.effective_content.is_some());
    }

    #[test]
    fn rootless_and_conflicting_paths_are_explicit() {
        let temp = TempDir::new();
        let root = temp.0.join("project");
        let outside = temp.0.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let resolver = InstructionResolver::default();

        let rootless = resolver.resolve(None, Some(&root));
        assert_eq!(rootless.project_root_status, ProjectRootStatus::Unavailable);

        let conflict = resolver.resolve(Some(&root), Some(&outside));
        assert_eq!(conflict.project_root_status, ProjectRootStatus::Conflict);
        assert!(
            conflict
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind
                    == InstructionDiagnosticKind::CwdOutsideProjectRoot)
        );

        let missing_cwd = resolver.resolve(Some(&root), Some(&root.join("missing")));
        assert_eq!(missing_cwd.project_root_status, ProjectRootStatus::Conflict);
        assert!(
            missing_cwd
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == InstructionDiagnosticKind::MissingCwd)
        );
    }

    #[test]
    fn configured_diagnostics_are_kept_in_resolution() {
        let temp = TempDir::new();
        let home = temp.0.join("codex");
        let root = temp.0.join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&root).unwrap();
        write(&home.join("config.toml"), "project_doc_max_bytes = 0");

        let (capture, loaded) = InstructionCaptureOptions::from_codex_home(&home, None);
        assert_eq!(loaded.diagnostics.len(), 1);
        let result = capture.resolver().resolve(Some(&root), Some(&root));

        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == InstructionDiagnosticKind::Config)
        );
    }

    #[test]
    fn rollout_snapshot_is_observed_and_unavailable_is_distinct() {
        let provenance = SourceRef::rollout(PathBuf::from("rollout.jsonl"), 2);
        let observed = snapshot_from_rollout(
            Some("session".to_owned()),
            Some("turn".to_owned()),
            Some("Run checks."),
            provenance.clone(),
        );
        assert_eq!(observed.source, InstructionSnapshotSource::Rollout);
        assert_eq!(observed.accuracy, InstructionSnapshotAccuracy::Observed);
        assert_eq!(observed.chain.len(), 1);
        assert_eq!(observed.byte_count, 11);

        let unavailable = unavailable_snapshot(Some("session".to_owned()), None, provenance);
        assert_eq!(unavailable.source, InstructionSnapshotSource::Unavailable);
        assert_eq!(
            unavailable.accuracy,
            InstructionSnapshotAccuracy::Unavailable
        );
        assert!(unavailable.content_hash.is_none());
    }
}
