use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use codexlens::discovery::{DiscoveredInput, InputKind, ReaderKind};
use codexlens::model::{
    DiagnosticKind, InstructionFile, InstructionFileKind, InstructionFileState, InstructionScope,
    ProjectRootStatus,
};
use codexlens::rollout::RolloutParseOptions;
use codexlens::store::{IngestInputKind, IngestOptions, SCHEMA_VERSION, Store};
use rusqlite::{Connection, params};

static NEXT_TEMP_STORE: AtomicUsize = AtomicUsize::new(0);

const REPORTING_COMMANDS: &[&[&str]] = &[
    &["analyze"],
    &["sessions"],
    &["failures"],
    &["corrections"],
    &["rework"],
    &["stuck"],
    &["verification"],
    &["knowledge"],
    &["rediscovery"],
    &["instructions"],
    &["doctor"],
    &["optimize", "--diff"],
];

fn temp_store_path(label: &str) -> PathBuf {
    let nonce = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-{label}-{nonce}.sqlite",
        std::process::id(),
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn fixture_store() -> PathBuf {
    let path = temp_store_path("reporting");
    let mut store = Store::open(&path).unwrap();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/analysis/lenses.jsonl");
    store
        .ingest_rollout_file(&fixture, &RolloutParseOptions::default())
        .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO sessions (session_id, source_identity, source_path) VALUES ('fixture-analysis-session-a', 'synthetic-second-source', 'synthetic.jsonl')",
            [],
        )
        .unwrap();
    path
}

fn empty_store() -> PathBuf {
    let path = temp_store_path("empty");
    Store::open(&path).unwrap();
    path
}

fn minimal_store() -> PathBuf {
    let path = temp_store_path("minimal");
    let store = Store::open(&path).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO sessions (session_id, source_identity, source_path) VALUES ('minimal-session', 'synthetic-minimal-source', 'minimal.jsonl')",
            [],
        )
        .unwrap();
    path
}

fn run_args(args: &[&str], store: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(args)
        .arg("--store")
        .arg(store)
        .output()
        .unwrap()
}

fn assert_finding_report(
    stdout: &str,
    kind: &str,
    severity: &str,
    confidence: &str,
    occurrences: usize,
    sessions: usize,
) {
    let lines: Vec<_> = stdout.lines().collect();
    let classification = format!("- {kind} / {severity} / {confidence}:");
    let finding_index = lines
        .iter()
        .position(|line| line.starts_with(&classification))
        .unwrap_or_else(|| panic!("missing {kind} classification: {stdout}"));
    let scope_index = lines[..=finding_index]
        .iter()
        .rposition(|line| line.starts_with('['))
        .unwrap_or(finding_index);
    let block_end = lines[finding_index + 1..]
        .iter()
        .position(|line| line.starts_with("- ") || line.starts_with('['))
        .map_or(lines.len(), |offset| finding_index + 1 + offset);
    let block = lines[scope_index..block_end].join("\n");

    assert!(
        stdout.contains(&format!("{kind}=")),
        "missing {kind} count: {stdout}"
    );
    assert!(
        block.contains(&classification),
        "missing {kind} classification: {stdout}"
    );
    assert!(
        block.contains(&format!("({occurrences} occurrences, {sessions} sessions)")),
        "missing {kind} counts: {stdout}"
    );
    assert!(
        block.contains("[project:/fixture/project]"),
        "missing project scope: {stdout}"
    );
    assert!(
        block.lines().any(|line| {
            line.strip_prefix("  evidence: ")
                .is_some_and(|evidence| evidence.contains("lenses.jsonl:"))
        }),
        "missing evidence: {stdout}"
    );
    assert!(block.contains("  action: "), "missing action: {stdout}");
}

