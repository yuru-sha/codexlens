use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::discovery::{DiscoveredInput, InputKind, ReaderKind};
use crate::model::{
    CanonicalData, CanonicalDiagnostic, DiagnosticKind, FileOperation, Message, Record, RecordKind,
    Session, SourceRef, TokenUsage, ToolCall, ToolOutcome, ToolResult, Turn,
};
use crate::normalize::{normalize_rollout, normalize_rollout_with_state};
use crate::rollout::{ParseDiagnosticKind, RolloutParseOptions, parse_rollout};
use crate::state::{StateDiagnosticKind, StateReadResult, read_state_database};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestInputKind {
    Rollout,
    State,
}

impl IngestInputKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rollout => "rollout",
            Self::State => "state",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestOptions {
    pub rollout: RolloutParseOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestSummary {
    pub source: PathBuf,
    pub skipped: bool,
    pub sessions: usize,
    pub turns: usize,
    pub records: usize,
    pub messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub diagnostics: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub files: Vec<IngestSummary>,
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path)?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn in_memory() -> Result<Self> {
        let mut connection = Connection::open_in_memory()?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn migrate(&mut self) -> Result<()> {
        migrate(&mut self.connection)
    }

    pub fn ingest_rollout_file(
        &mut self,
        path: &Path,
        options: &RolloutParseOptions,
    ) -> Result<IngestSummary> {
        let identity = canonical_identity(path)?;
        self.ingest_rollout_at(path, &identity, options, &[])
    }

    pub fn ingest_state_database(&mut self, path: &Path) -> Result<IngestSummary> {
        let identity = canonical_identity(path)?;
        let fingerprint = fingerprint(path)?;
        let source = path.to_path_buf();
        if self.is_unchanged(&identity, &fingerprint, IngestInputKind::State)? {
            return Ok(skipped_summary(source));
        }

        let read = read_state_database(path);
        self.ingest_state_read(path, &identity, &fingerprint, read, IngestInputKind::State)
    }

    pub fn ingest_inputs(
        &mut self,
        inputs: &[DiscoveredInput],
        options: &IngestOptions,
    ) -> Result<IngestReport> {
        let mut report = IngestReport::default();
        let mut state_sessions = Vec::new();

        for input in inputs {
            if input.kind != InputKind::StateDatabase {
                continue;
            }
            let identity = input.identity.clone();
            let fingerprint = fingerprint(&input.path)?;
            let read = read_state_database(&input.path);
            state_sessions.extend(read.sessions.iter().cloned());
            if self.is_unchanged(&identity, &fingerprint, IngestInputKind::State)? {
                report.files.push(skipped_summary(input.path.clone()));
            } else {
                report.files.push(self.ingest_state_read(
                    &input.path,
                    &identity,
                    &fingerprint,
                    read,
                    IngestInputKind::State,
                )?);
            }
        }

        for input in inputs {
            let InputKind::Rollout { .. } = input.kind else {
                continue;
            };
            match input.reader {
                Some(ReaderKind::PlainJsonl) => report.files.push(self.ingest_rollout_at(
                    &input.path,
                    &input.identity,
                    &options.rollout,
                    &state_sessions,
                )?),
                Some(ReaderKind::ZstdJsonl) => report.files.push(self.ingest_unsupported(
                    &input.path,
                    &input.identity,
                    IngestInputKind::Rollout,
                    "compressed rollout input is not supported by the current reader",
                )?),
                None => report.files.push(self.ingest_unsupported(
                    &input.path,
                    &input.identity,
                    IngestInputKind::Rollout,
                    "rollout input has no compatible reader",
                )?),
            }
        }
        Ok(report)
    }

    pub fn ingest_canonical(
        &mut self,
        source_identity: &Path,
        kind: IngestInputKind,
        data: &CanonicalData,
    ) -> Result<IngestSummary> {
        let identity = canonical_identity(source_identity)?;
        let fingerprint = fingerprint(source_identity)?;
        self.ingest_batch(source_identity, &identity, kind, &fingerprint, data)
    }

    fn ingest_rollout_at(
        &mut self,
        path: &Path,
        identity: &Path,
        options: &RolloutParseOptions,
        state_sessions: &[Session],
    ) -> Result<IngestSummary> {
        let fingerprint = fingerprint(path)?;
        if self.is_unchanged(identity, &fingerprint, IngestInputKind::Rollout)? {
            return Ok(skipped_summary(path.to_path_buf()));
        }
        let parsed = parse_rollout(path, options);
        if parsed
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == ParseDiagnosticKind::Unreadable)
        {
            let data = normalize_rollout(&parsed);
            self.persist_diagnostics(identity, &data.diagnostics)?;
            return Ok(summary_from_data(path.to_path_buf(), &data, false));
        }
        let data = normalize_rollout_with_state(&parsed, state_sessions);
        self.ingest_batch(
            path,
            identity,
            IngestInputKind::Rollout,
            &fingerprint,
            &data,
        )
    }

    fn ingest_state_read(
        &mut self,
        path: &Path,
        identity: &Path,
        fingerprint: &Fingerprint,
        read: StateReadResult,
        kind: IngestInputKind,
    ) -> Result<IngestSummary> {
        if read.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic.kind,
                StateDiagnosticKind::Unreadable | StateDiagnosticKind::Query
            )
        }) {
            let data = state_data(read);
            self.persist_diagnostics(identity, &data.diagnostics)?;
            return Ok(summary_from_data(path.to_path_buf(), &data, false));
        }
        let data = state_data(read);
        self.ingest_batch(path, identity, kind, fingerprint, &data)
    }

    fn ingest_unsupported(
        &mut self,
        path: &Path,
        identity: &Path,
        kind: IngestInputKind,
        message: &str,
    ) -> Result<IngestSummary> {
        let fingerprint = fingerprint(path)?;
        if self.is_unchanged(identity, &fingerprint, kind)? {
            return Ok(skipped_summary(path.to_path_buf()));
        }
        let data = CanonicalData {
            diagnostics: vec![CanonicalDiagnostic {
                kind: DiagnosticKind::UnsupportedReader,
                source: SourceRef::state(path.to_path_buf()),
                message: message.to_owned(),
            }],
            ..CanonicalData::default()
        };
        self.ingest_batch(path, identity, kind, &fingerprint, &data)
    }

    fn is_unchanged(
        &self,
        identity: &Path,
        fingerprint: &Fingerprint,
        kind: IngestInputKind,
    ) -> Result<bool> {
        let identity = identity.to_string_lossy();
        let previous = self
            .connection
            .query_row(
                "SELECT input_kind, size, modified_ns, digest FROM ingested_files WHERE identity = ?1",
                params![identity.as_ref()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            previous.is_some_and(|(previous_kind, size, modified_ns, digest)| {
                previous_kind == kind.as_str()
                    && size == i64::try_from(fingerprint.size).unwrap_or(i64::MAX)
                    && modified_ns == fingerprint.modified_ns.map(|value| value.to_string())
                    && digest == fingerprint.digest.to_string()
            }),
        )
    }

    fn ingest_batch(
        &mut self,
        source_path: &Path,
        source_identity: &Path,
        kind: IngestInputKind,
        fingerprint: &Fingerprint,
        data: &CanonicalData,
    ) -> Result<IngestSummary> {
        let identity = source_identity.to_string_lossy().into_owned();
        let transaction = self.connection.transaction()?;
        delete_source(&transaction, &identity)?;
        insert_data(&transaction, &identity, data)?;
        transaction.execute(
            "INSERT INTO ingested_files (identity, source_path, input_kind, size, modified_ns, digest) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(identity) DO UPDATE SET source_path = excluded.source_path, input_kind = excluded.input_kind, size = excluded.size, modified_ns = excluded.modified_ns, digest = excluded.digest",
            params![
                identity,
                source_path.to_string_lossy().as_ref(),
                kind.as_str(),
                i64::try_from(fingerprint.size).unwrap_or(i64::MAX),
                fingerprint.modified_ns.map(|value| value.to_string()),
                fingerprint.digest.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(summary_from_data(source_path.to_path_buf(), data, false))
    }

    fn persist_diagnostics(
        &mut self,
        source_identity: &Path,
        diagnostics: &[CanonicalDiagnostic],
    ) -> Result<()> {
        let identity = source_identity.to_string_lossy();
        let transaction = self.connection.transaction()?;
        for (index, diagnostic) in diagnostics.iter().enumerate() {
            insert_diagnostic(&transaction, identity.as_ref(), diagnostic, index)?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let current: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        bail!("store schema version {current} is newer than supported version {SCHEMA_VERSION}");
    }
    let transaction = connection.transaction()?;
    if current < 1 {
        transaction.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_versions (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT NOT NULL,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                created_at TEXT,
                updated_at TEXT,
                cwd TEXT,
                project TEXT,
                model TEXT,
                provider TEXT,
                source TEXT,
                thread_source TEXT,
                rollout_path TEXT,
                archive_state INTEGER,
                title TEXT,
                preview TEXT,
                parent_id TEXT,
                cli_version TEXT,
                originator TEXT,
                history_mode TEXT,
                reasoning_effort TEXT,
                PRIMARY KEY (session_id, source_identity)
            );
            CREATE TABLE IF NOT EXISTS turns (
                turn_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                session_id TEXT,
                turn_id TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT,
                cwd TEXT,
                model TEXT,
                reasoning_effort TEXT,
                sequence INTEGER NOT NULL,
                lifecycle_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS records (
                record_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                session_id TEXT,
                turn_id TEXT,
                timestamp TEXT,
                sequence INTEGER NOT NULL,
                kind TEXT NOT NULL,
                record_type TEXT,
                nested_type TEXT,
                raw_json TEXT
            );
            CREATE TABLE IF NOT EXISTS messages (
                message_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                message_id TEXT,
                session_id TEXT,
                turn_id TEXT,
                role TEXT,
                content TEXT NOT NULL,
                timestamp TEXT
            );
            CREATE TABLE IF NOT EXISTS tool_calls (
                call_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                item_id TEXT,
                call_id TEXT,
                session_id TEXT,
                turn_id TEXT,
                tool_name TEXT,
                input_summary TEXT,
                command TEXT,
                cwd TEXT,
                status TEXT
            );
            CREATE TABLE IF NOT EXISTS tool_results (
                result_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                result_id TEXT,
                call_id TEXT,
                session_id TEXT,
                turn_id TEXT,
                command TEXT,
                cwd TEXT,
                stdout TEXT,
                stderr TEXT,
                duration_ms INTEGER,
                exit_code INTEGER,
                status TEXT,
                outcome TEXT NOT NULL,
                outcome_source TEXT NOT NULL,
                matched_call INTEGER NOT NULL,
                deduplication_key TEXT,
                equivalent_to_path TEXT,
                equivalent_to_line INTEGER,
                is_duplicate INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS file_operations (
                operation_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                session_id TEXT,
                turn_id TEXT,
                path TEXT NOT NULL,
                operation TEXT NOT NULL,
                timestamp TEXT
            );
            CREATE TABLE IF NOT EXISTS token_usage (
                usage_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                session_id TEXT,
                turn_id TEXT,
                timestamp TEXT,
                input_tokens INTEGER,
                cached_input_tokens INTEGER,
                output_tokens INTEGER,
                reasoning_output_tokens INTEGER,
                sequence INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS diagnostics (
                diagnostic_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                kind TEXT NOT NULL,
                message TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ingested_files (
                identity TEXT PRIMARY KEY,
                source_path TEXT NOT NULL,
                input_kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                modified_ns TEXT,
                digest TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(session_id);
            CREATE INDEX IF NOT EXISTS idx_records_session ON records(session_id);
            CREATE INDEX IF NOT EXISTS idx_tool_results_call ON tool_results(call_id);
            INSERT OR IGNORE INTO schema_versions (version) VALUES (1);
            PRAGMA user_version = 1;
            "#,
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn state_data(read: StateReadResult) -> CanonicalData {
    CanonicalData {
        sessions: read.sessions,
        diagnostics: read
            .diagnostics
            .into_iter()
            .map(|diagnostic| CanonicalDiagnostic {
                kind: match diagnostic.kind {
                    StateDiagnosticKind::Unreadable => DiagnosticKind::Unreadable,
                    StateDiagnosticKind::SchemaMismatch => DiagnosticKind::StateSchemaMismatch,
                    StateDiagnosticKind::Query => DiagnosticKind::StateQuery,
                },
                source: diagnostic.source,
                message: diagnostic.message,
            })
            .collect(),
        ..CanonicalData::default()
    }
}

fn delete_source(transaction: &Transaction<'_>, identity: &str) -> rusqlite::Result<()> {
    for table in [
        "sessions",
        "turns",
        "records",
        "messages",
        "tool_calls",
        "tool_results",
        "file_operations",
        "token_usage",
        "diagnostics",
    ] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE source_identity = ?1"),
            params![identity],
        )?;
    }
    Ok(())
}

fn insert_data(transaction: &Transaction<'_>, identity: &str, data: &CanonicalData) -> Result<()> {
    for session in &data.sessions {
        insert_session(transaction, identity, session)?;
    }
    for turn in &data.turns {
        insert_turn(transaction, identity, turn)?;
    }
    for (index, record) in data.records.iter().enumerate() {
        insert_record(transaction, identity, record, index)?;
    }
    for (index, message) in data.messages.iter().enumerate() {
        insert_message(transaction, identity, message, index)?;
    }
    for (index, call) in data.tool_calls.iter().enumerate() {
        insert_tool_call(transaction, identity, call, index)?;
    }
    for (index, result) in data.tool_results.iter().enumerate() {
        insert_tool_result(transaction, identity, result, index)?;
    }
    for (index, operation) in data.file_operations.iter().enumerate() {
        insert_file_operation(transaction, identity, operation, index)?;
    }
    for (index, usage) in data.token_usage.iter().enumerate() {
        insert_token_usage(transaction, identity, usage, index)?;
    }
    for (index, diagnostic) in data.diagnostics.iter().enumerate() {
        insert_diagnostic(transaction, identity, diagnostic, index)?;
    }
    Ok(())
}

fn insert_session(transaction: &Transaction<'_>, identity: &str, session: &Session) -> Result<()> {
    transaction.execute(
        "INSERT OR REPLACE INTO sessions (session_id, source_identity, source_path, source_line, created_at, updated_at, cwd, project, model, provider, source, thread_source, rollout_path, archive_state, title, preview, parent_id, cli_version, originator, history_mode, reasoning_effort) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            session.id,
            identity,
            session.provenance.path.to_string_lossy().as_ref(),
            db_line(session.provenance.line),
            session.created_at,
            session.updated_at,
            session.cwd,
            session.project,
            session.model,
            session.provider,
            session.source,
            session.thread_source,
            session.rollout_path,
            session.archive_state.map(i64::from),
            session.title,
            session.preview,
            session.parent_id,
            session.cli_version,
            session.originator,
            session.history_mode,
            session.reasoning_effort,
        ],
    )?;
    Ok(())
}

fn insert_turn(transaction: &Transaction<'_>, identity: &str, turn: &Turn) -> Result<()> {
    let key = row_key(identity, &turn.provenance, &format!("turn:{}", turn.id));
    let lifecycle_json = serde_json::to_string(&turn.lifecycle)?;
    transaction.execute(
        "INSERT OR REPLACE INTO turns (turn_key, source_identity, source_path, source_line, session_id, turn_id, started_at, completed_at, cwd, model, reasoning_effort, sequence, lifecycle_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            key,
            identity,
            turn.provenance.path.to_string_lossy().as_ref(),
            db_line(turn.provenance.line),
            turn.session_id,
            turn.id,
            turn.started_at,
            turn.completed_at,
            turn.cwd,
            turn.model,
            turn.reasoning_effort,
            i64::try_from(turn.sequence).unwrap_or(i64::MAX),
            lifecycle_json,
        ],
    )?;
    Ok(())
}

fn insert_record(
    transaction: &Transaction<'_>,
    identity: &str,
    record: &Record,
    index: usize,
) -> Result<()> {
    let key = row_key(identity, &record.provenance, &format!("record:{index}"));
    let (kind, record_type, nested_type, raw_json) = record_kind_values(&record.kind);
    transaction.execute(
        "INSERT OR REPLACE INTO records (record_key, source_identity, source_path, source_line, session_id, turn_id, timestamp, sequence, kind, record_type, nested_type, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            key,
            identity,
            record.provenance.path.to_string_lossy().as_ref(),
            db_line(record.provenance.line),
            record.session_id,
            record.turn_id,
            record.timestamp,
            i64::try_from(record.sequence).unwrap_or(i64::MAX),
            kind,
            record_type,
            nested_type,
            raw_json,
        ],
    )?;
    Ok(())
}

fn insert_message(
    transaction: &Transaction<'_>,
    identity: &str,
    message: &Message,
    index: usize,
) -> Result<()> {
    let key = row_key(identity, &message.provenance, &format!("message:{index}"));
    transaction.execute(
        "INSERT OR REPLACE INTO messages (message_key, source_identity, source_path, source_line, message_id, session_id, turn_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            key,
            identity,
            message.provenance.path.to_string_lossy().as_ref(),
            db_line(message.provenance.line),
            message.id,
            message.session_id,
            message.turn_id,
            message.role.as_ref().map(|role| role.as_str()),
            message.content,
            message.timestamp,
        ],
    )?;
    Ok(())
}

fn insert_tool_call(
    transaction: &Transaction<'_>,
    identity: &str,
    call: &ToolCall,
    index: usize,
) -> Result<()> {
    let key = row_key(identity, &call.provenance, &format!("call:{index}"));
    transaction.execute(
        "INSERT OR REPLACE INTO tool_calls (call_key, source_identity, source_path, source_line, item_id, call_id, session_id, turn_id, tool_name, input_summary, command, cwd, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            key,
            identity,
            call.provenance.path.to_string_lossy().as_ref(),
            db_line(call.provenance.line),
            call.id,
            call.call_id,
            call.session_id,
            call.turn_id,
            call.tool_name,
            call.input_summary,
            call.command,
            call.cwd,
            call.status,
        ],
    )?;
    Ok(())
}

fn insert_tool_result(
    transaction: &Transaction<'_>,
    identity: &str,
    result: &ToolResult,
    index: usize,
) -> Result<()> {
    let key = row_key(identity, &result.provenance, &format!("result:{index}"));
    let (equivalent_path, equivalent_line) =
        result
            .equivalent_to
            .as_ref()
            .map_or((None, None), |source| {
                (
                    Some(source.path.to_string_lossy().into_owned()),
                    db_line(source.line),
                )
            });
    transaction.execute(
        "INSERT OR REPLACE INTO tool_results (result_key, source_identity, source_path, source_line, result_id, call_id, session_id, turn_id, command, cwd, stdout, stderr, duration_ms, exit_code, status, outcome, outcome_source, matched_call, deduplication_key, equivalent_to_path, equivalent_to_line, is_duplicate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            key,
            identity,
            result.provenance.path.to_string_lossy().as_ref(),
            db_line(result.provenance.line),
            result.id,
            result.call_id,
            result.session_id,
            result.turn_id,
            result.command,
            result.cwd,
            result.stdout,
            result.stderr,
            result.duration_ms,
            result.exit_code,
            outcome_name(result.outcome),
            outcome_source_name(result.outcome_source),
            i64::from(result.matched_call),
            result.deduplication_key,
            equivalent_path,
            equivalent_line,
            i64::from(result.is_duplicate),
        ],
    )?;
    Ok(())
}

fn insert_file_operation(
    transaction: &Transaction<'_>,
    identity: &str,
    operation: &FileOperation,
    index: usize,
) -> Result<()> {
    let key = row_key(
        identity,
        &operation.provenance,
        &format!("operation:{index}"),
    );
    transaction.execute(
        "INSERT OR REPLACE INTO file_operations (operation_key, source_identity, source_path, source_line, session_id, turn_id, path, operation, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            key,
            identity,
            operation.provenance.path.to_string_lossy().as_ref(),
            db_line(operation.provenance.line),
            operation.session_id,
            operation.turn_id,
            operation.path,
            operation.operation,
            operation.timestamp,
        ],
    )?;
    Ok(())
}

fn insert_token_usage(
    transaction: &Transaction<'_>,
    identity: &str,
    usage: &TokenUsage,
    index: usize,
) -> Result<()> {
    let key = row_key(identity, &usage.provenance, &format!("usage:{index}"));
    transaction.execute(
        "INSERT OR REPLACE INTO token_usage (usage_key, source_identity, source_path, source_line, session_id, turn_id, timestamp, input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens, sequence) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            key,
            identity,
            usage.provenance.path.to_string_lossy().as_ref(),
            db_line(usage.provenance.line),
            usage.session_id,
            usage.turn_id,
            usage.timestamp,
            db_u64(usage.input_tokens),
            db_u64(usage.cached_input_tokens),
            db_u64(usage.output_tokens),
            db_u64(usage.reasoning_output_tokens),
            i64::try_from(usage.sequence).unwrap_or(i64::MAX),
        ],
    )?;
    Ok(())
}

