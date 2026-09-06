use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::CanonicalData;
use crate::normalize::{
    RolloutNormalizationContext, normalize_rollout_incremental, pending_tool_calls_for_source,
    recent_tool_results_for_source,
};
use crate::rollout::{
    PlainJsonlReader, ReadLine, RolloutLineReader, RolloutParseOptions, RolloutParseResult,
    RolloutRecord, RolloutRecordKind, parse_rollout_reader,
};
use crate::store::{IngestInputKind, IngestSummary, Store};

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_POLL_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_EVENT_IDENTITIES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorCursor {
    pub source_identity: PathBuf,
    pub offset: u64,
    pub line: usize,
    pub sequence: usize,
    pub digest: u64,
    pub recent_event_identities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorOptions {
    pub max_line_bytes: usize,
    pub max_poll_bytes: usize,
    pub max_event_identities: usize,
    pub poll_interval: Duration,
}

impl Default for MonitorOptions {
    fn default() -> Self {
        Self {
            max_line_bytes: RolloutParseOptions::default().max_line_bytes,
            max_poll_bytes: DEFAULT_MAX_POLL_BYTES,
            max_event_identities: DEFAULT_MAX_EVENT_IDENTITIES,
            poll_interval: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatus {
    Idle,
    Updated,
    PartialLine,
    Rotated,
    Truncated,
    DuplicateIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorDiagnosticKind {
    SourceRotated,
    SourceTruncated,
    IncompleteFinalLine,
    DuplicateIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDiagnostic {
    pub kind: MonitorDiagnosticKind,
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorPoll {
    pub status: MonitorStatus,
    pub cursor: MonitorCursor,
    pub records: usize,
    pub skipped_duplicates: usize,
    pub diagnostics: Vec<MonitorDiagnostic>,
    pub ingest: Option<IngestSummary>,
}

pub trait MonitorClock {
    fn sleep(&mut self, duration: Duration);
}

pub struct SystemMonitorClock;

impl MonitorClock for SystemMonitorClock {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Clone, Copy)]
enum Source {
    Rollout,
    State,
}

pub struct LocalMonitor {
    source: Source,
    path: PathBuf,
    cursor: MonitorCursor,
    options: MonitorOptions,
    initial: bool,
    context: Option<RolloutNormalizationContext>,
    recent_event_identities: VecDeque<String>,
}

impl LocalMonitor {
    pub fn rollout(
        path: &Path,
        cursor: Option<MonitorCursor>,
        options: MonitorOptions,
    ) -> Result<Self> {
        Self::new(Source::Rollout, path, cursor, options)
    }

    pub fn state(
        path: &Path,
        cursor: Option<MonitorCursor>,
        options: MonitorOptions,
    ) -> Result<Self> {
        Self::new(Source::State, path, cursor, options)
    }

    fn new(
        source: Source,
        path: &Path,
        cursor: Option<MonitorCursor>,
        options: MonitorOptions,
    ) -> Result<Self> {
        let path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve monitor source {}", path.display()))?;
        let mut cursor = cursor.unwrap_or_else(|| MonitorCursor {
            source_identity: path.clone(),
            offset: 0,
            line: 0,
            sequence: 0,
            digest: FNV_OFFSET_BASIS,
            recent_event_identities: Vec::new(),
        });
        if cursor.source_identity != path {
            bail!(
                "monitor cursor belongs to {}, not {}",
                cursor.source_identity.display(),
                path.display()
            );
        }
        cursor
            .recent_event_identities
            .truncate(options.max_event_identities);
        let recent_event_identities = cursor.recent_event_identities.iter().cloned().collect();
        let initial = cursor.offset == 0 && cursor.sequence == 0;
        Ok(Self {
            source,
            path,
            cursor,
            options,
            initial,
            context: None,
            recent_event_identities,
        })
    }

    pub fn cursor(&self) -> &MonitorCursor {
        &self.cursor
    }

    pub fn poll(&mut self, store: &mut Store) -> Result<MonitorPoll> {
        match self.source {
            Source::Rollout => self.poll_rollout(store),
            Source::State => self.poll_state(store),
        }
    }

    pub fn run<C, F>(
        &mut self,
        store: &mut Store,
        clock: &mut C,
        mut should_stop: F,
    ) -> Result<MonitorCursor>
    where
        C: MonitorClock,
        F: FnMut(&MonitorPoll) -> bool,
    {
        loop {
            let poll = self.poll(store)?;
            if should_stop(&poll) {
                return Ok(self.cursor.clone());
            }
            clock.sleep(self.options.poll_interval);
        }
    }

    fn poll_rollout(&mut self, store: &mut Store) -> Result<MonitorPoll> {
        let size = fs::metadata(&self.path)?.len();
        let mut transition = None;
        if size < self.cursor.offset {
            transition = Some((
                MonitorStatus::Truncated,
                MonitorDiagnosticKind::SourceTruncated,
                "source was truncated; monitoring restarted at byte offset 0",
            ));
            self.reset_cursor();
        } else if prefix_digest(&self.path, self.cursor.offset)? != self.cursor.digest {
            transition = Some((
                MonitorStatus::Rotated,
                MonitorDiagnosticKind::SourceRotated,
                "source identity changed; monitoring restarted at byte offset 0",
            ));
            self.reset_cursor();
        }

        let start = self.cursor.offset;
        let line_start = self.cursor.line;
        let sequence_start = self.cursor.sequence;
        let complete_end = complete_end(&self.path, start, size, self.options.max_poll_bytes)?;
        let newline_count = count_newlines(&self.path, start, complete_end)?;
        let has_partial_line = complete_end < size;
        let mut diagnostics = Vec::new();
        if let Some((_, kind, message)) = transition {
            diagnostics.push(MonitorDiagnostic {
                kind,
                line: None,
                message: message.to_owned(),
            });
        }
        if has_partial_line {
            diagnostics.push(MonitorDiagnostic {
                kind: MonitorDiagnosticKind::IncompleteFinalLine,
                line: Some(line_start + newline_count + 1),
                message: "incomplete final line is held until a newline arrives".to_owned(),
            });
        }

        let parsed = if complete_end > start {
            parse_window(
                &self.path,
                start,
                complete_end,
                line_start,
                self.options.max_line_bytes,
            )?
        } else {
            RolloutParseResult::default()
        };
        let (records, skipped_duplicates) =
            self.filter_duplicates(parsed.records, &mut diagnostics);
        let parsed = RolloutParseResult {
            records,
            diagnostics: parsed.diagnostics,
        };
        let data = if complete_end > start {
            let (state_sessions, context) = if let Some(context) = self.context.clone() {
                (Vec::new(), Some(context))
            } else {
                let stored = store.load_canonical()?;
                let context = (self.cursor.sequence > 0)
                    .then(|| stored_context(&stored, &self.path))
                    .flatten();
                let state_sessions = stored
                    .sessions
                    .iter()
                    .filter(|session| session.provenance.path != self.path)
                    .cloned()
                    .collect::<Vec<_>>();
                (state_sessions, context)
            };
            let resolver = crate::store::resolver_for_source(&self.path);
            let (data, context) = normalize_rollout_incremental(
                &parsed,
                &state_sessions,
                &resolver,
                context.as_ref(),
                sequence_start,
            );
            self.context = Some(context);
            data
        } else {
            CanonicalData::default()
        };

        let should_write = self.initial
            || transition.is_some()
            || !data.records.is_empty()
            || !data.diagnostics.is_empty();
        let ingest = if should_write {
            let summary = if self.initial || transition.is_some() {
                store.ingest_canonical(&self.path, IngestInputKind::Rollout, &data)?
            } else {
                store.append_canonical(&self.path, IngestInputKind::Rollout, &data)?
            };
            Some(summary)
        } else {
            None
        };

        self.cursor.offset = complete_end;
        self.cursor.line = line_start + newline_count;
        self.cursor.sequence = sequence_start + data.records.len();
        self.cursor.digest = prefix_digest(&self.path, complete_end)?;
        self.cursor.recent_event_identities =
            self.recent_event_identities.iter().cloned().collect();
        self.initial = false;

        let status = transition.map_or_else(
            || {
                if skipped_duplicates > 0 {
                    MonitorStatus::DuplicateIdentity
                } else if has_partial_line {
                    MonitorStatus::PartialLine
                } else if ingest.is_some() {
                    MonitorStatus::Updated
                } else {
                    MonitorStatus::Idle
                }
            },
            |(status, _, _)| status,
        );
        Ok(MonitorPoll {
            status,
            cursor: self.cursor.clone(),
            records: data.records.len(),
            skipped_duplicates,
            diagnostics,
            ingest,
        })
    }

    fn poll_state(&mut self, store: &mut Store) -> Result<MonitorPoll> {
        let (size, digest) = source_fingerprint(&self.path)?;
        if !self.initial && self.cursor.offset == size && self.cursor.digest == digest {
            return Ok(MonitorPoll {
                status: MonitorStatus::Idle,
                cursor: self.cursor.clone(),
                records: 0,
                skipped_duplicates: 0,
                diagnostics: Vec::new(),
                ingest: None,
            });
        }
        let summary = store.ingest_state_database(&self.path)?;
        self.cursor.offset = size;
        self.cursor.digest = digest;
        self.cursor.recent_event_identities.clear();
        self.initial = false;
        let status = if summary.skipped {
            MonitorStatus::Idle
        } else {
            MonitorStatus::Updated
        };
        Ok(MonitorPoll {
            status,
            cursor: self.cursor.clone(),
            records: summary.records,
            skipped_duplicates: 0,
            diagnostics: Vec::new(),
            ingest: Some(summary),
        })
    }

    fn reset_cursor(&mut self) {
        self.cursor.offset = 0;
        self.cursor.line = 0;
        self.cursor.sequence = 0;
        self.cursor.digest = FNV_OFFSET_BASIS;
        self.cursor.recent_event_identities.clear();
        self.recent_event_identities.clear();
        self.context = None;
    }

    fn filter_duplicates(
        &mut self,
        records: Vec<RolloutRecord>,
        diagnostics: &mut Vec<MonitorDiagnostic>,
    ) -> (Vec<RolloutRecord>, usize) {
        let mut accepted = Vec::with_capacity(records.len());
        let mut skipped = 0;
        for record in records {
            let Some(identity) = record_identity(&record) else {
                accepted.push(record);
                continue;
            };
            if self
                .recent_event_identities
                .iter()
                .any(|seen| seen == &identity)
            {
                skipped += 1;
                diagnostics.push(MonitorDiagnostic {
                    kind: MonitorDiagnosticKind::DuplicateIdentity,
                    line: Some(record.source.line),
                    message: "duplicate event identity was skipped".to_owned(),
                });
                continue;
            }
            if self.options.max_event_identities > 0 {
                self.recent_event_identities.push_back(identity);
                while self.recent_event_identities.len() > self.options.max_event_identities {
                    self.recent_event_identities.pop_front();
                }
            }
            accepted.push(record);
        }
        (accepted, skipped)
    }
}

struct OffsetReader<R> {
    reader: PlainJsonlReader<R>,
    line_offset: usize,
}

impl<R: Read> RolloutLineReader for OffsetReader<R> {
    fn next_line(&mut self) -> io::Result<Option<ReadLine>> {
        self.reader.next_line().map(|line| {
            line.map(|line| match line {
                ReadLine::Line(mut line) => {
                    line.line = line.line.saturating_add(self.line_offset);
                    ReadLine::Line(line)
                }
                ReadLine::Oversized { line, byte_count } => ReadLine::Oversized {
                    line: line.saturating_add(self.line_offset),
                    byte_count,
                },
            })
        })
    }
}

fn parse_window(
    path: &Path,
    start: u64,
    end: u64,
    line_offset: usize,
    max_line_bytes: usize,
) -> Result<RolloutParseResult> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let reader = OffsetReader {
        reader: PlainJsonlReader::with_max_line_bytes(file.take(end - start), max_line_bytes),
        line_offset,
    };
    Ok(parse_rollout_reader(path, reader))
}

fn complete_end(path: &Path, start: u64, size: u64, max_poll_bytes: usize) -> io::Result<u64> {
    let poll_end = size.min(start.saturating_add(max_poll_bytes as u64));
    let mut cursor = poll_end;
    let mut file = File::open(path)?;
    while cursor > start {
        let chunk_start = start.max(cursor.saturating_sub(READ_CHUNK_BYTES as u64));
        let length = usize::try_from(cursor - chunk_start).unwrap_or(READ_CHUNK_BYTES);
        file.seek(SeekFrom::Start(chunk_start))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        if let Some(index) = bytes.iter().rposition(|byte| *byte == b'\n') {
            return Ok(chunk_start + index as u64 + 1);
        }
        cursor = chunk_start;
    }
    cursor = poll_end;
    while cursor < size {
        let length = usize::try_from((size - cursor).min(READ_CHUNK_BYTES as u64))
            .unwrap_or(READ_CHUNK_BYTES);
        file.seek(SeekFrom::Start(cursor))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
            return Ok(cursor + index as u64 + 1);
        }
        cursor += length as u64;
    }
    Ok(start)
}

fn count_newlines(path: &Path, start: u64, end: u64) -> io::Result<usize> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = file.take(end - start);
    let mut buffer = [0; READ_CHUNK_BYTES];
    let mut count = 0usize;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(count);
        }
        count = count.saturating_add(buffer[..read].iter().filter(|byte| **byte == b'\n').count());
    }
}

fn source_fingerprint(path: &Path) -> io::Result<(u64, u64)> {
    let size = fs::metadata(path)?.len();
    Ok((size, prefix_digest(path, size)?))
}

fn prefix_digest(path: &Path, length: u64) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut remaining = length;
    let mut digest = FNV_OFFSET_BASIS;
    let mut buffer = [0; READ_CHUNK_BYTES];
    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..requested])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "monitor source ended before the recorded offset",
            ));
        }
        digest = update_digest(digest, &buffer[..read]);
        remaining -= read as u64;
    }
    Ok(digest)
}