fn assert_doctor_report(stdout: &str) {
    assert!(
        stdout.contains("Analyzed period: 2026-01-03T00:00:00.000Z .. 2026-01-04T00:05:00.000Z")
    );
    assert!(stdout.contains("Sessions: 2"));
    assert!(stdout.contains("Finding counts:"));
    assert!(stdout.contains("  heuristic: "));
    assert!(stdout.contains("  action: "));
    assert!(stdout.contains("  evidence: "));
    let evidence_lines: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("  evidence: "))
        .collect();
    assert!(
        !evidence_lines.is_empty(),
        "doctor evidence sample is empty"
    );
    assert!(
        evidence_lines.iter().all(|line| {
            let Some(evidence) = line.strip_prefix("  evidence: ") else {
                return false;
            };
            let Some((source, excerpt)) = evidence.split_once(" — ") else {
                return false;
            };
            !source.trim().is_empty() && !excerpt.trim().is_empty() && excerpt.len() <= 256
        }),
        "doctor evidence sample is incomplete: {evidence_lines:?}"
    );
    assert!(
        evidence_lines
            .iter()
            .any(|line| line.contains("lenses.jsonl:")),
        "doctor evidence source line is missing: {evidence_lines:?}"
    );
}

fn rendered_diff_store() -> (PathBuf, PathBuf, PathBuf) {
    let source = fixture_store();
    let mut data = {
        let store = Store::open_read_only(&source).unwrap();
        store.load_canonical().unwrap()
    };
    let _ = fs::remove_file(source);

    data.sessions.sort_by(|left, right| left.id.cmp(&right.id));
    data.sessions.dedup_by(|left, right| left.id == right.id);
    data.tool_results
        .retain(|result| result.exit_code == Some(1) && !result.is_duplicate);
    data.turns.clear();
    data.records.clear();
    data.messages.clear();
    data.tool_calls.clear();
    data.file_operations.clear();
    data.token_usage.clear();
    data.diagnostics.clear();
    data.instruction_snapshots.clear();

    let project_root = temp_store_path("rendered-project");
    fs::create_dir(&project_root).unwrap();
    let target = project_root.join("AGENTS.md");
    let content = "Existing synthetic guidance.\n";
    fs::write(&target, content).unwrap();
    let content_hash = codexlens::instructions::content_hash(content.as_bytes());
    let project = project_root.to_string_lossy().into_owned();
    for session in &mut data.sessions {
        session.cwd = Some(project.clone());
        session.project = Some(project.clone());
    }
    for join in &mut data.instruction_joins {
        let file = InstructionFile {
            path: target.clone(),
            scope: InstructionScope::ProjectRoot,
            kind: InstructionFileKind::Standard,
            state: InstructionFileState::Selected,
            chain_position: Some(0),
            content: Some(content.to_owned()),
            content_hash: Some(content_hash.clone()),
            byte_count: content.len(),
            diagnostic: None,
        };
        join.cwd = Some(project_root.clone());
        join.project_root = Some(project_root.clone());
        join.project_root_status = ProjectRootStatus::Known;
        join.nearest_path = Some(target.clone());
        join.nearest_scope = Some(InstructionScope::ProjectRoot);
        join.resolution.project_root = Some(project_root.clone());
        join.resolution.cwd = Some(project_root.clone());
        join.resolution.project_root_status = ProjectRootStatus::Known;
        join.resolution.files = vec![file.clone()];
        join.resolution.chain = vec![file];
        join.resolution.effective_content = Some(content.to_owned());
        join.resolution.effective_chain_hash = Some(content_hash.clone());
        join.resolution.byte_count = content.len();
        join.resolution.truncated = false;
        join.resolution.diagnostics.clear();
    }

    let store_path = temp_store_path("rendered-store");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/analysis/lenses.jsonl");
    let mut store = Store::open(&store_path).unwrap();
    store
        .ingest_canonical(&fixture, IngestInputKind::Rollout, &data)
        .unwrap();
    drop(store);
    (store_path, target, project_root)
}

