use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use codexlens::analysis::analyze_default;
use codexlens::model::CanonicalData;
use codexlens::monitor::{LocalMonitor, MonitorClock, MonitorOptions, MonitorStatus};
use codexlens::rollout::RolloutParseOptions;
use codexlens::store::Store;
use rusqlite::Connection;

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

fn temp_source(label: &str) -> PathBuf {
    let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "codexlens-monitor-{label}-{}-{nonce}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    path
}

fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

type RecordSignature = (Option<usize>, usize, Option<String>, Option<String>);

#[test]
fn monitor_holds_a_partial_final_line_until_the_newline_arrives() {
    let source = temp_source("partial");
    let first = br#"{"type":"session_meta","payload":{"id":"monitor-session"}}
"#;
    let second = br#"{"type":"turn_context","payload":{"turn_id":"monitor-turn"}}"#;
    fs::write(&source, first).unwrap();
    append(&source, second);

    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();

    let first_poll = monitor.poll(&mut store).unwrap();
    assert_eq!(first_poll.status, MonitorStatus::PartialLine);
    assert_eq!(first_poll.records, 1);
    assert_eq!(store.load_canonical().unwrap().records.len(), 1);
    assert_eq!(monitor.cursor().offset, first.len() as u64);

    append(&source, b"\n");
    let second_poll = monitor.poll(&mut store).unwrap();
    assert_eq!(second_poll.status, MonitorStatus::Updated);
    assert_eq!(second_poll.records, 1);
    let data = store.load_canonical().unwrap();
    assert_eq!(data.records.len(), 2);
    assert_eq!(
        data.records
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    let _ = fs::remove_file(source);
}

fn record_signature(data: &CanonicalData) -> Vec<RecordSignature> {
    data.records
        .iter()
        .map(|record| {
            (
                record.provenance.line,
                record.sequence,
                record.session_id.clone(),
                record.turn_id.clone(),
            )
        })
        .collect()
}

fn finding_signature(data: &CanonicalData) -> Vec<(String, String, usize, usize)> {
    analyze_default(data)
        .into_iter()
        .map(|finding| {
            (
                finding.kind.as_str().to_owned(),
                finding.key,
                finding.occurrences,
                finding.distinct_sessions,
            )
        })
        .collect()
}

#[test]
fn finite_replay_matches_batch_ordering_and_findings() {
    let source = temp_source("replay");
    fs::write(&source, []).unwrap();
    let mut live_store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();
    let mut expected_source = Vec::new();
    for line in include_str!("fixtures/analysis/lenses.jsonl").lines() {
        expected_source.extend_from_slice(line.as_bytes());
        expected_source.push(b'\n');
        append(&source, line.as_bytes());
        append(&source, b"\n");
        monitor.poll(&mut live_store).unwrap();
    }

    let mut batch_store = Store::in_memory().unwrap();
    batch_store
        .ingest_rollout_file(&source, &RolloutParseOptions::default())
        .unwrap();
    let live = live_store.load_canonical().unwrap();
    let batch = batch_store.load_canonical().unwrap();
    assert_eq!(record_signature(&live), record_signature(&batch));
    assert_eq!(
        live.tool_results
            .iter()
            .map(|result| (
                result.call_id.clone(),
                result.matched_call,
                result.is_duplicate,
            ))
            .collect::<Vec<_>>(),
        batch
            .tool_results
            .iter()
            .map(|result| (
                result.call_id.clone(),
                result.matched_call,
                result.is_duplicate,
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        live.file_operations
            .iter()
            .map(|operation| (
                operation.session_id.clone(),
                operation.turn_id.clone(),
                operation.path.clone(),
                operation.operation.clone(),
            ))
            .collect::<Vec<_>>(),
        batch
            .file_operations
            .iter()
            .map(|operation| (
                operation.session_id.clone(),
                operation.turn_id.clone(),
                operation.path.clone(),
                operation.operation.clone(),
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(finding_signature(&live), finding_signature(&batch));
    assert_eq!(fs::read(&source).unwrap(), expected_source);

    let _ = fs::remove_file(source);
}

#[test]
fn tool_result_matches_a_call_across_poll_boundaries() {
    let source = temp_source("tool-correlation");
    let lines = include_str!("fixtures/rollout/monitoring.jsonl")
        .lines()
        .collect::<Vec<_>>();
    fs::write(&source, []).unwrap();
    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();

    for line in &lines[..4] {
        append(&source, line.as_bytes());
        append(&source, b"\n");
        monitor.poll(&mut store).unwrap();
    }

    let data = store.load_canonical().unwrap();
    assert_eq!(data.tool_calls.len(), 1);
    assert_eq!(data.tool_results.len(), 1);
    assert!(data.tool_results[0].matched_call);

    let _ = fs::remove_file(source);
}

#[test]
fn restarting_from_the_recorded_cursor_does_not_duplicate_complete_events() {
    let source = temp_source("restart");
    let lines = include_str!("fixtures/rollout/monitoring.jsonl")
        .lines()
        .collect::<Vec<_>>();
    fs::write(&source, []).unwrap();
    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();

    for line in &lines[..2] {
        append(&source, line.as_bytes());
        append(&source, b"\n");
        monitor.poll(&mut store).unwrap();
    }
    let cursor = monitor.cursor().clone();
    drop(monitor);

    let mut restarted =
        LocalMonitor::rollout(&source, Some(cursor), MonitorOptions::default()).unwrap();
    for line in &lines[2..] {
        append(&source, line.as_bytes());
        append(&source, b"\n");
        restarted.poll(&mut store).unwrap();
    }

    let data = store.load_canonical().unwrap();
    assert_eq!(data.records.len(), lines.len());
    assert_eq!(
        data.records
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        (1..=lines.len()).collect::<Vec<_>>()
    );

    let _ = fs::remove_file(source);
}

#[test]
fn partial_session_is_not_marked_complete_before_a_terminal_event() {
    let source = temp_source("partial-session");
    let lines = include_str!("fixtures/rollout/monitoring.jsonl")
        .lines()
        .collect::<Vec<_>>();
    fs::write(&source, []).unwrap();
    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();

    for line in &lines[..lines.len() - 1] {
        append(&source, line.as_bytes());
        append(&source, b"\n");
        monitor.poll(&mut store).unwrap();
    }
    let partial = store.load_canonical().unwrap();
    assert_eq!(
        partial
            .turns
            .iter()
            .find(|turn| turn.id == "monitor-turn-001")
            .and_then(|turn| turn.completed_at.as_deref()),
        None
    );

    let last = lines.last().unwrap();
    append(&source, last.as_bytes());
    append(&source, b"\n");
    monitor.poll(&mut store).unwrap();
    let completed = store.load_canonical().unwrap();
    assert!(
        completed
            .turns
            .iter()
            .find(|turn| turn.id == "monitor-turn-001")
            .and_then(|turn| turn.completed_at.as_deref())
            .is_some()
    );

    let _ = fs::remove_file(source);
}

#[test]
fn duplicate_event_identity_is_reported_and_not_ingested_twice() {
    let source = temp_source("duplicate");
    let line = br#"{"type":"session_meta","payload":{"id":"duplicate-session"}}"#;
    fs::write(&source, []).unwrap();
    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();

    append(&source, line);
    append(&source, b"\n");
    monitor.poll(&mut store).unwrap();
    append(&source, line);
    append(&source, b"\n");
    let duplicate = monitor.poll(&mut store).unwrap();

    assert_eq!(duplicate.status, MonitorStatus::DuplicateIdentity);
    assert_eq!(duplicate.skipped_duplicates, 1);
    assert_eq!(duplicate.diagnostics.len(), 1);
    assert_eq!(store.load_canonical().unwrap().records.len(), 1);

    let _ = fs::remove_file(source);
}

#[test]
fn rotation_and_truncation_are_explicit_source_transitions() {
    let source = temp_source("rotation");
    let original = br#"{"type":"session_meta","payload":{"id":"original-session"}}"#;
    let rotated =
        br#"{"type":"session_meta","payload":{"id":"rotated-session-with-a-longer-identity"}}"#;
    fs::write(&source, []).unwrap();
    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();

    append(&source, original);
    append(&source, b"\n");
    monitor.poll(&mut store).unwrap();
    let mut rotated_bytes = rotated.to_vec();
    rotated_bytes.push(b'\n');
    fs::write(&source, rotated_bytes).unwrap();
    let rotation = monitor.poll(&mut store).unwrap();
    assert_eq!(rotation.status, MonitorStatus::Rotated);
    assert_eq!(
        store.load_canonical().unwrap().sessions[0].id,
        "rotated-session-with-a-longer-identity"
    );

    fs::write(&source, []).unwrap();
    let truncation = monitor.poll(&mut store).unwrap();
    assert_eq!(truncation.status, MonitorStatus::Truncated);
    assert!(store.load_canonical().unwrap().records.is_empty());

    let _ = fs::remove_file(source);
}

struct FakeClock {
    waits: Vec<Duration>,
}

impl MonitorClock for FakeClock {
    fn sleep(&mut self, duration: Duration) {
        self.waits.push(duration);
    }
}

#[test]
fn run_uses_an_injected_clock_and_explicit_stop_boundary() {
    let source = temp_source("clock");
    fs::write(
        &source,
        br#"{"type":"session_meta","payload":{"id":"clock-session"}}
"#,
    )
    .unwrap();
    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::rollout(&source, None, MonitorOptions::default()).unwrap();
    let mut clock = FakeClock { waits: Vec::new() };
    let mut polls = 0;
    let cursor = monitor
        .run(&mut store, &mut clock, |_| {
            polls += 1;
            polls == 3
        })
        .unwrap();

    assert_eq!(polls, 3);
    assert_eq!(clock.waits, vec![Duration::from_millis(500); 2]);
    assert_eq!(cursor.sequence, 1);

    let _ = fs::remove_file(source);
}

#[test]
fn state_monitor_reingests_only_when_the_read_only_source_changes() {
    let source = temp_source("state");
    let connection = Connection::open(&source).unwrap();
    connection
        .execute_batch(include_str!("fixtures/state/current.sql"))
        .unwrap();
    drop(connection);

    let mut store = Store::in_memory().unwrap();
    let mut monitor = LocalMonitor::state(&source, None, MonitorOptions::default()).unwrap();
    assert_eq!(
        monitor.poll(&mut store).unwrap().status,
        MonitorStatus::Updated
    );
    assert_eq!(
        monitor.poll(&mut store).unwrap().status,
        MonitorStatus::Idle
    );

    let connection = Connection::open(&source).unwrap();
    connection
        .execute(
            "UPDATE threads SET updated_at = 'updated-again' WHERE id = 'fixture-current-session'",
            [],
        )
        .unwrap();
    drop(connection);
    assert_eq!(
        monitor.poll(&mut store).unwrap().status,
        MonitorStatus::Updated
    );
    assert_eq!(
        store.load_canonical().unwrap().sessions[0]
            .updated_at
            .as_deref(),
        Some("updated-again")
    );

    let _ = fs::remove_file(source);
}

#[test]
fn poll_window_bounds_batch_size_without_splitting_a_line() {
    let source = temp_source("window");
    let lines = include_str!("fixtures/rollout/monitoring.jsonl")
        .lines()
        .collect::<Vec<_>>();
    fs::write(&source, include_str!("fixtures/rollout/monitoring.jsonl")).unwrap();
    let mut store = Store::in_memory().unwrap();
    let options = MonitorOptions {
        max_poll_bytes: 32,
        ..MonitorOptions::default()
    };
    let mut monitor = LocalMonitor::rollout(&source, None, options).unwrap();
    let first = monitor.poll(&mut store).unwrap();
    assert_eq!(first.records, 1);
    assert!(first.cursor.offset < fs::metadata(&source).unwrap().len());

    while store.load_canonical().unwrap().records.len() < lines.len() {
        monitor.poll(&mut store).unwrap();
    }
    assert_eq!(store.load_canonical().unwrap().records.len(), lines.len());

    let _ = fs::remove_file(source);
}
