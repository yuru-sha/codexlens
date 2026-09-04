use std::collections::BTreeSet;
use std::env;
use std::fs::{self, DirEntry};
use std::io;
use std::path::{Path, PathBuf};

pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSource {
    Explicit,
    Environment,
    PlatformDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHome {
    pub path: PathBuf,
    pub source: HomeSource,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryOptions {
    pub explicit_home: Option<PathBuf>,
    pub include_archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputKind {
    StateDatabase,
    Rollout { archived: bool },
}

impl InputKind {
    pub fn is_archived(self) -> bool {
        matches!(self, Self::Rollout { archived: true })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReaderKind {
    PlainJsonl,
    ZstdJsonl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredInput {
    pub path: PathBuf,
    pub identity: PathBuf,
    pub kind: InputKind,
    pub reader: Option<ReaderKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticKind {
    HomeResolution,
    InvalidRoot,
    MissingInput,
    Unreadable,
    SymlinkEscapesRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryDiagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub home: Option<ResolvedHome>,
    pub inputs: Vec<DiscoveredInput>,
    pub diagnostics: Vec<DiscoveryDiagnostic>,
}

pub fn resolve_home(explicit_home: Option<&Path>) -> io::Result<ResolvedHome> {
    let environment_home = env::var_os(CODEX_HOME_ENV).map(PathBuf::from);
    let platform_home = platform_default_codex_home();
    resolve_home_from(
        explicit_home,
        environment_home.as_deref(),
        platform_home.as_deref(),
    )
}

fn resolve_home_from(
    explicit_home: Option<&Path>,
    environment_home: Option<&Path>,
    platform_home: Option<&Path>,
) -> io::Result<ResolvedHome> {
    if explicit_home.is_some_and(|path| path.as_os_str().is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "explicit Codex home is empty",
        ));
    }

    if let Some(path) = explicit_home {
        return Ok(ResolvedHome {
            path: path.to_path_buf(),
            source: HomeSource::Explicit,
        });
    }
    if let Some(path) = environment_home.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(ResolvedHome {
            path: path.to_path_buf(),
            source: HomeSource::Environment,
        });
    }
    if let Some(path) = platform_home.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(ResolvedHome {
            path: path.to_path_buf(),
            source: HomeSource::PlatformDefault,
        });
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "unable to determine the Codex home directory",
    ))
}

pub fn discover(options: &DiscoveryOptions) -> DiscoveryResult {
    let mut result = DiscoveryResult::default();
    let home = match resolve_home(options.explicit_home.as_deref()) {
        Ok(home) => home,
        Err(error) => {
            result.diagnostics.push(DiscoveryDiagnostic {
                path: PathBuf::new(),
                kind: DiagnosticKind::HomeResolution,
                message: error.to_string(),
            });
            return result;
        }
    };
    result.home = Some(home.clone());

    let root = match fs::canonicalize(&home.path) {
        Ok(root) => root,
        Err(error) => {
            result.diagnostics.push(DiscoveryDiagnostic {
                path: home.path,
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return result;
        }
    };

    match fs::metadata(&home.path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            result.diagnostics.push(DiscoveryDiagnostic {
                path: home.path,
                kind: DiagnosticKind::InvalidRoot,
                message: "configured Codex home is not a directory".to_owned(),
            });
            return result;
        }
        Err(error) => {
            result.diagnostics.push(DiscoveryDiagnostic {
                path: home.path,
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return result;
        }
    }

    discover_state_files(&home.path, &root, &mut result);
    discover_rollout_tree(&home.path.join("sessions"), &root, false, &mut result);
    if options.include_archived {
        discover_rollout_tree(
            &home.path.join("archived_sessions"),
            &root,
            true,
            &mut result,
        );
    }

    result.inputs.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.identity.cmp(&right.identity))
            .then_with(|| left.kind.cmp(&right.kind))
    });
    let mut identities = BTreeSet::new();
    result
        .inputs
        .retain(|input| identities.insert(input.identity.clone()));
    result.diagnostics.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.message.cmp(&right.message))
    });
    result
}

