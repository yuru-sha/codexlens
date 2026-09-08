use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use codexlens::advisor::DoctorReport;
use codexlens::discovery::{DiscoveredInput, InputKind, ReaderKind};
use codexlens::model::{
    DiagnosticKind, InstructionFile, InstructionFileKind, InstructionFileState, InstructionScope,
    ProjectRootStatus,
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
    freshness: KnownFreshness,
    coverage: KnownCoverage,
    sessions: Vec<KnownSession>,
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
    run_args_with_flags(args, &[], store)
}

fn run_args_with_flags(args: &[&str], flags: &[&str], store: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .args(args)
        .args(flags)
        .arg("--store")
        .arg(store)
        .stdin(Stdio::null())
        .output()
        .unwrap()
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

fn human_finding_counts(stdout: &str) -> BTreeMap<String, usize> {
    let Some(value) = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Finding counts: "))
    else {
        panic!("human report has no finding counts: {stdout}");
    };
    if value == "none" {
        return BTreeMap::new();
    }
    value
        .split(", ")
        .map(|entry| {
            let (kind, count) = entry.split_once('=').unwrap();
            (kind.to_owned(), count.parse().unwrap())
        })
        .collect()
}

fn human_finding_order(stdout: &str) -> Vec<String> {
    let mut scope = String::new();
    let mut findings = Vec::new();
    for line in stdout.lines() {
        if line.starts_with('[') {
            scope = line.to_owned();
        } else if let Some(finding) = line.strip_prefix("- ") {
            let classification = finding.split_once(':').unwrap().0;
            findings.push(format!("{scope}|{classification}"));
        }
    }
    findings
}

fn human_evidence_refs(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("  evidence: "))
        .map(|value| value.split_once(" — ").map_or(value, |(source, _)| source))
        .map(str::to_owned)
        .collect()
}

fn json_scope_label(scope: &Value) -> String {
    let kind = scope["kind"].as_str().unwrap();
    match scope.get("value").and_then(Value::as_str) {
        Some(value) => format!("[{kind}:{value}]"),
        None => format!("[{kind}]"),
    }
}

fn json_finding_counts(document: &Value) -> BTreeMap<String, usize> {
    document["data"]["finding_counts"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(kind, count)| (kind.clone(), count.as_u64().unwrap() as usize))
        .collect()
}

fn json_finding_order(document: &Value) -> Vec<String> {
    document["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| {
            let scope = json_scope_label(&group["scope"]);
            group["findings"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |finding| {
                    format!(
                        "{scope}|{} / {} / {}",
                        finding["kind"].as_str().unwrap(),
                        finding["severity"].as_str().unwrap(),
                        finding["confidence"].as_str().unwrap()
                    )
                })
        })
        .collect()
}