fn insert_diagnostic(
    transaction: &Transaction<'_>,
    identity: &str,
    diagnostic: &CanonicalDiagnostic,
    index: usize,
) -> Result<()> {
    let key = row_key(
        identity,
        &diagnostic.source,
        &format!("diagnostic:{index}:{}", diagnostic.kind.as_str()),
    );
    transaction.execute(
        "INSERT OR REPLACE INTO diagnostics (diagnostic_key, source_identity, source_path, source_line, kind, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            key,
            identity,
            diagnostic.source.path.to_string_lossy().as_ref(),
            db_line(diagnostic.source.line),
            diagnostic.kind.as_str(),
            diagnostic.message,
        ],
    )?;
    Ok(())
}

fn record_kind_values(
    kind: &RecordKind,
) -> (&'static str, Option<&str>, Option<&str>, Option<&str>) {
    match kind {
        RecordKind::SessionMetadata => ("session_metadata", None, None, None),
        RecordKind::TurnContext => ("turn_context", None, None, None),
        RecordKind::ResponseItem => ("response_item", None, None, None),
        RecordKind::EventMessage => ("event_message", None, None, None),
        RecordKind::Compacted => ("compacted", None, None, None),
        RecordKind::WorldState => ("world_state", None, None, None),
        RecordKind::Unknown {
            record_type,
            nested_type,
            raw_json,
        } => (
            "unknown",
            record_type.as_deref(),
            nested_type.as_deref(),
            Some(raw_json.as_str()),
        ),
    }
}