fn update_digest(mut digest: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(FNV_PRIME);
    }
    digest
}

fn record_identity(record: &RolloutRecord) -> Option<String> {
    let (category, value) = match &record.kind {
        RolloutRecordKind::Known {
            record_type,
            nested_type,
            payload,
        } => (
            format!(
                "{record_type:?}:{}",
                nested_type.as_deref().unwrap_or_default()
            ),
            payload.as_ref(),
        ),
        RolloutRecordKind::Unknown(unknown) => ("unknown".to_owned(), Some(&unknown.raw)),
    };
    let object = value?.as_object()?;
    ["event_id", "record_id", "id"]
        .iter()
        .find_map(|key| object.get(*key).and_then(|value| value.as_str()))
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(|value| format!("{category}:{value}"))
}

fn stored_context(
    data: &crate::model::CanonicalData,
    path: &Path,
) -> Option<RolloutNormalizationContext> {
    let last = data
        .records
        .iter()
        .filter(|record| record.provenance.path == path)
        .max_by_key(|record| record.sequence)?;
    let session = last.session_id.as_deref().and_then(|session_id| {
        data.sessions
            .iter()
            .find(|session| session.id == session_id && session.provenance.path == path)
            .cloned()
    });
    let turn = last.turn_id.as_deref().and_then(|turn_id| {
        data.turns
            .iter()
            .find(|turn| {
                turn.id == turn_id
                    && turn.session_id == last.session_id
                    && turn.provenance.path == path
            })
            .cloned()
    });
    Some(RolloutNormalizationContext {
        session,
        turn,
        pending_tool_calls: pending_tool_calls_for_source(data, path),
        recent_tool_results: recent_tool_results_for_source(data, path),
    })
}