#[test]
fn optimize_diff_renders_a_proposal_without_writing_the_target() {
    let (store, target, project_root) = rendered_diff_store();
    let before = fs::read_to_string(&target).unwrap();

    let output = run_args(&["optimize", "--diff"], &store);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Proposal add "), "{stdout}");
    assert!(stdout.contains("Observed: "), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "Target: A strict project majority selects {}",
            target.display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("Evidence: 2 occurrences across 2 sessions"));
    assert!(stdout.contains("Evidence ref: "), "{stdout}");
    assert!(stdout.contains("Confidence: high"));
    assert!(stdout.contains("Heuristic: repeated failed tool outcome"));
    assert!(stdout.contains("Limitation: "), "{stdout}");
    assert!(stdout.contains("Review the evidence and diff before applying this proposal"));
    assert!(stdout.contains("@@ "), "{stdout}");
    assert!(stdout.contains(&format!("--- a/{}", target.display())));
    assert!(stdout.contains(&format!("+++ b/{}", target.display())));
    assert!(stdout.contains("+Before running cargo test, verify the documented prerequisite."));

    let repeated = run_args(&["optimize", "--diff"], &store);
    assert_eq!(repeated.stdout, output.stdout);
    assert_eq!(repeated.stderr, output.stderr);
    assert_eq!(fs::read_to_string(&target).unwrap(), before);

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn reporting_commands_render_local_store_data() {
    let store = fixture_store();
    for args in REPORTING_COMMANDS {
        let output = run_args(args, &store);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if args[0] == "optimize" {
            assert!(
                stdout.contains("Proposal ")
                    || stdout.contains("No applicable proposals.")
                    || stderr.contains("Skipped "),
                "{args:?}: stdout={stdout}, stderr={stderr}"
            );
        } else {
            assert!(stdout.contains("Store freshness:"), "{args:?}: {stdout}");
            assert!(
                !stdout.contains("not implemented yet"),
                "{args:?}: {stdout}"
            );
        }
    }

    for (command, kind, severity, confidence, occurrences, sessions) in [
        ("failures", "failure", "medium", "high", 2, 2),
        ("corrections", "correction", "medium", "medium", 2, 2),
        ("rework", "rework", "medium", "high", 2, 1),
        ("verification", "verification", "medium", "medium", 1, 1),
        ("knowledge", "knowledge", "medium", "medium", 2, 2),
        ("instructions", "gap", "medium", "high", 2, 2),
    ] {
        let output = run_args(&[command], &store);
        assert!(output.status.success(), "{command} failed");
        assert_finding_report(
            &String::from_utf8_lossy(&output.stdout),
            kind,
            severity,
            confidence,
            occurrences,
            sessions,
        );
    }

    let analyze = run_args(&["analyze"], &store);
    assert!(analyze.status.success());
    for (kind, severity, confidence, occurrences, sessions) in [
        ("failure", "medium", "high", 2, 2),
        ("correction", "medium", "medium", 2, 2),
        ("rework", "medium", "high", 2, 1),
        ("stuck", "high", "medium", 2, 1),
        ("verification", "medium", "medium", 1, 1),
        ("knowledge", "medium", "medium", 2, 2),
        ("gap", "medium", "high", 2, 2),
    ] {
        assert_finding_report(
            &String::from_utf8_lossy(&analyze.stdout),
            kind,
            severity,
            confidence,
            occurrences,
            sessions,
        );
    }

    let doctor = run_args(&["doctor"], &store);
    assert!(doctor.status.success());
    let doctor_stdout = String::from_utf8_lossy(&doctor.stdout);
    assert_doctor_report(&doctor_stdout);

    let sessions = String::from_utf8_lossy(&run_args(&["sessions"], &store).stdout).into_owned();
    assert!(sessions.contains("fixture-analysis-session-a"));
    assert_eq!(sessions.matches("- fixture-analysis-session-a").count(), 1);
    let failures = String::from_utf8_lossy(&run_args(&["failures"], &store).stdout).into_owned();
    assert!(failures.contains("failure="));
    let corrections =
        String::from_utf8_lossy(&run_args(&["corrections"], &store).stdout).into_owned();
    assert!(corrections.contains("correction="));

    let _ = std::fs::remove_file(store);
}

#[test]
fn reporting_commands_cover_empty_and_minimal_stores() {
    for (store, expected_sessions) in [
        (empty_store(), "Sessions: 0"),
        (minimal_store(), "Sessions: 1"),
    ] {
        for args in REPORTING_COMMANDS {
            let output = run_args(args, &store);
            assert!(
                output.status.success(),
                "{args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty(), "{args:?}: stderr is not empty");
            let stdout = String::from_utf8_lossy(&output.stdout);
            if args[0] == "optimize" {
                assert_eq!(stdout, "No applicable proposals.\n", "{args:?}: {stdout}");
            } else {
                assert!(
                    stdout.contains("Store freshness: empty"),
                    "{args:?}: {stdout}"
                );
                assert!(stdout.contains(expected_sessions), "{args:?}: {stdout}");
                if args[0] != "sessions" {
                    assert!(
                        stdout.contains("Finding counts: none"),
                        "{args:?}: {stdout}"
                    );
                }
            }
        }
        let _ = std::fs::remove_file(store);
    }
}

#[test]
fn reporting_commands_explain_missing_store() {
    let path = temp_store_path("missing");
    for args in REPORTING_COMMANDS {
        let output = run_args(args, &path);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("store does not exist"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.len() < 512, "{args:?}: {stderr}");
    }
}

#[test]
fn reporting_commands_are_deterministic_and_aliases_match() {
    let store = fixture_store();
    for args in REPORTING_COMMANDS {
        let first = run_args(args, &store);
        let second = run_args(args, &store);
        assert_eq!(first.status, second.status, "{args:?} status changed");
        assert_eq!(first.stdout, second.stdout, "{args:?} stdout changed");
        assert_eq!(first.stderr, second.stderr, "{args:?} stderr changed");
    }

    let rework = run_args(&["rework"], &store);
    let stuck = run_args(&["stuck"], &store);
    assert_eq!(rework.status, stuck.status);
    assert_eq!(rework.stdout, stuck.stdout);
    assert_eq!(rework.stderr, stuck.stderr);

    let knowledge = run_args(&["knowledge"], &store);
    let rediscovery = run_args(&["rediscovery"], &store);
    assert_eq!(knowledge.status, rediscovery.status);
    assert_eq!(knowledge.stdout, rediscovery.stdout);
    assert_eq!(knowledge.stderr, rediscovery.stderr);

    let _ = std::fs::remove_file(store);
}

#[test]
fn reporting_commands_reject_uninitialized_store_without_writing() {
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-uninitialized.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    Connection::open(&path).unwrap();
    let before = std::fs::read(&path).unwrap();

    let output = run_args(&["analyze"], &path);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("schema version"), "{stderr}");
    assert!(stderr.len() < 512);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let _ = std::fs::remove_file(path);
}

#[test]
fn reporting_commands_migrate_legacy_store_without_writing_source() {
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-legacy.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).unwrap();
    store
        .connection()
        .execute_batch(
            "ALTER TABLE records DROP COLUMN is_error;
             ALTER TABLE records DROP COLUMN is_terminal;
             DELETE FROM schema_versions WHERE version = 6;
             PRAGMA user_version = 5;",
        )
        .unwrap();
    drop(store);
    let before = std::fs::read(&path).unwrap();

    let output = run_args(&["analyze"], &path);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Store freshness: empty"));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let _ = std::fs::remove_file(path);
}