fn outcome_name(outcome: ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Succeeded => "succeeded",
        ToolOutcome::Failed => "failed",
        ToolOutcome::Unknown => "unknown",
    }
}

fn outcome_source_name(source: crate::model::OutcomeSource) -> &'static str {
    match source {
        crate::model::OutcomeSource::ExitCode => "exit_code",
        crate::model::OutcomeSource::Status => "status",
        crate::model::OutcomeSource::OutputText => "output_text",
        crate::model::OutcomeSource::Unknown => "unknown",
    }
}

fn row_key(identity: &str, source: &SourceRef, suffix: &str) -> String {
    format!(
        "{}|{}|{}",
        identity,
        source.line.map_or(0, |line| line),
        suffix
    )
}

fn db_line(line: Option<usize>) -> Option<i64> {
    line.and_then(|line| i64::try_from(line).ok())
}

fn db_u64(value: Option<u64>) -> Option<i64> {
    value.and_then(|value| i64::try_from(value).ok())
}

fn canonical_identity(path: &Path) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(path)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    modified_ns: Option<u128>,
    digest: u64,
}

fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let metadata = std::fs::metadata(path)?;
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 16 * 1024];
    let mut digest = 0xcbf29ce484222325u64;
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        for byte in &buffer[..read] {
            digest ^= u64::from(*byte);
            digest = digest.wrapping_mul(0x100000001b3);
        }
    }
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(Fingerprint {
        size: size.max(metadata.len()),
        modified_ns,
        digest,
    })
}

