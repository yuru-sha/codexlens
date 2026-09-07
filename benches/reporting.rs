use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use codexlens::model::{
    CanonicalData, OutcomeSource, Record, RecordKind, Session, SourceRef, ToolCall, ToolOutcome,
    ToolResult,
};
use codexlens::store::Store;
use rusqlite::params;

const SESSION_COUNT: usize = 500;
const RECORD_COUNT: usize = 200_000;
const CALL_COUNT: usize = 50_000;
const TARGET_MS: u128 = 5_000;
const MAX_DOCTOR_JSON_BYTES: usize = 64 * 1024;
const REPORTING_COMMANDS: &[&str] = &[
    "analyze",
    "sessions",
    "failures",
    "corrections",
    "rework",
    "verification",
    "knowledge",
    "instructions",
    "doctor",
];

fn source(session: usize, line: usize) -> SourceRef {
    SourceRef::rollout(
        PathBuf::from(format!("/synthetic/session-{session}.jsonl")),
        line,
    )
}

fn synthetic_data() -> CanonicalData {
    let mut data = CanonicalData {
        sessions: (0..SESSION_COUNT)
            .map(|session| Session {
                id: format!("synthetic-session-{session}"),
                created_at: Some("2026-01-01T00:00:00Z".to_owned()),
                updated_at: Some("2026-01-01T00:01:00Z".to_owned()),
                cwd: Some("/synthetic/project".to_owned()),
                project: Some("/synthetic/project".to_owned()),
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
                provenance: source(session, 1),
            })
            .collect(),
        records: Vec::with_capacity(RECORD_COUNT),
        tool_calls: Vec::with_capacity(CALL_COUNT),
        tool_results: Vec::with_capacity(CALL_COUNT),
        ..CanonicalData::default()
    };

    for index in 0..RECORD_COUNT {
        let session = index % SESSION_COUNT;
        let line = index / SESSION_COUNT + 1;
        data.records.push(Record {
            session_id: Some(format!("synthetic-session-{session}")),
            turn_id: None,
            timestamp: None,
            sequence: index,
            original_record_type: Some("event_msg".to_owned()),
            original_nested_type: Some("synthetic".to_owned()),
            error_category: None,
            is_error: false,
            is_terminal: false,
            kind: RecordKind::EventMessage,
            provenance: source(session, line),
        });
    }

    for index in 0..CALL_COUNT {
        let record_index = index * RECORD_COUNT / CALL_COUNT;
        let session = record_index % SESSION_COUNT;
        let line = record_index / SESSION_COUNT + 1;
        let session_id = format!("synthetic-session-{session}");
        let call_id = format!("synthetic-call-{index}");
        let provenance = source(session, line);
        data.tool_calls.push(ToolCall {
            id: Some(call_id.clone()),
            call_id: Some(call_id.clone()),
            session_id: Some(session_id.clone()),
            turn_id: None,
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: Some("/synthetic/project".to_owned()),
            status: Some("completed".to_owned()),
            provenance: provenance.clone(),
        });
        data.tool_results.push(ToolResult {
            id: Some(format!("synthetic-result-{index}")),
            call_id: Some(call_id),
            session_id: Some(session_id),
            turn_id: None,
            command: Some("cargo test".to_owned()),
            cwd: Some("/synthetic/project".to_owned()),
            stdout: None,
            stderr: Some("synthetic failure".to_owned()),
            duration_ms: None,
            exit_code: Some(1),
            status: Some("failed".to_owned()),
            outcome: ToolOutcome::Failed,
            outcome_source: OutcomeSource::ExitCode,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance,
        });
    }

    data
}