#[test]
fn reporting_commands_reject_incomplete_schema_history_without_writing() {
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-incomplete-schema-history.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).unwrap();
    store
        .connection()
        .execute(
            "DELETE FROM schema_versions WHERE version = ?1",
            params![SCHEMA_VERSION],
        )
        .unwrap();
    drop(store);
    let before = std::fs::read(&path).unwrap();

    let output = run_args(&["analyze"], &path);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("schema history"), "{stderr}");
    assert!(stderr.len() < 512);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let _ = std::fs::remove_file(path);
}

#[test]
fn reporting_commands_reject_same_version_mismatched_schema_without_writing() {
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-mismatched-schema.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).unwrap();
    store
        .connection()
        .execute(
            "ALTER TABLE sessions RENAME COLUMN project TO foreign_project",
            [],
        )
        .unwrap();
    drop(store);
    let before = std::fs::read(&path).unwrap();

    let output = run_args(&["analyze"], &path);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing column sessions.project"),
        "{stderr}"
    );
    assert!(stderr.len() < 512);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let _ = std::fs::remove_file(path);
}

#[test]
fn readiness_document_tracks_the_mvp_boundary_and_entry_condition() {
    let readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let readiness =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/readiness/mvp.md"))
            .unwrap();

    assert!(readme.contains("docs/readiness/mvp.md"));
    assert!(readme.contains("docs/specs/post-mvp.md"));
    assert!(readiness.contains("../specs/post-mvp.md"));
    for marker in [
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo test --all-features",
        "optimize --apply",
        "compressed rollout",
        "#53",
        "#54",
        "Next-phase entry condition",
        "read-only",
    ] {
        assert!(
            readiness.contains(marker),
            "missing readiness marker: {marker}"
        );
    }
}

