use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use codexlens::advisor::DoctorReport;
use codexlens::discovery::{DiscoveredInput, InputKind, ReaderKind};
use codexlens::model::{
    DiagnosticKind, InstructionFile, InstructionFileKind, InstructionFileState, InstructionScope,
    InstructionSnapshot, InstructionSnapshotAccuracy, InstructionSnapshotSource, ProjectRootStatus,
    SourceRef, Surface, SurfaceKind, SurfaceLoadMode, SurfaceScope, SurfaceUsageState,
};
use codexlens::rollout::RolloutParseOptions;
use codexlens::store::{IngestInputKind, IngestOptions, SCHEMA_VERSION, Store};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

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

#[derive(Debug, Deserialize)]
struct KnownScope {
    kind: String,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KnownFreshness {
    state: String,
    source_count: usize,
    latest_ingested_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KnownCoverage {
    scope: String,
    status: String,
    activity_start: Option<String>,
    activity_end: Option<String>,
    valid_activity_timestamps: usize,
    missing_activity_timestamps: usize,
    invalid_activity_timestamps: usize,
    session_count: usize,
    record_count: usize,
}

#[derive(Debug, Deserialize)]
struct KnownSource {
    kind: String,
    path: String,
    line: Option<usize>,
    ingested_at: Option<String>,
    parser_schema_version: u32,
}

#[derive(Debug, Deserialize)]
struct KnownEvidence {
    session_id: Option<String>,
    source: KnownSource,
    role: String,
    excerpt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KnownFinding {
    kind: String,
    severity: String,
    confidence: String,
    scope: KnownScope,
    key: String,
    summary: String,
    evidence: Vec<KnownEvidence>,
    occurrences: usize,
    distinct_sessions: usize,
    affected_paths: Vec<String>,
    observed_commands: Vec<String>,
    sequence: Vec<String>,
    suggested_action: String,
    limitations: Vec<String>,
    verification_status: Option<String>,
    heuristic: String,
}

#[derive(Debug, Deserialize)]
struct KnownFindingGroup {
    scope: KnownScope,
    findings: Vec<KnownFinding>,
}

#[derive(Debug, Deserialize)]
struct KnownFindingData {
    period_start: Option<String>,
    period_end: Option<String>,
    session_count: usize,
    freshness: KnownFreshness,
    coverage: KnownCoverage,
    finding_counts: BTreeMap<String, usize>,
    groups: Vec<KnownFindingGroup>,
}

#[derive(Debug, Deserialize)]
struct KnownFindingDocument {
    schema_version: u32,
    command: String,
    data: KnownFindingData,
}

#[derive(Debug, Deserialize)]
struct KnownSession {
    id: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    cwd: Option<String>,
    project: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KnownSessionsData {
    rows: Vec<KnownSession>,
}

#[derive(Debug, Deserialize)]
struct KnownSessionsDocument {
    schema_version: u32,
    command: String,
    data: KnownSessionsData,
}

#[derive(Debug, Deserialize)]
struct KnownProposal {
    target_scope: KnownScope,
    target_path: String,
    action: String,
    review_only: bool,
    observed_problem: String,
    evidence_count: usize,
    distinct_sessions: usize,
    confidence: String,
    heuristic: String,
    evidence: Vec<KnownEvidence>,
    proposed_text: Option<String>,
    existing_text: Option<String>,
    source_path: Option<String>,
    expected_target_hash: Option<String>,
    expected_source_hash: Option<String>,
    target_rationale: String,
    limitations: Vec<String>,
    review_reminder: String,
}

#[derive(Debug, Deserialize)]
struct KnownRenderedDiff {
    proposal: KnownProposal,
    diff: String,
}

#[derive(Debug, Deserialize)]
struct KnownSkippedProposal {
    target_path: String,
    reason: String,
    proposal: Option<KnownProposal>,
}

#[derive(Debug, Deserialize)]
struct KnownOptimizeData {
    rendered: Vec<KnownRenderedDiff>,
    skipped: Vec<KnownSkippedProposal>,
}

#[derive(Debug, Deserialize)]
struct KnownOptimizeDocument {
    schema_version: u32,
    command: String,
    data: KnownOptimizeData,
}

fn assert_known_coverage(coverage: &KnownCoverage) {
    assert_eq!(coverage.scope, "selected_store");
    assert!(matches!(
        coverage.status.as_str(),
        "empty" | "observed" | "partial"
    ));
    let _ = (
        &coverage.activity_start,
        &coverage.activity_end,
        coverage.valid_activity_timestamps,
        coverage.missing_activity_timestamps,
        coverage.invalid_activity_timestamps,
        coverage.session_count,
        coverage.record_count,
    );
}

fn assert_known_proposal(proposal: &KnownProposal) {
    let _ = (
        &proposal.target_scope.kind,
        &proposal.target_scope.value,
        &proposal.target_path,
        &proposal.action,
        proposal.review_only,
        &proposal.observed_problem,
        proposal.evidence_count,
        proposal.distinct_sessions,
        &proposal.confidence,
        &proposal.heuristic,
        &proposal.proposed_text,
        &proposal.existing_text,
        &proposal.source_path,
        &proposal.expected_target_hash,
        &proposal.expected_source_hash,
        &proposal.target_rationale,
        &proposal.limitations,
        &proposal.review_reminder,
    );
    assert!(!proposal.evidence.is_empty());
    for evidence in &proposal.evidence {
        let source = &evidence.source;
        let _ = (
            &evidence.session_id,
            &source.kind,
            &source.path,
            &source.line,
            &source.ingested_at,
            source.parser_schema_version,
            &evidence.role,
            &evidence.excerpt,
        );
    }
}

fn temp_store_path(label: &str) -> PathBuf {
    let nonce = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
    let base = if cfg!(windows) {
        std::env::temp_dir()
    } else {
        fs::canonicalize(std::env::temp_dir()).unwrap()
    };
    let path = base.join(format!(
        "codexlens-cli-{}-{label}-{nonce}.sqlite",
        std::process::id(),
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn long_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"\\?\{}", path.display()))
    } else {
        path.to_path_buf()
    }
}

fn temp_rollout_path(label: &str) -> PathBuf {
    let nonce = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{label}-{}-{nonce}.jsonl",
        std::process::id(),
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn write_compressed(path: &Path, payload: &[u8]) {
    fs::write(path, zstd::stream::encode_all(payload, 0).unwrap()).unwrap();
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

fn typed_view_store() -> PathBuf {
    let path = fixture_store();
    let mut store = Store::open(&path).unwrap();
    store
        .replace_surfaces(&[
            Surface {
                id: "global-unused-config".to_owned(),
                kind: SurfaceKind::Config,
                name: "global-config".to_owned(),
                path: Some(PathBuf::from("/synthetic/codex/config.toml")),
                scope: SurfaceScope::Global,
                enabled: Some(true),
                load_mode: SurfaceLoadMode::StartupFull,
                static_bytes: Some(9000),
                startup_bytes: Some(9000),
                observed_uses: 0,
                observed_sessions: 0,
                usage_state: SurfaceUsageState::Unused,
                limitations: Vec::new(),
            },
            Surface {
                id: "project-used-instruction".to_owned(),
                kind: SurfaceKind::Instruction,
                name: "AGENTS.md".to_owned(),
                path: Some(PathBuf::from("/fixture/project/AGENTS.md")),
                scope: SurfaceScope::Project(PathBuf::from("/fixture/project")),
                enabled: Some(true),
                load_mode: SurfaceLoadMode::StartupFull,
                static_bytes: Some(128),
                startup_bytes: Some(128),
                observed_uses: 4,
                observed_sessions: 2,
                usage_state: SurfaceUsageState::Used,
                limitations: Vec::new(),
            },
        ])
        .unwrap();
    path
}

fn command_contract_store() -> PathBuf {
    let path = temp_store_path("command-contract");
    let mut store = Store::open(&path).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/analysis/command-contract.jsonl");
    store
        .ingest_rollout_file(&fixture, &RolloutParseOptions::default())
        .unwrap();

    let mut surfaces = vec![Surface {
        id: "unused-skill".to_owned(),
        kind: SurfaceKind::Skill,
        name: "unused-skill".to_owned(),
        path: Some(PathBuf::from("/fixture/codex/skills/unused/SKILL.md")),
        scope: SurfaceScope::Global,
        enabled: Some(true),
        load_mode: SurfaceLoadMode::OnDemand,
        static_bytes: Some(256),
        startup_bytes: Some(0),
        observed_uses: 0,
        observed_sessions: 0,
        usage_state: SurfaceUsageState::Unused,
        limitations: Vec::new(),
    }];
    // Keep the percentile boundary at 4096 so the 8192-byte row is actionable.
    for index in 0..9 {
        surfaces.push(Surface {
            id: format!("baseline-rule-{index}"),
            kind: SurfaceKind::Rule,
            name: format!("base-{index}.rules"),
            path: Some(PathBuf::from(format!(
                "/fixture/codex/rules/base-{index}.rules"
            ))),
            scope: SurfaceScope::Global,
            enabled: Some(true),
            load_mode: SurfaceLoadMode::StartupFull,
            static_bytes: Some(4096),
            startup_bytes: Some(4096),
            observed_uses: 1,
            observed_sessions: 1,
            usage_state: SurfaceUsageState::Used,
            limitations: Vec::new(),
        });
    }
    surfaces.push(Surface {
        id: "heavy-skill".to_owned(),
        kind: SurfaceKind::Skill,
        name: "heavy-skill".to_owned(),
        path: Some(PathBuf::from("/fixture/codex/skills/heavy/SKILL.md")),
        scope: SurfaceScope::Global,
        enabled: Some(true),
        load_mode: SurfaceLoadMode::StartupFull,
        static_bytes: Some(8192),
        startup_bytes: Some(8192),
        observed_uses: 1,
        observed_sessions: 1,
        usage_state: SurfaceUsageState::Used,
        limitations: Vec::new(),
    });
    store.replace_surfaces(&surfaces).unwrap();
    path
}

fn session_selection_store() -> PathBuf {
    let path = temp_store_path("session-selection");
    let mut store = Store::open(&path).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rollout/session-selection.jsonl");
    store
        .ingest_rollout_file(&fixture, &RolloutParseOptions::default())
        .unwrap();
    let archived_fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rollout/archived-selection.jsonl");
    store
        .ingest_rollout_file(&archived_fixture, &RolloutParseOptions::default())
        .unwrap();
    store
        .connection()
        .execute(
            "UPDATE sessions SET archive_state = 1 WHERE session_id = 'archived-recent'",
            [],
        )
        .unwrap();
    path
}

fn coverage_timestamp_fallback_store() -> PathBuf {
    let path = temp_store_path("coverage-timestamp-fallback");
    let mut store = Store::open(&path).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rollout/coverage-timestamp-fallback.jsonl");
    store
        .ingest_rollout_file(&fixture, &RolloutParseOptions::default())
        .unwrap();
    for table in ["messages", "file_operations", "token_usage"] {
        let changed = store
            .connection()
            .execute(&format!("UPDATE {table} SET timestamp = NULL"), [])
            .unwrap();
        assert!(changed > 0, "fixture did not create {table} rows");
    }
    path
}

fn filtered_coverage_period_store() -> PathBuf {
    let path = temp_store_path("filtered-coverage-period");
    let mut store = Store::open(&path).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rollout/filtered-coverage-period.jsonl");
    store
        .ingest_rollout_file(&fixture, &RolloutParseOptions::default())
        .unwrap();
    let changed = store
        .connection()
        .execute(
            "UPDATE messages SET timestamp = 'invalid-out-of-period' WHERE session_id = 'fixture-filtered-out-of-range' AND role = 'assistant'",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    path
}

fn coverage_limitation_store() -> PathBuf {
    let path = fixture_store();
    let mut store = Store::open(&path).unwrap();
    let limitation_fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rollout/coverage-limitations.jsonl");
    store
        .ingest_rollout_file(&limitation_fixture, &RolloutParseOptions::default())
        .unwrap();
    let changed = store
        .connection()
        .execute(
            "UPDATE turns SET started_at = NULL WHERE turn_key = (SELECT turn_key FROM turns ORDER BY turn_key LIMIT 1)",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    let changed = store
        .connection()
        .execute(
            "UPDATE records SET timestamp = 'not-a-timestamp' WHERE record_key = (SELECT record_key FROM records ORDER BY record_key LIMIT 1)",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    store
        .connection()
        .execute(
            "INSERT INTO diagnostics (diagnostic_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, kind, message) VALUES (?1, ?2, ?3, ?4, ?5, NULL, 1, ?6, ?7)",
            params![
                "coverage-oversized",
                "synthetic-coverage-source",
                limitation_fixture.to_string_lossy().as_ref(),
                7,
                "rollout",
                "oversized_line",
                "synthetic oversized line",
            ],
        )
        .unwrap();
    for (key, path, line, kind, message, source_kind) in [
        (
            "coverage-unreadable",
            "unreadable.jsonl",
            Some(1),
            "unreadable",
            "synthetic unreadable source",
            "rollout",
        ),
        (
            "coverage-conflict",
            "state.sqlite",
            None,
            "metadata_conflict",
            "synthetic metadata conflict",
            "state",
        ),
    ] {
        store
            .connection()
            .execute(
                "INSERT INTO diagnostics (diagnostic_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, kind, message) VALUES (?1, ?2, ?3, ?4, ?5, NULL, 1, ?6, ?7)",
                params![key, "synthetic-coverage-source", path, line, source_kind, kind, message],
            )
            .unwrap();
    }
    store
        .connection()
        .execute(
            "UPDATE diagnostics SET session_id = 'fixture-analysis-session-a' WHERE diagnostic_key = 'coverage-conflict'",
            [],
        )
        .unwrap();
    path
}

fn boundary_turn_coverage_store() -> PathBuf {
    let path = temp_store_path("boundary-turn-coverage");
    let mut store = Store::open(&path).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rollout/boundary-turn-coverage.jsonl");
    store
        .ingest_rollout_file(&fixture, &RolloutParseOptions::default())
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

fn unknown_surface_store() -> PathBuf {
    let path = temp_store_path("unknown-surface");
    let mut store = Store::open(&path).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO sessions (session_id, source_identity, source_path, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "unknown-surface-session",
                "unknown-surface-source",
                "unknown-surface.jsonl",
                "2026-01-03T00:00:00Z",
                "2026-01-03T00:01:00Z",
            ],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO records (record_key, source_identity, source_path, source_line, source_kind, parser_schema_version, session_id, timestamp, sequence, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                "unknown-surface-record",
                "unknown-surface-source",
                "unknown-surface.jsonl",
                1,
                "rollout",
                SCHEMA_VERSION,
                "unknown-surface-session",
                "2026-01-03T00:00:30Z",
                0,
                "event_message",
            ],
        )
        .unwrap();
    let snapshot_content = "synthetic instruction snapshot";
    let snapshot_hash = codexlens::instructions::content_hash(snapshot_content.as_bytes());
    store
        .connection()
        .execute(
            "INSERT INTO instruction_blobs (blob_key, content_hash, byte_count, content) VALUES (?1, ?2, ?3, ?4)",
            params![
                "unknown-surface-snapshot",
                snapshot_hash,
                snapshot_content.len(),
                snapshot_content,
            ],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO instruction_snapshots (snapshot_key, source_identity, source_path, source_line, source_kind, parser_schema_version, session_id, snapshot_source, accuracy, blob_key, content_hash, byte_count, effective_chain_hash, truncated, chain_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                "unknown-surface-snapshot",
                "unknown-surface-source",
                "unknown-surface.jsonl",
                1,
                "rollout",
                SCHEMA_VERSION,
                "unknown-surface-session",
                "rollout",
                "observed",
                "unknown-surface-snapshot",
                snapshot_hash,
                snapshot_content.len(),
                snapshot_hash,
                0,
                "[]",
            ],
        )
        .unwrap();
    store
        .replace_surfaces(&[Surface {
            id: "unknown-surface".to_owned(),
            kind: SurfaceKind::Skill,
            name: "unknown-skill".to_owned(),
            path: Some(PathBuf::from("/synthetic/unknown-skill/SKILL.md")),
            scope: SurfaceScope::Global,
            enabled: None,
            load_mode: SurfaceLoadMode::OnDemand,
            static_bytes: Some(64),
            startup_bytes: Some(0),
            observed_uses: 0,
            observed_sessions: 0,
            usage_state: SurfaceUsageState::Unknown,
            limitations: vec!["synthetic usage state is unknown".to_owned()],
        }])
        .unwrap();
    path
}

fn chronological_period_store() -> PathBuf {
    let path = temp_store_path("chronological-period");
    let store = Store::open(&path).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO sessions (session_id, source_identity, source_path, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "synthetic-period-session",
                "synthetic-period-source",
                "synthetic.jsonl",
                "2026-01-03T09:00:00+09:00",
                "2026-01-03T00:30:00.123456789Z",
            ],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO records (record_key, source_identity, source_path, source_line, session_id, timestamp, sequence, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "synthetic-period-record",
                "synthetic-period-source",
                "synthetic.jsonl",
                3,
                "synthetic-period-session",
                "not-a-timestamp",
                0,
                "response_item",
            ],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO records (record_key, source_identity, source_path, source_line, session_id, timestamp, sequence, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "synthetic-period-record-valid",
                "synthetic-period-source",
                "synthetic.jsonl",
                4,
                "synthetic-period-session",
                "2026-01-03T00:30:00.123456789Z",
                1,
                "response_item",
            ],
        )
        .unwrap();
    path
}

fn run_args(args: &[&str], store: &Path) -> Output {
    run_args_with_flags(args, &[], store)
}

fn run_args_with_flags(args: &[&str], flags: &[&str], store: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexlens"));
    command.args(args).args(flags);
    if !args.contains(&"--frozen") && !flags.contains(&"--frozen") {
        command.arg("--frozen");
    }
    command
        .args(["--store", store.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn run_query(args: &[&str], store: &Path, stdin: Option<&[u8]>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexlens"));
    command
        .arg("query")
        .args(args)
        .args(["--store", store.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn run_sql(args: &[&str], store: &Path, stdin: Option<&[u8]>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexlens"));
    command
        .arg("sql")
        .args(args)
        .args(["--store", store.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn assert_file_unchanged(path: &Path, before: &[u8], label: &str) {
    assert_eq!(fs::read(path).unwrap(), before, "{label} changed");
}

fn assert_output_omits(output: &Output, marker: &[u8], label: &str) {
    for (stream, bytes) in [
        ("stdout", output.stdout.as_slice()),
        ("stderr", output.stderr.as_slice()),
    ] {
        assert!(
            !bytes.windows(marker.len()).any(|window| window == marker),
            "{label} leaked the raw marker to {stream}"
        );
    }
}

fn refresh_home() -> (PathBuf, PathBuf) {
    let home = temp_store_path("refresh-home");
    let session_directory = home.join("sessions").join("2026");
    fs::create_dir_all(&session_directory).unwrap();
    let source = session_directory.join("fixture.jsonl");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/analysis/lenses.jsonl");
    fs::copy(fixture, &source).unwrap();
    (home, source)
}

fn run_refresh(home: &Path, store: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["refresh", "--codex-home"])
        .arg(home)
        .args(["--store"])
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
    assert!(stdout.starts_with("WHAT TO FIX FIRST"));
    assert!(stdout.contains("Scope: global + projects"));
    assert!(stdout.contains("Coverage: partial (2 sessions)"));
    assert!(stdout.contains("COST"));
    assert!(stdout.contains("  owner: "));
    assert!(stdout.contains("  action: "));
    assert!(stdout.contains("  follow-up: "));
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

fn parse_json_report(output: &Output, command: &str) -> Value {
    assert!(
        output.status.success(),
        "{command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "{command}: stderr is not empty");
    let document: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{command} did not emit one JSON document: {error}"));
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["command"], command);
    assert!(document["data"].is_object());
    document
}

fn assert_human_limitation_details(output: &str, coverage: &Value) {
    let limitation = coverage["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|limitation| limitation["kind"] == "oversized_line")
        .expect("synthetic oversized-line limitation");
    let selected_sessions = limitation["selected_sessions"].as_u64().unwrap();
    let selected_records = limitation["selected_records"].as_u64().unwrap();
    let affected_lenses = limitation["affected_lenses"].as_array().unwrap();
    assert!(selected_sessions > 0);
    assert!(selected_records > 0);
    assert!(!affected_lenses.is_empty());

    let line = output
        .lines()
        .find(|line| line.starts_with("Limitation oversized_line at "))
        .expect("human report includes the oversized-line limitation");
    assert!(
        line.contains(limitation["message"].as_str().unwrap()),
        "{line}"
    );
    assert!(
        line.contains(&format!(
            "selected sessions: {selected_sessions}; selected records: {selected_records}"
        )),
        "{line}"
    );
    for lens in affected_lenses {
        assert!(line.contains(lens.as_str().unwrap()), "{line}");
    }
}

fn rendered_diff_store() -> (PathBuf, PathBuf, PathBuf) {
    rendered_diff_store_with_content("Existing synthetic guidance.\n")
}

fn rendered_diff_store_with_content(content: &str) -> (PathBuf, PathBuf, PathBuf) {
    rendered_diff_store_with_options(content, false)
}

fn rendered_overhead_store() -> (PathBuf, PathBuf, PathBuf) {
    let content = "synthetic overhead guidance\n".repeat(400);
    rendered_diff_store_with_options(&content, true)
}

fn rendered_diff_store_with_options(
    content: &str,
    include_overhead: bool,
) -> (PathBuf, PathBuf, PathBuf) {
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
    data.messages.clear();
    data.tool_calls.clear();
    data.file_operations.clear();
    data.token_usage.clear();
    data.diagnostics.clear();
    data.instruction_snapshots.clear();

    let project_root = temp_store_path("rendered-project");
    fs::create_dir(&project_root).unwrap();
    let target = project_root.join("AGENTS.md");
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
    if include_overhead {
        data.surfaces.push(Surface {
            id: "synthetic-heavy-instruction".to_owned(),
            kind: SurfaceKind::Instruction,
            name: "AGENTS.md".to_owned(),
            path: Some(target.clone()),
            scope: SurfaceScope::Project(project_root.clone()),
            enabled: Some(true),
            load_mode: SurfaceLoadMode::StartupFull,
            static_bytes: Some(8_192),
            startup_bytes: Some(8_192),
            observed_uses: data.sessions.len(),
            observed_sessions: data.sessions.len(),
            usage_state: SurfaceUsageState::Used,
            limitations: Vec::new(),
        });
        let content_hash = codexlens::instructions::content_hash(content.as_bytes());
        data.instruction_snapshots = data
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| InstructionSnapshot {
                session_id: Some(session.id.clone()),
                turn_id: None,
                source: InstructionSnapshotSource::Rollout,
                accuracy: InstructionSnapshotAccuracy::Observed,
                content: Some(content.to_owned()),
                content_hash: Some(content_hash.clone()),
                byte_count: content.len(),
                chain: Vec::new(),
                effective_chain_hash: Some(content_hash.clone()),
                truncated: false,
                provenance: SourceRef::rollout(PathBuf::from("synthetic.jsonl"), index + 1),
            })
            .collect();
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
fn query_renders_table_markdown_and_json_from_an_existing_store() {
    let store = fixture_store();
    let before = fs::read(&store).unwrap();

    let table = run_query(&["SELECT 1 AS value"], &store, None);
    assert!(
        table.status.success(),
        "{}",
        String::from_utf8_lossy(&table.stderr)
    );
    assert!(table.stderr.is_empty());
    let table_stdout = String::from_utf8_lossy(&table.stdout);
    assert!(table_stdout.contains("QUERY"), "{table_stdout}");
    assert!(table_stdout.contains("value"), "{table_stdout}");
    assert!(table_stdout.contains("1"), "{table_stdout}");

    let pragma = run_query(&["PRAGMA user_version", "--format", "json"], &store, None);
    let pragma_document = parse_json_report(&pragma, "query");
    assert_eq!(pragma_document["data"]["columns"], json!(["user_version"]));
    assert_eq!(pragma_document["data"]["rows"].as_array().unwrap().len(), 1);

    let compile_options = run_query(
        &["PRAGMA compile_options", "--format", "json"],
        &store,
        None,
    );
    let compile_options_document = parse_json_report(&compile_options, "query");
    assert_eq!(
        compile_options_document["data"]["columns"],
        json!(["compile_options"])
    );

    let markdown = run_query(&["SELECT 1 AS value", "--format", "markdown"], &store, None);
    assert!(
        markdown.status.success(),
        "{}",
        String::from_utf8_lossy(&markdown.stderr)
    );
    assert!(String::from_utf8_lossy(&markdown.stdout).starts_with("# QUERY"));

    let json_output = run_query(&["--format", "json"], &store, Some(b"SELECT 2 AS value\n"));
    let document = parse_json_report(&json_output, "query");
    assert_eq!(document["data"]["columns"], json!(["value"]));
    assert_eq!(document["data"]["rows"][0][0], 2);
    assert_eq!(document["data"]["omitted_count"], 0);
    assert_eq!(fs::read(&store).unwrap(), before);
    let _ = fs::remove_file(store);
}

#[test]
fn cli_help_documents_command_semantics_and_read_only_boundaries() {
    let help = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let stdout = String::from_utf8_lossy(&help.stdout);
    for description in [
        "Build or update the derived SQLite store",
        "Refresh (unless --frozen) and report all canonical findings",
        "Show configured surfaces, ownership, and observed use",
        "Explain always-on context cost and residuals",
        "Show where tool, Skill, model, prompt, and subagent effort goes",
        "Rank actionable configuration and workflow opportunities",
        "Report recurring tool failures with scoped fixes",
        "Report repeated edit/failure loops and targets",
        "Report steering, correction, question, and instruction patterns",
        "Show bounded, action-first health fixes by scope",
        "Run a bounded read-only SQL query",
        "Print/diff a reviewable optimization plan or apply it explicitly",
    ] {
        assert!(
            stdout.contains(description),
            "missing help text: {description}\n{stdout}"
        );
    }

    let sql_help = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["sql", "--help"])
        .output()
        .unwrap();
    assert!(sql_help.status.success());
    let sql_stdout = String::from_utf8_lossy(&sql_help.stdout);
    assert!(sql_stdout.contains("One read-only SQL statement or stdin"));
    assert!(sql_stdout.contains("50 columns and 50 rows"));

    let analyze_help = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["analyze", "--help"])
        .output()
        .unwrap();
    assert!(analyze_help.status.success());
    assert!(
        String::from_utf8_lossy(&analyze_help.stdout)
            .contains("refresh progress goes to stderr and JSON stdout stays one document")
    );
}

#[test]
fn query_rejects_writes_and_bounds_rows_without_creating_a_store() {
    let store = fixture_store();
    {
        let connection = Connection::open(&store).unwrap();
        for index in 0..60 {
            connection
                .execute(
                    "INSERT INTO sessions (session_id, source_identity, source_path) VALUES (?1, 'synthetic-query', 'synthetic-query.jsonl')",
                    params![format!("query-session-{index:03}")],
                )
                .unwrap();
        }
    }
    let before = fs::read(&store).unwrap();

    let output = run_query(
        &[
            "SELECT session_id FROM sessions ORDER BY session_id",
            "--format",
            "json",
        ],
        &store,
        None,
    );
    let document = parse_json_report(&output, "query");
    assert_eq!(document["data"]["rows"].as_array().unwrap().len(), 50);
    assert!(document["data"]["omitted_count"].as_u64().unwrap() > 0);
    assert!(output.stdout.len() < 16 * 1024);

    let write = run_query(
        &["INSERT INTO sessions (session_id) VALUES ('query-write')"],
        &store,
        None,
    );
    assert!(!write.status.success());
    assert!(String::from_utf8_lossy(&write.stderr).contains("read-only"));
    assert_eq!(fs::read(&store).unwrap(), before);

    for sql in [
        "PRAGMA journal_mode=WAL",
        "PRAGMA query_only = OFF",
        "PRAGMA query_only(OFF)",
        "PRAGMA user_version = 42",
        "ATTACH ':memory:' AS external_store",
        "DETACH external_store",
        "SELECT LOAD_EXTENSION('synthetic-extension')",
        "SELECT 1; SELECT 2",
    ] {
        let rejected = run_query(&[sql], &store, None);
        assert!(!rejected.status.success(), "{sql} unexpectedly succeeded");
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("statement"));
    }
    assert_eq!(fs::read(&store).unwrap(), before);

    let missing = temp_store_path("query-missing");
    let missing_output = run_query(&["SELECT 1"], &missing, None);
    assert!(!missing_output.status.success());
    assert!(String::from_utf8_lossy(&missing_output.stderr).contains("store does not exist"));
    assert!(!missing.exists());
    let _ = fs::remove_file(store);
}

#[test]
fn sql_is_the_read_only_escape_hatch_and_query_remains_compatible() {
    let store = fixture_store();
    let before = fs::read(&store).unwrap();
    let table = run_sql(&["SELECT(7)"], &store, None);
    assert!(String::from_utf8_lossy(&table.stdout).starts_with("SQL\n"));
    let output = run_sql(&["SELECT 7 AS value", "--format", "json"], &store, None);
    let document = parse_json_report(&output, "sql");
    assert_eq!(document["data"]["columns"], json!(["value"]));
    assert_eq!(document["data"]["rows"][0][0], 7);
    assert_eq!(fs::read(&store).unwrap(), before);
    let _ = fs::remove_file(store);
}

#[test]
fn optimize_print_is_read_only_and_scope_matches_findings() {
    let (store, target, project_root) = rendered_diff_store();
    let before = fs::read(&target).unwrap();

    let printed = run_args(&["optimize", "--print"], &store);
    assert!(
        printed.status.success(),
        "{}",
        String::from_utf8_lossy(&printed.stderr)
    );
    let printed_stdout = String::from_utf8_lossy(&printed.stdout);
    assert!(printed_stdout.contains("Target: "), "{printed_stdout}");
    assert!(
        printed_stdout.contains("Evidence ref: "),
        "{printed_stdout}"
    );
    assert!(
        printed_stdout.contains("Verification: "),
        "{printed_stdout}"
    );
    assert_eq!(fs::read(&target).unwrap(), before);

    let project_scope = format!("project:{}", project_root.display());
    let project = run_args(
        &["optimize", "--diff", "--scope", project_scope.as_str()],
        &store,
    );
    assert!(
        project.status.success(),
        "{}",
        String::from_utf8_lossy(&project.stderr)
    );
    assert!(String::from_utf8_lossy(&project.stdout).contains(&target.display().to_string()));

    let global = run_args(&["optimize", "--diff", "--scope", "global"], &store);
    assert!(
        global.status.success(),
        "{}",
        String::from_utf8_lossy(&global.stderr)
    );
    assert!(!String::from_utf8_lossy(&global.stdout).contains(&target.display().to_string()));
    assert!(!String::from_utf8_lossy(&global.stderr).contains(&target.display().to_string()));

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_print_keeps_the_complete_action_briefing_in_json() {
    let (store, target, project_root) = rendered_diff_store();
    let output = run_args(&["optimize", "--print", "--format", "json"], &store);
    let document = parse_json_report(&output, "optimize");
    let data = document["data"].as_object().unwrap();
    for field in [
        "findings",
        "configuration_waste",
        "overhead",
        "proposals",
        "next_steps",
        "limitations",
    ] {
        assert!(data.get(field).is_some(), "missing briefing field {field}");
    }
    assert!(
        data["findings"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
    assert!(
        data["next_steps"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_chain_preserves_the_finding_target_and_evidence() {
    let (store, target, project_root) = rendered_diff_store();
    let doctor = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    let top_fix = doctor["data"]["top_fixes"]
        .as_array()
        .and_then(|fixes| fixes.first())
        .expect("synthetic doctor finding");

    let optimize = parse_json_report(
        &run_args(&["optimize", "--print", "--format", "json"], &store),
        "optimize",
    );
    let target_value = top_fix["target"].as_str().expect("doctor target");
    let proposal = optimize["data"]["proposals"]["rendered"]
        .as_array()
        .and_then(|proposals| {
            proposals
                .iter()
                .find(|rendered| rendered["proposal"]["target_path"] == target_value)
        })
        .expect("matching optimize proposal");
    assert_eq!(proposal["proposal"]["action"], "add");
    assert_eq!(
        proposal["proposal"]["evidence_count"],
        top_fix["occurrences"]
    );
    assert_eq!(
        proposal["proposal"]["distinct_sessions"],
        top_fix["distinct_sessions"]
    );
    let optimize_finding = optimize["data"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == top_fix["id"])
        .expect("matching finding in optimize briefing");
    assert_eq!(
        optimize_finding["investigation"]["proposal_status"],
        "reviewable"
    );
    assert!(
        proposal["proposal"]["evidence"]
            .as_array()
            .is_some_and(|evidence| !evidence.is_empty())
    );
    assert_eq!(
        optimize["data"]["findings"]
            .as_array()
            .and_then(|findings| findings.first())
            .and_then(|finding| finding["target"].as_str()),
        Some(target_value)
    );

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_routes_doctor_overhead_to_a_review_only_skip() {
    let (store, target, project_root) = rendered_overhead_store();
    let before_target = fs::read(&target).unwrap();

    let doctor = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    let overhead = doctor["data"]["top_fixes"]
        .as_array()
        .and_then(|fixes| {
            fixes.iter().find(|fix| {
                fix["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("overhead:project:"))
            })
        })
        .expect("synthetic overhead opportunity");
    assert_eq!(
        overhead["target"],
        target.display().to_string(),
        "doctor should retain the concrete overhead target"
    );

    let optimize = parse_json_report(
        &run_args(&["optimize", "--print", "--format", "json"], &store),
        "optimize",
    );
    let briefing_finding = optimize["data"]["findings"]
        .as_array()
        .and_then(|findings| {
            findings
                .iter()
                .find(|finding| finding["id"] == overhead["id"])
        })
        .expect("doctor overhead opportunity in optimize findings");
    for field in [
        "id",
        "scope",
        "target",
        "impact",
        "occurrences",
        "distinct_sessions",
        "action",
        "evidence",
    ] {
        assert_eq!(briefing_finding[field], overhead[field], "optimize {field}");
    }
    assert_eq!(
        briefing_finding["investigation"]["proposal_status"],
        "review_only"
    );
    assert!(
        briefing_finding["investigation"]["root_cause_question"]
            .as_str()
            .is_some_and(|question| question.contains("sections"))
    );
    assert!(
        briefing_finding["investigation"]["inspect_sections"]
            .as_str()
            .is_some_and(|sections| sections.contains("aggregate startup bytes"))
    );
    assert!(
        briefing_finding["investigation"]["unknowns"]
            .as_array()
            .is_some_and(|unknowns| unknowns.iter().any(|value| value
                .as_str()
                .is_some_and(|text| text.contains("aggregate startup overhead"))))
    );
    let findings = optimize["data"]["findings"].as_array().unwrap();
    let friction = findings
        .iter()
        .position(|finding| {
            !finding["id"].as_str().unwrap().starts_with("surface:")
                && !finding["id"].as_str().unwrap().starts_with("overhead:")
        })
        .expect("recurring friction opportunity");
    let configuration = findings
        .iter()
        .position(|finding| {
            finding["id"].as_str().unwrap().starts_with("surface:")
                || finding["id"].as_str().unwrap().starts_with("overhead:")
        })
        .expect("configuration opportunity");
    assert!(
        friction < configuration,
        "friction must precede configuration trimming"
    );

    let skipped = optimize["data"]["proposals"]["skipped"].as_array().unwrap();
    let proposal = skipped
        .iter()
        .find(|skipped| {
            skipped["proposal"]["heuristic"] == "measured always-on startup context overhead"
        })
        .map(|skipped| &skipped["proposal"])
        .expect("overhead proposal metadata");
    assert_eq!(proposal["target_path"], target.display().to_string());
    assert_eq!(proposal["action"], "modify");
    assert_eq!(proposal["review_only"], true);
    assert!(proposal["proposed_text"].is_null());
    assert!(
        proposal["evidence"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );

    let diff = run_args(&["optimize", "--diff"], &store);
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    assert!(String::from_utf8_lossy(&diff.stdout).contains("Proposal add"));
    assert!(String::from_utf8_lossy(&diff.stderr).contains("review-only"));

    let printed = run_args(&["optimize", "--print"], &store);
    assert!(printed.status.success());
    let stdout = String::from_utf8_lossy(&printed.stdout);
    assert!(stdout.contains(overhead["id"].as_str().unwrap()));
    assert!(stdout.contains("Root-cause question:"));
    assert!(stdout.contains("Inspect sections:"));
    assert!(stdout.contains("Proposal status: review-only"));
    assert!(!stdout.contains("No selected findings were observed."));
    assert_eq!(fs::read(&target).unwrap(), before_target);

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_print_keeps_a_bounded_plan_when_every_proposal_is_skipped() {
    let (store, target, project_root) = rendered_overhead_store();
    let before_target = fs::read(&target).unwrap();
    {
        let store = Store::open(&store).unwrap();
        store
            .connection()
            .execute("DELETE FROM tool_results", [])
            .unwrap();
    }

    let printed = run_args(&["optimize", "--print"], &store);
    assert!(printed.status.success());
    assert!(printed.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&printed.stdout);
    assert!(
        stdout.len() < 20_000,
        "briefing exceeded its synthetic bound"
    );
    assert!(stdout.contains("No reviewable diffs were rendered."));
    assert!(stdout.contains("Reduce startup context overhead"));
    assert!(stdout.contains("Inspect sections:"));
    assert!(stdout.contains("Review and slim always-on configuration at "));
    assert!(!stdout.contains("No selected findings were observed."));

    let optimize = parse_json_report(
        &run_args(&["optimize", "--print", "--format", "json"], &store),
        "optimize",
    );
    assert_eq!(optimize["data"]["counts"]["reviewable_proposal_count"], 0);
    assert!(
        optimize["data"]["counts"]["skipped_proposal_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(
        optimize["data"]["findings"]
            .as_array()
            .is_some_and(|findings| !findings.is_empty())
    );
    assert_eq!(fs::read(&target).unwrap(), before_target);

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn doctor_does_not_call_an_empty_store_healthy() {
    let store = empty_store();
    let output = run_args(&["doctor"], &store);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No actionable finding was observed"),
        "{stdout}"
    );
    assert!(!stdout.contains("LOOKS HEALTHY"), "{stdout}");
    let _ = fs::remove_file(store);
}

#[test]
fn doctor_promotes_actionable_findings_beyond_configuration_waste() {
    let store = fixture_store();

    let document = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    let top_fixes = document["data"]["top_fixes"].as_array().unwrap();

    assert!(
        top_fixes.iter().any(|opportunity| opportunity["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("gap:"))),
        "doctor omitted the actionable instruction-gap finding: {top_fixes:?}"
    );
    let ids = top_fixes
        .iter()
        .map(|opportunity| {
            format!(
                "{}|{}",
                opportunity["id"].as_str().unwrap(),
                serde_json::to_string(&opportunity["scope"]).unwrap()
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ids.len(),
        top_fixes.len(),
        "doctor duplicated an opportunity"
    );
    let stuck = top_fixes
        .iter()
        .find(|opportunity| opportunity["id"] == "stuck:src/lib.rs|loop")
        .unwrap();
    assert_eq!(stuck["occurrences"], 4);
    assert_eq!(stuck["distinct_sessions"], 2);
    let failure = top_fixes
        .iter()
        .find(|opportunity| {
            opportunity["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("failure:"))
        })
        .expect("doctor omitted the recurring failure finding");
    assert_eq!(failure["scope"]["kind"], "project");
    assert_eq!(failure["occurrences"], 2);
    assert_eq!(failure["distinct_sessions"], 2);

    let _ = fs::remove_file(store);
}

#[test]
fn doctor_reports_top_fix_omissions_when_limit_bounds_the_summary() {
    let store = fixture_store();

    let machine = parse_json_report(
        &run_args(&["doctor", "--limit", "1", "--format", "json"], &store),
        "doctor",
    );
    assert_eq!(machine["data"]["top_fixes"].as_array().unwrap().len(), 1);
    assert!(machine["data"]["top_fixes_omitted_count"].as_u64().unwrap() > 0);

    let human = run_args(&["doctor", "--limit", "1"], &store);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("Omitted"), "{stdout}");

    let _ = fs::remove_file(store);
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
fn optimize_apply_requires_confirmation_and_applies_only_reviewed_proposals() {
    let (store, target, project_root) = rendered_diff_store();
    let before_target = fs::read(&target).unwrap();
    let before_store = fs::read(&store).unwrap();
    let raw_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/analysis/lenses.jsonl");
    let raw_before = fs::read(&raw_source).unwrap();

    let missing_confirmation = run_args(&["optimize", "--apply"], &store);

    assert!(!missing_confirmation.status.success());
    assert!(String::from_utf8_lossy(&missing_confirmation.stderr).contains("--yes"));
    assert_eq!(fs::read(&target).unwrap(), before_target);
    assert_eq!(fs::read(&store).unwrap(), before_store);
    assert_file_unchanged(&raw_source, &raw_before, "apply raw fixture");

    let applied = run_args(&["optimize", "--apply", "--yes"], &store);

    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied_stdout = String::from_utf8_lossy(&applied.stdout);
    assert!(applied_stdout.contains("Applied"));
    assert!(applied_stdout.contains("backup"));
    assert!(applied_stdout.contains("Recovery: not needed."));
    let backup_dir = applied_stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("Backups retained at ")
                .and_then(|path| path.strip_suffix('.'))
                .map(PathBuf::from)
        })
        .expect("apply must report its backup directory");
    let manifest = fs::read_to_string(backup_dir.join("manifest.tsv")).unwrap();
    let canonical_target = fs::canonicalize(&target).unwrap();
    assert!(manifest.contains(&canonical_target.display().to_string()));
    assert_eq!(
        fs::read(backup_dir.join("0000.bak")).unwrap(),
        before_target
    );
    assert_ne!(fs::read(&target).unwrap(), before_target);
    assert_eq!(fs::read(&store).unwrap(), before_store);
    assert_file_unchanged(&raw_source, &raw_before, "apply raw fixture");

    let _ = fs::remove_dir_all(backup_dir);
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
    assert!(failures.starts_with("RECURRING FAILURES"));
    assert!(failures.contains("exit_code_1 / unknown_tool / cargo test"));
    assert!(failures.contains("action: "));
    let stuck = String::from_utf8_lossy(&run_args(&["stuck"], &store).stdout).into_owned();
    assert!(stuck.starts_with("STUCK WORK"));
    assert!(stuck.contains("sequence: "));
    let corrections =
        String::from_utf8_lossy(&run_args(&["corrections"], &store).stdout).into_owned();
    assert!(corrections.contains("correction="));

    let _ = std::fs::remove_file(store);
}

#[test]
fn typed_views_have_distinct_bounded_formats_and_json_envelopes() {
    let store = typed_view_store();
    for (command, heading, data_key) in [
        ("inventory", "CONFIGURATION INVENTORY", "rows"),
        ("overhead", "CONTEXT COST", "rows"),
        ("usage", "WHERE EFFORT GOES", "rows"),
        ("waste", "OPPORTUNITIES", "opportunities"),
        ("failures", "RECURRING FAILURES", "rows"),
        ("stuck", "STUCK WORK", "rows"),
        ("prompts", "HOW YOU STEER CODEX", "rows"),
    ] {
        let human = run_args(&[command], &store);
        assert!(human.status.success(), "{command}: {:?}", human);
        let stdout = String::from_utf8_lossy(&human.stdout);
        assert!(stdout.starts_with(heading), "{command}: {stdout}");
        assert!(stdout.len() < 20_000, "{command} is unbounded");
        if command == "inventory" {
            assert!(stdout.contains("owner: "), "{command}: {stdout}");
        }

        let markdown = run_args(&[command, "--format", "markdown"], &store);
        assert!(markdown.status.success(), "{command} markdown failed");
        assert!(
            String::from_utf8_lossy(&markdown.stdout).starts_with(&format!("# {heading}")),
            "{command} markdown has no heading"
        );

        let machine = run_args(&[command, "--format", "json"], &store);
        let document = parse_json_report(&machine, command);
        assert_eq!(document["scope"]["kind"], "all");
        assert!(document["coverage"].is_object());
        assert!(document["freshness"].is_object());
        assert!(
            document["data"][data_key].is_array(),
            "{command} data shape"
        );
        if command == "inventory" {
            assert!(document["data"][data_key][0]["owner"].is_string());
        }
        if command == "overhead" {
            assert!(
                stdout.contains("residual (system/tool): "),
                "{command}: {stdout}"
            );
            assert!(
                document["data"][data_key]
                    .as_array()
                    .expect("overhead rows")
                    .iter()
                    .all(|row| row["residual_source"].is_string())
            );
        }
    }
    let _ = fs::remove_file(store);
}

#[test]
fn overhead_marks_unknown_residual_source_in_human_and_json() {
    let store = minimal_store();

    let human = run_args(&["overhead"], &store);
    assert!(human.status.success());
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_stdout.contains("residual (unknown): unknown"),
        "{human_stdout}"
    );
    assert!(
        !human_stdout.contains("residual (system/tool): unknown"),
        "{human_stdout}"
    );

    let document = parse_json_report(
        &run_args(&["overhead", "--format", "json"], &store),
        "overhead",
    );
    let row = document["data"]["rows"]
        .as_array()
        .expect("overhead rows")
        .first()
        .expect("global overhead row");
    assert!(row["residual_bytes"].is_null());
    assert_eq!(row["residual_source"], "unknown");
    assert_eq!(row["unknown_cost"], true);

    let _ = fs::remove_file(store);
}

#[test]
fn command_contract_fixture_preserves_scopes_targets_and_evidence() {
    let store = command_contract_store();
    let privacy_marker = "contract-private-value";
    let mut headings = Vec::new();
    let assert_evidence = |row: &Value| {
        let evidence = row["evidence"].as_array().expect("evidence array");
        assert!(!evidence.is_empty());
        assert!(evidence.len() <= 3);
    };
    let assert_opportunity = |opportunity: &Value| {
        for field in [
            "id",
            "title",
            "scope",
            "target",
            "impact",
            "confidence",
            "occurrences",
            "distinct_sessions",
            "action",
            "evidence",
            "limitations",
        ] {
            assert!(
                !opportunity[field].is_null(),
                "missing {field}: {opportunity}"
            );
        }
        assert_evidence(opportunity);
    };
    let assert_actionable_order = |text: &str, label: &str| {
        let target = text.find("  target: ").expect("target field");
        let action = text.find("  action: ").expect("action field");
        let evidence = text.find("  evidence: ").expect("evidence field");
        assert!(
            target < action && action < evidence,
            "{label} fields reordered"
        );
    };

    for (command, heading, data_key) in [
        ("inventory", "CONFIGURATION INVENTORY", "rows"),
        ("overhead", "CONTEXT COST", "rows"),
        ("usage", "WHERE EFFORT GOES", "rows"),
        ("waste", "OPPORTUNITIES", "opportunities"),
        ("failures", "RECURRING FAILURES", "rows"),
        ("stuck", "STUCK WORK", "rows"),
        ("prompts", "HOW YOU STEER CODEX", "rows"),
    ] {
        let human = run_args(&[command], &store);
        assert!(human.status.success(), "{command}: {:?}", human);
        let stdout = String::from_utf8_lossy(&human.stdout);
        assert!(stdout.starts_with(heading), "{command}: {stdout}");
        assert!(
            !stdout.contains(privacy_marker),
            "{command} leaked private text"
        );
        assert!(headings.iter().all(|seen| *seen != heading));
        headings.push(heading);

        let markdown = run_args(&[command, "--format", "markdown"], &store);
        assert!(markdown.status.success(), "{command} Markdown failed");
        let markdown_stdout = String::from_utf8_lossy(&markdown.stdout);
        assert!(
            markdown_stdout.starts_with(&format!("# {heading}")),
            "{command} Markdown has no heading"
        );
        assert!(
            !markdown_stdout.contains(privacy_marker),
            "{command} leaked private text"
        );
        if command == "waste" || command == "failures" || command == "stuck" {
            for field in ["target: ", "action: ", "evidence: "] {
                assert!(stdout.contains(field), "{command} omitted {field}");
                assert!(
                    markdown_stdout.contains(field),
                    "{command} Markdown omitted {field}"
                );
            }
            assert_actionable_order(&stdout, command);
            assert_actionable_order(&markdown_stdout, &format!("{command} Markdown"));
        }

        let machine = run_args(&[command, "--format", "json"], &store);
        let repeat = run_args(&[command, "--format", "json"], &store);
        assert_eq!(
            machine.stdout, repeat.stdout,
            "{command} is not deterministic"
        );
        assert!(
            !String::from_utf8_lossy(&machine.stdout).contains(privacy_marker),
            "{command} leaked private text"
        );
        let document = parse_json_report(&machine, command);
        let rows = document["data"][data_key]
            .as_array()
            .expect("contract rows array");
        assert!(!rows.is_empty(), "{command} lost its typed rows");
        assert!(!document["data"]["groups"].is_array());
        for row in rows {
            let evidence_row = if command == "failures" || command == "stuck" {
                &row["opportunity"]
            } else {
                row
            };
            assert_evidence(evidence_row);
        }
        if command == "waste" {
            for opportunity in rows {
                assert_opportunity(opportunity);
            }
        }
        if command == "failures" || command == "stuck" {
            for row in rows {
                assert_opportunity(&row["opportunity"]);
            }
        }
    }

    let analyze_machine = run_args(&["analyze", "--format", "json"], &store);
    assert!(!String::from_utf8_lossy(&analyze_machine.stdout).contains(privacy_marker));
    let analyze = parse_json_report(&analyze_machine, "analyze");
    let groups = analyze["data"]["groups"].as_array().unwrap();
    assert!(!groups.is_empty());
    assert_eq!(groups[0]["scope"]["kind"], "global");
    assert!(groups.iter().skip(1).any(|group| {
        group["scope"]["kind"] == "project" && group["scope"]["value"] == "/fixture/project-a"
    }));
    for group in groups {
        for finding in group["findings"].as_array().unwrap() {
            for field in [
                "kind",
                "severity",
                "confidence",
                "scope",
                "key",
                "summary",
                "evidence",
                "occurrences",
                "distinct_sessions",
                "affected_paths",
                "observed_commands",
                "sequence",
                "suggested_action",
                "limitations",
                "verification_status",
                "heuristic",
            ] {
                assert!(
                    finding.get(field).is_some(),
                    "missing analyze field {field}"
                );
            }
            let evidence = finding["evidence"].as_array().unwrap();
            assert!(!evidence.is_empty());
            assert!(evidence.len() <= 12);
        }
    }
    let analyze_human = run_args(&["analyze"], &store);
    assert!(analyze_human.status.success());
    let analyze_stdout = String::from_utf8_lossy(&analyze_human.stdout);
    assert!(analyze_stdout.starts_with("Analyzed period:"));
    assert!(!analyze_stdout.contains(privacy_marker));
    for field in ["Finding counts:", "  action: ", "  evidence: "] {
        assert!(analyze_stdout.contains(field), "analyze omitted {field}");
    }
    assert!(
        analyze_stdout.find("  action: ").unwrap() < analyze_stdout.find("  evidence: ").unwrap(),
        "analyze fields reordered"
    );
    let analyze_markdown = run_args(&["analyze", "--format", "markdown"], &store);
    assert!(analyze_markdown.status.success());
    let analyze_markdown_stdout = String::from_utf8_lossy(&analyze_markdown.stdout);
    assert!(analyze_markdown_stdout.starts_with("# analyze\n\nAnalyzed period:"));
    assert!(!analyze_markdown_stdout.contains(privacy_marker));
    assert!(analyze_markdown_stdout.contains("  action: "));
    assert!(analyze_markdown_stdout.contains("  evidence: "));

    let inventory = parse_json_report(
        &run_args(&["inventory", "--format", "json"], &store),
        "inventory",
    );
    let inventory_rows = inventory["data"]["rows"].as_array().unwrap();
    assert!(inventory_rows.iter().any(|row| {
        row["name"] == "unused-skill"
            && row["usage_state"] == "unused"
            && row["action"]
                .as_str()
                .is_some_and(|action| action.starts_with("Remove"))
    }));
    assert!(inventory_rows.iter().any(|row| {
        row["name"] == "heavy-skill"
            && row["action"]
                .as_str()
                .is_some_and(|action| action.starts_with("Slim"))
    }));

    let overhead = parse_json_report(
        &run_args(&["overhead", "--format", "json"], &store),
        "overhead",
    );
    let overhead_rows = overhead["data"]["rows"].as_array().unwrap();
    assert_eq!(overhead_rows.len(), 3);
    assert_eq!(overhead_rows[0]["scope"]["kind"], "global");
    assert_eq!(overhead_rows[1]["project"], "/fixture/project-a");
    assert_eq!(overhead_rows[2]["project"], "/fixture/project-b");

    let waste = parse_json_report(&run_args(&["waste", "--format", "json"], &store), "waste");
    let opportunities = waste["data"]["opportunities"].as_array().unwrap();
    assert_eq!(opportunities[0]["id"], "stuck:src/lib.rs|loop");
    assert!(opportunities.iter().any(|opportunity| {
        opportunity["id"] == "surface:unused-skill"
            && opportunity["target"] == "/fixture/codex/skills/unused/SKILL.md"
            && opportunity["action"]
                .as_str()
                .is_some_and(|action| action.starts_with("Remove"))
    }));
    assert!(opportunities.iter().any(|opportunity| {
        opportunity["id"] == "surface:heavy-skill"
            && opportunity["target"] == "/fixture/codex/skills/heavy/SKILL.md"
            && opportunity["action"]
                .as_str()
                .is_some_and(|action| action.starts_with("Slim"))
    }));

    let failures = parse_json_report(
        &run_args(&["failures", "--format", "json"], &store),
        "failures",
    );
    let failure_rows = failures["data"]["rows"].as_array().unwrap();
    assert_eq!(failure_rows[0]["category"], "exit_code_1");
    assert_eq!(failure_rows[1]["category"], "command_not_found");
    assert!(failure_rows.iter().any(|row| {
        row["category"] == "command_not_found" && row["opportunity"]["scope"]["kind"] == "global"
    }));
    assert!(failure_rows.iter().any(|row| {
        row["category"] == "exit_code_1" && row["opportunity"]["scope"]["kind"] == "project"
    }));

    let stuck = parse_json_report(&run_args(&["stuck", "--format", "json"], &store), "stuck");
    let stuck_rows = stuck["data"]["rows"].as_array().unwrap();
    assert!(stuck_rows.iter().any(|row| {
        row["path"] == "src/lib.rs"
            && row["sequence"]
                .as_array()
                .is_some_and(|sequence| sequence.len() >= 4)
    }));

    let doctor_machine = run_args(&["doctor", "--format", "json"], &store);
    assert!(!String::from_utf8_lossy(&doctor_machine.stdout).contains(privacy_marker));
    let doctor = parse_json_report(&doctor_machine, "doctor");
    let top_fixes = doctor["data"]["top_fixes"].as_array().unwrap();
    assert!(!top_fixes.is_empty());
    assert!(
        top_fixes
            .iter()
            .any(|opportunity| opportunity["id"] == "stuck:src/lib.rs|loop")
    );
    assert!(
        top_fixes
            .iter()
            .any(|opportunity| opportunity["id"] == "overhead:global")
    );
    for opportunity in top_fixes {
        assert_opportunity(opportunity);
    }
    assert!(
        top_fixes
            .iter()
            .any(|opportunity| opportunity["scope"]["kind"] == "global")
    );
    assert!(
        top_fixes
            .iter()
            .any(|opportunity| opportunity["scope"]["kind"] == "project")
    );

    let doctor_human = run_args(&["doctor"], &store);
    assert!(doctor_human.status.success());
    let doctor_stdout = String::from_utf8_lossy(&doctor_human.stdout);
    assert!(doctor_stdout.starts_with("WHAT TO FIX FIRST"));
    assert!(!doctor_stdout.contains(privacy_marker));
    assert_actionable_order(&doctor_stdout, "doctor");
    let doctor_markdown = run_args(&["doctor", "--format", "markdown"], &store);
    assert!(doctor_markdown.status.success());
    let doctor_markdown_stdout = String::from_utf8_lossy(&doctor_markdown.stdout);
    assert!(doctor_markdown_stdout.starts_with("# WHAT TO FIX FIRST"));
    assert!(!doctor_markdown_stdout.contains(privacy_marker));
    assert_actionable_order(&doctor_markdown_stdout, "doctor Markdown");

    let optimize_machine = run_args(&["optimize", "--print", "--format", "json"], &store);
    assert!(optimize_machine.status.success());
    assert!(!String::from_utf8_lossy(&optimize_machine.stdout).contains(privacy_marker));
    let optimize = parse_json_report(&optimize_machine, "optimize");
    for finding in optimize["data"]["findings"].as_array().unwrap() {
        for field in ["target", "action", "evidence"] {
            assert!(!finding[field].is_null(), "missing optimize field {field}");
        }
        assert!(finding["evidence"].as_array().unwrap().len() <= 3);
    }
    let configuration_waste = optimize["data"]["configuration_waste"].as_array().unwrap();
    assert!(!configuration_waste.is_empty());
    for opportunity in configuration_waste {
        assert_opportunity(opportunity);
    }

    let optimize_human = run_args(&["optimize", "--print"], &store);
    assert!(optimize_human.status.success());
    let optimize_stdout = String::from_utf8_lossy(&optimize_human.stdout);
    assert!(optimize_stdout.starts_with("OPTIMIZATION BRIEFING"));
    assert!(!optimize_stdout.contains(privacy_marker));
    assert_actionable_order(&optimize_stdout, "optimize");
    let optimize_sections = [
        "\nFINDINGS\n",
        "\nCONFIGURATION WASTE\n",
        "\nOVERHEAD\n",
        "\nREVIEWABLE PROPOSALS\n",
        "\nNEXT WORKFLOW\n",
    ];
    for pair in optimize_sections.windows(2) {
        assert!(
            optimize_stdout.find(pair[0]).unwrap() < optimize_stdout.find(pair[1]).unwrap(),
            "optimize sections reordered"
        );
    }
    let optimize_markdown = run_args(&["optimize", "--print", "--format", "markdown"], &store);
    assert!(optimize_markdown.status.success());
    let optimize_markdown_stdout = String::from_utf8_lossy(&optimize_markdown.stdout);
    assert!(optimize_markdown_stdout.starts_with("# OPTIMIZE\n\nOPTIMIZATION BRIEFING"));
    assert!(!optimize_markdown_stdout.contains(privacy_marker));
    assert_actionable_order(&optimize_markdown_stdout, "optimize Markdown");

    for command in ["sql", "query"] {
        let output = if command == "sql" {
            run_sql(
                &[
                    "SELECT COUNT(*) AS sessions FROM sessions",
                    "--format",
                    "json",
                ],
                &store,
                None,
            )
        } else {
            run_query(
                &[
                    "SELECT COUNT(*) AS sessions FROM sessions",
                    "--format",
                    "json",
                ],
                &store,
                None,
            )
        };
        assert!(!String::from_utf8_lossy(&output.stdout).contains(privacy_marker));
        let document = parse_json_report(&output, command);
        assert_eq!(document["data"]["columns"][0], "sessions");
        assert!(document["data"]["rows"].is_array());
        assert!(document["freshness"].is_null());
        assert!(document["coverage"].is_null());
    }

    let _ = fs::remove_file(store);
}

#[test]
fn command_contract_finding_stays_consistent_across_cli_chain() {
    let store = command_contract_store();
    let analyze = parse_json_report(
        &run_args(&["analyze", "--format", "json"], &store),
        "analyze",
    );
    let analyze_finding = analyze["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["findings"].as_array().unwrap())
        .find(|finding| finding["kind"] == "stuck" && finding["key"] == "src/lib.rs|loop")
        .expect("stable stuck finding in analyze");

    let waste = parse_json_report(&run_args(&["waste", "--format", "json"], &store), "waste");
    let waste_finding = waste["data"]["opportunities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == "stuck:src/lib.rs|loop")
        .expect("same stable finding in waste");

    let doctor = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    let doctor_finding = doctor["data"]["top_fixes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == waste_finding["id"])
        .expect("same stable finding in doctor");

    let optimize = parse_json_report(
        &run_args(&["optimize", "--print", "--format", "json"], &store),
        "optimize",
    );
    let optimize_finding = optimize["data"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| {
            finding["kind"] == analyze_finding["kind"]
                && finding["key"] == analyze_finding["key"]
                && finding["scope"] == analyze_finding["scope"]
                && finding["target"] == waste_finding["target"]
        })
        .expect("same stable finding in optimize");
    let optimize_opportunity = optimize["data"]["configuration_waste"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == waste_finding["id"])
        .expect("same stable opportunity in optimize");

    assert_eq!(analyze_finding["occurrences"], 4);
    assert_eq!(analyze_finding["distinct_sessions"], 1);
    assert_eq!(analyze_finding["affected_paths"][0], "src/lib.rs");
    assert_eq!(waste_finding["id"], "stuck:src/lib.rs|loop");
    assert_eq!(waste_finding["scope"], analyze_finding["scope"]);

    let analyze_evidence = analyze_finding["evidence"].as_array().unwrap();
    assert!(!analyze_evidence.is_empty());
    let expected_evidence =
        serde_json::Value::Array(analyze_evidence.iter().take(3).cloned().collect());
    assert_eq!(expected_evidence.as_array().unwrap().len(), 3);
    for (command, finding) in [
        ("waste", waste_finding),
        ("doctor", doctor_finding),
        ("optimize", optimize_finding),
        ("optimize waste", optimize_opportunity),
    ] {
        assert_eq!(
            finding["scope"], analyze_finding["scope"],
            "{command} scope"
        );
        assert_eq!(finding["target"], "src/lib.rs", "{command} target");
        assert_eq!(
            finding["action"], analyze_finding["suggested_action"],
            "{command} action"
        );
        assert_eq!(finding["occurrences"], analyze_finding["occurrences"]);
        assert_eq!(
            finding["distinct_sessions"],
            analyze_finding["distinct_sessions"]
        );
        assert_eq!(finding["evidence"], expected_evidence, "{command} evidence");
    }
    assert_eq!(optimize_finding["problem"], analyze_finding["summary"]);
    assert_eq!(optimize_opportunity["id"], waste_finding["id"]);

    let failure_finding = analyze["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["findings"].as_array().unwrap())
        .find(|finding| {
            finding["kind"] == "failure" && finding["scope"]["value"] == "/fixture/project-a"
        })
        .expect("synthetic project failure finding");
    let failures = parse_json_report(
        &run_args(&["failures", "--format", "json"], &store),
        "failures",
    );
    let failure_opportunity = failures["data"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["category"] == "exit_code_1"
                && row["opportunity"]["scope"] == failure_finding["scope"]
        })
        .map(|row| &row["opportunity"])
        .expect("same canonical action in failures");
    assert_eq!(
        failure_opportunity["action"],
        failure_finding["suggested_action"]
    );

    let _ = fs::remove_file(store);
}

#[test]
fn command_contract_fixture_covers_empty_and_partial_reports() {
    let commands: &[(&[&str], &str)] = &[
        (&["analyze", "--format", "json"], "analyze"),
        (&["sessions", "--format", "json"], "sessions"),
        (&["usage", "--format", "json"], "usage"),
        (&["inventory", "--format", "json"], "inventory"),
        (&["waste", "--format", "json"], "waste"),
        (&["overhead", "--format", "json"], "overhead"),
        (&["prompts", "--format", "json"], "prompts"),
        (&["failures", "--format", "json"], "failures"),
        (&["stuck", "--format", "json"], "stuck"),
        (&["doctor", "--format", "json"], "doctor"),
        (&["optimize", "--print", "--format", "json"], "optimize"),
    ];
    let period_flags = [
        "--since",
        "2026-01-01T00:00:00Z",
        "--until",
        "2027-01-01T00:00:00Z",
    ];

    for (store, expected_status) in [
        (empty_store(), "empty"),
        (coverage_limitation_store(), "partial"),
    ] {
        for (args, command) in commands {
            let document = parse_json_report(&run_args(args, &store), command);
            let coverage = if *command == "analyze" {
                &document["data"]["coverage"]
            } else {
                &document["coverage"]
            };
            assert_eq!(coverage["status"], expected_status, "{command}");
            if expected_status == "partial" {
                assert!(!coverage["limitations"].as_array().unwrap().is_empty());

                let mut human_args = vec![*command];
                if *command == "optimize" {
                    human_args.push("--print");
                }
                let human = run_args(&human_args, &store);
                assert!(
                    human.status.success(),
                    "{command} human report failed: {}",
                    String::from_utf8_lossy(&human.stderr)
                );
                let human_stdout = String::from_utf8_lossy(&human.stdout);
                let coverage_summary = if *command == "analyze" {
                    "Coverage: selected store (partial"
                } else {
                    "Coverage: partial"
                };
                assert!(
                    human_stdout.contains(coverage_summary),
                    "{command} omitted partial coverage: {human_stdout}"
                );
                assert!(
                    human_stdout.contains("Limitations:"),
                    "{command} omitted coverage limitations: {human_stdout}"
                );
                assert_human_limitation_details(&human_stdout, coverage);
            }
        }

        let optimize_diff = parse_json_report(
            &run_args_with_flags(
                &["optimize", "--diff", "--format", "json"],
                &period_flags,
                &store,
            ),
            "optimize_diff",
        );
        assert_eq!(
            optimize_diff["data"]["coverage"]["status"], expected_status,
            "optimize --diff"
        );
        if expected_status == "partial" {
            let human = run_args_with_flags(&["optimize", "--diff"], &period_flags, &store);
            assert!(
                human.status.success(),
                "optimize --diff human report failed: {}",
                String::from_utf8_lossy(&human.stderr)
            );
            let human_stdout = String::from_utf8_lossy(&human.stdout);
            assert!(
                human_stdout.contains("Coverage: selected store (partial"),
                "optimize --diff omitted partial coverage: {human_stdout}"
            );
            assert!(
                human_stdout.contains("Limitations:"),
                "optimize --diff omitted coverage limitations: {human_stdout}"
            );
            assert_human_limitation_details(&human_stdout, &optimize_diff["data"]["coverage"]);
        }

        for command in ["sql", "query"] {
            let output = if command == "sql" {
                run_sql(
                    &[
                        "SELECT COUNT(*) AS records FROM records",
                        "--format",
                        "json",
                    ],
                    &store,
                    None,
                )
            } else {
                run_query(
                    &[
                        "SELECT COUNT(*) AS records FROM records",
                        "--format",
                        "json",
                    ],
                    &store,
                    None,
                )
            };
            let document = parse_json_report(&output, command);
            assert_eq!(document["data"]["columns"][0], "records");
            let count = document["data"]["rows"][0][0].as_u64().unwrap();
            if expected_status == "empty" {
                assert_eq!(count, 0, "{command} empty result");
            } else {
                assert!(count > 0, "{command} partial result");
            }
            assert!(document["coverage"].is_null());
            assert!(document["freshness"].is_null());
        }
        let _ = fs::remove_file(store);
    }
}

#[test]
fn first_run_analyze_refreshes_and_uses_a_private_default_store() {
    let (home, _) = refresh_home();
    fs::write(home.join("config.toml"), "model = \"synthetic-model\"\n").unwrap();
    let state_home = temp_store_path("first-run-state").with_extension("state");
    let expected_store = state_home.join("codexlens/codexlens.db");
    let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["analyze", "--codex-home"])
        .arg(&home)
        .env("XDG_STATE_HOME", &state_home)
        .env_remove("HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Finding counts:"), "{stdout}");
    assert!(stderr.contains("Refreshed store:"));
    assert!(expected_store.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(expected_store.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    let machine = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["doctor", "--format", "json", "--frozen"])
        .arg("--codex-home")
        .arg(&home)
        .env("XDG_STATE_HOME", &state_home)
        .env_remove("HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let document = parse_json_report(&machine, "doctor");
    assert_eq!(document["data"]["looks_healthy"], false);
    assert!(machine.stderr.is_empty());

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_dir_all(state_home);
}

#[test]
fn analyze_refresh_keeps_progress_off_json_stdout_and_frozen_is_store_only() {
    let (home, source) = refresh_home();
    let store = temp_store_path("analyze-json-boundary");
    let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["analyze", "--codex-home"])
        .arg(&home)
        .args(["--store"])
        .arg(&store)
        .args(["--format", "json"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["command"], "analyze");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Refreshed store:"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Refreshed store:"));

    let before = fs::read(&store).unwrap();
    fs::write(&source, b"synthetic raw input changed after analyze\n").unwrap();
    let frozen = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["analyze", "--store"])
        .arg(&store)
        .args(["--format", "json", "--frozen"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        frozen.status.success(),
        "{}",
        String::from_utf8_lossy(&frozen.stderr)
    );
    let frozen_document: Value = serde_json::from_slice(&frozen.stdout).unwrap();
    assert_eq!(frozen_document["command"], "analyze");
    assert!(frozen.stderr.is_empty());
    assert_eq!(fs::read(&store).unwrap(), before);

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn analyze_validates_reporting_filters_before_refreshing_the_store() {
    let (home, _) = refresh_home();
    let store = fixture_store();
    let before = fs::read(&store).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["analyze", "--codex-home"])
        .arg(&home)
        .args(["--store"])
        .arg(&store)
        .args(["--since", "not-a-timestamp"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reporting period"));
    assert_eq!(fs::read(&store).unwrap(), before);

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn doctor_does_not_report_opaque_renderer_payload_as_a_wrapper_failure() {
    let home = temp_store_path("renderer-doctor-home");
    let session_directory = home.join("sessions").join("2026");
    fs::create_dir_all(&session_directory).unwrap();
    let source = session_directory.join("renderer-doctor.jsonl");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rollout/renderer-doctor.jsonl");
    fs::copy(fixture, &source).unwrap();
    let store = temp_store_path("renderer-doctor-store");

    let refreshed = run_refresh(&home, &store);
    assert!(
        refreshed.status.success(),
        "refresh failed: {}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let data = Store::open_read_only(&store)
        .unwrap()
        .load_canonical()
        .unwrap();
    assert_eq!(data.tool_results.len(), 2);
    assert!(data.tool_results.iter().all(|result| {
        result.outcome == codexlens::model::ToolOutcome::Unknown
            && result.outcome_source == codexlens::model::OutcomeSource::Unknown
    }));

    let doctor = run_args(&["doctor", "--format", "json"], &store);
    let document = parse_json_report(&doctor, "doctor");
    assert_eq!(document["data"]["top_fixes"].as_array().unwrap().len(), 0);
    assert_eq!(document["data"]["looks_healthy"], false);
    assert_eq!(document["data"]["analysis_sufficient"], false);

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn typed_views_honor_scope_and_archive_subagent_selection() {
    let store = typed_view_store();
    let global = run_args(&["inventory", "--scope", "global"], &store);
    assert!(global.status.success());
    let global_stdout = String::from_utf8_lossy(&global.stdout);
    assert!(global_stdout.contains("global-config"));
    assert!(!global_stdout.contains("AGENTS.md"));

    let project = run_args(
        &["inventory", "--scope", "project:/fixture/project"],
        &store,
    );
    assert!(project.status.success());
    let project_stdout = String::from_utf8_lossy(&project.stdout);
    assert!(project_stdout.contains("AGENTS.md"));
    assert!(!project_stdout.contains("global-config"));

    let project_usage = run_args(
        &[
            "inventory",
            "--scope",
            "project:/fixture/project",
            "--format",
            "json",
        ],
        &store,
    );
    let project_document = parse_json_report(&project_usage, "inventory");
    let project_surface = project_document["data"]["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["id"] == "project-used-instruction")
        })
        .expect("project surface row");
    assert_eq!(project_surface["observed_uses"], 0);
    assert_eq!(project_surface["usage_state"], "unused");

    let selection_store = fixture_store();
    {
        let store_handle = Store::open(&selection_store).unwrap();
        store_handle
            .connection()
            .execute(
                "UPDATE sessions SET archive_state = 1 WHERE session_id = 'fixture-analysis-session-a'",
                [],
            )
            .unwrap();
        store_handle
            .connection()
            .execute(
                "UPDATE sessions SET parent_id = 'fixture-analysis-session-a' WHERE session_id = 'fixture-analysis-session-b'",
                [],
            )
            .unwrap();
    }
    let normal = run_args(&["sessions"], &selection_store);
    assert!(normal.status.success());
    assert!(String::from_utf8_lossy(&normal.stdout).contains("No selected sessions."));

    let archived = run_args(&["sessions", "--include-archived"], &selection_store);
    let archived_stdout = String::from_utf8_lossy(&archived.stdout);
    assert!(archived.status.success());
    assert!(archived_stdout.contains("fixture-analysis-session-a"));
    assert!(!archived_stdout.contains("fixture-analysis-session-b"));

    let subagents = run_args(&["sessions", "--include-subagents"], &selection_store);
    let subagents_stdout = String::from_utf8_lossy(&subagents.stdout);
    assert!(subagents.status.success());
    assert!(!subagents_stdout.contains("fixture-analysis-session-a"));
    assert!(subagents_stdout.contains("fixture-analysis-session-b"));

    let both = run_args(
        &["sessions", "--include-archived", "--include-subagents"],
        &selection_store,
    );
    let both_stdout = String::from_utf8_lossy(&both.stdout);
    assert!(both.status.success());
    assert!(both_stdout.contains("fixture-analysis-session-a"));
    assert!(both_stdout.contains("fixture-analysis-session-b"));

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(selection_store);
}

#[test]
fn typed_views_report_missing_stores_and_reject_relative_codex_homes() {
    let missing = temp_store_path("typed-missing");
    for command in [
        "inventory",
        "overhead",
        "usage",
        "waste",
        "failures",
        "stuck",
        "prompts",
        "sessions",
    ] {
        let output = run_args(&[command], &missing);
        assert!(!output.status.success(), "{command} unexpectedly succeeded");
        assert!(String::from_utf8_lossy(&output.stderr).contains("store does not exist"));
    }

    let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["analyze", "--codex-home", "relative-codex-home"])
        .arg("--store")
        .arg(&missing)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be an absolute directory"));
}

#[test]
fn sessions_report_applies_default_window_and_subagent_opt_in() {
    let store = session_selection_store();

    let default = run_args(&["sessions"], &store);
    assert!(default.status.success());
    let default_stdout = String::from_utf8_lossy(&default.stdout);
    assert!(default_stdout.contains("main-recent"));
    assert!(!default_stdout.contains("child-recent"));
    assert!(!default_stdout.contains("main-old"));
    assert!(default_stdout.contains("Omitted sessions: 0"));

    let with_children = run_args_with_flags(&["sessions"], &["--include-subagents"], &store);
    assert!(with_children.status.success());
    let with_children_stdout = String::from_utf8_lossy(&with_children.stdout);
    assert!(with_children_stdout.contains("main-recent"));
    assert!(with_children_stdout.contains("child-recent"));
    assert!(!with_children_stdout.contains("archived-recent"));
    assert!(!with_children_stdout.contains("main-old"));

    let with_archived = run_args_with_flags(&["sessions"], &["--include-archived"], &store);
    assert!(with_archived.status.success());
    let with_archived_stdout = String::from_utf8_lossy(&with_archived.stdout);
    assert!(with_archived_stdout.contains("main-recent"));
    assert!(with_archived_stdout.contains("archived-recent"));
    assert!(!with_archived_stdout.contains("child-recent"));

    let with_all = run_args_with_flags(
        &["sessions"],
        &["--include-subagents", "--include-archived"],
        &store,
    );
    assert!(with_all.status.success());
    let with_all_stdout = String::from_utf8_lossy(&with_all.stdout);
    assert!(with_all_stdout.contains("main-recent"));
    assert!(with_all_stdout.contains("child-recent"));
    assert!(with_all_stdout.contains("archived-recent"));

    {
        let store_with_older_session = Store::open(&store).unwrap();
        store_with_older_session
            .connection()
            .execute(
                "INSERT INTO sessions (session_id, source_identity, source_path, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "days-only-old",
                    "synthetic-days-source",
                    "days.jsonl",
                    "2026-01-10T00:00:00Z",
                    "2026-01-10T00:00:00Z",
                ],
            )
            .unwrap();
    }
    let with_days = run_args_with_flags(
        &["sessions"],
        &["--days", "7", "--include-subagents", "--include-archived"],
        &store,
    );
    assert!(with_days.status.success());
    let with_days_stdout = String::from_utf8_lossy(&with_days.stdout);
    assert!(with_days_stdout.contains("main-recent"));
    assert!(with_days_stdout.contains("child-recent"));
    assert!(with_days_stdout.contains("archived-recent"));
    assert!(!with_days_stdout.contains("days-only-old"));

    {
        let store_with_archived_session = Store::open(&store).unwrap();
        store_with_archived_session
            .connection()
            .execute(
                "INSERT INTO sessions (session_id, source_identity, source_path, created_at, updated_at, archive_state) VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                params![
                    "archived-only-period",
                    "synthetic-archived-period-source",
                    "archived-period.jsonl",
                    "2026-02-01T00:00:00Z",
                    "2026-02-01T00:00:00Z",
                ],
            )
            .unwrap();
    }
    let explicit_period = parse_json_report(
        &run_args(
            &[
                "sessions",
                "--format",
                "json",
                "--since",
                "2026-02-01T00:00:00Z",
                "--until",
                "2026-02-02T00:00:00Z",
            ],
            &store,
        ),
        "sessions",
    );
    assert_eq!(explicit_period["coverage"]["session_count"], 0);
    assert!(explicit_period["coverage"]["activity_start"].is_null());
    assert!(explicit_period["coverage"]["activity_end"].is_null());

    let conflicting = run_args_with_flags(
        &["sessions", "--since", "2026-01-01T00:00:00Z"],
        &["--days", "7"],
        &store,
    );
    assert!(!conflicting.status.success());
    assert!(
        String::from_utf8_lossy(&conflicting.stderr)
            .contains("--days cannot be combined with --since or --until")
    );

    let bounded_store = session_selection_store();
    {
        let bounded = Store::open(&bounded_store).unwrap();
        for index in 0..51 {
            let id = format!("bounded-{index:02}");
            bounded
                .connection()
                .execute(
                    "INSERT INTO sessions (session_id, source_identity, source_path, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id,
                        "synthetic-bounded-source",
                        "bounded.jsonl",
                        "2026-01-30T00:00:00Z",
                        "2026-01-30T00:00:00Z",
                    ],
                )
                .unwrap();
            bounded
                .connection()
                .execute(
                    "INSERT INTO records (record_key, source_identity, source_path, source_line, session_id, timestamp, sequence, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        format!("bounded-record-{index:02}"),
                        "synthetic-bounded-source",
                        "bounded.jsonl",
                        index + 1,
                        id,
                        "2026-01-30T00:00:00Z",
                        index,
                        "response_item",
                    ],
                )
                .unwrap();
        }
    }
    let bounded = run_args_with_flags(
        &["sessions"],
        &["--days", "1", "--include-subagents", "--include-archived"],
        &bounded_store,
    );
    assert!(bounded.status.success());
    let bounded_stdout = String::from_utf8_lossy(&bounded.stdout);
    assert_eq!(
        bounded_stdout
            .lines()
            .filter(|line| line.starts_with("- "))
            .count(),
        50
    );
    assert!(bounded_stdout.contains("Omitted sessions: 3"));

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(bounded_store);
}

#[test]
fn reporting_period_filter_is_half_open_and_visible_in_human_and_json() {
    let store = fixture_store();
    let human = run_args(
        &[
            "sessions",
            "--since",
            "2026-01-03T00:00:00Z",
            "--until",
            "2026-01-04T00:00:00Z",
        ],
        &store,
    );
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("Requested period: [2026-01-03T00:00:00.000Z, 2026-01-04T00:00:00.000Z)")
    );
    assert!(
        stdout.contains("Coverage: partial (1 sessions)"),
        "{stdout}"
    );
    assert!(stdout.contains("fixture-analysis-session-a"), "{stdout}");
    assert!(!stdout.contains("fixture-analysis-session-b"), "{stdout}");
    assert!(stdout.contains("Observed records: "), "{stdout}");
    assert!(stdout.contains("Coverage: "), "{stdout}");

    let utc = run_args(
        &[
            "analyze",
            "--format",
            "json",
            "--since",
            "2026-01-03T00:00:00Z",
            "--until",
            "2026-01-04T00:00:00Z",
        ],
        &store,
    );
    let offset = run_args(
        &[
            "analyze",
            "--format",
            "json",
            "--since",
            "2026-01-03T09:00:00+09:00",
            "--until",
            "2026-01-04T09:00:00+09:00",
        ],
        &store,
    );
    assert_eq!(utc.stdout, offset.stdout, "equivalent instants must match");
    let document = parse_json_report(&utc, "analyze");
    let coverage = &document["data"]["coverage"];
    assert_eq!(coverage["requested_start"], "2026-01-03T00:00:00.000Z");
    assert_eq!(coverage["requested_end"], "2026-01-04T00:00:00.000Z");
    assert_eq!(coverage["included_sessions"], 1);
    assert!(coverage["included_records"].as_u64().unwrap() > 0);
    assert!(coverage["excluded_records"].as_u64().unwrap() > 0);
    assert_eq!(coverage["unknown_timestamp_records"], 0);

    let _ = fs::remove_file(store);
}

#[test]
fn filtered_coverage_excludes_trimmed_boundary_turn_timestamps() {
    let store = boundary_turn_coverage_store();
    let flags = [
        "--since",
        "2026-01-03T00:00:00Z",
        "--until",
        "2026-01-04T00:00:00Z",
    ];

    let human = run_args_with_flags(&["sessions"], &flags, &store);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout
            .contains("Observed records: 3 (excluded: 2, unknown timestamps: 0 records, 0 events)")
    );
    assert!(stdout.contains("Period coverage: complete"));
    assert!(stdout.contains("Observed records: 3 (excluded: 2"));
    assert!(stdout.contains("Coverage: observed (1 sessions)"));

    for args in REPORTING_COMMANDS {
        let output = run_args_with_flags(args, &flags, &store);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        if matches!(args[0], "sessions" | "failures" | "stuck" | "doctor") {
            assert!(
                stdout.contains("Period coverage: complete"),
                "{args:?}: {stdout}"
            );
            assert!(
                stdout.contains("Observed records: 3 (excluded: 2"),
                "{args:?}: {stdout}"
            );
        } else {
            assert!(stdout.contains("Coverage: complete"), "{args:?}: {stdout}");
            assert!(
                stdout.contains("Coverage: selected store (observed;"),
                "{args:?}: {stdout}"
            );
            assert!(
                stdout.contains("Activity timestamps: 6 valid, 0 missing, 0 invalid"),
                "{args:?}: {stdout}"
            );
            assert!(stdout.contains("Sessions: 1"), "{args:?}: {stdout}");
            assert!(stdout.contains("Records: 3"), "{args:?}: {stdout}");
        }

        let mut json_args = args.to_vec();
        json_args.extend(["--format", "json"]);
        let command = match args[0] {
            "optimize" => "optimize_diff",
            "rediscovery" => "knowledge",
            command => command,
        };
        let document = parse_json_report(&run_args_with_flags(&json_args, &flags, &store), command);
        let coverage = if matches!(args[0], "sessions" | "failures" | "stuck" | "doctor") {
            &document["coverage"]
        } else {
            &document["data"]["coverage"]
        };
        if matches!(args[0], "sessions" | "failures" | "stuck" | "doctor") {
            assert_eq!(coverage["status"], "observed", "{args:?}");
            assert_eq!(coverage["period_status"], "complete", "{args:?}");
            assert_eq!(coverage["included_records"], 3, "{args:?}");
            assert_eq!(coverage["excluded_records"], 2, "{args:?}");
        } else {
            assert_eq!(coverage["status"], "observed", "{args:?}");
            assert_eq!(coverage["state"], "complete", "{args:?}");
            assert_eq!(coverage["missing_activity_timestamps"], 0, "{args:?}");
            assert_eq!(coverage["invalid_activity_timestamps"], 0, "{args:?}");
            assert_eq!(coverage["valid_activity_timestamps"], 6, "{args:?}");
            assert_eq!(coverage["unknown_timestamp_events"], 0, "{args:?}");
            assert_eq!(coverage["included_sessions"], 1, "{args:?}");
            assert_eq!(coverage["included_records"], 3, "{args:?}");
        }
    }

    let _ = fs::remove_file(store);
}

#[test]
fn empty_reporting_period_has_no_selected_activity() {
    let store = fixture_store();
    let output = run_args(
        &[
            "sessions",
            "--since",
            "2026-01-03T00:00:00Z",
            "--until",
            "2026-01-03T00:00:00Z",
        ],
        &store,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Coverage: empty (0 sessions)"), "{stdout}");
    assert!(!stdout.contains("fixture-analysis-session-"), "{stdout}");
    assert!(stdout.contains("Coverage: empty"), "{stdout}");
    let _ = fs::remove_file(store);
}

#[test]
fn monitor_help_omits_reporting_period_filters() {
    let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(["monitor", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("--since"), "{stdout}");
    assert!(!stdout.contains("--until"), "{stdout}");
}

#[test]
fn reporting_period_rejects_invalid_and_reversed_bounds() {
    let store = empty_store();
    for args in [
        &[
            "doctor",
            "--since",
            "not-a-timestamp",
            "--until",
            "2026-01-04T00:00:00Z",
        ][..],
        &[
            "doctor",
            "--since",
            "2026-01-04T00:00:00Z",
            "--until",
            "2026-01-03T00:00:00Z",
        ][..],
    ] {
        let output = run_args(args, &store);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("reporting period"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.len() < 512, "{args:?} error is unbounded");
    }
    let monitor = run_args_with_flags(
        &["monitor", "--source", "synthetic.jsonl"],
        &["--since", "2026-01-03T00:00:00Z"],
        &store,
    );
    assert!(!monitor.status.success());
    let monitor_stderr = String::from_utf8_lossy(&monitor.stderr);
    assert!(
        monitor_stderr
            .contains("--since and --until are reporting filters; monitor does not accept them"),
        "{monitor_stderr}"
    );
    let _ = fs::remove_file(store);
}

#[test]
fn reporting_coverage_marks_unknown_event_timestamps_as_partial() {
    let store = fixture_store();
    Store::open(&store)
        .unwrap()
        .connection()
        .execute("UPDATE records SET timestamp = NULL WHERE sequence = 1", [])
        .unwrap();
    Store::open(&store)
        .unwrap()
        .connection()
        .execute(
            "UPDATE messages SET timestamp = 'invalid-event-timestamp' WHERE timestamp IS NOT NULL",
            [],
        )
        .unwrap();

    let output = run_args_with_flags(
        &["doctor", "--format", "json"],
        &[
            "--since",
            "2026-01-03T00:00:00Z",
            "--until",
            "2026-01-04T00:00:00Z",
        ],
        &store,
    );
    let document = parse_json_report(&output, "doctor");
    let coverage = &document["coverage"];
    assert_eq!(coverage["unknown_timestamp_records"], 1);
    assert!(coverage["unknown_timestamp_events"].as_u64().unwrap() > 0);
    assert!(coverage["missing_activity_timestamps"].as_u64().unwrap() > 0);
    assert!(coverage["invalid_activity_timestamps"].as_u64().unwrap() > 0);
    assert_eq!(coverage["status"], "partial");
    assert!(coverage["activity_start"].is_string());
    assert!(coverage["activity_end"].is_string());

    let _ = fs::remove_file(store);
}

#[test]
fn filtered_coverage_resolves_missing_event_timestamps_from_source_records() {
    let store = coverage_timestamp_fallback_store();
    let flags = [
        "--since",
        "2026-01-03T00:00:00Z",
        "--until",
        "2026-01-04T00:00:00Z",
    ];

    let human = run_args_with_flags(&["analyze"], &flags, &store);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_stdout.contains("Activity timestamps: 14 valid, 0 missing, 0 invalid"),
        "{human_stdout}"
    );

    let mut json_args = vec!["analyze", "--format", "json"];
    json_args.extend(flags);
    let document = parse_json_report(&run_args(&json_args, &store), "analyze");
    let coverage = &document["data"]["coverage"];
    assert_eq!(coverage["status"], "observed");
    assert_eq!(coverage["state"], "complete");
    assert_eq!(coverage["missing_activity_timestamps"], 0);
    assert_eq!(coverage["invalid_activity_timestamps"], 0);
    assert_eq!(coverage["unknown_timestamp_events"], 0);

    let _ = fs::remove_file(store);
}

#[test]
fn filtered_coverage_preserves_invalid_event_timestamps() {
    let store = coverage_timestamp_fallback_store();
    Store::open(&store)
        .unwrap()
        .connection()
        .execute(
            "UPDATE messages SET timestamp = 'invalid-event-timestamp' WHERE timestamp IS NULL",
            [],
        )
        .unwrap();

    let json_args = [
        "analyze",
        "--format",
        "json",
        "--since",
        "2026-01-03T00:00:00Z",
        "--until",
        "2026-01-04T00:00:00Z",
    ];
    let document = parse_json_report(&run_args(&json_args, &store), "analyze");
    let coverage = &document["data"]["coverage"];
    assert_eq!(coverage["status"], "partial");
    assert_eq!(coverage["state"], "partial");
    assert_eq!(coverage["missing_activity_timestamps"], 0);
    assert!(coverage["invalid_activity_timestamps"].as_u64().unwrap() > 0);
    assert!(
        coverage["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["kind"] == "invalid_timestamp"
                    && limitation["source"]["line"].is_number()
            })
    );

    let _ = fs::remove_file(store);
}

#[test]
fn filtered_coverage_ignores_out_of_period_unknowns_and_diagnostics() {
    let store = filtered_coverage_period_store();
    let json_args = [
        "analyze",
        "--format",
        "json",
        "--since",
        "2026-01-03T00:00:00Z",
        "--until",
        "2026-01-04T00:00:00Z",
    ];

    let output = run_args(&json_args, &store);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = parse_json_report(&output, "analyze");
    let coverage = &document["data"]["coverage"];
    assert_eq!(coverage["status"], "observed");
    assert_eq!(coverage["included_sessions"], 1);
    assert_eq!(coverage["included_records"], 5);
    assert_eq!(coverage["invalid_activity_timestamps"], 0);
    assert_eq!(coverage["missing_activity_timestamps"], 0);
    assert_eq!(coverage["limitations"], json!([]));
    assert_eq!(coverage["unknown_timestamp_events"], 1);
    assert_eq!(coverage["state"], "partial");

    let _ = fs::remove_file(store);
}

#[test]
fn all_read_only_reports_share_period_selection_and_json_coverage() {
    let store = fixture_store();
    let flags = [
        "--since",
        "2026-01-03T00:00:00Z",
        "--until",
        "2026-01-04T00:00:00Z",
    ];
    for args in REPORTING_COMMANDS {
        let output = run_args_with_flags(args, &flags, &store);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Requested period:"),
            "{args:?} omitted the selected period"
        );
    }

    for (command, typed) in [
        ("sessions", true),
        ("inventory", true),
        ("overhead", true),
        ("usage", true),
        ("waste", true),
        ("failures", true),
        ("stuck", true),
        ("prompts", true),
        ("analyze", false),
        ("corrections", false),
        ("rework", false),
        ("verification", false),
        ("knowledge", false),
        ("instructions", false),
        ("doctor", true),
    ] {
        let mut args = vec![command, "--format", "json"];
        args.extend(flags);
        let document = parse_json_report(&run_args(&args, &store), command);
        let coverage = if typed {
            &document["coverage"]
        } else {
            &document["data"]["coverage"]
        };
        assert_eq!(coverage["requested_start"], "2026-01-03T00:00:00.000Z");
        assert_eq!(coverage["requested_end"], "2026-01-04T00:00:00.000Z");
        if typed {
            assert!(coverage["period_status"].is_string());
            assert!(coverage["included_records"].as_u64().unwrap() > 0);
        } else {
            assert_eq!(coverage["included_sessions"], 1);
        }
    }

    let mut optimize_args = vec!["optimize", "--diff", "--format", "json"];
    optimize_args.extend(flags);
    let optimize = parse_json_report(&run_args(&optimize_args, &store), "optimize_diff");
    assert_eq!(optimize["data"]["coverage"]["included_sessions"], 1);
    assert!(optimize["data"]["freshness"]["state"].is_string());

    let apply = run_args_with_flags(&["optimize", "--apply", "--yes"], &flags, &store);
    assert!(!apply.status.success());
    assert!(String::from_utf8_lossy(&apply.stderr).contains("optimize --diff only"));

    let _ = fs::remove_file(store);
}

#[test]
fn period_filtered_global_inventory_recomputes_surface_usage() {
    let store = fixture_store();
    Store::open(&store)
        .unwrap()
        .replace_surfaces(&[Surface {
            id: "apply-patch-skill".to_owned(),
            kind: SurfaceKind::Skill,
            name: "apply_patch".to_owned(),
            path: Some(PathBuf::from("/synthetic/skills/apply_patch/SKILL.md")),
            scope: SurfaceScope::Global,
            enabled: Some(true),
            load_mode: SurfaceLoadMode::OnDemand,
            static_bytes: Some(32),
            startup_bytes: Some(32),
            observed_uses: 2,
            observed_sessions: 2,
            usage_state: SurfaceUsageState::Used,
            limitations: Vec::new(),
        }])
        .unwrap();

    let unfiltered = parse_json_report(
        &run_args(
            &["inventory", "--scope", "global", "--format", "json"],
            &store,
        ),
        "inventory",
    );
    let period = parse_json_report(
        &run_args_with_flags(
            &["inventory", "--scope", "global", "--format", "json"],
            &[
                "--since",
                "2026-01-03T00:00:00Z",
                "--until",
                "2026-01-04T00:00:00Z",
            ],
            &store,
        ),
        "inventory",
    );
    assert_eq!(unfiltered["data"]["rows"][0]["observed_uses"], 4);
    assert_eq!(period["data"]["rows"][0]["observed_uses"], 2);

    let _ = fs::remove_file(store);
}

#[test]
fn monitor_command_updates_a_local_store_and_honors_max_polls() {
    let source = temp_rollout_path("monitor");
    let store = temp_store_path("monitor-store");
    let cursor = source.with_extension("cursor.json");
    let secret_marker = b"synthetic raw secret=must-not-report";
    fs::write(
        &source,
        br#"{"type":"session_meta","payload":{"id":"cli-monitor-session","note":"synthetic raw secret=must-not-report"}}
"#,
    )
    .unwrap();
    let source_before = fs::read(&source).unwrap();

    let output = run_args(
        &[
            "monitor",
            "--source",
            source.to_str().unwrap(),
            "--cursor",
            cursor.to_str().unwrap(),
            "--max-polls",
            "1",
        ],
        &store,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_output_omits(&output, secret_marker, "monitor initial");
    assert_file_unchanged(&source, &source_before, "monitor initial source");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Monitor Rollout: Updated"), "{stdout}");
    let saved_cursor: codexlens::monitor::MonitorCursor =
        serde_json::from_slice(&fs::read(&cursor).unwrap()).unwrap();
    assert!(saved_cursor.offset > 0);

    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"cli-monitor-session-2\"}}\n")
        .unwrap();
    let source_after_append = fs::read(&source).unwrap();
    let restarted = run_args(
        &[
            "monitor",
            "--source",
            source.to_str().unwrap(),
            "--cursor",
            cursor.to_str().unwrap(),
            "--max-polls",
            "1",
        ],
        &store,
    );
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert_output_omits(&restarted, secret_marker, "monitor restart");
    assert_file_unchanged(&source, &source_after_append, "monitor restart source");
    assert!(
        String::from_utf8_lossy(&restarted.stdout).contains("(1 records"),
        "{}",
        String::from_utf8_lossy(&restarted.stdout)
    );
    let persisted = Store::open_read_only(&store)
        .unwrap()
        .load_canonical()
        .unwrap();
    assert_eq!(persisted.sessions.len(), 2);
    assert!(
        persisted
            .sessions
            .iter()
            .any(|session| session.id == "cli-monitor-session")
    );
    assert!(
        persisted
            .sessions
            .iter()
            .any(|session| session.id == "cli-monitor-session-2")
    );

    let _ = fs::remove_file(source);
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(cursor);
}

#[test]
fn reporting_commands_cover_empty_and_minimal_stores() {
    for (store, expected_sessions) in [
        (empty_store(), "Coverage: empty (0 sessions)"),
        (minimal_store(), "Coverage: empty (0 sessions)"),
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
                if matches!(args[0], "sessions" | "failures" | "stuck" | "doctor") {
                    assert!(stdout.contains(expected_sessions), "{args:?}: {stdout}");
                    assert!(
                        stdout.contains(match args[0] {
                            "failures" => "RECURRING FAILURES",
                            "stuck" => "STUCK WORK",
                            "doctor" => "WHAT TO FIX FIRST",
                            _ => "SESSIONS",
                        }),
                        "{args:?}: {stdout}"
                    );
                } else {
                    assert!(
                        stdout.contains("Coverage: selected store ("),
                        "{args:?}: {stdout}"
                    );
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
fn reporting_errors_bound_long_store_paths() {
    let root = long_path(&temp_store_path("reporting-long"));
    let mut parent = root.join("long");
    for index in 0..4 {
        parent = parent.join(format!("segment-{index}-{}", "x".repeat(40)));
    }
    fs::create_dir_all(&parent).unwrap();

    let missing = parent.join("missing-store-secret-tail.sqlite");
    for args in REPORTING_COMMANDS {
        let output = run_args(args, &missing);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            stderr.contains("store does not exist"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.len() < 512, "{args:?}: {stderr}");
        assert!(!stderr.contains("secret-tail"), "{args:?}: {stderr}");
    }

    let invalid = parent.join("invalid-store-secret-tail.sqlite");
    fs::write(&invalid, b"not a sqlite database").unwrap();
    for args in REPORTING_COMMANDS {
        let output = run_args(args, &invalid);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(stderr.len() < 512, "{args:?}: {stderr}");
        assert!(!stderr.contains("secret-tail"), "{args:?}: {stderr}");
    }

    let _ = fs::remove_dir_all(root);
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
    assert_ne!(rework.stdout, stuck.stdout);
    assert!(String::from_utf8_lossy(&stuck.stdout).starts_with("STUCK WORK"));

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
fn readiness_document_tracks_phase5_completion_and_phase6_entry_condition() {
    let readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let readiness =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/readiness/mvp.md"))
            .unwrap();

    assert!(readme.contains("docs/readiness/mvp.md"));
    assert!(readme.contains("docs/specs/post-mvp.md"));
    assert!(readme.contains("#156"));
    assert!(readme.contains("owner-authorized"));
    assert!(readme.contains("real-history smoke run"));
    assert!(readiness.contains("../specs/post-mvp.md"));
    assert!(readiness.contains("#156"));
    assert!(readiness.contains("owner-authorized"));
    assert!(readiness.contains("real-history smoke run"));
    assert!(!readme.contains("being finalized in"));
    assert!(!readme.contains("remains pending the cclens contract"));
    assert!(!readiness.contains("product rebaseline #143 is pending"));
    for marker in [
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo test --all-features",
        "optimize --apply",
        "compressed rollout",
        "refresh",
        "versioned JSON",
        "monitor",
        "#57",
        "#58",
        "#59",
        "#60",
        "#61",
        "#54",
        "Phase 5 is complete",
        "Phase 6 entry condition",
        "read-only",
    ] {
        assert!(
            readiness.contains(marker),
            "missing readiness marker: {marker}"
        );
    }
    assert!(!readiness.contains("## Deferred work"));
    assert!(!readiness.contains("remain deferred"));
}

#[test]
fn final_audit_records_release_evidence_and_boundaries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    let readiness = fs::read_to_string(root.join("docs/readiness/mvp.md")).unwrap();
    let audit = fs::read_to_string(root.join("docs/readiness/final-audit.md")).unwrap();

    assert!(readme.contains("docs/readiness/final-audit.md"));
    assert!(readiness.contains("final-audit.md"));
    assert!(audit.contains("#143 was pending when this audit ran"));
    assert!(audit.contains("current readiness approval"));
    assert!(!audit.contains("#143 remains pending"));
    for marker in [
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo build --all-features",
        "cargo test --all-features",
        "Rust 1.85.0",
        "Rust 1.92.0",
        "macos-latest",
        "windows-latest",
        "deterministic",
        "bounded and redacted",
        "source read-only",
        "optimize --diff",
        "optimize --apply",
        "backup",
        "recovery",
        "tests/fixtures",
        "No speculative feature work",
    ] {
        assert!(
            audit.contains(marker),
            "missing final-audit marker: {marker}"
        );
    }
}

#[test]
fn release_documents_track_current_version_and_source_release() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    let changelog = fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    let release = fs::read_to_string(root.join("docs/release.md")).unwrap();
    let license = fs::read_to_string(root.join("LICENSE")).unwrap();
    let version = env!("CARGO_PKG_VERSION");

    assert!(readme.contains("CHANGELOG.md"));
    assert!(readme.contains("docs/release.md"));
    assert!(readme.contains("## Inspiration"));
    assert!(readme.contains("cclens"));
    assert!(readme.contains("codex-session-insights"));
    assert!(readme.contains("independent implementation"));
    assert!(license.starts_with("MIT License"));
    assert!(changelog.contains(&format!("## [{version}]")));
    assert!(release.contains(&format!("`{version}`")));
    assert!(release.contains(&format!("v{version}")));
    let changelog_lower = changelog.to_ascii_lowercase();

    for marker in [
        "compressed rollout",
        "refresh",
        "frozen",
        "versioned",
        "monitor",
        "optimize --apply",
    ] {
        assert!(
            changelog_lower.contains(marker),
            "missing changelog marker: {marker}"
        );
    }

    for marker in [
        "Supported platforms",
        "macOS",
        "Ubuntu",
        "Windows",
        "Rust 1.85.0",
        "Rust 1.92.0",
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo build --all-features",
        "cargo test --all-features",
        "privacy",
        "read-only",
        "recovery",
        "MIT",
        "Inspiration",
        "source archive",
        "git tag",
        "package-manager publishing",
        "final-audit.md",
        "optimize --apply",
        "--format json",
    ] {
        assert!(release.contains(marker), "missing release marker: {marker}");
    }

    for args in [&["--version"][..], &["--help"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_codexlens"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "release CLI example failed: {args:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args == ["--version"] {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(&format!("codexlens {version}")),
                "CLI version output is inconsistent: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    for document in [&changelog, &release] {
        assert!(!document.contains("/Users/"));
        assert!(!document.contains("/home/"));
        assert!(!document.contains("BEGIN PRIVATE KEY"));
    }
}

#[test]
fn readme_documents_current_cli_surface_and_mvp_boundaries() {
    let readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let readme_lower = readme.to_ascii_lowercase();

    assert!(readme.contains("## CLI surface"));
    assert!(readme.contains("explicit refresh workflow"));
    assert!(readme.contains("Run `analyze` or `refresh`"));
    assert!(readme.contains("Phase 5 compressed rollout reader milestone"));
    assert!(readme.contains("Phase 5 safe optimize apply"));
    assert!(readme.contains("Phase 6"));
    assert!(readme.contains("scripts/real_history_smoke.py"));
    assert!(readme.contains("raw-input immutability"));
    assert!(!readme.contains("## Deliberately deferred"));
    for stale in [
        "optimize --apply is unavailable",
        "compressed inputs are reported as unsupported",
        "skipping refresh is not a current CLI behavior",
        "neither is part of the MVP command surface",
    ] {
        assert!(!readme.contains(stale), "stale README claim: {stale}");
    }
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
        "derived SQLite store",
        "local-only",
        "deterministic",
        "evidence-backed",
        "does not modify the supplied store or target files",
        "temporary migrated copy",
        "`optimize --apply`",
        "compressed rollout readers",
        "`refresh`",
        "`--codex-home PATH`",
        "zstd-compressed rollout",
        "`--frozen`",
        "optional cursor file",
    ] {
        assert!(
            readme_lower.contains(&boundary.to_ascii_lowercase()),
            "README is missing MVP boundary: {boundary}"
        );
    }
}

#[test]
fn post_mvp_contract_spec_tracks_documented_boundaries() {
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
            "## 6. Scoped finding evaluation",
        ),
        (
            "## 6. Scoped finding evaluation",
            "## 7. Explicit reporting periods",
        ),
        (
            "## 7. Explicit reporting periods",
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
        "compatibility and privacy contract for the implemented Phase 5",
        "Implementation status: implemented by Issue #57",
        "Implementation status: implemented by Issue #58",
        "Implementation status: implemented by Issues #59 and #82",
        "Implementation status: implemented by Issue #60",
        "Implementation status: implemented by Issue #61",
        "Implementation status: implemented by Issue #83",
        "Before a future feature issue extends",
        "primary compatibility and privacy boundaries",
        "The remaining cases are contract requirements",
    ] {
        assert!(
            spec.contains(marker),
            "missing implementation marker: {marker}"
        );
    }
    assert!(!spec.contains("implemented and still-deferred boundaries"));
    for stale in [
        "The refresh command and `--frozen` option must be explicit",
        "Add a compressed rollout reader in the adapter only",
        "Introduce an explicit refresh workflow",
        "Add live monitoring only as an explicit local runtime boundary",
    ] {
        assert!(!spec.contains(stale), "stale post-MVP claim: {stale}");
    }
    for marker in [
        "Current regression coverage",
        "Compatibility tests",
        "Privacy tests",
        "source read-only",
        "deterministic",
        "human-readable",
        "machine-readable",
        "backup",
        "recovery",
        "confirmation",
        "before implementation",
        "period_start",
        "schema_version",
        "nullable",
        "canonical",
        "canonical source path",
        "compressed-byte fingerprint",
        "scope",
        "LF line endings",
        "exactly one final LF",
        "future human-readable serializer",
        "existing MVP human renderers",
        "No applicable proposals.",
        "RenderedDiff",
        "SkippedProposal",
        "escape backslash",
        "optimize --diff` is the explicit read-only",
        "validated proposal write set",
        "`source_path` and `target_path`",
        "every file in the validated write set",
        "atomic across each proposal write set",
        "source and target paths",
        "allowed roots and file classes",
        "existing regular file with no symlink",
        "source_path field is not an independent authority",
        "canonical selected-path set",
        "unselected same-name",
        "Backups are local, scoped to the validated write set",
        "one transaction over the complete proposal batch",
        "multi-proposal failure",
        "out-of-scope source path",
    ] {
        assert!(spec.contains(marker), "missing contract marker: {marker}");
    }
    assert!(!spec.contains("a configured fallback"));
}

#[test]
fn finding_evaluation_plan_is_bounded_human_reviewed_and_private() {
    let plan = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/evaluations/finding-usefulness-pilot.md"),
    )
    .unwrap();

    for marker in [
        "No real-history pilot has been run",
        "set -eu",
        "AUTHORIZATION_RECORD",
        "authorization gate incomplete",
        "source_scope",
        "project_scope",
        "source/project scope",
        "Project scope check",
        "SELECTED_PROJECT",
        "project scope check failed",
        "observation period",
        "period_since",
        "period_until",
        "canonical bounds",
        "validate_pilot_authorization.py",
        "archive inclusion",
        "retention/deletion policy",
        "exact code version",
        "resolved interval",
        "Owner-authorized complete RFC3339 start bound",
        "Owner-authorized complete RFC3339 end bound",
        "--since",
        "--until",
        "PERIOD_ARGS",
        "coverage",
        "freshness",
        "actionable",
        "incorrect",
        "inconclusive",
        "sample denominator",
        "independent activity sample",
        "optimize --diff",
        "optimize --apply requires separate review and authorization",
        "no raw-history upload",
        "external analytics service",
        "background monitoring",
        "comparable windows",
        "causal",
        "synthetic regression",
        "#82",
        "#83",
        "Prioritized decision",
    ] {
        assert!(
            plan.contains(marker),
            "evaluation plan is missing: {marker}"
        );
    }
    for forbidden in [
        "raw logs",
        "credentials",
        "personal identifiers",
        "private paths",
    ] {
        assert!(
            plan.contains(forbidden),
            "privacy boundary is missing: {forbidden}"
        );
    }
    assert!(plan.contains("\"$PYTHON_BIN\" - \"$PILOT_DIR/sessions.json\" \"$SELECTED_PROJECT\""));
    assert!(!plan.contains("python3 - \"$PILOT_DIR/sessions.json\" \"$SELECTED_PROJECT\""));
}

#[test]
fn finding_pilot_authorization_binds_and_validates_period_bounds() {
    let validator =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/validate_pilot_authorization.py");
    let authorization = temp_store_path("pilot-authorization").with_extension("json");
    let python = std::env::var("PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_owned()
        } else {
            "python3".to_owned()
        }
    });
    let run = |since: &str, until: &str| {
        let record = json!({
            "source_scope": "synthetic-source",
            "project_scope": "synthetic-project",
            "observation_period": "owner-authorized synthetic interval",
            "period_since": since,
            "period_until": until,
            "archive_inclusion": "no",
            "storage_location": "synthetic-local-storage",
            "retention_deletion_policy": "delete after review",
            "owner_authorization": "synthetic-owner/2026-09-09",
        });
        fs::write(&authorization, record.to_string()).unwrap();
        Command::new(&python)
            .args([
                "-B",
                validator.to_str().unwrap(),
                authorization.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };

    let since = "2026-01-01T00:00:00.123456789Z";
    let until = "2026-01-01T00:00:01Z";
    let valid = run(since, until);
    assert!(
        valid.status.success(),
        "validator rejected valid bounds: {valid:?}"
    );
    assert_eq!(
        String::from_utf8(valid.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        format!("{since}\t{until}\n")
    );

    let nanos_since = "2026-01-01T00:00:00.123456000Z";
    let nanos_until = "2026-01-01T00:00:00.123456001Z";
    let nanos = run(nanos_since, nanos_until);
    assert!(
        nanos.status.success(),
        "validator rejected a one-nanosecond interval: {nanos:?}"
    );

    for (invalid_since, invalid_until) in [
        ("2026-01-01T00:00Z", until),
        ("2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z"),
        (
            "2026-01-01T00:00:00.123456001Z",
            "2026-01-01T00:00:00.123456000Z",
        ),
        ("2026-01-01T00:00:00+00:00:00", until),
    ] {
        let invalid = run(invalid_since, invalid_until);
        assert!(
            !invalid.status.success(),
            "validator accepted invalid bounds"
        );
        assert_eq!(
            String::from_utf8(invalid.stderr).unwrap().trim(),
            "authorization period invalid"
        );
    }
    let _ = fs::remove_file(authorization);
}

#[test]
fn compressed_rollout_input_is_ingested_incrementally_and_read_only() {
    let source_a = temp_store_path("compressed-a").with_extension("jsonl.zst");
    let source_b = temp_store_path("compressed-b").with_extension("jsonl.zst");
    let source_a_v1 = br#"{"type":"session_meta","payload":{"id":"fixture-compressed-a-v1"}}"#;
    let source_a_v2 = br#"{"type":"session_meta","payload":{"id":"fixture-compressed-a-v2"}}"#;
    let source_b_payload = br#"{"type":"session_meta","payload":{"id":"fixture-compressed-b"}}"#;
    write_compressed(&source_a, source_a_v1);
    write_compressed(&source_b, source_b_payload);
    let source_b_before = fs::read(&source_b).unwrap();

    let input = |path: &Path| DiscoveredInput {
        path: path.to_path_buf(),
        identity: fs::canonicalize(path).unwrap(),
        kind: InputKind::Rollout { archived: false },
        reader: Some(ReaderKind::ZstdJsonl),
    };
    let inputs = vec![input(&source_a), input(&source_b)];
    let mut store = Store::in_memory().unwrap();

    let first = store
        .ingest_inputs(&inputs, &IngestOptions::default())
        .unwrap();
    assert!(first.files.iter().all(|file| !file.skipped));
    assert_eq!(
        first.files.iter().map(|file| file.records).sum::<usize>(),
        2
    );
    assert_eq!(fs::read(&source_b).unwrap(), source_b_before);

    let second = store
        .ingest_inputs(&inputs, &IngestOptions::default())
        .unwrap();
    assert!(second.files.iter().all(|file| file.skipped));
    assert_eq!(fs::read(&source_b).unwrap(), source_b_before);

    write_compressed(&source_a, source_a_v2);
    let source_a_after_change = fs::read(&source_a).unwrap();
    let changed = store
        .ingest_inputs(&inputs, &IngestOptions::default())
        .unwrap();
    assert!(
        !changed
            .files
            .iter()
            .find(|file| file.source == source_a)
            .unwrap()
            .skipped
    );
    assert!(
        changed
            .files
            .iter()
            .find(|file| file.source == source_b)
            .unwrap()
            .skipped
    );

    let data = store.load_canonical().unwrap();
    assert_eq!(data.sessions.len(), 2);
    assert!(
        data.sessions
            .iter()
            .any(|session| session.id == "fixture-compressed-a-v2")
    );
    assert!(
        data.sessions
            .iter()
            .any(|session| session.id == "fixture-compressed-b")
    );
    assert!(
        !data
            .sessions
            .iter()
            .any(|session| session.id == "fixture-compressed-a-v1")
    );
    assert_eq!(fs::read(&source_a).unwrap(), source_a_after_change);
    assert_eq!(fs::read(&source_b).unwrap(), source_b_before);

    let _ = fs::remove_file(source_a);
    let _ = fs::remove_file(source_b);
}

#[test]
fn ingest_inputs_uses_reader_kind_for_dispatch() {
    let plain_source = temp_store_path("reader-kind-plain").with_extension("jsonl.zst");
    let compressed_source = temp_store_path("reader-kind-compressed").with_extension("jsonl");
    let plain_payload = br#"{"type":"session_meta","payload":{"id":"fixture-reader-kind-plain"}}"#;
    let compressed_payload =
        br#"{"type":"session_meta","payload":{"id":"fixture-reader-kind-compressed"}}"#;
    fs::write(&plain_source, plain_payload).unwrap();
    write_compressed(&compressed_source, compressed_payload);

    let input = |path: &Path, reader| DiscoveredInput {
        path: path.to_path_buf(),
        identity: fs::canonicalize(path).unwrap(),
        kind: InputKind::Rollout { archived: false },
        reader: Some(reader),
    };
    let inputs = vec![
        input(&plain_source, ReaderKind::PlainJsonl),
        input(&compressed_source, ReaderKind::ZstdJsonl),
    ];
    let mut store = Store::in_memory().unwrap();
    let report = store
        .ingest_inputs(&inputs, &IngestOptions::default())
        .unwrap();

    assert!(report.files.iter().all(|file| file.diagnostics == 0));
    assert_eq!(
        report.files.iter().map(|file| file.sessions).sum::<usize>(),
        2
    );
    let data = store.load_canonical().unwrap();
    assert_eq!(data.sessions.len(), 2);

    let _ = fs::remove_file(plain_source);
    let _ = fs::remove_file(compressed_source);
}

#[test]
fn corrupt_compressed_rollout_does_not_block_valid_sibling() {
    let corrupt = temp_store_path("corrupt-compressed").with_extension("jsonl.zst");
    let valid = temp_store_path("valid-compressed").with_extension("jsonl.zst");
    let corrupt_payload = b"synthetic secret=do-not-print\n";
    fs::write(&corrupt, corrupt_payload).unwrap();
    let corrupt_before = fs::read(&corrupt).unwrap();
    write_compressed(
        &valid,
        br#"{"type":"session_meta","payload":{"id":"fixture-valid-compressed"}}"#,
    );
    let valid_before = fs::read(&valid).unwrap();

    let input = |path: &Path| DiscoveredInput {
        path: path.to_path_buf(),
        identity: fs::canonicalize(path).unwrap(),
        kind: InputKind::Rollout { archived: false },
        reader: Some(ReaderKind::ZstdJsonl),
    };
    let mut store = Store::in_memory().unwrap();
    let report = store
        .ingest_inputs(&[input(&corrupt), input(&valid)], &IngestOptions::default())
        .unwrap();

    let corrupt_summary = report
        .files
        .iter()
        .find(|file| file.source == corrupt)
        .unwrap();
    assert_eq!(corrupt_summary.records, 0);
    assert_eq!(corrupt_summary.diagnostics, 1);
    let valid_summary = report
        .files
        .iter()
        .find(|file| file.source == valid)
        .unwrap();
    assert_eq!(valid_summary.sessions, 1);
    let data = store.load_canonical().unwrap();
    assert_eq!(data.sessions.len(), 1);
    assert_eq!(data.diagnostics.len(), 1);
    assert_eq!(data.diagnostics[0].kind, DiagnosticKind::Unreadable);
    assert!(!data.diagnostics[0].message.contains("do-not-print"));
    assert_eq!(fs::read(&corrupt).unwrap(), corrupt_before);
    assert_eq!(fs::read(&valid).unwrap(), valid_before);

    let second = store
        .ingest_inputs(&[input(&corrupt), input(&valid)], &IngestOptions::default())
        .unwrap();
    assert!(second.files.iter().all(|file| file.skipped));

    let _ = fs::remove_file(corrupt);
    let _ = fs::remove_file(valid);
}

#[test]
fn unreadable_compressed_replacement_clears_rows_and_recovers() {
    let source = temp_store_path("recover-compressed").with_extension("jsonl.zst");
    let source_v1 = br#"{"type":"session_meta","payload":{"id":"fixture-recover-compressed-v1"}}"#;
    let source_v2 = br#"{"type":"session_meta","payload":{"id":"fixture-recover-compressed-v2"}}"#;
    write_compressed(&source, source_v1);

    let input = || DiscoveredInput {
        path: source.clone(),
        identity: fs::canonicalize(&source).unwrap(),
        kind: InputKind::Rollout { archived: false },
        reader: Some(ReaderKind::ZstdJsonl),
    };
    let mut store = Store::in_memory().unwrap();
    let first = store
        .ingest_inputs(&[input()], &IngestOptions::default())
        .unwrap();
    assert_eq!(first.files[0].sessions, 1);

    fs::write(&source, b"synthetic truncated compressed input").unwrap();
    let failed = store
        .ingest_inputs(&[input()], &IngestOptions::default())
        .unwrap();
    assert!(!failed.files[0].skipped);
    assert_eq!(failed.files[0].diagnostics, 1);
    let data = store.load_canonical().unwrap();
    assert!(data.sessions.is_empty());
    assert!(data.records.is_empty());
    assert_eq!(data.diagnostics.len(), 1);

    let second = store
        .ingest_inputs(&[input()], &IngestOptions::default())
        .unwrap();
    assert!(second.files[0].skipped);

    write_compressed(&source, source_v2);
    let recovered = store
        .ingest_inputs(&[input()], &IngestOptions::default())
        .unwrap();
    assert!(!recovered.files[0].skipped);
    let data = store.load_canonical().unwrap();
    assert_eq!(data.sessions.len(), 1);
    assert_eq!(data.sessions[0].id, "fixture-recover-compressed-v2");
    assert!(data.diagnostics.is_empty());

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
    assert_file_unchanged(&store, &store_before, "derived store");
    assert_file_unchanged(&raw_source, &raw_before, "raw source");

    let _ = fs::remove_file(raw_source);
    let _ = fs::remove_file(store);
}

#[test]
fn reporting_command_surface_stays_read_only_and_private() {
    let (home, source) = refresh_home();
    let store = temp_store_path("reporting-command-surface");
    let refreshed = run_refresh(&home, &store);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );

    let store_before = fs::read(&store).unwrap();
    let secret_marker = b"synthetic raw secret=must-not-report";
    let mut source_payload = secret_marker.to_vec();
    source_payload.push(b'\n');
    fs::write(&source, source_payload).unwrap();
    let source_before = fs::read(&source).unwrap();

    let variants: &[(&str, &[&str])] = &[
        ("human", &[]),
        ("json", &["--format", "json"]),
        ("frozen", &["--frozen"]),
        ("frozen json", &["--format", "json", "--frozen"]),
    ];
    for args in REPORTING_COMMANDS {
        for (variant, flags) in variants {
            let output = run_args_with_flags(args, flags, &store);
            let label = format!("{args:?} ({variant})");
            assert!(
                output.status.success(),
                "{label}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_output_omits(&output, secret_marker, &label);
            assert_file_unchanged(&store, &store_before, &format!("{label} store"));
            assert_file_unchanged(&source, &source_before, &format!("{label} source"));
        }
    }

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn unfrozen_reporting_does_not_refresh_or_read_raw_inputs() {
    let (home, source) = refresh_home();
    let store = temp_store_path("unfrozen-reporting");
    let refreshed = run_refresh(&home, &store);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let store_before = fs::read(&store).unwrap();
    fs::write(&source, b"synthetic raw input changed after analyze\n").unwrap();
    let source_after = fs::read(&source).unwrap();

    let output = run_args_with_flags(
        &["doctor"],
        &["--codex-home", home.to_str().unwrap()],
        &store,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Refreshed store:"));
    assert_file_unchanged(&store, &store_before, "unfrozen report store");
    assert_file_unchanged(&source, &source_after, "unfrozen report source");

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn refresh_and_frozen_reporting_are_explicit_and_read_only() {
    let (home, source) = refresh_home();
    let store = temp_store_path("refresh-store");
    let secret_marker = b"synthetic raw secret=must-not-report";
    fs::write(
        &source,
        br#"{"type":"session_meta","payload":{"id":"refresh-secret-session","note":"synthetic raw secret=must-not-report"}}
"#,
    )
    .unwrap();
    let source_before = fs::read(&source).unwrap();

    let first = run_refresh(&home, &store);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_output_omits(&first, secret_marker, "refresh initial");
    assert_file_unchanged(&source, &source_before, "refresh initial source");
    assert!(String::from_utf8_lossy(&first.stdout).contains("ingested"));
    let first_store = fs::read(&store).unwrap();

    let second = run_refresh(&home, &store);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_output_omits(&second, secret_marker, "refresh repeat");
    assert_file_unchanged(&source, &source_before, "refresh repeat source");
    assert!(String::from_utf8_lossy(&second.stdout).contains("skipped"));
    assert_eq!(
        Store::open_read_only(&store)
            .unwrap()
            .freshness()
            .unwrap()
            .source_count,
        1
    );

    let frozen = run_args(&["doctor", "--frozen"], &store);
    assert!(frozen.status.success());
    fs::write(&source, b"synthetic raw secret=must-not-be-read\n").unwrap();
    let frozen_again = run_args(&["doctor", "--frozen"], &store);
    let normal = run_args(&["doctor"], &store);
    assert_eq!(frozen.stdout, frozen_again.stdout);
    assert_eq!(frozen.stderr, frozen_again.stderr);
    assert_eq!(frozen.stdout, normal.stdout);
    assert_eq!(frozen.stderr, normal.stderr);
    assert_eq!(fs::read(&store).unwrap(), first_store);
    assert_eq!(
        fs::read(&source).unwrap(),
        b"synthetic raw secret=must-not-be-read\n"
    );

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn failed_refresh_keeps_the_previous_derived_store() {
    let (home, source) = refresh_home();
    let store = temp_store_path("refresh-rollback");
    assert!(run_refresh(&home, &store).status.success());
    Connection::open(&store)
        .unwrap()
        .execute_batch(include_str!("fixtures/store/rollback-trigger.sql"))
        .unwrap();
    let before = fs::read(&store).unwrap();
    let mut changed_source = fs::read(&source).unwrap();
    changed_source.extend_from_slice(b"{\"type\":\"future\",\"payload\":{}}\n");
    fs::write(&source, changed_source).unwrap();

    let output = run_refresh(&home, &store);

    assert!(!output.status.success());
    assert_eq!(fs::read(&store).unwrap(), before);
    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn refresh_errors_bound_long_store_paths() {
    let (home, _) = refresh_home();
    let mut parent = long_path(&home).join("long");
    for index in 0..4 {
        parent = parent.join(format!("segment-{index}-{}", "x".repeat(40)));
    }
    fs::create_dir_all(&parent).unwrap();
    let store = parent.join("invalid-store-secret-tail.sqlite");
    fs::write(&store, b"not a sqlite database").unwrap();

    let output = run_refresh(&home, &store);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.len() < 512, "{stderr}");
    assert!(!stderr.contains("secret-tail"), "{stderr}");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn refresh_rejects_a_raw_input_as_the_derived_store() {
    let (home, source) = refresh_home();
    let before = fs::read(&source).unwrap();

    let output = run_refresh(&home, &source);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not be a raw or instruction input"),
        "{stderr}"
    );
    assert!(stderr.len() < 512, "{stderr}");
    assert_eq!(fs::read(&source).unwrap(), before);

    let _ = fs::remove_dir_all(home);
}

#[test]
fn refresh_rejects_archived_and_instruction_sources_as_the_derived_store() {
    let (home, _) = refresh_home();
    let archived = home
        .join("archived_sessions")
        .join("2026")
        .join("old.jsonl");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::write(&archived, b"").unwrap();
    let agents = home.join("AGENTS.md");
    let config = home.join("config.toml");

    for protected in [&archived, &agents, &config] {
        fs::write(protected, b"").unwrap();
        let before = fs::read(protected).unwrap();
        let output = run_refresh(&home, protected);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("must not be a raw or instruction input")
        );
        assert_eq!(fs::read(protected).unwrap(), before);
    }

    let _ = fs::remove_dir_all(home);
}

#[test]
fn refresh_protects_turn_context_instruction_sources() {
    let (home, source) = refresh_home();
    let project = home.join("project");
    let instruction = project.join("AGENTS.md");
    fs::create_dir_all(&project).unwrap();
    fs::write(&instruction, b"").unwrap();
    fs::write(
        &source,
        format!(
            "{}\n{}\n",
            json!({
                "type": "session_meta",
                "payload": {"id": "synthetic-turn-context"},
            }),
            json!({
                "type": "turn_context",
                "payload": {
                    "turn_id": "synthetic-turn",
                    "cwd": project.to_string_lossy(),
                    "project_root": project.to_string_lossy(),
                },
            }),
        ),
    )
    .unwrap();
    let before = fs::read(&instruction).unwrap();

    let output = run_refresh(&home, &instruction);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must not be a raw or instruction input")
    );
    assert_eq!(fs::read(&instruction).unwrap(), before);

    let _ = fs::remove_dir_all(home);
}

#[cfg(any(unix, windows))]
#[test]
fn refresh_rejects_a_hard_link_to_a_raw_input_as_the_derived_store() {
    let (home, source) = refresh_home();
    let store = temp_store_path("hard-linked-refresh-store");
    fs::hard_link(&source, &store).unwrap();
    let before = fs::read(&source).unwrap();

    let output = run_refresh(&home, &store);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not be a raw or instruction input"),
        "{stderr}"
    );
    assert!(stderr.len() < 512, "{stderr}");
    assert_eq!(fs::read(&source).unwrap(), before);

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[cfg(any(unix, windows))]
#[test]
fn refresh_rejects_a_hard_link_to_an_instruction_source_as_the_derived_store() {
    let (home, _) = refresh_home();
    let instruction = home.join("AGENTS.md");
    let store = temp_store_path("hard-linked-instruction-store");
    fs::write(&instruction, b"").unwrap();
    fs::hard_link(&instruction, &store).unwrap();
    let before = fs::read(&instruction).unwrap();

    let output = run_refresh(&home, &store);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must not be a raw or instruction input")
    );
    assert_eq!(fs::read(&instruction).unwrap(), before);

    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_file(store);
}

#[test]
fn machine_readable_output_is_versioned_deterministic_and_canonical() {
    let store = fixture_store();
    for (args, command) in [
        (&["analyze", "--format", "json"][..], "analyze"),
        (&["sessions", "--format", "json"][..], "sessions"),
        (&["failures", "--format", "json"][..], "failures"),
        (&["corrections", "--format", "json"][..], "corrections"),
        (&["rework", "--format", "json"][..], "rework"),
        (&["stuck", "--format", "json"][..], "stuck"),
        (&["verification", "--format", "json"][..], "verification"),
        (&["knowledge", "--format", "json"][..], "knowledge"),
        (&["rediscovery", "--format", "json"][..], "knowledge"),
        (&["instructions", "--format", "json"][..], "instructions"),
        (&["doctor", "--format", "json"][..], "doctor"),
    ] {
        let first = run_args(args, &store);
        let second = run_args(args, &store);
        assert_eq!(first.stdout, second.stdout, "{args:?} is not deterministic");
        let document = parse_json_report(&first, command);
        let data = document["data"].as_object().unwrap();
        if matches!(command, "sessions" | "failures" | "stuck") {
            assert!(data["rows"].is_array());
            for session in data["rows"].as_array().unwrap() {
                if command != "sessions" {
                    assert!(session["opportunity"].is_object());
                    continue;
                }
                for field in ["id", "created_at", "updated_at", "cwd", "project"] {
                    assert!(
                        session.get(field).is_some(),
                        "missing session field {field}"
                    );
                }
            }
        } else if command == "doctor" {
            assert!(data["top_fixes"].is_array());
            assert!(data["cost"].is_object());
            assert!(data["config_pruning"].is_object());
            assert!(data["looks_healthy"].is_boolean());
            for field in [
                "period_start",
                "period_end",
                "session_count",
                "freshness",
                "finding_counts",
                "groups",
            ] {
                assert!(data.get(field).is_some(), "missing doctor field {field}");
            }
        } else {
            for field in [
                "period_start",
                "period_end",
                "session_count",
                "freshness",
                "finding_counts",
                "groups",
            ] {
                assert!(
                    data.get(field).is_some(),
                    "missing finding report field {field}"
                );
            }
            for group in data["groups"].as_array().unwrap() {
                assert!(group["scope"].is_object());
                for finding in group["findings"].as_array().unwrap() {
                    for field in [
                        "kind",
                        "severity",
                        "confidence",
                        "scope",
                        "key",
                        "summary",
                        "evidence",
                        "occurrences",
                        "distinct_sessions",
                        "affected_paths",
                        "observed_commands",
                        "sequence",
                        "suggested_action",
                        "limitations",
                        "verification_status",
                        "heuristic",
                    ] {
                        assert!(
                            finding.get(field).is_some(),
                            "missing finding field {field}"
                        );
                    }
                    for evidence in finding["evidence"].as_array().unwrap() {
                        for field in ["session_id", "source", "role", "excerpt"] {
                            assert!(
                                evidence.get(field).is_some(),
                                "missing evidence field {field}"
                            );
                        }
                        for field in [
                            "kind",
                            "path",
                            "line",
                            "ingested_at",
                            "parser_schema_version",
                        ] {
                            assert!(
                                evidence["source"].get(field).is_some(),
                                "missing source field {field}"
                            );
                        }
                    }
                }
            }
        }
    }
    let _ = fs::remove_file(store);
}

#[test]
fn json_doctor_matches_human_counts_scopes_evidence_and_order() {
    let store = fixture_store();
    let human = run_args(&["doctor"], &store);
    let machine = run_args(&["doctor", "--format", "json"], &store);
    assert!(human.status.success());
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    let document = parse_json_report(&machine, "doctor");
    let top_fixes = document["data"]["top_fixes"].as_array().unwrap();
    assert!(!top_fixes.is_empty());
    for opportunity in top_fixes {
        assert!(human_stdout.contains(opportunity["title"].as_str().unwrap()));
        assert!(opportunity["evidence"].as_array().unwrap().len() <= 3);
        for field in [
            "id",
            "title",
            "scope",
            "owner",
            "target",
            "impact",
            "confidence",
            "occurrences",
            "distinct_sessions",
            "action",
            "follow_up",
            "limitations",
        ] {
            assert!(opportunity.get(field).is_some(), "missing {field}");
        }
    }
    assert_eq!(document["scope"]["kind"], "all");
    let _ = fs::remove_file(store);
}

#[test]
fn reporting_metadata_exposes_store_coverage_and_separates_ingestion_time() {
    let store = fixture_store();
    let human = run_args(&["doctor"], &store);
    assert!(human.status.success());
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(human_stdout.starts_with("WHAT TO FIX FIRST"));
    assert!(human_stdout.contains("Coverage: partial (2 sessions)"));
    assert!(human_stdout.contains("Store freshness: recorded at "));

    let document = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    let coverage = &document["coverage"];
    assert_eq!(coverage["status"], "partial");
    assert_eq!(coverage["activity_start"], "2026-01-03T00:00:00.000Z");
    assert_eq!(coverage["activity_end"], "2026-01-04T00:05:00.000Z");
    assert_eq!(coverage["session_count"], 2);
    assert_eq!(coverage["record_count"], 16);
    assert!(coverage["valid_activity_timestamps"].as_u64().unwrap() > 0);
    assert!(coverage["missing_activity_timestamps"].as_u64().unwrap() > 0);
    assert_eq!(coverage["invalid_activity_timestamps"], 0);
    assert_ne!(
        coverage["activity_end"],
        document["freshness"]["latest_ingested_at"]
    );

    let sessions = parse_json_report(
        &run_args(&["sessions", "--format", "json"], &store),
        "sessions",
    );
    assert_eq!(sessions["coverage"], coverage.clone());
    let _ = fs::remove_file(store);
}

#[test]
fn coverage_limitations_are_visible_in_table_markdown_and_json() {
    let partial_store = coverage_limitation_store();
    let table = run_args(&["doctor"], &partial_store);
    assert!(
        table.status.success(),
        "{}",
        String::from_utf8_lossy(&table.stderr)
    );
    let table_stdout = String::from_utf8_lossy(&table.stdout);
    assert!(table_stdout.contains("Limitations:"), "{table_stdout}");
    assert!(table_stdout.contains("oversized_line"), "{table_stdout}");
    assert!(table_stdout.contains("selected records"), "{table_stdout}");

    let markdown = run_args(&["doctor", "--format", "markdown"], &partial_store);
    assert!(markdown.status.success());
    let markdown_stdout = String::from_utf8_lossy(&markdown.stdout);
    assert!(
        markdown_stdout.starts_with("# WHAT TO FIX FIRST"),
        "{markdown_stdout}"
    );
    assert!(
        markdown_stdout.contains("metadata_conflict"),
        "{markdown_stdout}"
    );

    let json = parse_json_report(
        &run_args(&["doctor", "--format", "json"], &partial_store),
        "doctor",
    );
    let limitations = json["coverage"]["limitations"].as_array().unwrap();
    for kind in [
        "missing_lifecycle_timestamp",
        "invalid_timestamp",
        "oversized_line",
        "unreadable",
        "metadata_conflict",
    ] {
        assert!(
            limitations
                .iter()
                .any(|limitation| limitation["kind"] == kind),
            "{kind}: {limitations:?}"
        );
    }
    let oversized = limitations
        .iter()
        .find(|limitation| limitation["kind"] == "oversized_line")
        .unwrap();
    assert_eq!(oversized["source"]["line"], 7);
    assert!(oversized["selected_sessions"].as_u64().unwrap() > 0);
    assert!(oversized["selected_records"].as_u64().unwrap() > 0);
    assert!(
        oversized["affected_lenses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|lens| lens == "usage")
    );
    let metadata_conflict = limitations
        .iter()
        .find(|limitation| limitation["kind"] == "metadata_conflict")
        .unwrap();
    assert!(metadata_conflict["selected_sessions"].as_u64().unwrap() > 0);
    assert!(metadata_conflict["selected_records"].as_u64().unwrap() > 0);
    assert!(json["coverage"]["limitations_omitted"].is_number());
    let _ = fs::remove_file(partial_store);

    let complete_store = coverage_timestamp_fallback_store();
    let complete_json = parse_json_report(
        &run_args(&["doctor", "--format", "json"], &complete_store),
        "doctor",
    );
    assert_eq!(complete_json["coverage"]["status"], "observed");
    assert_eq!(complete_json["coverage"]["limitations"], json!([]));
    for format in ["table", "markdown"] {
        let args = if format == "table" {
            vec!["doctor"]
        } else {
            vec!["doctor", "--format", format]
        };
        let output = run_args(&args, &complete_store);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Limitations: none"));
    }
    let _ = fs::remove_file(complete_store);

    let empty = empty_store();
    let empty_json =
        parse_json_report(&run_args(&["doctor", "--format", "json"], &empty), "doctor");
    assert_eq!(empty_json["coverage"]["status"], "empty");
    assert_eq!(empty_json["coverage"]["limitations"], json!([]));
    let empty_table = run_args(&["doctor"], &empty);
    assert!(String::from_utf8_lossy(&empty_table.stdout).contains("Limitations: none"));
    let empty_markdown = run_args(&["doctor", "--format", "markdown"], &empty);
    assert!(String::from_utf8_lossy(&empty_markdown.stdout).contains("Limitations: none"));
    let _ = fs::remove_file(empty);
}

#[test]
fn unfiltered_reports_share_chronological_valid_activity_period() {
    let store = chronological_period_store();
    let expected_start = "2026-01-03T09:00:00+09:00";
    let expected_end = "2026-01-03T00:30:00.123456789Z";

    {
        let command = "analyze";
        let human = run_args(&[command], &store);
        assert!(human.status.success(), "{command}: human report failed");
        let stdout = String::from_utf8_lossy(&human.stdout);
        assert!(
            stdout.contains(&format!(
                "Analyzed period: {expected_start} .. {expected_end}"
            )),
            "{command}: {stdout}"
        );
        assert!(
            stdout.contains(&format!("Activity: {expected_start} .. {expected_end}")),
            "{command}: {stdout}"
        );
        assert!(stdout.contains("Activity timestamps: 3 valid, 0 missing, 1 invalid"));

        let document =
            parse_json_report(&run_args(&[command, "--format", "json"], &store), command);
        let data = &document["data"];
        assert_eq!(data["period_start"], expected_start);
        assert_eq!(data["period_end"], expected_end);
        assert_eq!(data["coverage"]["activity_start"], data["period_start"]);
        assert_eq!(data["coverage"]["activity_end"], data["period_end"]);
        assert_eq!(data["coverage"]["invalid_activity_timestamps"], 1);
    }

    let doctor = run_args(&["doctor"], &store);
    assert!(doctor.status.success(), "doctor: human report failed");
    let doctor_stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(doctor_stdout.starts_with("WHAT TO FIX FIRST"));
    assert!(doctor_stdout.contains("Coverage: partial (1 sessions)"));
    let doctor_document =
        parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    assert_eq!(
        doctor_document["coverage"]["activity_start"],
        expected_start
    );
    assert_eq!(doctor_document["coverage"]["activity_end"], expected_end);
    assert_eq!(
        doctor_document["coverage"]["invalid_activity_timestamps"],
        1
    );

    let _ = fs::remove_file(store);
}

#[test]
fn empty_reporting_store_marks_activity_unknown_without_using_ingestion_time() {
    let store = empty_store();
    for args in [
        &["sessions"][..],
        &["sessions", "--format", "json"][..],
        &["doctor"][..],
        &["doctor", "--format", "json"][..],
    ] {
        let output = run_args(args, &store);
        assert!(output.status.success(), "{args:?}");
        if args.contains(&"--format") {
            let command = args[0];
            let document = parse_json_report(&output, command);
            let coverage = &document["coverage"];
            assert_eq!(coverage["status"], "empty", "{args:?}");
            assert!(coverage["activity_start"].is_null(), "{args:?}");
            assert!(coverage["activity_end"].is_null(), "{args:?}");
            assert_eq!(coverage["session_count"], 0, "{args:?}");
            assert_eq!(coverage["record_count"], 0, "{args:?}");
            assert!(document["freshness"]["latest_ingested_at"].is_null());
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("Coverage: empty (0 sessions)"), "{stdout}");
            assert!(stdout.contains("Store freshness: empty"), "{stdout}");
        }
    }
    let _ = fs::remove_file(store);
}

#[test]
fn doctor_marks_unknown_surface_usage_inconclusive() {
    let store = unknown_surface_store();
    let document = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    assert_eq!(document["data"]["looks_healthy"], false);
    assert_eq!(document["data"]["analysis_sufficient"], false);
    assert_eq!(document["coverage"]["status"], "observed");
    assert_eq!(document["data"]["cost"]["rows"][0]["unknown_cost"], false);
    assert!(
        document["data"]["config_pruning"]["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let _ = fs::remove_file(store);
}

#[test]
fn optimize_reports_unavailable_surface_evidence_without_recommending_removal() {
    let store = unknown_surface_store();

    let human = run_args(&["optimize", "--print"], &store);
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains(
        "No findings had enough evidence for an actionable recommendation; skipped items are listed below."
    ));
    assert!(stdout.contains("/synthetic/unknown-skill/SKILL.md"));
    assert!(
        stdout.contains("configuration surface was skipped because usage evidence is unavailable")
    );
    assert!(!stdout.contains("Proposal remove"));
    assert!(!stdout.contains("synthetic instruction snapshot"));

    let json = run_args(&["optimize", "--print", "--format", "json"], &store);
    let document = parse_json_report(&json, "optimize");
    assert!(document["data"]["findings"].as_array().unwrap().is_empty());
    let skipped = document["data"]["proposals"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target_path"] == "/synthetic/unknown-skill/SKILL.md")
        .expect("unavailable surface evidence should be listed as skipped");
    assert!(skipped["proposal"].is_null());
    assert!(
        skipped["reason"]
            .as_str()
            .unwrap()
            .contains("configuration surface was skipped because usage evidence is unavailable")
    );
    assert!(
        document["data"]["counts"]["skipped_proposal_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(!String::from_utf8_lossy(&json.stdout).contains("synthetic instruction snapshot"));
    assert!(
        !human
            .stdout
            .windows("synthetic instruction snapshot".len())
            .any(|window| window == "synthetic instruction snapshot".as_bytes())
    );

    let _ = fs::remove_file(store);
}

#[test]
fn optimize_print_sends_omitted_opportunity_details_to_stderr() {
    let store = fixture_store();
    let project_root = temp_store_path("omitted-opportunities");
    fs::create_dir(&project_root).unwrap();
    let mut surfaces = Vec::new();
    let mut targets = Vec::new();
    for index in 0..=50 {
        let target = project_root.join(format!("skill-{index:02}/SKILL.md"));
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "synthetic review-only surface\n").unwrap();
        targets.push(target.clone());
        surfaces.push(Surface {
            id: format!("synthetic-unused-{index:02}"),
            kind: SurfaceKind::Skill,
            name: format!("synthetic-unused-{index:02}"),
            path: Some(target),
            scope: SurfaceScope::Global,
            enabled: Some(true),
            load_mode: SurfaceLoadMode::OnDemand,
            static_bytes: Some(64),
            startup_bytes: Some(0),
            observed_uses: 0,
            observed_sessions: 1,
            usage_state: SurfaceUsageState::Unused,
            limitations: Vec::new(),
        });
    }
    Store::open(&store)
        .unwrap()
        .replace_surfaces(&surfaces)
        .unwrap();

    let human = run_args(&["optimize", "--print"], &store);
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(stdout.contains("Omitted ") && stdout.contains(" additional rows."));
    assert!(!stdout.contains("surface:synthetic-unused-50"));
    assert!(stderr.contains("surface:synthetic-unused-50"));
    assert!(stderr.contains("Skip reason: configuration opportunity is actionable"));

    let json = run_args(&["optimize", "--print", "--format", "json"], &store);
    assert!(json.status.success());
    let document: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["command"], "optimize");
    assert!(
        document["data"]["counts"]["finding_count"]
            .as_u64()
            .is_some_and(|count| count > 50)
    );
    assert!(
        document["data"]["counts"]["finding_omitted_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(!String::from_utf8_lossy(&json.stdout).contains("surface:synthetic-unused-50"));
    assert!(String::from_utf8_lossy(&json.stderr).contains("surface:synthetic-unused-50"));

    for target in targets {
        let _ = fs::remove_file(target);
    }
    for index in 0..=50 {
        let _ = fs::remove_dir(project_root.join(format!("skill-{index:02}")));
    }
    let _ = fs::remove_dir(project_root);
    let _ = fs::remove_file(store);
}

#[test]
fn optimize_json_sends_capped_unlinked_skip_details_to_stderr() {
    let store = fixture_store();
    let project_root = temp_store_path("capped-skips");
    fs::create_dir(&project_root).unwrap();
    let mut surfaces = Vec::new();
    let mut reviewable_targets = Vec::new();
    let mut unavailable_targets = Vec::new();
    for index in 0..20 {
        let target = project_root.join(format!("a-skill-{index:02}/SKILL.md"));
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "synthetic review-only surface\n").unwrap();
        reviewable_targets.push(target.clone());
        surfaces.push(Surface {
            id: format!("synthetic-reviewable-{index:02}"),
            kind: SurfaceKind::Skill,
            name: format!("synthetic-reviewable-{index:02}"),
            path: Some(target),
            scope: SurfaceScope::Global,
            enabled: Some(true),
            load_mode: SurfaceLoadMode::OnDemand,
            static_bytes: Some(64),
            startup_bytes: Some(0),
            observed_uses: 0,
            observed_sessions: 1,
            usage_state: SurfaceUsageState::Unused,
            limitations: Vec::new(),
        });
    }
    for index in 0..40 {
        let target = project_root.join(format!("z-unknown-{index:02}/SKILL.md"));
        unavailable_targets.push(target.clone());
        surfaces.push(Surface {
            id: format!("synthetic-unknown-{index:02}"),
            kind: SurfaceKind::Skill,
            name: format!("synthetic-unknown-{index:02}"),
            path: Some(target),
            scope: SurfaceScope::Global,
            enabled: Some(true),
            load_mode: SurfaceLoadMode::OnDemand,
            static_bytes: Some(64),
            startup_bytes: Some(0),
            observed_uses: 0,
            observed_sessions: 0,
            usage_state: SurfaceUsageState::Unknown,
            limitations: vec!["synthetic usage evidence is unavailable".to_owned()],
        });
    }
    Store::open(&store)
        .unwrap()
        .replace_surfaces(&surfaces)
        .unwrap();

    let output = run_args(&["optimize", "--print", "--format", "json"], &store);
    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        document["data"]["proposals"]["skipped_omitted_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let visible_skips = document["data"]["proposals"]["skipped"].as_array().unwrap();
    let omitted_unknown = unavailable_targets
        .iter()
        .find(|target| {
            let target = target.to_string_lossy();
            !visible_skips
                .iter()
                .any(|row| row["target_path"].as_str() == Some(target.as_ref()))
        })
        .expect("at least one unknown skip should be omitted from bounded JSON");
    assert!(stderr.contains(&omitted_unknown.to_string_lossy().to_string()));

    for target in reviewable_targets {
        let _ = fs::remove_file(&target);
        let _ = fs::remove_dir(target.parent().unwrap());
    }
    let _ = fs::remove_dir(project_root);
    let _ = fs::remove_file(store);
}

#[test]
fn optimize_print_formats_report_diff_skips_consistently() {
    let (store, target, project_root) =
        rendered_diff_store_with_content("token=synthetic-redaction-value\n");
    let surfaces = (0..50)
        .map(|index| Surface {
            id: format!("synthetic-unavailable-{index:02}"),
            kind: SurfaceKind::Skill,
            name: format!("synthetic-unavailable-{index:02}"),
            path: Some(project_root.join(format!("000-skip-{index:02}.md"))),
            scope: SurfaceScope::Global,
            enabled: Some(true),
            load_mode: SurfaceLoadMode::OnDemand,
            static_bytes: Some(64),
            startup_bytes: Some(0),
            observed_uses: 0,
            observed_sessions: 0,
            usage_state: SurfaceUsageState::Unknown,
            limitations: vec!["synthetic usage evidence is unavailable".to_owned()],
        })
        .collect::<Vec<_>>();
    Store::open(&store)
        .unwrap()
        .replace_surfaces(&surfaces)
        .unwrap();

    for args in [
        &["optimize", "--print"][..],
        &["optimize", "--print", "--format", "markdown"][..],
    ] {
        let output = run_args(args, &store);
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let target_line = format!("  target: {}", target.display());
        let opportunity_lines = stdout
            .lines()
            .skip_while(|line| *line != target_line)
            .take(8)
            .collect::<Vec<_>>();
        assert!(
            opportunity_lines
                .iter()
                .any(|line| line.contains("Proposal status: skipped")),
            "proposal status was not skipped for {target_line}"
        );
        assert!(
            opportunity_lines
                .iter()
                .any(|line| { line.contains("Skip reason:") && line.contains("redaction") }),
            "redaction skip reason was missing for {target_line}"
        );
        assert!(!stdout.contains("synthetic-redaction-value"));
    }

    let output = run_args(&["optimize", "--print", "--format", "json"], &store);
    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["command"], "optimize");
    let proposals = &document["data"]["proposals"];
    let counts = &document["data"]["counts"];
    let rendered_count = proposals["rendered"].as_array().unwrap().len() as u64
        + proposals["rendered_omitted_count"].as_u64().unwrap();
    let skipped_count = proposals["skipped"].as_array().unwrap().len() as u64
        + proposals["skipped_omitted_count"].as_u64().unwrap();
    assert_eq!(counts["reviewable_proposal_count"], rendered_count);
    assert_eq!(
        counts["reviewable_proposal_omitted_count"],
        proposals["rendered_omitted_count"]
    );
    assert_eq!(counts["skipped_proposal_count"], skipped_count);
    assert_eq!(
        counts["skipped_proposal_omitted_count"],
        proposals["skipped_omitted_count"]
    );
    let redacted_opportunity = document["data"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| {
            finding["investigation"]["inspect_target"] == target.to_string_lossy().as_ref()
        })
        .expect("redacted proposal should remain linked to its optimize finding");
    assert_eq!(
        redacted_opportunity["investigation"]["proposal_status"],
        "skipped"
    );
    assert!(
        redacted_opportunity["investigation"]["skip_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("redaction"))
    );
    assert!(
        proposals["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["target_path"] != target.to_string_lossy().as_ref())
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&target.to_string_lossy().to_string()));
    assert!(stderr.contains("redaction"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-redaction-value"));

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_print_json_lists_normalized_skip_limitation_once() {
    let (store, target, project_root) = rendered_diff_store_with_content("token=diff-secret\n");
    let output = run_args(&["optimize", "--print", "--format", "json"], &store);
    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    let skipped = document["data"]["proposals"]["skipped"].as_array().unwrap();
    let redaction_reason = skipped
        .iter()
        .find_map(|row| {
            let reason = row["reason"].as_str()?;
            reason.contains("redaction").then_some(reason)
        })
        .expect("normalized redaction skip should be listed");
    let limitations = document["data"]["limitations"].as_array().unwrap();
    assert_eq!(
        limitations
            .iter()
            .filter(|limitation| limitation.as_str() == Some(redaction_reason))
            .count(),
        1
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("diff-secret"));

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_keeps_ambiguous_same_target_skips_individual() {
    let store = fixture_store();
    let target = PathBuf::from("/synthetic/shared/AGENTS.md");
    Store::open(&store)
        .unwrap()
        .replace_surfaces(&[
            Surface {
                id: "synthetic-skip-a".to_owned(),
                kind: SurfaceKind::Instruction,
                name: "shared-a".to_owned(),
                path: Some(target.clone()),
                scope: SurfaceScope::Project(PathBuf::from("/synthetic/shared")),
                enabled: Some(true),
                load_mode: SurfaceLoadMode::StartupFull,
                static_bytes: Some(64),
                startup_bytes: Some(64),
                observed_uses: 0,
                observed_sessions: 1,
                usage_state: SurfaceUsageState::Unused,
                limitations: Vec::new(),
            },
            Surface {
                id: "synthetic-skip-b".to_owned(),
                kind: SurfaceKind::Instruction,
                name: "shared-b".to_owned(),
                path: Some(target.clone()),
                scope: SurfaceScope::Project(PathBuf::from("/synthetic/shared")),
                enabled: Some(true),
                load_mode: SurfaceLoadMode::StartupFull,
                static_bytes: Some(64),
                startup_bytes: Some(64),
                observed_uses: 0,
                observed_sessions: 1,
                usage_state: SurfaceUsageState::Unused,
                limitations: Vec::new(),
            },
        ])
        .unwrap();

    let human = run_args(&["optimize", "--print"], &store);
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    let reason = "Skipped /synthetic/shared/AGENTS.md: configuration opportunity is actionable but the configured surface has no readable stored instruction baseline";
    assert_eq!(stdout.matches(reason).count(), 2, "{stdout}");

    let json = parse_json_report(
        &run_args(&["optimize", "--print", "--format", "json"], &store),
        "optimize",
    );
    let skipped = json["data"]["proposals"]["skipped"].as_array().unwrap();
    assert_eq!(
        skipped
            .iter()
            .filter(|row| row["target_path"] == "/synthetic/shared/AGENTS.md")
            .count(),
        2
    );

    let _ = fs::remove_file(store);
}

#[test]
fn optimize_json_keeps_review_only_configuration_proposal_bounded() {
    let (store, target, project_root) =
        rendered_diff_store_with_content("Existing synthetic guidance.\n");
    let config_path = project_root.join("config.toml");
    fs::write(
        &config_path,
        "synthetic configuration declaration = \"redacted\"\n",
    )
    .unwrap();
    Store::open(&store)
        .unwrap()
        .replace_surfaces(&[Surface {
            id: "synthetic-config".to_owned(),
            kind: SurfaceKind::Config,
            name: "synthetic.toml".to_owned(),
            path: Some(config_path.clone()),
            scope: SurfaceScope::Project(project_root.clone()),
            enabled: Some(true),
            load_mode: SurfaceLoadMode::StartupFull,
            static_bytes: Some(64),
            startup_bytes: Some(64),
            observed_uses: 0,
            observed_sessions: 0,
            usage_state: SurfaceUsageState::Unused,
            limitations: Vec::new(),
        }])
        .unwrap();

    let output = run_args(&["optimize", "--diff", "--format", "json"], &store);
    let document = parse_json_report(&output, "optimize_diff");
    let skipped = document["data"]["skipped"].as_array().unwrap();
    let configuration = skipped
        .iter()
        .find(|entry| entry["proposal"]["review_only"] == true)
        .expect("configuration proposal should be present as a review-only skip");
    assert_eq!(configuration["proposal"]["action"], "remove");
    assert!(configuration["proposal"]["expected_target_hash"].is_string());
    assert!(configuration["proposal"]["existing_text"].is_null());
    assert!(configuration["proposal"]["proposed_text"].is_null());
    assert!(
        configuration["reason"]
            .as_str()
            .unwrap()
            .contains("review-only")
    );
    assert!(
        !output
            .stdout
            .windows("redacted".len())
            .any(|window| { window == "redacted".as_bytes() })
    );

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_file(config_path);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_json_contains_typed_proposals_and_keeps_skips_in_document() {
    let (store, target, project_root) = rendered_diff_store();
    let output = run_args(&["optimize", "--diff", "--format", "json"], &store);
    let document = parse_json_report(&output, "optimize_diff");
    let data = document["data"].as_object().unwrap();
    assert!(data["rendered"].is_array());
    assert!(data["skipped"].is_array());
    let rendered = &data["rendered"].as_array().unwrap()[0];
    assert!(rendered["diff"].is_string());
    for field in [
        "target_scope",
        "target_path",
        "action",
        "observed_problem",
        "evidence_count",
        "distinct_sessions",
        "confidence",
        "heuristic",
        "evidence",
        "proposed_text",
        "existing_text",
        "source_path",
        "expected_target_hash",
        "expected_source_hash",
        "target_rationale",
        "limitations",
        "review_reminder",
        "verification",
    ] {
        assert!(
            rendered["proposal"].get(field).is_some(),
            "missing proposal field {field}"
        );
    }
    assert_eq!(
        rendered["proposal"]["action"], "add",
        "unexpected proposal action"
    );
    for skipped in data["skipped"].as_array().unwrap() {
        for field in ["target_path", "reason", "proposal"] {
            assert!(
                skipped.get(field).is_some(),
                "missing skipped field {field}"
            );
        }
    }
    let typed: KnownOptimizeDocument = serde_json::from_value(document.clone()).unwrap();
    assert_eq!(typed.schema_version, 1);
    assert_eq!(typed.command, "optimize_diff");
    assert!(!typed.data.rendered.is_empty());
    for rendered in &typed.data.rendered {
        assert!(!rendered.diff.is_empty());
        assert_known_proposal(&rendered.proposal);
    }
    for skipped in &typed.data.skipped {
        let _ = (&skipped.target_path, &skipped.reason);
        if let Some(proposal) = &skipped.proposal {
            assert_known_proposal(proposal);
        }
    }

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_json_bounds_and_redacts_large_diff_content() {
    let content = format!(
        "synthetic guidance\n{}",
        "synthetic guidance\n".repeat(4_000)
    );
    let (store, target, project_root) = rendered_diff_store_with_content(&content);
    let output = run_args(&["optimize", "--diff", "--format", "json"], &store);
    let mut document = parse_json_report(&output, "optimize_diff");
    assert!(document["data"]["rendered"].as_array().unwrap().is_empty());
    assert!(
        document["data"]["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("16384-byte JSON limit"))
            }),
        "unexpected optimize diff skips: {}",
        document["data"]["skipped"]
    );
    let oversized_index = document["data"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .position(|entry| {
            entry["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("16384-byte JSON limit"))
        })
        .unwrap();
    document["data"]["skipped"][oversized_index]["future_optional"] = json!(true);
    document["data"]["skipped"][oversized_index]["proposal"]["future_optional"] = json!(true);
    document["data"]["skipped"][oversized_index]["proposal"]["target_scope"]["future_optional"] =
        json!(true);
    document["data"]["skipped"][oversized_index]["proposal"]["evidence"][0]["future_optional"] =
        json!(true);
    document["data"]["skipped"][oversized_index]["proposal"]["evidence"][0]["source"]["future_optional"] =
        json!(true);
    let typed: KnownOptimizeDocument = serde_json::from_value(document.clone()).unwrap();
    assert!(typed.data.rendered.is_empty());
    assert!(typed.data.skipped.iter().any(|skipped| {
        skipped.reason.contains("16384-byte JSON limit")
            && skipped.proposal.as_ref().is_some_and(|proposal| {
                assert_known_proposal(proposal);
                true
            })
    }));

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_json_omits_redacted_diff_without_leaking_secret() {
    let (store, target, project_root) = rendered_diff_store_with_content("token=diff-secret\n");
    let output = run_args(&["optimize", "--diff", "--format", "json"], &store);
    let mut document = parse_json_report(&output, "optimize_diff");
    assert!(document["data"]["rendered"].as_array().unwrap().is_empty());
    assert!(
        document["data"]["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("redaction"))
                    && entry["proposal"].is_object()
                    && entry["proposal"]["evidence"].is_array()
            })
    );
    let redacted_index = document["data"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .position(|entry| {
            entry["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("redaction"))
        })
        .unwrap();
    document["data"]["skipped"][redacted_index]["future_optional"] = json!(true);
    document["data"]["skipped"][redacted_index]["proposal"]["future_optional"] = json!(true);
    document["data"]["skipped"][redacted_index]["proposal"]["target_scope"]["future_optional"] =
        json!(true);
    document["data"]["skipped"][redacted_index]["proposal"]["evidence"][0]["future_optional"] =
        json!(true);
    document["data"]["skipped"][redacted_index]["proposal"]["evidence"][0]["source"]["future_optional"] =
        json!(true);
    let typed: KnownOptimizeDocument = serde_json::from_value(document.clone()).unwrap();
    assert!(typed.data.rendered.is_empty());
    assert!(typed.data.skipped.iter().any(|skipped| {
        skipped.reason.contains("redaction")
            && skipped.proposal.as_ref().is_some_and(|proposal| {
                assert_known_proposal(proposal);
                true
            })
    }));
    assert!(
        !output
            .stdout
            .windows("diff-secret".len())
            .any(|window| { window == "diff-secret".as_bytes() })
    );

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn optimize_human_omits_redacted_diff_without_leaking_secret() {
    let (store, target, project_root) = rendered_diff_store_with_content("token=diff-secret\n");
    let output = run_args(&["optimize", "--diff"], &store);
    assert!(
        output.status.success(),
        "optimize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Diff omitted: redaction is required"),
        "{stdout}"
    );
    assert!(!stdout.contains("diff-secret"), "{stdout}");
    assert!(!stderr.contains("diff-secret"), "{stderr}");

    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn json_errors_stay_on_stderr_for_every_reporting_command() {
    let store = temp_store_path("missing-json");
    for args in [
        &["analyze", "--format", "json"][..],
        &["sessions", "--format", "json"][..],
        &["failures", "--format", "json"][..],
        &["corrections", "--format", "json"][..],
        &["rework", "--format", "json"][..],
        &["stuck", "--format", "json"][..],
        &["verification", "--format", "json"][..],
        &["knowledge", "--format", "json"][..],
        &["rediscovery", "--format", "json"][..],
        &["instructions", "--format", "json"][..],
        &["doctor", "--format", "json"][..],
        &["optimize", "--diff", "--format", "json"][..],
    ] {
        let output = run_args(args, &store);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(output.stdout.is_empty(), "{args:?} wrote to stdout");
        assert!(!output.stderr.is_empty(), "{args:?} omitted the error");
        assert!(output.stderr.len() < 512, "{args:?} error is unbounded");
        assert!(
            !output.stderr.starts_with(b"{"),
            "{args:?} wrote JSON to stderr"
        );
    }
}

#[test]
fn doctor_report_public_struct_literal_remains_compatible() {
    let report = DoctorReport {
        period_start: None,
        period_end: None,
        session_count: 0,
        freshness: codexlens::store::StoreFreshness::recorded(0, None),
        finding_counts: BTreeMap::new(),
        groups: Vec::new(),
    };

    assert_eq!(report.session_count, 0);
}

#[test]
fn json_schema_readers_can_ignore_unknown_optional_fields() {
    let store = fixture_store();
    let mut document = parse_json_report(
        &run_args(&["analyze", "--format", "json"], &store),
        "analyze",
    );
    document["future_optional"] = json!({"new_field": true});
    document["data"]["future_optional"] = json!("ignored");
    document["data"]["groups"][0]["future_optional"] = json!(true);
    document["data"]["groups"][0]["scope"]["future_optional"] = json!(true);
    document["data"]["groups"][0]["findings"][0]["future_optional"] = json!(true);
    document["data"]["groups"][0]["findings"][0]["scope"]["future_optional"] = json!(true);
    document["data"]["groups"][0]["findings"][0]["evidence"][0]["future_optional"] = json!(true);
    document["data"]["groups"][0]["findings"][0]["evidence"][0]["source"]["future_optional"] =
        json!(true);
    let decoded: KnownFindingDocument = serde_json::from_value(document).unwrap();
    assert_eq!(decoded.schema_version, 1);
    assert_eq!(decoded.command, "analyze");
    assert!(decoded.data.period_start.is_some());
    assert!(decoded.data.period_end.is_some());
    assert_eq!(decoded.data.session_count, 2);
    assert_eq!(decoded.data.freshness.state, "recorded");
    assert!(decoded.data.freshness.source_count > 0);
    assert!(decoded.data.freshness.latest_ingested_at.is_some());
    assert_known_coverage(&decoded.data.coverage);
    assert!(decoded.data.coverage.valid_activity_timestamps > 0);
    assert!(decoded.data.coverage.record_count > 0);
    assert!(!decoded.data.finding_counts.is_empty());
    assert!(!decoded.data.groups.is_empty());
    assert!(decoded.data.groups.iter().any(|group| {
        let _ = (&group.scope.kind, &group.scope.value);
        group.findings.iter().any(|finding| {
            let _ = (
                &finding.kind,
                &finding.severity,
                &finding.confidence,
                &finding.scope.kind,
                &finding.scope.value,
                &finding.key,
                &finding.summary,
                finding.occurrences,
                finding.distinct_sessions,
                &finding.affected_paths,
                &finding.observed_commands,
                &finding.sequence,
                &finding.suggested_action,
                &finding.limitations,
                &finding.verification_status,
                &finding.heuristic,
            );
            finding.evidence.iter().any(|evidence| {
                let source = &evidence.source;
                let _ = (
                    &evidence.session_id,
                    &source.kind,
                    &source.path,
                    &source.line,
                    &source.ingested_at,
                    source.parser_schema_version,
                    &evidence.role,
                    &evidence.excerpt,
                );
                true
            })
        })
    }));
    let _ = fs::remove_file(store);
}

#[test]
fn json_schema_readers_cover_sessions_and_optimize_shapes() {
    let sessions_store = fixture_store();
    let mut sessions_document = parse_json_report(
        &run_args(&["sessions", "--format", "json"], &sessions_store),
        "sessions",
    );
    sessions_document["future_optional"] = json!(true);
    sessions_document["data"]["future_optional"] = json!("ignored");
    sessions_document["data"]["rows"][0]["future_optional"] = json!(false);
    let sessions: KnownSessionsDocument = serde_json::from_value(sessions_document).unwrap();
    assert_eq!(sessions.schema_version, 1);
    assert_eq!(sessions.command, "sessions");
    assert!(!sessions.data.rows.is_empty());
    for session in &sessions.data.rows {
        let _ = (
            &session.id,
            &session.created_at,
            &session.updated_at,
            &session.cwd,
            &session.project,
        );
    }
    let _ = fs::remove_file(sessions_store);

    let (store, target, project_root) = rendered_diff_store();
    let mut optimize_document = parse_json_report(
        &run_args(&["optimize", "--diff", "--format", "json"], &store),
        "optimize_diff",
    );
    optimize_document["future_optional"] = json!(true);
    optimize_document["data"]["future_optional"] = json!("ignored");
    optimize_document["data"]["rendered"][0]["future_optional"] = json!(1);
    optimize_document["data"]["rendered"][0]["proposal"]["future_optional"] = json!(2);
    optimize_document["data"]["rendered"][0]["proposal"]["evidence"][0]["future_optional"] =
        json!(3);
    if !optimize_document["data"]["skipped"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        optimize_document["data"]["skipped"][0]["future_optional"] = json!(4);
    }
    let optimize: KnownOptimizeDocument = serde_json::from_value(optimize_document).unwrap();
    assert_eq!(optimize.schema_version, 1);
    assert_eq!(optimize.command, "optimize_diff");
    assert!(!optimize.data.rendered.is_empty());
    for rendered in &optimize.data.rendered {
        assert!(!rendered.diff.is_empty());
        assert_known_proposal(&rendered.proposal);
    }
    for skipped in &optimize.data.skipped {
        let _ = (&skipped.target_path, &skipped.reason);
        if let Some(proposal) = &skipped.proposal {
            assert_known_proposal(proposal);
        }
    }
    let pre_render_store = fixture_store();
    let pre_render_document = parse_json_report(
        &run_args(
            &["optimize", "--diff", "--format", "json"],
            &pre_render_store,
        ),
        "optimize_diff",
    );
    let pre_render: KnownOptimizeDocument = serde_json::from_value(pre_render_document).unwrap();
    assert!(pre_render.data.rendered.is_empty());
    assert!(!pre_render.data.skipped.is_empty());
    assert!(
        pre_render
            .data
            .skipped
            .iter()
            .all(|skipped| skipped.proposal.is_none())
    );
    let _ = fs::remove_file(pre_render_store);
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}

#[test]
fn empty_json_reports_keep_nullable_fields_and_empty_arrays() {
    let store = empty_store();
    for args in [
        &["analyze", "--format", "json"][..],
        &["sessions", "--format", "json"][..],
        &["optimize", "--diff", "--format", "json"][..],
    ] {
        let document = parse_json_report(
            &run_args(args, &store),
            match args[0] {
                "optimize" => "optimize_diff",
                command => command,
            },
        );
        let data = &document["data"];
        match args[0] {
            "analyze" => {
                assert!(data["period_start"].is_null());
                assert!(data["period_end"].is_null());
                assert_eq!(data["session_count"], 0);
                assert_eq!(data["freshness"]["state"], "empty");
                assert!(data["freshness"]["latest_ingested_at"].is_null());
                assert_eq!(data["groups"], Value::Array(Vec::new()));
            }
            "sessions" => assert_eq!(data["rows"], Value::Array(Vec::new())),
            "optimize" => {
                assert_eq!(data["rendered"], Value::Array(Vec::new()));
                assert_eq!(data["skipped"], Value::Array(Vec::new()));
            }
            _ => unreachable!(),
        }
    }
    let _ = fs::remove_file(store);
}

#[test]
fn optimize_apply_requires_explicit_confirmation() {
    let (store, target, project_root) = rendered_diff_store();
    let before = fs::read(&store).unwrap();
    let output = run_args(&["optimize", "--apply"], &store);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
    assert_eq!(fs::read(&store).unwrap(), before);
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(target);
    let _ = fs::remove_dir(project_root);
}