fn platform_default_codex_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let user_profile = env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        let home = user_profile.or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            let mut home = PathBuf::from(drive);
            home.push(path);
            Some(home)
        });
        codex_home_from_base(home)
    }

    #[cfg(not(windows))]
    {
        codex_home_from_base(env::var_os("HOME").map(PathBuf::from))
    }
}

fn codex_home_from_base(home: Option<PathBuf>) -> Option<PathBuf> {
    let home = home.filter(|path| !path.as_os_str().is_empty())?;
    Some(home.join(".codex"))
}

fn discover_state_files(home: &Path, root: &Path, result: &mut DiscoveryResult) {
    let entries = match read_sorted_entries(home, &mut result.diagnostics) {
        Some(entries) => entries,
        None => return,
    };
    let mut found = false;
    for entry in entries {
        let path = entry.path();
        if is_state_database(&path) {
            found = true;
            add_input(
                &path,
                root,
                InputKind::StateDatabase,
                None,
                &mut result.inputs,
                &mut result.diagnostics,
            );
        }
    }
    if !found {
        result.diagnostics.push(DiscoveryDiagnostic {
            path: home.join("state_*.sqlite"),
            kind: DiagnosticKind::MissingInput,
            message: "no state_*.sqlite file found".to_owned(),
        });
    }
}

fn discover_rollout_tree(path: &Path, root: &Path, archived: bool, result: &mut DiscoveryResult) {
    let before = result.diagnostics.len();
    let mut visited = BTreeSet::new();
    let found = walk_rollout_tree(
        path,
        root,
        InputKind::Rollout { archived },
        &mut visited,
        &mut result.inputs,
        &mut result.diagnostics,
    );
    if !found && result.diagnostics.len() == before {
        result.diagnostics.push(DiscoveryDiagnostic {
            path: path.to_path_buf(),
            kind: DiagnosticKind::MissingInput,
            message: "no rollout JSONL file found".to_owned(),
        });
    }
}

fn walk_rollout_tree(
    path: &Path,
    root: &Path,
    kind: InputKind,
    visited: &mut BTreeSet<PathBuf>,
    inputs: &mut Vec<DiscoveredInput>,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) -> bool {
    let identity = match canonical_path_within_root(path, root, diagnostics) {
        Some(identity) => identity,
        None => return false,
    };
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return false,
        Err(error) => {
            diagnostics.push(DiscoveryDiagnostic {
                path: path.to_path_buf(),
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return false;
        }
    }
    if !visited.insert(identity) {
        return false;
    }

    let entries = match read_sorted_entries(path, diagnostics) {
        Some(entries) => entries,
        None => return false,
    };
    let mut found = false;
    for entry in entries {
        let entry_path = entry.path();
        let metadata = match fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                diagnostics.push(DiscoveryDiagnostic {
                    path: entry_path,
                    kind: diagnostic_kind_for_io(&error),
                    message: error.to_string(),
                });
                continue;
            }
        };

        if metadata.file_type().is_dir() {
            found |= walk_rollout_tree(&entry_path, root, kind, visited, inputs, diagnostics);
            continue;
        }

        if metadata.file_type().is_symlink() {
            match fs::metadata(&entry_path) {
                Ok(target) if target.is_dir() => {
                    found |=
                        walk_rollout_tree(&entry_path, root, kind, visited, inputs, diagnostics);
                }
                Ok(target) if target.is_file() => {
                    if let Some(reader) = reader_kind(&entry_path) {
                        found = true;
                        add_input(&entry_path, root, kind, Some(reader), inputs, diagnostics);
                    }
                }
                Ok(_) => {}
                Err(error) => diagnostics.push(DiscoveryDiagnostic {
                    path: entry_path,
                    kind: diagnostic_kind_for_io(&error),
                    message: error.to_string(),
                }),
            }
            continue;
        }

        if metadata.file_type().is_file() {
            if let Some(reader) = reader_kind(&entry_path) {
                found = true;
                add_input(&entry_path, root, kind, Some(reader), inputs, diagnostics);
            }
        }
    }
    found
}

fn read_sorted_entries(
    path: &Path,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) -> Option<Vec<DirEntry>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(DiscoveryDiagnostic {
                path: path.to_path_buf(),
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return None;
        }
    };
    let mut sorted = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => sorted.push(entry),
            Err(error) => diagnostics.push(DiscoveryDiagnostic {
                path: path.to_path_buf(),
                kind: DiagnosticKind::Unreadable,
                message: error.to_string(),
            }),
        }
    }
    sorted.sort_by_key(|entry| entry.path());
    Some(sorted)
}