#[test]
fn readme_documents_current_cli_surface_and_mvp_boundaries() {
    let readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let readme_lower = readme.to_ascii_lowercase();

    assert!(readme.contains("## CLI surface"));
    for args in REPORTING_COMMANDS {
        let command = args.join(" ");
        assert!(
            readme.contains(&format!("| `{command}` |")),
            "README is missing the `{command}` command"
        );
    }

    let examples: Vec<_> = readme
        .lines()
        .filter_map(|line| line.trim().strip_prefix("cargo run -- "))
        .collect();
    assert!(!examples.is_empty(), "README has no runnable CLI examples");

    let store = fixture_store();
    let store_path = store.to_string_lossy().into_owned();
    for example in examples {
        let args: Vec<_> = example
            .split_whitespace()
            .map(|arg| {
                if arg == ".codexlens.sqlite" {
                    store_path.clone()
                } else {
                    arg.to_owned()
                }
            })
            .collect();
        let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "README example is not accepted: cargo run -- {example}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = fs::remove_file(store);

    for boundary in [
        "existing derived SQLite store",
        "local-only",
        "deterministic",
        "evidence-backed",
        "does not modify the supplied store or target files",
        "temporary migrated copy",
        "`optimize --apply`",
        "compressed rollout readers",
        "`--frozen`",
    ] {
        assert!(
            readme_lower.contains(&boundary.to_ascii_lowercase()),
            "README is missing MVP boundary: {boundary}"
        );
    }
}

#[test]
fn post_mvp_contract_spec_tracks_each_deferred_boundary() {
    let spec =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/specs/post-mvp.md"))
            .unwrap();

    let sections = [
        (
            "## 1. Compressed rollout readers",
            "## 2. Refresh and frozen reporting",
        ),
        (
            "## 2. Refresh and frozen reporting",
            "## 3. Machine-readable output",
        ),
        ("## 3. Machine-readable output", "## 4. Live monitoring"),
        ("## 4. Live monitoring", "## 5. `optimize --apply`"),
        (
            "## 5. `optimize --apply`",
            "## Entry gate for implementation issues",
        ),
    ];
    for (heading, next_heading) in sections {
        assert!(
            spec.contains(heading),
            "missing contract section: {heading}"
        );
        let section = spec
            .split_once(heading)
            .and_then(|(_, rest)| rest.split_once(next_heading).map(|(body, _)| body))
            .unwrap_or_else(|| panic!("could not isolate contract section: {heading}"));
        assert!(
            section.contains("### Compatibility tests"),
            "missing compatibility tests for {heading}"
        );
        assert!(
            section.contains("### Privacy tests"),
            "missing privacy tests for {heading}"
        );
    }
    for marker in [
        "Current MVP regression coverage",
        "Compatibility tests",
        "Privacy tests",
        "source read-only",
        "deterministic",
        "human-readable",
        "machine-readable",
        "backup",
        "recovery",
        "confirmation",
        "before implementation starts",
        "period_start",
        "schema_version",
        "nullable",
        "canonical",
        "scope",
        "LF line endings",
        "exactly one final LF",
        "No applicable proposals.",
        "RenderedDiff",
        "SkippedProposal",
        "escape backslash",
    ] {
        assert!(spec.contains(marker), "missing contract marker: {marker}");
    }
}

#[test]
fn compressed_rollout_input_is_explicitly_unsupported_and_read_only() {
    let source = temp_store_path("compressed-rollout").with_extension("jsonl.zst");
    let payload = b"synthetic secret=do-not-print\n";
    fs::write(&source, payload).unwrap();
    let before = fs::read(&source).unwrap();
    let identity = fs::canonicalize(&source).unwrap();

    let mut store = Store::in_memory().unwrap();
    let report = store
        .ingest_inputs(
            &[DiscoveredInput {
                path: source.clone(),
                identity,
                kind: InputKind::Rollout { archived: false },
                reader: Some(ReaderKind::ZstdJsonl),
            }],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].diagnostics, 1);
    assert_eq!(report.files[0].records, 0);
    let data = store.load_canonical().unwrap();
    assert_eq!(data.records.len(), 0);
    assert_eq!(data.diagnostics.len(), 1);
    assert_eq!(data.diagnostics[0].kind, DiagnosticKind::UnsupportedReader);
    assert!(!data.diagnostics[0].message.contains("do-not-print"));
    assert_eq!(fs::read(&source).unwrap(), before);

    let _ = fs::remove_file(source);
}