fn skipped_summary(source: PathBuf) -> IngestSummary {
    IngestSummary {
        source,
        skipped: true,
        sessions: 0,
        turns: 0,
        records: 0,
        messages: 0,
        tool_calls: 0,
        tool_results: 0,
        diagnostics: 0,
    }
}

fn summary_from_data(source: PathBuf, data: &CanonicalData, skipped: bool) -> IngestSummary {
    IngestSummary {
        source,
        skipped,
        sessions: data.sessions.len(),
        turns: data.turns.len(),
        records: data.records.len(),
        messages: data.messages.len(),
        tool_calls: data.tool_calls.len(),
        tool_results: data.tool_results.len(),
        diagnostics: data.diagnostics.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "codexlens-store-{}-{}-{suffix}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn migrations_create_all_required_tables() {
        let store = Store::in_memory().unwrap();
        for table in [
            "schema_versions",
            "sessions",
            "turns",
            "records",
            "messages",
            "tool_calls",
            "tool_results",
            "file_operations",
            "token_usage",
            "diagnostics",
            "ingested_files",
        ] {
            assert_eq!(
                store
                    .connection()
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        params![table],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1,
                "missing {table}"
            );
        }
        assert_eq!(
            store
                .connection()
                .query_row("SELECT version FROM schema_versions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn rollout_ingest_is_idempotent_and_changed_input_replaces_rows() {
        let source = temp_path("rollout.jsonl");
        fs::write(
            &source,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n",
        )
        .unwrap();
        let mut store = Store::in_memory().unwrap();
        let first = store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();
        let second = store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();
        assert!(!first.skipped);
        assert!(second.skipped);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM records", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );

        fs::write(
            &source,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n{\"type\":\"future\",\"payload\":{\"keep\":true}}\n",
        )
        .unwrap();
        let changed = store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();
        assert!(!changed.skipped);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM records", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM records WHERE kind = 'unknown' AND raw_json IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn failed_replacement_rolls_back_source_deletion() {
        let source = temp_path("rollback.jsonl");
        fs::write(
            &source,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n",
        )
        .unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_record_insert BEFORE INSERT ON records BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;",
            )
            .unwrap();
        fs::write(
            &source,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n{\"type\":\"world_state\"}\n",
        )
        .unwrap();

        assert!(
            store
                .ingest_rollout_file(&source, &RolloutParseOptions::default())
                .is_err()
        );
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM records", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn state_ingest_persists_read_only_metadata() {
        let source = temp_path("state.sqlite");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT, rollout_path TEXT, cwd TEXT, model_provider TEXT, archived INTEGER, git_repo_root TEXT); INSERT INTO threads VALUES ('s', '/fixture.jsonl', '/fixture', 'provider', 1, '/project');",
            )
            .unwrap();
        drop(connection);

        let mut store = Store::in_memory().unwrap();
        let summary = store.ingest_state_database(&source).unwrap();
        assert_eq!(summary.sessions, 1);
        let row = store
            .connection()
            .query_row(
                "SELECT session_id, cwd, provider, archive_state, project FROM sessions",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "s".to_owned(),
                "/fixture".to_owned(),
                "provider".to_owned(),
                1,
                "/project".to_owned()
            )
        );
        let _ = fs::remove_file(source);
    }
}