fn add_input(
    path: &Path,
    root: &Path,
    kind: InputKind,
    reader: Option<ReaderKind>,
    inputs: &mut Vec<DiscoveredInput>,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) {
    let identity = match canonical_path_within_root(path, root, diagnostics) {
        Some(identity) => identity,
        None => return,
    };
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => inputs.push(DiscoveredInput {
            path: path.to_path_buf(),
            identity,
            kind,
            reader,
        }),
        Ok(_) => diagnostics.push(DiscoveryDiagnostic {
            path: path.to_path_buf(),
            kind: DiagnosticKind::Unreadable,
            message: "discovered path is not a regular file".to_owned(),
        }),
        Err(error) => diagnostics.push(DiscoveryDiagnostic {
            path: path.to_path_buf(),
            kind: diagnostic_kind_for_io(&error),
            message: error.to_string(),
        }),
    }
}

fn is_state_database(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.starts_with("state_") && name.ends_with(".sqlite")
    })
}

pub(crate) fn codex_home_for_source(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    if is_state_database(path) {
        return path.parent().map(Path::to_path_buf);
    }
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory.file_name().is_some_and(|name| {
            matches!(
                name.to_string_lossy().as_ref(),
                "sessions" | "archived_sessions"
            )
        }) {
            return directory.parent().map(Path::to_path_buf);
        }
        current = directory.parent();
    }
    None
}