#[test]
fn reporting_is_deterministic_bounded_and_does_not_refresh_or_write() {
    let store = fixture_store();
    let raw_source = store.with_extension("jsonl");
    let raw_payload = b"synthetic raw secret=do-not-report\n";
    fs::write(&raw_source, raw_payload).unwrap();
    let store_before = fs::read(&store).unwrap();
    let raw_before = fs::read(&raw_source).unwrap();

    let first = run_args(&["doctor"], &store);
    let second = run_args(&["doctor"], &store);

    assert!(first.status.success());
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stderr, second.stderr);
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert_doctor_report(&stdout);
    assert!(!stdout.contains("do-not-report"));
    assert_eq!(fs::read(&store).unwrap(), store_before);
    assert_eq!(fs::read(&raw_source).unwrap(), raw_before);

    let _ = fs::remove_file(raw_source);
    let _ = fs::remove_file(store);
}

fn assert_deferred_surface_is_rejected(args: &[&str]) {
    let store = empty_store();
    let before = fs::read(&store).unwrap();
    let output = run_args(args, &store);

    assert!(!output.status.success(), "unexpectedly accepted {args:?}");
    assert!(output.stderr.len() < 512, "unbounded error for {args:?}");
    assert_eq!(fs::read(&store).unwrap(), before);
    let _ = fs::remove_file(store);
}

#[test]
fn refresh_and_frozen_reporting_are_not_currently_exposed() {
    assert_deferred_surface_is_rejected(&["refresh"]);
    assert_deferred_surface_is_rejected(&["analyze", "--frozen"]);
}

#[test]
fn machine_readable_output_is_not_currently_exposed() {
    assert_deferred_surface_is_rejected(&["analyze", "--format", "json"]);
}

#[test]
fn live_monitoring_is_not_currently_exposed() {
    assert_deferred_surface_is_rejected(&["monitor"]);
}

#[test]
fn optimize_apply_is_not_currently_exposed() {
    assert_deferred_surface_is_rejected(&["optimize", "--apply"]);
}