fn synthetic_store(data: &CanonicalData, path: &Path) -> Store {
    let store = Store::open(path).expect("synthetic store opens");
    let transaction = store
        .connection()
        .unchecked_transaction()
        .expect("synthetic transaction opens");
    transaction
        .execute(
            "INSERT INTO ingested_files (identity, source_path, input_kind, size, digest) VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["synthetic-source", "/synthetic/source.jsonl", "rollout", 1_i64, "synthetic"],
        )
        .expect("synthetic source persists");

    let mut sessions = transaction
        .prepare(
            "INSERT INTO sessions (session_id, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, created_at, updated_at, cwd, project) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, ?8, ?8)",
        )
        .expect("session statement prepares");
    for session in &data.sessions {
        sessions
            .execute(params![
                session.id,
                "synthetic-source",
                session.provenance.path.to_string_lossy(),
                session.provenance.line,
                "2026-01-01T00:01:00Z",
                session.created_at,
                session.updated_at,
                session.project,
            ])
            .expect("session persists");
    }
    drop(sessions);

    let mut records = transaction
        .prepare(
            "INSERT INTO records (record_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, timestamp, sequence, kind, record_type, nested_type, is_error, is_terminal) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, NULL, NULL, ?7, 'event_message', 'event_msg', 'synthetic', 0, 0)",
        )
        .expect("record statement prepares");
    for (index, record) in data.records.iter().enumerate() {
        records
            .execute(params![
                format!("record-{index}"),
                "synthetic-source",
                record.provenance.path.to_string_lossy(),
                record.provenance.line,
                "2026-01-01T00:01:00Z",
                record.session_id,
                record.sequence,
            ])
            .expect("record persists");
    }
    drop(records);

    let mut calls = transaction
        .prepare(
            "INSERT INTO tool_calls (call_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, item_id, call_id, session_id, turn_id, tool_name, command, cwd, status) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?6, ?7, NULL, 'exec_command', 'cargo test', '/synthetic/project', 'completed')",
        )
        .expect("call statement prepares");
    for (index, call) in data.tool_calls.iter().enumerate() {
        calls
            .execute(params![
                format!("call-{index}"),
                "synthetic-source",
                call.provenance.path.to_string_lossy(),
                call.provenance.line,
                "2026-01-01T00:01:00Z",
                call.call_id,
                call.session_id,
            ])
            .expect("call persists");
    }
    drop(calls);

    let mut results = transaction
        .prepare(
            "INSERT INTO tool_results (result_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, result_id, call_id, session_id, turn_id, command, cwd, stderr, exit_code, status, outcome, outcome_source, matched_call, is_duplicate) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, ?8, NULL, 'cargo test', '/synthetic/project', 'synthetic failure', 1, 'failed', 'failed', 'exit_code', 1, 0)",
        )
        .expect("result statement prepares");
    for (index, result) in data.tool_results.iter().enumerate() {
        results
            .execute(params![
                format!("result-{index}"),
                "synthetic-source",
                result.provenance.path.to_string_lossy(),
                result.provenance.line,
                "2026-01-01T00:01:00Z",
                result.id,
                result.call_id,
                result.session_id,
            ])
            .expect("result persists");
    }
    drop(results);
    transaction.commit().expect("synthetic transaction commits");
    store
}

fn main() {
    let data = synthetic_data();
    let store_path = std::env::temp_dir().join(format!(
        "codexlens-reporting-benchmark-{}.sqlite",
        std::process::id()
    ));
    let store = synthetic_store(&data, &store_path);
    drop(store);
    let binary = std::env::current_exe()
        .expect("benchmark executable path resolves")
        .parent()
        .and_then(|path| path.parent())
        .map(|path| path.join(format!("codexlens{}", std::env::consts::EXE_SUFFIX)))
        .expect("codexlens binary path resolves");
    let started = Instant::now();
    let output = Command::new(&binary)
        .args([
            "doctor",
            "--format",
            "json",
            "--store",
            store_path.to_str().expect("synthetic store path is UTF-8"),
        ])
        .output()
        .expect("doctor binary runs");
    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = String::from_utf8(output.stdout).expect("doctor emits UTF-8 JSON");
    let document: serde_json::Value = serde_json::from_str(&json).expect("synthetic JSON parses");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["data"]["session_count"], SESSION_COUNT);
    assert!(json.len() <= MAX_DOCTOR_JSON_BYTES);
    assert!(json.ends_with('\n'));
    assert!(elapsed_ms <= TARGET_MS, "doctor took {elapsed_ms} ms");

    for command in REPORTING_COMMANDS {
        let output = Command::new(&binary)
            .args([
                *command,
                "--format",
                "json",
                "--store",
                store_path.to_str().expect("synthetic store path is UTF-8"),
            ])
            .output()
            .expect("reporting binary runs");
        assert!(
            output.status.success(),
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("reporting command emits JSON");
        assert_eq!(document["schema_version"], 1);
    }
    fs::remove_file(&store_path).expect("synthetic store is removable");
    println!(
        "sessions={SESSION_COUNT} records={RECORD_COUNT} tool_calls={CALL_COUNT} elapsed_ms={elapsed_ms} json_bytes={}",
        json.len()
    );
}
