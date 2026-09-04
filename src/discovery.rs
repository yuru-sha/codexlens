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

pub fn resolve_home_from(
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
        let home = env::var_os("USERPROFILE").or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            let mut home = PathBuf::from(drive);
            home.push(path);
            Some(home.into_os_string())
        })?;
        Some(PathBuf::from(home).join(".codex"))
    }

    #[cfg(not(windows))]
    {
        env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
    }
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
    let identity = match fs::canonicalize(path) {
        Ok(identity) => identity,
        Err(error) => {
            diagnostics.push(DiscoveryDiagnostic {
                path: path.to_path_buf(),
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return false;
        }
    };
    if !identity.starts_with(root) {
        diagnostics.push(DiscoveryDiagnostic {
            path: path.to_path_buf(),
            kind: DiagnosticKind::SymlinkEscapesRoot,
            message: "symlink target is outside the configured Codex home".to_owned(),
        });
        return false;
    }
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

        if metadata.file_type().is_file()
            && let Some(reader) = reader_kind(&entry_path)
        {
            found = true;
            add_input(&entry_path, root, kind, Some(reader), inputs, diagnostics);
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
    let identity = match fs::canonicalize(path) {
        Ok(identity) => identity,
        Err(error) => {
            diagnostics.push(DiscoveryDiagnostic {
                path: path.to_path_buf(),
                kind: diagnostic_kind_for_io(&error),
                message: error.to_string(),
            });
            return;
        }
    };
    if !identity.starts_with(root) {
        diagnostics.push(DiscoveryDiagnostic {
            path: path.to_path_buf(),
            kind: DiagnosticKind::SymlinkEscapesRoot,
            message: "symlink target is outside the configured Codex home".to_owned(),
        });
        return;
    }
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

    fn file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, []).unwrap();
    }

    fn options(home: &Path) -> DiscoveryOptions {
        DiscoveryOptions {
            explicit_home: Some(home.to_path_buf()),
            include_archived: false,
        }
    }

    #[test]
    fn resolves_explicit_then_environment_then_platform_default() {
        let explicit = Path::new("/explicit/.codex");
        let environment = Path::new("/environment/.codex");
        let platform = Path::new("/platform/.codex");

        assert_eq!(
            resolve_home_from(Some(explicit), Some(environment), Some(platform))
                .unwrap()
                .source,
            HomeSource::Explicit
        );
        assert_eq!(
            resolve_home_from(None, Some(environment), Some(platform))
                .unwrap()
                .path,
            environment
        );
        assert_eq!(
            resolve_home_from(None, None, Some(platform)).unwrap().path,
            platform
        );
    }

    #[test]
    fn discovers_sorted_unique_inputs_and_reader_selection() {
        let temp = TempDir::new();
        let state_a = temp.path.join("state_a.sqlite");
        let state_b = temp.path.join("state_b.sqlite");
        let plain = temp.path.join("sessions").join("nested").join("a.jsonl");
        let compressed = temp.path.join("sessions").join("z.jsonl.zst");
        file(&state_b);
        file(&state_a);
        file(&plain);
        file(&compressed);
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &plain,
            temp.path
                .join("sessions")
                .join("nested")
                .join("alias.jsonl"),
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
    fn archive_discovery_is_opt_in_and_missing_inputs_are_diagnostic() {
        let temp = TempDir::new();
        let normal = temp.path.join("sessions").join("normal.jsonl");
        let archived = temp.path.join("archived_sessions").join("old.jsonl");
        file(&normal);
        file(&archived);

        let result = discover(&options(&temp.path));
        assert_eq!(result.inputs.len(), 1);
        assert!(!result.inputs[0].kind.is_archived());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::MissingInput
                    && diagnostic.path.ends_with("state_*.sqlite"))
        );

        let result = discover(&DiscoveryOptions {
            explicit_home: Some(temp.path.clone()),
            include_archived: true,
        });
        assert_eq!(result.inputs.len(), 2);
        assert!(result.inputs.iter().any(|input| input.kind.is_archived()));
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
        file(&outside.path.join("secret.jsonl"));
        fs::create_dir_all(root.path.join("sessions")).unwrap();
        symlink(&outside.path, root.path.join("sessions").join("outside")).unwrap();

        let result = discover(&options(&root.path));
        assert!(result.inputs.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::SymlinkEscapesRoot)
        );
    }
}