fn json_evidence_refs(document: &Value) -> Vec<String> {
    document["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["findings"].as_array().unwrap())
        .flat_map(|finding| finding["evidence"].as_array().unwrap())
        .map(|evidence| {
            let source = &evidence["source"];
            match source["line"].as_u64() {
                Some(line) => format!("{}:{line}", source["path"].as_str().unwrap()),
                None => source["path"].as_str().unwrap().to_owned(),
            }
        })
        .collect()
}

fn rendered_diff_store() -> (PathBuf, PathBuf, PathBuf) {
    rendered_diff_store_with_content("Existing synthetic guidance.\n")
}

fn rendered_diff_store_with_content(content: &str) -> (PathBuf, PathBuf, PathBuf) {
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
    assert!(stdout.contains("Sessions: 1"), "{stdout}");
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
    assert!(stdout.contains("Sessions: 0"), "{stdout}");
    assert!(!stdout.contains("fixture-analysis-session-"), "{stdout}");
    assert!(stdout.contains("Coverage: empty"), "{stdout}");
    let _ = fs::remove_file(store);
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
    assert!(String::from_utf8_lossy(&monitor.stderr).contains("monitor"));
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
    let coverage = &document["data"]["coverage"];
    assert_eq!(coverage["unknown_timestamp_records"], 1);
    assert!(coverage["unknown_timestamp_events"].as_u64().unwrap() > 0);
    assert_eq!(coverage["state"], "partial");
    assert!(coverage["observed_start"].is_string());
    assert!(coverage["observed_end"].is_string());

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

    for command in [
        "sessions",
        "failures",
        "corrections",
        "rework",
        "verification",
        "knowledge",
        "instructions",
        "doctor",
    ] {
        let mut args = vec![command, "--format", "json"];
        args.extend(flags);
        let document = parse_json_report(&run_args(&args, &store), command);
        let coverage = &document["data"]["coverage"];
        assert_eq!(coverage["requested_start"], "2026-01-03T00:00:00.000Z");
        assert_eq!(coverage["requested_end"], "2026-01-04T00:00:00.000Z");
        assert_eq!(coverage["included_sessions"], 1);
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
fn readiness_document_tracks_phase5_completion_and_phase6_entry_condition() {
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
    assert!(readme.contains("explicit raw-input workflow"));
    assert!(readme.contains("never refreshes implicitly"));
    assert!(readme.contains("Phase 5 compressed rollout reader milestone"));
    assert!(readme.contains("Phase 5 safe optimize apply"));
    assert!(readme.contains("Phase 6"));
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
        "archive inclusion",
        "retention/deletion policy",
        "exact code version",
        "resolved interval",
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
        (&["stuck", "--format", "json"][..], "rework"),
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
        if command == "sessions" {
            assert!(data["freshness"].is_object());
            assert!(data["sessions"].is_array());
            for session in data["sessions"].as_array().unwrap() {
                for field in ["id", "created_at", "updated_at", "cwd", "project"] {
                    assert!(
                        session.get(field).is_some(),
                        "missing session field {field}"
                    );
                }
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

    assert_eq!(
        human_finding_counts(&human_stdout),
        json_finding_counts(&document)
    );
    assert_eq!(
        human_finding_order(&human_stdout),
        json_finding_order(&document)
    );
    assert_eq!(
        human_evidence_refs(&human_stdout),
        json_evidence_refs(&document)
    );
    let _ = fs::remove_file(store);
}

#[test]
fn reporting_metadata_exposes_store_coverage_and_separates_ingestion_time() {
    let store = fixture_store();
    let human = run_args(&["doctor"], &store);
    assert!(human.status.success());
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(human_stdout.contains(
        "Coverage: selected store (partial; not necessarily all historical activity or current raw inputs; refresh explicitly, archives via --include-archived)"
    ));
    assert!(
        human_stdout.contains("Activity: 2026-01-03T00:00:00.000Z .. 2026-01-04T00:05:00.000Z")
    );
    assert!(human_stdout.contains("Records: 16"), "{human_stdout}");
    assert!(
        human_stdout.contains("Latest ingestion: "),
        "{human_stdout}"
    );

    let document = parse_json_report(&run_args(&["doctor", "--format", "json"], &store), "doctor");
    let coverage = &document["data"]["coverage"];
    assert_eq!(coverage["scope"], "selected_store");
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
        document["data"]["freshness"]["latest_ingested_at"]
    );

    let sessions = parse_json_report(
        &run_args(&["sessions", "--format", "json"], &store),
        "sessions",
    );
    assert_eq!(sessions["data"]["coverage"], coverage.clone());
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
            let coverage = &document["data"]["coverage"];
            assert_eq!(coverage["status"], "empty", "{args:?}");
            assert!(coverage["activity_start"].is_null(), "{args:?}");
            assert!(coverage["activity_end"].is_null(), "{args:?}");
            assert_eq!(coverage["session_count"], 0, "{args:?}");
            assert_eq!(coverage["record_count"], 0, "{args:?}");
            assert!(document["data"]["freshness"]["latest_ingested_at"].is_null());
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("Coverage: selected store (empty;"),
                "{stdout}"
            );
            assert!(stdout.contains("Activity: unknown"), "{stdout}");
            assert!(stdout.contains("Latest ingestion: unknown"), "{stdout}");
        }
    }
    let _ = fs::remove_file(store);
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
    sessions_document["data"]["freshness"]["future_optional"] = json!(1);
    sessions_document["data"]["sessions"][0]["future_optional"] = json!(false);
    let sessions: KnownSessionsDocument = serde_json::from_value(sessions_document).unwrap();
    assert_eq!(sessions.schema_version, 1);
    assert_eq!(sessions.command, "sessions");
    assert_eq!(sessions.data.freshness.state, "recorded");
    assert_known_coverage(&sessions.data.coverage);
    assert!(sessions.data.coverage.session_count > 0);
    assert!(sessions.data.coverage.record_count > 0);
    assert!(!sessions.data.sessions.is_empty());
    for session in &sessions.data.sessions {
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
            "sessions" => assert_eq!(data["sessions"], Value::Array(Vec::new())),
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
