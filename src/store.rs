use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::discovery::{DiscoveredInput, InputKind, ReaderKind, codex_home_for_source};
use crate::instructions::{
    InstructionCaptureOptions, InstructionResolver, join_sessions, snapshot_entries,
    snapshot_from_resolution,
};
use crate::model::{
    CanonicalData, CanonicalDiagnostic, DiagnosticKind, FileOperation, InstructionFile,
    InstructionJoin, InstructionScope, InstructionSnapshot, Message, Record, RecordKind, Session,
    SourceKind, SourceRef, TokenUsage, ToolCall, ToolOutcome, ToolResult, Turn,
};
use crate::normalize::{normalize_rollout, normalize_rollout_with_instructions};
use crate::rollout::{ParseDiagnosticKind, RolloutParseOptions, parse_rollout};
use crate::state::{
    StateDiagnostic, StateDiagnosticKind, StateReadResult, merge_state_results, read_state_database,
};

pub const SCHEMA_VERSION: i64 = 4;

const STATE_STORAGE_STANDALONE: &str = "standalone";
const STATE_STORAGE_ENRICHMENT: &str = "enrichment";

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

#[derive(Debug, Clone, Copy)]
struct IngestBatchOptions<'a> {
    storage_mode: Option<&'a str>,
    preserve_instruction_snapshots: bool,
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
        self.ingest_rollout_file_with_resolver(path, options, &resolver_for_source(path))
    }

    pub fn ingest_rollout_file_with_instructions(
        &mut self,
        path: &Path,
        options: &RolloutParseOptions,
        capture: &InstructionCaptureOptions,
    ) -> Result<IngestSummary> {
        self.ingest_rollout_file_with_resolver(path, options, &capture.resolver())
    }

    fn ingest_rollout_file_with_resolver(
        &mut self,
        path: &Path,
        options: &RolloutParseOptions,
        resolver: &InstructionResolver,
    ) -> Result<IngestSummary> {
        let identity = canonical_identity(path).unwrap_or_else(|_| path.to_path_buf());
        self.ingest_rollout_at(path, &identity, options, &[], false, resolver)
    }

    pub fn ingest_state_database(&mut self, path: &Path) -> Result<IngestSummary> {
        self.ingest_state_database_with_resolver(path, &resolver_for_source(path))
    }

    pub fn ingest_state_database_with_instructions(
        &mut self,
        path: &Path,
        capture: &InstructionCaptureOptions,
    ) -> Result<IngestSummary> {
        let resolver = capture.resolver();
        self.ingest_state_database_with_resolver(path, &resolver)
    }

    fn ingest_state_database_with_resolver(
        &mut self,
        path: &Path,
        resolver: &InstructionResolver,
    ) -> Result<IngestSummary> {
        let identity = canonical_identity(path).unwrap_or_else(|_| path.to_path_buf());
        let fingerprint = match fingerprint(path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                let data = unreadable_data(path, SourceKind::State, &error.to_string());
                self.persist_diagnostics(&identity, &data.diagnostics)?;
                return Ok(summary_from_data(path.to_path_buf(), &data, false));
            }
        };
        let source = path.to_path_buf();
        if self.is_unchanged(
            &identity,
            &fingerprint,
            IngestInputKind::State,
            Some(STATE_STORAGE_STANDALONE),
        )? {
            return Ok(skipped_summary(source));
        }

        let read = read_state_database(path);
        self.ingest_state_read(path, &identity, &fingerprint, read, true, resolver)
    }

    pub fn ingest_inputs(
        &mut self,
        inputs: &[DiscoveredInput],
        options: &IngestOptions,
    ) -> Result<IngestReport> {
        self.ingest_inputs_with_resolver(inputs, options, &resolver_for_inputs(inputs))
    }

    pub fn ingest_inputs_with_instructions(
        &mut self,
        inputs: &[DiscoveredInput],
        options: &IngestOptions,
        capture: &InstructionCaptureOptions,
    ) -> Result<IngestReport> {
        let resolver = capture.resolver();
        self.ingest_inputs_with_resolver(inputs, options, &resolver)
    }

    fn ingest_inputs_with_resolver(
        &mut self,
        inputs: &[DiscoveredInput],
        options: &IngestOptions,
        resolver: &InstructionResolver,
    ) -> Result<IngestReport> {
        let mut report = IngestReport::default();
        let mut state_reads = Vec::new();
        let mut state_changed = false;
        let mut state_refresh_blocked = false;
        let has_plain_rollout = inputs.iter().any(|input| {
            matches!(input.kind, InputKind::Rollout { .. })
                && matches!(input.reader, Some(ReaderKind::PlainJsonl))
        });
        let state_storage_mode = if has_plain_rollout {
            STATE_STORAGE_ENRICHMENT
        } else {
            STATE_STORAGE_STANDALONE
        };

        for input in inputs {
            if input.kind != InputKind::StateDatabase {
                continue;
            }
            let identity = input.identity.clone();
            let fingerprint = match fingerprint(&input.path) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    let read = StateReadResult {
                        sessions: Vec::new(),
                        diagnostics: vec![StateDiagnostic {
                            source: SourceRef::state(input.path.clone()),
                            kind: StateDiagnosticKind::Unreadable,
                            message: bounded_message(&error.to_string()),
                        }],
                    };
                    state_reads.push(read.clone());
                    let data = state_data(read);
                    self.persist_diagnostics(&identity, &data.diagnostics)?;
                    report
                        .files
                        .push(summary_from_data(input.path.clone(), &data, false));
                    state_refresh_blocked = true;
                    continue;
                }
            };
            let read = read_state_database(&input.path);
            state_reads.push(read.clone());
            if self.is_unchanged(
                &identity,
                &fingerprint,
                IngestInputKind::State,
                Some(state_storage_mode),
            )? {
                report.files.push(skipped_summary(input.path.clone()));
            } else {
                let refreshable = !read.diagnostics.iter().any(|diagnostic| {
                    matches!(
                        diagnostic.kind,
                        StateDiagnosticKind::Unreadable
                            | StateDiagnosticKind::Query
                            | StateDiagnosticKind::SchemaMismatch
                    )
                });
                state_changed |= refreshable;
                state_refresh_blocked |= !refreshable;
                report.files.push(self.ingest_state_read(
                    &input.path,
                    &identity,
                    &fingerprint,
                    read,
                    !has_plain_rollout,
                    resolver,
                )?);
            }
        }

        let combined_state = merge_state_results(state_reads);
        let state_sessions = combined_state.sessions;
        for input in inputs {
            if input.kind != InputKind::StateDatabase {
                continue;
            }
            let diagnostics = combined_state
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.source.path == input.path)
                .map(canonical_state_diagnostic)
                .collect::<Vec<_>>();
            if !diagnostics.is_empty() {
                self.persist_diagnostics(&input.identity, &diagnostics)?;
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
                    state_changed && !state_refresh_blocked,
                    resolver,
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
        self.ingest_batch(
            source_identity,
            &identity,
            kind,
            &fingerprint,
            data,
            IngestBatchOptions {
                storage_mode: (kind == IngestInputKind::State).then_some(STATE_STORAGE_STANDALONE),
                preserve_instruction_snapshots: false,
            },
        )
    }

    fn ingest_rollout_at(
        &mut self,
        path: &Path,
        identity: &Path,
        options: &RolloutParseOptions,
        state_sessions: &[Session],
        force_refresh: bool,
        resolver: &InstructionResolver,
    ) -> Result<IngestSummary> {
        let fingerprint = match fingerprint(path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                let data = unreadable_data(path, SourceKind::Rollout, &error.to_string());
                self.persist_diagnostics(identity, &data.diagnostics)?;
                return Ok(summary_from_data(path.to_path_buf(), &data, false));
            }
        };
        let unchanged =
            self.is_unchanged(identity, &fingerprint, IngestInputKind::Rollout, None)?;
        if !force_refresh && unchanged {
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
        let data = normalize_rollout_with_instructions(&parsed, state_sessions, resolver);
        self.ingest_batch(
            path,
            identity,
            IngestInputKind::Rollout,
            &fingerprint,
            &data,
            IngestBatchOptions {
                storage_mode: None,
                preserve_instruction_snapshots: force_refresh && unchanged,
            },
        )
    }

    fn ingest_state_read(
        &mut self,
        path: &Path,
        identity: &Path,
        fingerprint: &Fingerprint,
        read: StateReadResult,
        persist_sessions: bool,
        resolver: &InstructionResolver,
    ) -> Result<IngestSummary> {
        if read.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic.kind,
                StateDiagnosticKind::Unreadable
                    | StateDiagnosticKind::Query
                    | StateDiagnosticKind::SchemaMismatch
            )
        }) {
            let data = state_data(read);
            self.persist_diagnostics(identity, &data.diagnostics)?;
            return Ok(summary_from_data(path.to_path_buf(), &data, false));
        }
        let mut data = state_data(read);
        if !persist_sessions {
            data.sessions.clear();
        } else {
            data.instruction_joins = join_sessions(&data.sessions, resolver);
            data.instruction_snapshots = data
                .instruction_joins
                .iter()
                .map(|join| {
                    snapshot_from_resolution(
                        Some(join.session_id.clone()),
                        None,
                        &join.resolution,
                        join.provenance.clone(),
                    )
                })
                .collect();
        }
        self.ingest_batch(
            path,
            identity,
            IngestInputKind::State,
            fingerprint,
            &data,
            IngestBatchOptions {
                storage_mode: Some(if persist_sessions {
                    STATE_STORAGE_STANDALONE
                } else {
                    STATE_STORAGE_ENRICHMENT
                }),
                preserve_instruction_snapshots: false,
            },
        )
    }

    fn ingest_unsupported(
        &mut self,
        path: &Path,
        identity: &Path,
        kind: IngestInputKind,
        message: &str,
    ) -> Result<IngestSummary> {
        let fingerprint = match fingerprint(path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                let data = unreadable_data(path, SourceKind::Rollout, &error.to_string());
                self.persist_diagnostics(identity, &data.diagnostics)?;
                return Ok(summary_from_data(path.to_path_buf(), &data, false));
            }
        };
        if self.is_unchanged(identity, &fingerprint, kind, None)? {
            return Ok(skipped_summary(path.to_path_buf()));
        }
        let data = CanonicalData {
            diagnostics: vec![CanonicalDiagnostic {
                kind: DiagnosticKind::UnsupportedReader,
                source: SourceRef::rollout(path.to_path_buf(), 1),
                message: message.to_owned(),
            }],
            ..CanonicalData::default()
        };
        self.ingest_batch(
            path,
            identity,
            kind,
            &fingerprint,
            &data,
            IngestBatchOptions {
                storage_mode: None,
                preserve_instruction_snapshots: false,
            },
        )
    }

    fn is_unchanged(
        &self,
        identity: &Path,
        fingerprint: &Fingerprint,
        kind: IngestInputKind,
        storage_mode: Option<&str>,
    ) -> Result<bool> {
        let identity = identity.to_string_lossy();
        let previous = self
            .connection
            .query_row(
                "SELECT input_kind, size, modified_ns, digest, storage_mode FROM ingested_files WHERE identity = ?1",
                params![identity.as_ref()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(previous.is_some_and(
            |(previous_kind, size, modified_ns, digest, previous_storage_mode)| {
                previous_kind == kind.as_str()
                    && size == i64::try_from(fingerprint.size).unwrap_or(i64::MAX)
                    && modified_ns == fingerprint.modified_ns.map(|value| value.to_string())
                    && digest == fingerprint.digest.to_string()
                    && previous_storage_mode.as_deref() == storage_mode
            },
        ))
    }

    fn ingest_batch(
        &mut self,
        source_path: &Path,
        source_identity: &Path,
        kind: IngestInputKind,
        fingerprint: &Fingerprint,
        data: &CanonicalData,
        options: IngestBatchOptions<'_>,
    ) -> Result<IngestSummary> {
        let identity = source_identity.to_string_lossy().into_owned();
        let mut stamped_data = data.clone();
        stamp_data(&mut stamped_data, &current_timestamp());
        let transaction = self.connection.transaction()?;
        delete_source(
            &transaction,
            &identity,
            options.preserve_instruction_snapshots,
        )?;
        insert_data(
            &transaction,
            &identity,
            &stamped_data,
            options.preserve_instruction_snapshots,
        )?;
        transaction.execute(
            "INSERT INTO ingested_files (identity, source_path, input_kind, size, modified_ns, digest, storage_mode) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(identity) DO UPDATE SET source_path = excluded.source_path, input_kind = excluded.input_kind, size = excluded.size, modified_ns = excluded.modified_ns, digest = excluded.digest, storage_mode = excluded.storage_mode",
            params![
                identity,
                source_path.to_string_lossy().as_ref(),
                kind.as_str(),
                i64::try_from(fingerprint.size).unwrap_or(i64::MAX),
                fingerprint.modified_ns.map(|value| value.to_string()),
                fingerprint.digest.to_string(),
                options.storage_mode,
            ],
        )?;
        transaction.commit()?;
        Ok(summary_from_data(
            source_path.to_path_buf(),
            &stamped_data,
            false,
        ))
    }

    fn persist_diagnostics(
        &mut self,
        source_identity: &Path,
        diagnostics: &[CanonicalDiagnostic],
    ) -> Result<()> {
        let identity = source_identity.to_string_lossy();
        let timestamp = current_timestamp();
        let transaction = self.connection.transaction()?;
        for (index, diagnostic) in diagnostics.iter().enumerate() {
            let mut stamped = diagnostic.clone();
            stamped.source.stamp_ingest_time(&timestamp);
            insert_diagnostic(&transaction, identity.as_ref(), &stamped, index)?;
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
                content TEXT,
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
    if current < 2 {
        transaction.execute_batch(
            r#"
            CREATE TABLE messages_v2 (
                message_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                message_id TEXT,
                session_id TEXT,
                turn_id TEXT,
                role TEXT,
                content TEXT,
                timestamp TEXT
            );
            INSERT INTO messages_v2 (message_key, source_identity, source_path, source_line, message_id, session_id, turn_id, role, content, timestamp)
                SELECT message_key, source_identity, source_path, source_line, message_id, session_id, turn_id, role, content, timestamp
                FROM messages;
            DROP TABLE messages;
            ALTER TABLE messages_v2 RENAME TO messages;
            INSERT OR IGNORE INTO schema_versions (version) VALUES (2);
            PRAGMA user_version = 2;
            "#,
        )?;
    }
    if current < 3 {
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
            add_provenance_columns(&transaction, table)?;
        }
        add_column_if_table_exists(&transaction, "ingested_files", "storage_mode TEXT")?;
        transaction.execute(
            "INSERT OR IGNORE INTO schema_versions (version) VALUES (3)",
            [],
        )?;
        transaction.execute_batch("PRAGMA user_version = 3;")?;
    }
    if current < 4 {
        transaction.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS instruction_blobs (
                blob_key TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                byte_count INTEGER NOT NULL,
                content TEXT NOT NULL,
                UNIQUE(content_hash, byte_count)
            );
            CREATE TABLE IF NOT EXISTS instruction_snapshots (
                snapshot_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                source_kind TEXT NOT NULL,
                ingested_at TEXT,
                parser_schema_version INTEGER NOT NULL,
                session_id TEXT,
                turn_id TEXT,
                snapshot_source TEXT NOT NULL,
                accuracy TEXT NOT NULL,
                blob_key TEXT,
                content_hash TEXT,
                byte_count INTEGER NOT NULL,
                effective_chain_hash TEXT,
                truncated INTEGER NOT NULL,
                chain_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS instruction_files (
                file_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                source_kind TEXT NOT NULL,
                ingested_at TEXT,
                parser_schema_version INTEGER NOT NULL,
                session_id TEXT,
                path TEXT NOT NULL,
                scope TEXT NOT NULL,
                file_kind TEXT NOT NULL,
                state TEXT NOT NULL,
                chain_position INTEGER,
                blob_key TEXT,
                content_hash TEXT,
                byte_count INTEGER NOT NULL,
                diagnostic TEXT
            );
            CREATE TABLE IF NOT EXISTS instruction_joins (
                join_key TEXT PRIMARY KEY,
                source_identity TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_line INTEGER,
                source_kind TEXT NOT NULL,
                ingested_at TEXT,
                parser_schema_version INTEGER NOT NULL,
                session_id TEXT NOT NULL,
                cwd TEXT,
                project_root TEXT,
                project_root_status TEXT NOT NULL,
                nearest_path TEXT,
                nearest_scope TEXT,
                effective_chain_hash TEXT,
                chain_json TEXT NOT NULL,
                diagnostics_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_instruction_snapshots_session
                ON instruction_snapshots(session_id);
            CREATE INDEX IF NOT EXISTS idx_instruction_joins_session
                ON instruction_joins(session_id);
            INSERT OR IGNORE INTO schema_versions (version) VALUES (4);
            PRAGMA user_version = 4;
            "#,
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn add_provenance_columns(transaction: &Transaction<'_>, table: &str) -> Result<()> {
    add_column_if_table_exists(
        transaction,
        table,
        "source_kind TEXT NOT NULL DEFAULT 'rollout'",
    )?;
    add_column_if_table_exists(transaction, table, "ingested_at TEXT")?;
    add_column_if_table_exists(
        transaction,
        table,
        "parser_schema_version INTEGER NOT NULL DEFAULT 1",
    )
}

fn add_column_if_table_exists(
    transaction: &Transaction<'_>,
    table: &str,
    definition: &str,
) -> Result<()> {
    let exists = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if exists {
        transaction.execute(
            &format!(
                "ALTER TABLE {} ADD COLUMN {definition}",
                quote_identifier(table)
            ),
            [],
        )?;
    }
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn state_data(read: StateReadResult) -> CanonicalData {
    let StateReadResult {
        sessions,
        diagnostics,
    } = read;
    CanonicalData {
        sessions,
        diagnostics: diagnostics.iter().map(canonical_state_diagnostic).collect(),
        ..CanonicalData::default()
    }
}

fn unreadable_data(path: &Path, kind: SourceKind, message: &str) -> CanonicalData {
    let source = match kind {
        SourceKind::Rollout => SourceRef::rollout(path.to_path_buf(), 1),
        SourceKind::State => SourceRef::state(path.to_path_buf()),
    };
    CanonicalData {
        diagnostics: vec![CanonicalDiagnostic {
            kind: DiagnosticKind::Unreadable,
            source,
            message: bounded_message(message),
        }],
        ..CanonicalData::default()
    }
}

fn stamp_data(data: &mut CanonicalData, timestamp: &str) {
    for session in &mut data.sessions {
        session.provenance.stamp_ingest_time(timestamp);
    }
    for turn in &mut data.turns {
        turn.provenance.stamp_ingest_time(timestamp);
        for event in &mut turn.lifecycle {
            event.provenance.stamp_ingest_time(timestamp);
        }
    }
    for record in &mut data.records {
        record.provenance.stamp_ingest_time(timestamp);
    }
    for message in &mut data.messages {
        message.provenance.stamp_ingest_time(timestamp);
    }
    for call in &mut data.tool_calls {
        call.provenance.stamp_ingest_time(timestamp);
    }
    for result in &mut data.tool_results {
        result.provenance.stamp_ingest_time(timestamp);
    }
    for operation in &mut data.file_operations {
        operation.provenance.stamp_ingest_time(timestamp);
    }
    for usage in &mut data.token_usage {
        usage.provenance.stamp_ingest_time(timestamp);
    }
    for diagnostic in &mut data.diagnostics {
        diagnostic.source.stamp_ingest_time(timestamp);
    }
    for snapshot in &mut data.instruction_snapshots {
        snapshot.provenance.stamp_ingest_time(timestamp);
    }
    for join in &mut data.instruction_joins {
        join.provenance.stamp_ingest_time(timestamp);
    }
}

fn current_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

fn source_kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Rollout => "rollout",
        SourceKind::State => "state",
    }
}

fn bounded_message(message: &str) -> String {
    const MAX_BYTES: usize = 512;
    if message.len() <= MAX_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

fn canonical_state_diagnostic(diagnostic: &StateDiagnostic) -> CanonicalDiagnostic {
    CanonicalDiagnostic {
        kind: diagnostic.kind.canonical_kind(),
        source: diagnostic.source.clone(),
        message: diagnostic.message.clone(),
    }
}

fn delete_source(
    transaction: &Transaction<'_>,
    identity: &str,
    preserve_instruction_snapshots: bool,
) -> rusqlite::Result<()> {
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
        "instruction_files",
        "instruction_joins",
    ] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE source_identity = ?1"),
            params![identity],
        )?;
    }
    if !preserve_instruction_snapshots {
        transaction.execute(
            "DELETE FROM instruction_snapshots WHERE source_identity = ?1",
            params![identity],
        )?;
    }
    Ok(())
}

fn insert_data(
    transaction: &Transaction<'_>,
    identity: &str,
    data: &CanonicalData,
    preserve_instruction_snapshots: bool,
) -> Result<()> {
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
    if !preserve_instruction_snapshots {
        for (index, snapshot) in data.instruction_snapshots.iter().enumerate() {
            insert_instruction_snapshot(transaction, identity, snapshot, index)?;
        }
    }
    for join in &data.instruction_joins {
        insert_instruction_join(transaction, identity, join)?;
    }
    Ok(())
}

fn insert_session(transaction: &Transaction<'_>, identity: &str, session: &Session) -> Result<()> {
    transaction.execute(
        "INSERT OR REPLACE INTO sessions (session_id, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, created_at, updated_at, cwd, project, model, provider, source, thread_source, rollout_path, archive_state, title, preview, parent_id, cli_version, originator, history_mode, reasoning_effort) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        params![
            session.id,
            identity,
            session.provenance.path.to_string_lossy().as_ref(),
            db_line(session.provenance.line),
            source_kind_name(session.provenance.kind),
            session.provenance.ingested_at,
            i64::from(session.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO turns (turn_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, started_at, completed_at, cwd, model, reasoning_effort, sequence, lifecycle_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            key,
            identity,
            turn.provenance.path.to_string_lossy().as_ref(),
            db_line(turn.provenance.line),
            source_kind_name(turn.provenance.kind),
            turn.provenance.ingested_at,
            i64::from(turn.provenance.parser_schema_version),
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
    let (kind, record_type, nested_type, raw_json) = record_kind_values(record);
    transaction.execute(
        "INSERT OR REPLACE INTO records (record_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, timestamp, sequence, kind, record_type, nested_type, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            key,
            identity,
            record.provenance.path.to_string_lossy().as_ref(),
            db_line(record.provenance.line),
            source_kind_name(record.provenance.kind),
            record.provenance.ingested_at,
            i64::from(record.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO messages (message_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, message_id, session_id, turn_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            key,
            identity,
            message.provenance.path.to_string_lossy().as_ref(),
            db_line(message.provenance.line),
            source_kind_name(message.provenance.kind),
            message.provenance.ingested_at,
            i64::from(message.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO tool_calls (call_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, item_id, call_id, session_id, turn_id, tool_name, input_summary, command, cwd, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            key,
            identity,
            call.provenance.path.to_string_lossy().as_ref(),
            db_line(call.provenance.line),
            source_kind_name(call.provenance.kind),
            call.provenance.ingested_at,
            i64::from(call.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO tool_results (result_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, result_id, call_id, session_id, turn_id, command, cwd, stdout, stderr, duration_ms, exit_code, status, outcome, outcome_source, matched_call, deduplication_key, equivalent_to_path, equivalent_to_line, is_duplicate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
        params![
            key,
            identity,
            result.provenance.path.to_string_lossy().as_ref(),
            db_line(result.provenance.line),
            source_kind_name(result.provenance.kind),
            result.provenance.ingested_at,
            i64::from(result.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO file_operations (operation_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, path, operation, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            key,
            identity,
            operation.provenance.path.to_string_lossy().as_ref(),
            db_line(operation.provenance.line),
            source_kind_name(operation.provenance.kind),
            operation.provenance.ingested_at,
            i64::from(operation.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO token_usage (usage_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, timestamp, input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens, sequence) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            key,
            identity,
            usage.provenance.path.to_string_lossy().as_ref(),
            db_line(usage.provenance.line),
            source_kind_name(usage.provenance.kind),
            usage.provenance.ingested_at,
            i64::from(usage.provenance.parser_schema_version),
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
        "INSERT OR REPLACE INTO diagnostics (diagnostic_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, kind, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            key,
            identity,
            diagnostic.source.path.to_string_lossy().as_ref(),
            db_line(diagnostic.source.line),
            source_kind_name(diagnostic.source.kind),
            diagnostic.source.ingested_at,
            i64::from(diagnostic.source.parser_schema_version),
            diagnostic.kind.as_str(),
            diagnostic.message,
        ],
    )?;
    Ok(())
}

fn insert_instruction_snapshot(
    transaction: &Transaction<'_>,
    identity: &str,
    snapshot: &InstructionSnapshot,
    index: usize,
) -> Result<()> {
    let key = row_key(
        identity,
        &snapshot.provenance,
        &format!(
            "snapshot:{index}:{}:{}",
            snapshot.session_id.as_deref().unwrap_or(""),
            snapshot.turn_id.as_deref().unwrap_or("")
        ),
    );
    let blob_key = match (&snapshot.content_hash, &snapshot.content) {
        (Some(content_hash), Some(content)) => Some(ensure_instruction_blob(
            transaction,
            content_hash,
            snapshot.byte_count,
            content,
        )?),
        _ => None,
    };
    let chain_json = serde_json::to_string(&snapshot.chain)?;
    transaction.execute(
        "INSERT OR REPLACE INTO instruction_snapshots (snapshot_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, turn_id, snapshot_source, accuracy, blob_key, content_hash, byte_count, effective_chain_hash, truncated, chain_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            key,
            identity,
            snapshot.provenance.path.to_string_lossy().as_ref(),
            db_line(snapshot.provenance.line),
            source_kind_name(snapshot.provenance.kind),
            snapshot.provenance.ingested_at,
            i64::from(snapshot.provenance.parser_schema_version),
            snapshot.session_id,
            snapshot.turn_id,
            snapshot.source.as_str(),
            snapshot.accuracy.as_str(),
            blob_key,
            snapshot.content_hash,
            i64::try_from(snapshot.byte_count).unwrap_or(i64::MAX),
            snapshot.effective_chain_hash,
            i64::from(snapshot.truncated),
            chain_json,
        ],
    )?;
    Ok(())
}

fn insert_instruction_join(
    transaction: &Transaction<'_>,
    identity: &str,
    join: &InstructionJoin,
) -> Result<()> {
    for (index, file) in join.resolution.files.iter().enumerate() {
        insert_instruction_file(transaction, identity, join, file, index)?;
    }
    let key = row_key(
        identity,
        &join.provenance,
        &format!("join:{}", join.session_id),
    );
    let chain = snapshot_entries(&join.resolution.chain);
    let chain_json = serde_json::to_string(&chain)?;
    let diagnostics_json = serde_json::to_string(&join.resolution.diagnostics)?;
    transaction.execute(
        "INSERT OR REPLACE INTO instruction_joins (join_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, cwd, project_root, project_root_status, nearest_path, nearest_scope, effective_chain_hash, chain_json, diagnostics_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            key,
            identity,
            join.provenance.path.to_string_lossy().as_ref(),
            db_line(join.provenance.line),
            source_kind_name(join.provenance.kind),
            join.provenance.ingested_at,
            i64::from(join.provenance.parser_schema_version),
            join.session_id,
            join.cwd.as_ref().map(|path| path.to_string_lossy().into_owned()),
            join.project_root
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            join.project_root_status.as_str(),
            join.nearest_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            join.nearest_scope.map(InstructionScope::as_str),
            join.resolution.effective_chain_hash,
            chain_json,
            diagnostics_json,
        ],
    )?;
    Ok(())
}

fn insert_instruction_file(
    transaction: &Transaction<'_>,
    identity: &str,
    join: &InstructionJoin,
    file: &InstructionFile,
    index: usize,
) -> Result<()> {
    let blob_key = match (&file.content_hash, &file.content) {
        (Some(content_hash), Some(content)) => Some(ensure_instruction_blob(
            transaction,
            content_hash,
            content.len(),
            content,
        )?),
        _ => None,
    };
    let key = row_key(
        identity,
        &join.provenance,
        &format!(
            "instruction-file:{index}:{}:{}",
            join.session_id,
            file.path.display()
        ),
    );
    transaction.execute(
        "INSERT OR REPLACE INTO instruction_files (file_key, source_identity, source_path, source_line, source_kind, ingested_at, parser_schema_version, session_id, path, scope, file_kind, state, chain_position, blob_key, content_hash, byte_count, diagnostic) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            key,
            identity,
            join.provenance.path.to_string_lossy().as_ref(),
            db_line(join.provenance.line),
            source_kind_name(join.provenance.kind),
            join.provenance.ingested_at,
            i64::from(join.provenance.parser_schema_version),
            join.session_id,
            file.path.to_string_lossy().as_ref(),
            file.scope.as_str(),
            file.kind.as_str(),
            file.state.as_str(),
            file.chain_position.and_then(|position| i64::try_from(position).ok()),
            blob_key,
            file.content_hash,
            i64::try_from(file.byte_count).unwrap_or(i64::MAX),
            file.diagnostic,
        ],
    )?;
    Ok(())
}

fn ensure_instruction_blob(
    transaction: &Transaction<'_>,
    content_hash: &str,
    byte_count: usize,
    content: &str,
) -> Result<String> {
    let key = format!("{content_hash}:{byte_count}");
    transaction.execute(
        "INSERT OR IGNORE INTO instruction_blobs (blob_key, content_hash, byte_count, content) VALUES (?1, ?2, ?3, ?4)",
        params![
            key,
            content_hash,
            i64::try_from(byte_count).unwrap_or(i64::MAX),
            content,
        ],
    )?;
    Ok(key)
}

fn record_kind_values(record: &Record) -> (&'static str, Option<&str>, Option<&str>, Option<&str>) {
    match &record.kind {
        RecordKind::SessionMetadata => (
            "session_metadata",
            record
                .original_record_type
                .as_deref()
                .or(Some("session_meta")),
            record.original_nested_type.as_deref(),
            None,
        ),
        RecordKind::TurnContext => (
            "turn_context",
            record
                .original_record_type
                .as_deref()
                .or(Some("turn_context")),
            record.original_nested_type.as_deref(),
            None,
        ),
        RecordKind::ResponseItem => (
            "response_item",
            record
                .original_record_type
                .as_deref()
                .or(Some("response_item")),
            record.original_nested_type.as_deref(),
            None,
        ),
        RecordKind::EventMessage => (
            "event_message",
            record.original_record_type.as_deref().or(Some("event_msg")),
            record.original_nested_type.as_deref(),
            None,
        ),
        RecordKind::Compacted => (
            "compacted",
            record.original_record_type.as_deref().or(Some("compacted")),
            record.original_nested_type.as_deref(),
            None,
        ),
        RecordKind::WorldState => (
            "world_state",
            record
                .original_record_type
                .as_deref()
                .or(Some("world_state")),
            record.original_nested_type.as_deref(),
            None,
        ),
        RecordKind::Unknown {
            record_type,
            nested_type,
            raw_json,
        } => (
            "unknown",
            record
                .original_record_type
                .as_deref()
                .or(record_type.as_deref()),
            record
                .original_nested_type
                .as_deref()
                .or(nested_type.as_deref()),
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

fn capture_options_for_inputs(inputs: &[DiscoveredInput]) -> InstructionCaptureOptions {
    inputs
        .iter()
        .find_map(|input| codex_home_for_source(&input.path))
        .map_or_else(InstructionCaptureOptions::default, |codex_home| {
            InstructionCaptureOptions::from_codex_home(&codex_home, None).0
        })
}

fn resolver_for_source(path: &Path) -> InstructionResolver {
    codex_home_for_source(path)
        .map_or_else(InstructionCaptureOptions::default, |codex_home| {
            InstructionCaptureOptions::from_codex_home(&codex_home, None).0
        })
        .resolver()
}

fn resolver_for_inputs(inputs: &[DiscoveredInput]) -> InstructionResolver {
    capture_options_for_inputs(inputs).resolver()
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
            "instruction_blobs",
            "instruction_snapshots",
            "instruction_files",
            "instruction_joins",
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
                .query_row("SELECT MAX(version) FROM schema_versions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn instruction_snapshots_keep_turns_and_deduplicate_blobs() {
        let source = temp_path("instructions.jsonl");
        let instruction_text = "synthetic observed instruction";
        let rollout = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"fixture-instruction-session\",\"cwd\":\"/fixture/project\",\"project\":\"/fixture\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-instruction-turn-001\",\"cwd\":\"/fixture/project\",\"user_instructions\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-instruction-turn-002\",\"cwd\":\"/fixture/project\",\"user_instructions\":\"{}\"}}}}\n",
            instruction_text, instruction_text
        );
        fs::write(&source, rollout).unwrap();
        let mut store = Store::in_memory().unwrap();

        store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();

        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM instruction_snapshots", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM instruction_blobs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let snapshot = store
            .connection()
            .query_row(
                "SELECT snapshot_source, accuracy, content_hash, byte_count, effective_chain_hash, chain_json FROM instruction_snapshots ORDER BY turn_id LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(snapshot.0, "rollout");
        assert_eq!(snapshot.1, "observed");
        assert_eq!(snapshot.3, i64::try_from(instruction_text.len()).unwrap());
        assert_eq!(snapshot.2, snapshot.4);
        assert!(snapshot.5.contains("Observed"));
        let _ = fs::remove_file(source);
    }

    #[test]
    fn filesystem_instruction_join_is_stored_without_using_the_checkout() {
        let root = temp_path("project");
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("AGENTS.md"), "root instruction").unwrap();
        fs::write(nested.join("AGENTS.override.md"), "nested instruction").unwrap();
        let source = temp_path("filesystem.jsonl");
        let rollout = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"fixture-filesystem-session\",\"cwd\":\"{}\",\"project\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-filesystem-turn\",\"cwd\":\"{}\"}}}}\n",
            nested.display(),
            root.display(),
            nested.display(),
        );
        fs::write(&source, rollout).unwrap();
        let mut store = Store::in_memory().unwrap();

        store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();

        let join = store
            .connection()
            .query_row(
                "SELECT project_root_status, nearest_path, nearest_scope, chain_json FROM instruction_joins WHERE session_id = ?1",
                params!["fixture-filesystem-session"],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(join.0, "known");
        assert!(join.1.ends_with("AGENTS.override.md"));
        assert_eq!(join.2, "project_nested");
        assert!(join.3.contains("AGENTS.md"));
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM instruction_files WHERE session_id = ?1",
                    params!["fixture-filesystem-session"],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM instruction_blobs WHERE content IN ('root instruction', 'nested instruction')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let snapshot = store
            .connection()
            .query_row(
                "SELECT snapshot_source, accuracy, effective_chain_hash FROM instruction_snapshots WHERE session_id = ?1",
                params!["fixture-filesystem-session"],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(snapshot.0, "filesystem_at_ingest");
        assert_eq!(snapshot.1, "reconstructed");
        assert!(!snapshot.2.is_empty());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn unchanged_rollout_keeps_snapshot_when_instruction_file_changes() {
        let root = temp_path("stable-project");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("AGENTS.md"), "before").unwrap();
        let source = temp_path("stable.jsonl");
        let rollout = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"fixture-stable-session\",\"cwd\":\"{}\",\"project\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-stable-turn\",\"cwd\":\"{}\"}}}}\n",
            root.display(),
            root.display(),
            root.display(),
        );
        fs::write(&source, rollout).unwrap();
        let mut store = Store::in_memory().unwrap();
        let capture = InstructionCaptureOptions::default();

        let first = store
            .ingest_rollout_file_with_instructions(
                &source,
                &RolloutParseOptions::default(),
                &capture,
            )
            .unwrap();
        assert!(!first.skipped);
        let before: String = store
            .connection()
            .query_row("SELECT content FROM instruction_blobs", [], |row| {
                row.get(0)
            })
            .unwrap();

        fs::write(root.join("AGENTS.md"), "after").unwrap();
        let second = store
            .ingest_rollout_file_with_instructions(
                &source,
                &RolloutParseOptions::default(),
                &capture,
            )
            .unwrap();

        assert!(second.skipped);
        let after: String = store
            .connection()
            .query_row("SELECT content FROM instruction_blobs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(before, "before");
        assert_eq!(after, before);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn rollout_ingest_is_idempotent_and_changed_input_replaces_rows() {
        let source = temp_path("rollout.jsonl");
        copy_fixture("rollout", "store-initial.jsonl", &source);
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
        let provenance = store
            .connection()
            .query_row(
                "SELECT source_kind, record_type, ingested_at, parser_schema_version FROM records",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(provenance.0, "rollout");
        assert_eq!(provenance.1, "session_meta");
        assert!(provenance.2.is_some());
        assert_eq!(provenance.3, 1);

        copy_fixture("rollout", "store-changed.jsonl", &source);
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
        copy_fixture("rollout", "store-initial.jsonl", &source);
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_rollout_file(&source, &RolloutParseOptions::default())
            .unwrap();
        store
            .connection
            .execute_batch(include_str!("../tests/fixtures/store/rollback-trigger.sql"))
            .unwrap();
        copy_fixture("rollout", "store-changed.jsonl", &source);

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
        create_database(&source, include_str!("../tests/fixtures/state/current.sql"));

        let mut store = Store::in_memory().unwrap();
        let summary = store.ingest_state_database(&source).unwrap();
        assert_eq!(summary.sessions, 1);
        let row = store
            .connection()
            .query_row(
                "SELECT session_id, cwd, provider, archive_state, project, source_kind FROM sessions",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "fixture-current-session".to_owned(),
                "/fixture".to_owned(),
                "provider".to_owned(),
                1,
                "/project".to_owned(),
                "state".to_owned(),
            )
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn schema_mismatch_preserves_previous_state_ingest() {
        let source = temp_path("state-rollback.sqlite");
        create_database(&source, include_str!("../tests/fixtures/state/current.sql"));

        let mut store = Store::in_memory().unwrap();
        store.ingest_state_database(&source).unwrap();
        std::fs::remove_file(&source).unwrap();
        create_database(
            &source,
            include_str!("../tests/fixtures/state/incompatible.sql"),
        );

        let summary = store.ingest_state_database(&source).unwrap();

        assert_eq!(summary.sessions, 0);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM diagnostics WHERE kind = 'state_schema_mismatch'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn v1_message_schema_migrates_to_nullable_content() {
        let source = temp_path("schema-v1.sqlite");
        create_database(
            &source,
            include_str!("../tests/fixtures/store/schema-v1.sql"),
        );

        let store = Store::open(&source).unwrap();
        let not_null = store
            .connection()
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('messages') WHERE name = 'content'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();

        assert_eq!(not_null, 0);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT MAX(version) FROM schema_versions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn changed_state_refreshes_enriched_rollout_without_duplicate_sessions() {
        let state_source = temp_path("enrichment.sqlite");
        let rollout_source = temp_path("enrichment.jsonl");
        create_database(
            &state_source,
            include_str!("../tests/fixtures/state/enrichment.sql"),
        );
        copy_fixture("rollout", "enrichment.jsonl", &rollout_source);
        let connection = Connection::open(&state_source).unwrap();
        connection
            .execute(
                "UPDATE threads SET rollout_path = ?1",
                params![rollout_source.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);

        let inputs = vec![
            DiscoveredInput {
                path: state_source.clone(),
                identity: fs::canonicalize(&state_source).unwrap(),
                kind: InputKind::StateDatabase,
                reader: None,
            },
            DiscoveredInput {
                path: rollout_source.clone(),
                identity: fs::canonicalize(&rollout_source).unwrap(),
                kind: InputKind::Rollout { archived: false },
                reader: Some(ReaderKind::PlainJsonl),
            },
        ];
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_inputs(&inputs, &IngestOptions::default())
            .unwrap();

        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT project FROM sessions WHERE session_id = 'fixture-enrichment-session'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "/state-project-v1"
        );

        let connection = Connection::open(&state_source).unwrap();
        connection
            .execute("UPDATE threads SET project_path = '/state-project-v2'", [])
            .unwrap();
        drop(connection);

        let report = store
            .ingest_inputs(&inputs, &IngestOptions::default())
            .unwrap();

        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT project FROM sessions WHERE session_id = 'fixture-enrichment-session'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "/state-project-v2"
        );
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            report
                .files
                .iter()
                .any(|file| file.source == rollout_source && !file.skipped)
        );

        let standalone = store.ingest_state_database(&state_source).unwrap();
        assert!(!standalone.skipped);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );

        let mixed_again = store
            .ingest_inputs(&inputs, &IngestOptions::default())
            .unwrap();
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            mixed_again
                .files
                .iter()
                .any(|file| file.source == state_source && !file.skipped)
        );
        let _ = fs::remove_file(state_source);
        let _ = fs::remove_file(rollout_source);
    }

    #[test]
    fn state_refresh_updates_join_without_replacing_rollout_snapshot() {
        let state_source = temp_path("historical-state.sqlite");
        let rollout_source = temp_path("historical-rollout.jsonl");
        let project_root = temp_path("historical-project");
        fs::create_dir_all(&project_root).unwrap();
        fs::write(project_root.join("AGENTS.md"), "before").unwrap();
        create_database(
            &state_source,
            include_str!("../tests/fixtures/state/enrichment.sql"),
        );
        let connection = Connection::open(&state_source).unwrap();
        connection
            .execute(
                "UPDATE threads SET rollout_path = ?1, project_path = ?2",
                params![
                    rollout_source.to_string_lossy().as_ref(),
                    project_root.to_string_lossy().as_ref(),
                ],
            )
            .unwrap();
        drop(connection);
        fs::write(
            &rollout_source,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"fixture-enrichment-session\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"fixture-enrichment-turn\",\"cwd\":\"{}\"}}}}\n",
                project_root.display()
            ),
        )
        .unwrap();
        let inputs = vec![
            DiscoveredInput {
                path: state_source.clone(),
                identity: fs::canonicalize(&state_source).unwrap(),
                kind: InputKind::StateDatabase,
                reader: None,
            },
            DiscoveredInput {
                path: rollout_source.clone(),
                identity: fs::canonicalize(&rollout_source).unwrap(),
                kind: InputKind::Rollout { archived: false },
                reader: Some(ReaderKind::PlainJsonl),
            },
        ];
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_inputs(&inputs, &IngestOptions::default())
            .unwrap();
        let before: String = store
            .connection()
            .query_row(
                "SELECT b.content FROM instruction_snapshots AS s JOIN instruction_blobs AS b ON b.blob_key = s.blob_key WHERE s.session_id = ?1",
                params!["fixture-enrichment-session"],
                |row| row.get(0),
            )
            .unwrap();

        fs::write(project_root.join("AGENTS.md"), "after").unwrap();
        let connection = Connection::open(&state_source).unwrap();
        connection
            .execute("UPDATE threads SET model = 'state-model-v2'", [])
            .unwrap();
        drop(connection);

        store
            .ingest_inputs(&inputs, &IngestOptions::default())
            .unwrap();
        let snapshot: String = store
            .connection()
            .query_row(
                "SELECT b.content FROM instruction_snapshots AS s JOIN instruction_blobs AS b ON b.blob_key = s.blob_key WHERE s.session_id = ?1",
                params!["fixture-enrichment-session"],
                |row| row.get(0),
            )
            .unwrap();
        let current_file: String = store
            .connection()
            .query_row(
                "SELECT b.content FROM instruction_files AS f JOIN instruction_blobs AS b ON b.blob_key = f.blob_key WHERE f.session_id = ?1 AND f.state = 'selected'",
                params!["fixture-enrichment-session"],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(before, "before");
        assert_eq!(snapshot, before);
        assert_eq!(current_file, "after");
        let _ = fs::remove_file(state_source);
        let _ = fs::remove_file(rollout_source);
        let _ = fs::remove_dir_all(project_root);
    }

    #[test]
    fn unreadable_state_does_not_abort_other_inputs() {
        let missing_state = temp_path("missing-state.sqlite");
        let rollout_source = temp_path("readable-rollout.jsonl");
        copy_fixture("rollout", "store-initial.jsonl", &rollout_source);
        let inputs = vec![
            DiscoveredInput {
                path: missing_state.clone(),
                identity: missing_state.clone(),
                kind: InputKind::StateDatabase,
                reader: None,
            },
            DiscoveredInput {
                path: rollout_source.clone(),
                identity: fs::canonicalize(&rollout_source).unwrap(),
                kind: InputKind::Rollout { archived: false },
                reader: Some(ReaderKind::PlainJsonl),
            },
        ];

        let mut store = Store::in_memory().unwrap();
        let report = store
            .ingest_inputs(&inputs, &IngestOptions::default())
            .unwrap();

        assert_eq!(report.files.len(), 2);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM records", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM diagnostics WHERE kind = 'unreadable'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let _ = fs::remove_file(rollout_source);
    }

    fn copy_fixture(category: &str, name: &str, destination: &Path) {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(category)
            .join(name);
        fs::copy(source, destination).unwrap();
    }

    fn create_database(path: &Path, schema: &str) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(schema).unwrap();
    }
}
