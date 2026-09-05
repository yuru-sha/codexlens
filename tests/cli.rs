use std::path::{Path, PathBuf};
use std::process::Command;

use codexlens::rollout::RolloutParseOptions;
use codexlens::store::Store;
use rusqlite::Connection;

fn fixture_store() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-reporting.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
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

fn run(command: &str, store: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codexlens"))
        .arg(command)
        .arg("--store")
        .arg(store)
        .output()
        .unwrap()
}

#[test]
fn reporting_commands_render_local_store_data() {
    let store = fixture_store();
    for command in [
        "analyze",
        "sessions",
        "failures",
        "corrections",
        "rework",
        "stuck",
        "verification",
        "knowledge",
        "rediscovery",
        "instructions",
    ] {
        let output = run(command, &store);
        assert!(
            output.status.success(),
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Store freshness:"), "{command}: {stdout}");
        assert!(
            !stdout.contains("not implemented yet"),
            "{command}: {stdout}"
        );
    }

    let sessions = String::from_utf8_lossy(&run("sessions", &store).stdout).into_owned();
    assert!(sessions.contains("fixture-analysis-session-a"));
    assert_eq!(sessions.matches("- fixture-analysis-session-a").count(), 1);
    let failures = String::from_utf8_lossy(&run("failures", &store).stdout).into_owned();
    assert!(failures.contains("failure="));
    let corrections = String::from_utf8_lossy(&run("corrections", &store).stdout).into_owned();
    assert!(corrections.contains("correction="));

    let _ = std::fs::remove_file(store);
}

#[test]
fn reporting_commands_explain_missing_store() {
    let path = std::env::temp_dir().join(format!(
        "codexlens-cli-{}-missing.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let output = run("analyze", &path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("store does not exist"));
    assert!(stderr.len() < 512);
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

    let output = run("analyze", &path);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("schema version"), "{stderr}");
    assert!(stderr.len() < 512);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let _ = std::fs::remove_file(path);
}