fn canonical_path_within_root(
    path: &Path,
    root: &Path,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) -> Option<PathBuf> {
    let identity = match fs::canonicalize(path) {
        Ok(identity) => identity,
        Err(error) => {
            diagnostics.push(DiscoveryDiagnostic {
                path: path.to_path_buf(),
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return None;
        }
    };
    if !identity.starts_with(root) {
        diagnostics.push(DiscoveryDiagnostic {
            path: path.to_path_buf(),
            kind: DiagnosticKind::SymlinkEscapesRoot,
            message: "symlink target is outside the configured Codex home".to_owned(),
        });
        return None;
    }
    Some(identity)
}

fn reader_kind(path: &Path) -> Option<ReaderKind> {
    let name = path.file_name()?.to_string_lossy();
    if name.ends_with(".jsonl.zst") {
        Some(ReaderKind::ZstdJsonl)
    } else if name.ends_with(".jsonl") {
        Some(ReaderKind::PlainJsonl)
    } else {
        None
    }
}

fn diagnostic_kind_for_io(error: &io::Error) -> DiagnosticKind {
    if error.kind() == io::ErrorKind::NotFound {
        DiagnosticKind::MissingInput
    } else {
        DiagnosticKind::Unreadable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "codexlens-discovery-{}-{}",
                std::process::id(),
                NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    const DISCOVERY_FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/discovery");

    fn copy_fixture(temp: &TempDir) {
        copy_tree(Path::new(DISCOVERY_FIXTURE), &temp.path);
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&source_path, &destination_path);
            } else {
                fs::copy(&source_path, &destination_path).unwrap();
            }
        }
    }

    fn options(home: &Path) -> DiscoveryOptions {
        DiscoveryOptions {
            explicit_home: Some(home.to_path_buf()),
            include_archived: false,
        }
    }

    #[test]
    fn resolves_explicit_then_environment_then_platform_default() {
        let explicit = TempDir::new();
        let environment = TempDir::new();
        let platform = TempDir::new();
        copy_fixture(&explicit);
        copy_fixture(&environment);
        copy_fixture(&platform);

        assert_eq!(
            resolve_home_from(
                Some(&explicit.path),
                Some(&environment.path),
                Some(&platform.path),
            )
            .unwrap()
            .source,
            HomeSource::Explicit
        );
        assert_eq!(
            resolve_home_from(None, Some(&environment.path), Some(&platform.path))
                .unwrap()
                .source,
            HomeSource::Environment
        );
        assert_eq!(
            resolve_home_from(None, None, Some(&platform.path))
                .unwrap()
                .source,
            HomeSource::PlatformDefault
        );
    }

    #[test]
    fn empty_platform_home_is_not_current_directory() {
        assert_eq!(codex_home_from_base(Some(PathBuf::new())), None);
    }

    #[test]
    fn discovers_sorted_unique_inputs_and_reader_selection() {
        let temp = TempDir::new();
        copy_fixture(&temp);
        let plain = temp.path.join("sessions").join("2026").join("a.jsonl");
        let compressed = temp.path.join("sessions").join("2026").join("z.jsonl.zst");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &plain,
            temp.path
                .join("sessions")
                .join("2026")
                .join("zz-alias.jsonl"),
        )
        .unwrap();

        let result = discover(&options(&temp.path));
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(result.inputs.len(), 4);
        assert!(
            result
                .inputs
                .windows(2)
                .all(|pair| pair[0].identity != pair[1].identity)
        );
        #[cfg(unix)]
        assert!(
            !result
                .inputs
                .iter()
                .any(|input| input.path.ends_with("alias.jsonl"))
        );
        assert!(
            result
                .inputs
                .windows(2)
                .all(|pair| pair[0].path <= pair[1].path)
        );
        assert_eq!(
            result
                .inputs
                .iter()
                .find(|input| input.path == plain)
                .unwrap()
                .reader,
            Some(ReaderKind::PlainJsonl)
        );
        assert_eq!(
            result
                .inputs
                .iter()
                .find(|input| input.path == compressed)
                .unwrap()
                .reader,
            Some(ReaderKind::ZstdJsonl)
        );
    }

    #[test]
    fn archive_discovery_is_opt_in() {
        let temp = TempDir::new();
        copy_fixture(&temp);

        let result = discover(&options(&temp.path));
        assert_eq!(result.inputs.len(), 4);
        assert!(result.inputs.iter().all(|input| !input.kind.is_archived()));
        assert!(
            !result
                .inputs
                .iter()
                .any(|input| input.path.to_string_lossy().contains("archived_sessions"))
        );

        let result = discover(&DiscoveryOptions {
            explicit_home: Some(temp.path.clone()),
            include_archived: true,
        });
        assert_eq!(result.inputs.len(), 5);
        assert!(result.inputs.iter().any(|input| input.kind.is_archived()));
    }

    #[test]
    fn missing_state_does_not_hide_rollout_inputs() {
        let temp = TempDir::new();
        copy_fixture(&temp);
        fs::remove_file(temp.path.join("state_a.sqlite")).unwrap();
        fs::remove_file(temp.path.join("state_b.sqlite")).unwrap();

        let result = discover(&options(&temp.path));
        assert_eq!(result.inputs.len(), 2);
        assert!(result.inputs.iter().all(|input| !input.kind.is_archived()));
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::MissingInput
                && diagnostic.path.ends_with("state_*.sqlite")
        }));
    }

    #[test]
    fn missing_rollout_does_not_hide_state_inputs() {
        let temp = TempDir::new();
        copy_fixture(&temp);
        fs::remove_dir_all(temp.path.join("sessions")).unwrap();

        let result = discover(&options(&temp.path));
        assert_eq!(result.inputs.len(), 2);
        assert!(
            result
                .inputs
                .iter()
                .all(|input| input.kind == InputKind::StateDatabase)
        );
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::MissingInput && diagnostic.path.ends_with("sessions")
        }));
    }

    #[test]
    fn missing_root_returns_a_partial_result_with_diagnostic() {
        let temp = TempDir::new();
        let missing = temp.path.join("missing");
        let result = discover(&options(&missing));

        assert!(result.inputs.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::MissingInput)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollout_symlink_cannot_escape_configured_root() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new();
        let outside = TempDir::new();
        copy_fixture(&root);
        copy_fixture(&outside);
        fs::remove_dir_all(root.path.join("sessions")).unwrap();
        symlink(outside.path.join("sessions"), root.path.join("sessions")).unwrap();

        let result = discover(&options(&root.path));
        let outside_sessions = fs::canonicalize(outside.path.join("sessions")).unwrap();
        assert_eq!(result.inputs.len(), 2);
        assert!(
            result
                .inputs
                .iter()
                .all(|input| !input.identity.starts_with(&outside_sessions))
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::SymlinkEscapesRoot)
        );
    }
}
