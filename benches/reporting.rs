use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use codexlens::model::{
    CanonicalData, FileOperation, InstructionSnapshot, InstructionSnapshotAccuracy,
    InstructionSnapshotSource, Message, MessageRole, OutcomeSource, Record, RecordKind, Session,
    SourceRef, ToolCall, ToolOutcome, ToolResult,
};
use codexlens::store::Store;
use rusqlite::params;

const SESSION_COUNT: usize = 500;
const RECORD_COUNT: usize = 200_000;
const CALL_COUNT: usize = 50_000;
const MESSAGE_COUNT: usize = 10_000;
const FILE_OPERATION_COUNT: usize = 1_661;
const SNAPSHOT_COUNT: usize = 1_650;
const FAILURE_OUTPUT_BYTES: usize = 1_024;
const TARGET_MS: u128 = 5_000;
const MAX_DOCTOR_JSON_BYTES: usize = 64 * 1024;
const MAX_REPORT_JSON_BYTES: usize = 256 * 1024;
const REPORTING_COMMANDS: &[&str] = &[
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
    "doctor",
];

fn source(session: usize, line: usize) -> SourceRef {
    SourceRef::rollout(
        PathBuf::from(format!("/synthetic/session-{session}.jsonl")),
        line,
    )
}

fn synthetic_failure_output() -> String {
    let mut output = "synthetic failure: permission denied ".to_owned();
    output.push_str(&"x".repeat(FAILURE_OUTPUT_BYTES.saturating_sub(output.len())));
    output
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
        messages: Vec::with_capacity(MESSAGE_COUNT),
        tool_calls: Vec::with_capacity(CALL_COUNT),
        tool_results: Vec::with_capacity(CALL_COUNT),
        file_operations: Vec::with_capacity(FILE_OPERATION_COUNT),
        instruction_snapshots: Vec::with_capacity(SNAPSHOT_COUNT),
        ..CanonicalData::default()
    };

    for index in 0..RECORD_COUNT {
        let session = index % SESSION_COUNT;
        let line = index / SESSION_COUNT + 1;
        let turn_id = (line == 1).then(|| format!("synthetic-turn-{session}"));
        data.records.push(Record {
            session_id: Some(format!("synthetic-session-{session}")),
            turn_id,
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

    let failure_output = synthetic_failure_output();
    for index in 0..CALL_COUNT {
        let session = index % SESSION_COUNT;
        let line = index / SESSION_COUNT + 1;
        let session_id = format!("synthetic-session-{session}");
        let call_id = format!("synthetic-call-{index}");
        let turn_id = (line == 1).then(|| format!("synthetic-turn-{session}"));
        let provenance = source(session, line);
        data.tool_calls.push(ToolCall {
            id: Some(call_id.clone()),
            call_id: Some(call_id.clone()),
            session_id: Some(session_id.clone()),
            turn_id: turn_id.clone(),
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
            turn_id,
            command: Some("cargo test".to_owned()),
            cwd: Some("/synthetic/project".to_owned()),
            stdout: None,
            stderr: Some(failure_output.clone()),
            duration_ms: None,
            exit_code: None,
            status: None,
            outcome: ToolOutcome::Failed,
            outcome_source: OutcomeSource::OutputText,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance,
        });
    }

    for index in 0..MESSAGE_COUNT {
        let session = index % SESSION_COUNT;
        let message_number = index / SESSION_COUNT;
        let is_user = message_number % 2 == 1;
        data.messages.push(Message {
            id: Some(format!("synthetic-message-{index}")),
            session_id: Some(format!("synthetic-session-{session}")),
            turn_id: None,
            role: Some(if is_user {
                MessageRole::User
            } else {
                MessageRole::Assistant
            }),
            content: Some(if is_user {
                "Remember that cargo test is required.".to_owned()
            } else {
                "Synthetic assistant action.".to_owned()
            }),
            timestamp: None,
            provenance: source(session, 1_000 + message_number),
        });
    }

    for index in 0..FILE_OPERATION_COUNT {
        let session = if index < 2 { 0 } else { index % SESSION_COUNT };
        let path = if index < 2 {
            "src/retry.rs".to_owned()
        } else {
            format!("src/synthetic-{index}.rs")
        };
        let timestamp = match index {
            0 => Some("2026-01-01T00:02:00Z".to_owned()),
            1 => Some("2026-01-01T00:03:00Z".to_owned()),
            _ => None,
        };
        data.file_operations.push(FileOperation {
            session_id: Some(format!("synthetic-session-{session}")),
            turn_id: None,
            path,
            operation: "edit".to_owned(),
            timestamp,
            provenance: source(session, 2_000 + index / SESSION_COUNT),
        });
    }

    let snapshot_content = "Remember that cargo test is required.".to_owned();
    let snapshot_hash = codexlens::instructions::content_hash(snapshot_content.as_bytes());
    for index in 0..SNAPSHOT_COUNT {
        let session = index % SESSION_COUNT;
        let occurrence = index / SESSION_COUNT;
        let line = if occurrence == 1 {
            1_001
        } else {
            9_000 + occurrence
        };
        data.instruction_snapshots.push(InstructionSnapshot {
            session_id: Some(format!("synthetic-session-{session}")),
            turn_id: Some(format!("synthetic-turn-{session}")),
            source: InstructionSnapshotSource::Rollout,
            accuracy: InstructionSnapshotAccuracy::Observed,
            content: Some(snapshot_content.clone()),
            content_hash: Some(snapshot_hash.clone()),
            byte_count: snapshot_content.len(),
            chain: Vec::new(),
            effective_chain_hash: Some(snapshot_hash.clone()),
            truncated: false,
            provenance: source(session, line),
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
            "INSERT INTO records (record_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, timestamp, sequence, kind, record_type, nested_type, is_error, is_terminal) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, ?8, ?9, 'event_message', 'event_msg', 'synthetic', 0, 0)",
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
                record.turn_id,
                record.timestamp,
                record.sequence,
            ])
            .expect("record persists");
    }
    drop(records);

    let mut messages = transaction
        .prepare(
            "INSERT INTO messages (message_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, message_id, session_id, turn_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, ?8, ?9, ?10, NULL)",
        )
        .expect("message statement prepares");
    for (index, message) in data.messages.iter().enumerate() {
        messages
            .execute(params![
                format!("message-{index}"),
                "synthetic-source",
                message.provenance.path.to_string_lossy(),
                message.provenance.line,
                "2026-01-01T00:01:00Z",
                message.id,
                message.session_id,
                message.turn_id,
                message.role.as_ref().map(|role| role.as_str()),
                message.content,
            ])
            .expect("message persists");
    }
    drop(messages);

    let mut calls = transaction
        .prepare(
            "INSERT INTO tool_calls (call_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, item_id, call_id, session_id, turn_id, tool_name, command, cwd, status) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?6, ?7, ?8, 'exec_command', 'cargo test', '/synthetic/project', 'completed')",
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
                call.turn_id,
            ])
            .expect("call persists");
    }
    drop(calls);

    let mut results = transaction
        .prepare(
            "INSERT INTO tool_results (result_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, result_id, call_id, session_id, turn_id, command, cwd, stdout, stderr, duration_ms, exit_code, status, outcome, outcome_source, matched_call, is_duplicate) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, ?8, ?9, 'cargo test', '/synthetic/project', NULL, ?10, NULL, NULL, NULL, 'failed', 'output_text', 1, 0)",
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
                result.turn_id,
                result.stderr,
            ])
            .expect("result persists");
    }
    drop(results);

    let mut operations = transaction
        .prepare(
            "INSERT INTO file_operations (operation_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, path, operation, timestamp) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, ?8, ?9, ?10)",
        )
        .expect("file operation statement prepares");
    for (index, operation) in data.file_operations.iter().enumerate() {
        operations
            .execute(params![
                format!("operation-{index}"),
                "synthetic-source",
                operation.provenance.path.to_string_lossy(),
                operation.provenance.line,
                "2026-01-01T00:01:00Z",
                operation.session_id,
                operation.turn_id,
                operation.path,
                operation.operation,
                operation.timestamp,
            ])
            .expect("file operation persists");
    }
    drop(operations);

    let snapshot = data
        .instruction_snapshots
        .first()
        .expect("synthetic snapshot exists");
    transaction
        .execute(
            "INSERT INTO instruction_blobs (blob_key, content_hash, byte_count, content) VALUES (?1, ?2, ?3, ?4)",
            params![
                "synthetic-instruction",
                snapshot.content_hash,
                snapshot.byte_count,
                snapshot.content,
            ],
        )
        .expect("instruction blob persists");
    let mut snapshots = transaction
        .prepare(
            "INSERT INTO instruction_snapshots (snapshot_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, snapshot_source, accuracy, blob_key, content_hash, byte_count, effective_chain_hash, truncated, chain_json) VALUES (?1, ?2, ?3, ?4, 'rollout', ?5, 1, ?6, ?7, 'rollout', 'observed', 'synthetic-instruction', ?8, ?9, ?10, 0, ?11)",
        )
        .expect("snapshot statement prepares");
    for (index, snapshot) in data.instruction_snapshots.iter().enumerate() {
        snapshots
            .execute(params![
                format!("snapshot-{index}"),
                "synthetic-source",
                snapshot.provenance.path.to_string_lossy(),
                snapshot.provenance.line,
                "2026-01-01T00:01:00Z",
                snapshot.session_id,
                snapshot.turn_id,
                snapshot.content_hash,
                snapshot.byte_count,
                snapshot.effective_chain_hash,
                serde_json::to_string(&snapshot.chain).expect("snapshot chain serializes"),
            ])
            .expect("snapshot persists");
    }
    drop(snapshots);
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
    for kind in ["failure", "correction", "rework", "knowledge"] {
        assert!(
            document["data"]["finding_counts"][kind]
                .as_u64()
                .is_some_and(|count| count > 0),
            "benchmark did not exercise {kind} findings"
        );
    }
    assert!(json.len() <= MAX_DOCTOR_JSON_BYTES);
    assert!(json.ends_with('\n'));
    assert!(elapsed_ms <= TARGET_MS, "doctor took {elapsed_ms} ms");

    let mut command_timings = Vec::new();
    for command in REPORTING_COMMANDS {
        let started = Instant::now();
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
        assert!(
            output.stdout.len() <= MAX_REPORT_JSON_BYTES,
            "{command} JSON is too large: {} bytes",
            output.stdout.len()
        );
        let elapsed_ms = started.elapsed().as_millis();
        assert!(elapsed_ms <= TARGET_MS, "{command} took {elapsed_ms} ms");
        command_timings.push((*command, elapsed_ms));
    }

    let started = Instant::now();
    let output = Command::new(&binary)
        .args([
            "optimize",
            "--diff",
            "--format",
            "json",
            "--store",
            store_path.to_str().expect("synthetic store path is UTF-8"),
        ])
        .output()
        .expect("optimize --diff binary runs");
    assert!(
        output.status.success(),
        "optimize --diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("optimize --diff emits JSON");
    assert_eq!(document["schema_version"], 1);
    assert!(
        output.stdout.len() <= MAX_REPORT_JSON_BYTES,
        "optimize --diff JSON is too large: {} bytes",
        output.stdout.len()
    );
    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= TARGET_MS,
        "optimize --diff took {elapsed_ms} ms"
    );
    command_timings.push(("optimize --diff", elapsed_ms));
    fs::remove_file(&store_path).expect("synthetic store is removable");
    println!(
        "sessions={SESSION_COUNT} records={RECORD_COUNT} messages={MESSAGE_COUNT} file_operations={FILE_OPERATION_COUNT} instruction_snapshots={SNAPSHOT_COUNT} tool_calls={CALL_COUNT} failure_output_bytes={FAILURE_OUTPUT_BYTES} doctor_elapsed_ms={elapsed_ms} focused_timings={command_timings:?} json_bytes={}",
        json.len()
    );
}
