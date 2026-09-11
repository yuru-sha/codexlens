//! Explicit, read-only reporting-period selection.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{
    CanonicalData, FileOperation, Message, MessageRole, SourceRef, TokenUsage, ToolCall,
    ToolResult, Turn,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Timestamp {
    seconds: i64,
    nanos: u32,
}

impl Timestamp {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.len() < 20
            || value.as_bytes().get(4) != Some(&b'-')
            || value.as_bytes().get(7) != Some(&b'-')
            || !matches!(value.as_bytes().get(10), Some(b'T' | b't'))
            || value.as_bytes().get(13) != Some(&b':')
            || value.as_bytes().get(16) != Some(&b':')
        {
            return None;
        }
        let year = digits(value, 0, 4)?;
        let month = digits(value, 5, 2)?;
        let day = digits(value, 8, 2)?;
        let hour = digits(value, 11, 2)?;
        let minute = digits(value, 14, 2)?;
        let second = digits(value, 17, 2)?;
        if month == 0
            || month > 12
            || day == 0
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }

        let suffix = value.get(19..)?;
        let (nanos, suffix) = if let Some(fraction) = suffix.strip_prefix('.') {
            let length = fraction.bytes().take_while(u8::is_ascii_digit).count();
            if length == 0 || length > 9 {
                return None;
            }
            let mut digits = fraction[..length].to_owned();
            digits.extend(std::iter::repeat_n('0', 9 - length));
            (digits.parse().ok()?, &fraction[length..])
        } else {
            (0, suffix)
        };
        let offset = if suffix.eq_ignore_ascii_case("z") {
            0
        } else if suffix.len() == 6
            && matches!(suffix.as_bytes().first(), Some(b'+' | b'-'))
            && suffix.as_bytes().get(3) == Some(&b':')
        {
            let sign = if suffix.starts_with('+') { 1 } else { -1 };
            let hours = digits(suffix, 1, 2)?;
            let minutes = digits(suffix, 4, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3_600 + minutes * 60)
        } else {
            return None;
        };
        Some(Self {
            seconds: days_from_civil(year, month, day) * 86_400
                + hour * 3_600
                + minute * 60
                + second
                - offset,
            nanos,
        })
    }

    pub(crate) fn format(self) -> String {
        let days = self.seconds.div_euclid(86_400);
        let remainder = self.seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = remainder / 3_600;
        let minute = remainder / 60 % 60;
        let second = remainder % 60;
        let fraction = if self.nanos == 0 {
            ".000".to_owned()
        } else {
            let value = format!("{:09}", self.nanos);
            format!(".{}", value.trim_end_matches('0'))
        };
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}Z")
    }

    pub(crate) fn within_seconds(self, start: Self, max_seconds: i64) -> bool {
        if max_seconds < 0 || self < start {
            return false;
        }
        let seconds = self.seconds - start.seconds;
        seconds < max_seconds || seconds == max_seconds && self.nanos <= start.nanos
    }
}

fn digits(value: &str, start: usize, length: usize) -> Option<i64> {
    let value = value.get(start..start + length)?;
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PeriodError {
    #[error("invalid reporting period: --{bound} must be an RFC3339 timestamp with Z or ±HH:MM")]
    InvalidTimestamp { bound: &'static str },
    #[error("invalid reporting period: --since must not be later than --until")]
    Reversed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportingPeriod {
    start: Option<Timestamp>,
    end: Option<Timestamp>,
}

impl ReportingPeriod {
    pub fn from_bounds(
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Option<Self>, PeriodError> {
        if since.is_none() && until.is_none() {
            return Ok(None);
        }
        let start = since
            .map(|value| {
                Timestamp::parse(value).ok_or(PeriodError::InvalidTimestamp { bound: "since" })
            })
            .transpose()?;
        let end = until
            .map(|value| {
                Timestamp::parse(value).ok_or(PeriodError::InvalidTimestamp { bound: "until" })
            })
            .transpose()?;
        if start.zip(end).is_some_and(|(start, end)| start > end) {
            return Err(PeriodError::Reversed);
        }
        Ok(Some(Self { start, end }))
    }

    pub(crate) fn contains(self, timestamp: Timestamp) -> bool {
        self.start.is_none_or(|start| timestamp >= start)
            && self.end.is_none_or(|end| timestamp < end)
    }

    fn overlaps(self, start: Timestamp, end: Timestamp) -> bool {
        self.start
            .zip(self.end)
            .is_none_or(|(lower, upper)| lower < upper)
            && start <= end
            && self.end.is_none_or(|bound| start < bound)
            && self.start.is_none_or(|bound| end > bound)
    }

    fn is_empty(self) -> bool {
        self.start
            .zip(self.end)
            .is_some_and(|(start, end)| start == end)
    }

    pub fn start_text(self) -> Option<String> {
        self.start.map(Timestamp::format)
    }

    pub fn end_text(self) -> Option<String> {
        self.end.map(Timestamp::format)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeriodCoverageState {
    Empty,
    Complete,
    Partial,
}

impl PeriodCoverageState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodCoverage {
    pub requested_start: Option<String>,
    pub requested_end: Option<String>,
    pub observed_start: Option<String>,
    pub observed_end: Option<String>,
    pub included_sessions: usize,
    pub included_records: usize,
    pub excluded_records: usize,
    pub unknown_timestamp_records: usize,
    pub unknown_timestamp_events: usize,
    pub state: PeriodCoverageState,
}

#[derive(Debug, Clone)]
pub struct SelectedData {
    pub data: CanonicalData,
    pub coverage: PeriodCoverage,
}

pub(crate) type SourceKey = (PathBuf, Option<usize>);
type TurnKey = (String, String);
type ToolContextKey<'a> = (Option<&'a str>, Option<&'a str>);

pub(crate) fn record_timestamps(data: &CanonicalData) -> HashMap<SourceKey, Option<Timestamp>> {
    data.records
        .iter()
        .map(|record| {
            (
                source_key(&record.provenance),
                record.timestamp.as_deref().and_then(Timestamp::parse),
            )
        })
        .collect()
}

pub fn select_report_data(data: &CanonicalData, period: Option<&ReportingPeriod>) -> SelectedData {
    let record_times = record_timestamps(data);
    let mut observed_times = Vec::new();
    let mut selected_session_ids = HashSet::new();
    let mut selected_turns = HashSet::new();
    let mut included_records = 0;
    let mut excluded_records = 0;
    let mut unknown_timestamp_records = 0;
    let mut unknown_timestamp_events = 0;
    let mut records = Vec::new();

    for record in &data.records {
        let timestamp = record.timestamp.as_deref().and_then(Timestamp::parse);
        match (period, timestamp) {
            (None, None) => unknown_timestamp_records += 1,
            (Some(_), None) => unknown_timestamp_records += 1,
            (None, Some(timestamp)) => observed_times.push(timestamp),
            (Some(period), Some(timestamp)) if period.contains(timestamp) => {
                observed_times.push(timestamp)
            }
            (Some(_), Some(_)) => excluded_records += 1,
        }
        if period.is_none_or(|period| timestamp.is_some_and(|value| period.contains(value))) {
            included_records += 1;
            if let Some(session_id) = &record.session_id {
                selected_session_ids.insert(session_id.clone());
            }
            add_turn(
                &mut selected_turns,
                record.session_id.as_ref(),
                record.turn_id.as_ref(),
            );
            records.push(record.clone());
        }
    }

    for session in &data.sessions {
        let created =
            timestamp_with_unknown(session.created_at.as_deref(), &mut unknown_timestamp_events);
        let updated =
            timestamp_with_unknown(session.updated_at.as_deref(), &mut unknown_timestamp_events);
        let selected = period.is_none_or(|period| session_intersects(period, created, updated));
        if selected {
            selected_session_ids.insert(session.id.clone());
            add_observed(&mut observed_times, created, period);
            add_observed(&mut observed_times, updated, period);
        }
    }

    let mut selected_message_indices = HashSet::new();
    let mut selected_user_indices = Vec::new();
    for (index, message) in data.messages.iter().enumerate() {
        let timestamp = event_timestamp_with_unknown(
            message.timestamp.as_deref(),
            &message.provenance,
            &record_times,
            &mut unknown_timestamp_events,
        );
        if period.is_none_or(|period| timestamp.is_some_and(|value| period.contains(value))) {
            selected_message_indices.insert(index);
            add_observed(&mut observed_times, timestamp, period);
            add_session_and_turn(
                &mut selected_session_ids,
                &mut selected_turns,
                message.session_id.as_ref(),
                message.turn_id.as_ref(),
            );
            if message.role == Some(MessageRole::User) {
                selected_user_indices.push(index);
            }
        }
    }
    for user_index in selected_user_indices {
        let Some(user) = data.messages.get(user_index) else {
            continue;
        };
        let preceding = data
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.role == Some(MessageRole::Assistant)
                    && message.session_id == user.session_id
                    && before_message(message, user, &record_times)
            })
            .max_by(|(_, left), (_, right)| message_order(left, right, &record_times));
        if let Some((index, _)) = preceding {
            selected_message_indices.insert(index);
        }
    }
    let messages = data
        .messages
        .iter()
        .enumerate()
        .filter(|(index, _)| selected_message_indices.contains(index))
        .map(|(_, message)| message.clone())
        .collect::<Vec<_>>();

    let mut selected_result_indices = HashSet::new();
    for (index, result) in data.tool_results.iter().enumerate() {
        let timestamp = source_timestamp_with_unknown(
            &result.provenance,
            &record_times,
            &mut unknown_timestamp_events,
        );
        if period.is_none_or(|period| timestamp.is_some_and(|value| period.contains(value))) {
            selected_result_indices.insert(index);
            add_observed(&mut observed_times, timestamp, period);
            add_session_and_turn(
                &mut selected_session_ids,
                &mut selected_turns,
                result.session_id.as_ref(),
                result.turn_id.as_ref(),
            );
        }
    }
    let directly_selected_result_indices = selected_result_indices.clone();
    let mut selected_call_indices = HashSet::new();
    for (index, call) in data.tool_calls.iter().enumerate() {
        let timestamp = source_timestamp_with_unknown(
            &call.provenance,
            &record_times,
            &mut unknown_timestamp_events,
        );
        let own_event_selected =
            period.is_none_or(|period| timestamp.is_some_and(|value| period.contains(value)));
        if own_event_selected {
            selected_call_indices.insert(index);
            add_observed(&mut observed_times, timestamp, period);
            add_session_and_turn(
                &mut selected_session_ids,
                &mut selected_turns,
                call.session_id.as_ref(),
                call.turn_id.as_ref(),
            );
        }
    }
    let directly_selected_call_indices = selected_call_indices.clone();
    let mut call_indices_by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut call_indices_by_context: HashMap<ToolContextKey<'_>, Vec<usize>> = HashMap::new();
    for (index, call) in data.tool_calls.iter().enumerate() {
        if let Some(call_id) = call.call_id.as_deref() {
            call_indices_by_id.entry(call_id).or_default().push(index);
        } else {
            call_indices_by_context
                .entry((call.session_id.as_deref(), call.turn_id.as_deref()))
                .or_default()
                .push(index);
        }
    }
    let mut result_indices_by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut result_indices_by_context: HashMap<ToolContextKey<'_>, Vec<usize>> = HashMap::new();
    let mut non_duplicate_result_counts: HashMap<ToolContextKey<'_>, usize> = HashMap::new();
    for (index, result) in data.tool_results.iter().enumerate() {
        if let Some(call_id) = result.call_id.as_deref() {
            result_indices_by_id.entry(call_id).or_default().push(index);
        } else {
            let context = (result.session_id.as_deref(), result.turn_id.as_deref());
            result_indices_by_context
                .entry(context)
                .or_default()
                .push(index);
            if !result.is_duplicate {
                *non_duplicate_result_counts.entry(context).or_default() += 1;
            }
        }
    }
    for result_index in directly_selected_result_indices {
        let result = &data.tool_results[result_index];
        if let Some(call_id) = result.call_id.as_deref() {
            for call_index in call_indices_by_id
                .get(call_id)
                .into_iter()
                .flatten()
                .filter(|call_index| {
                    compatible_call_result_context(&data.tool_calls[**call_index], result)
                })
            {
                selected_call_indices.insert(*call_index);
            }
        } else {
            let context = (result.session_id.as_deref(), result.turn_id.as_deref());
            if call_indices_by_context
                .get(&context)
                .is_some_and(|indices| indices.len() == 1)
                && non_duplicate_result_counts.get(&context).copied() == Some(1)
            {
                selected_call_indices.insert(call_indices_by_context[&context][0]);
            }
        }
    }
    for call_index in directly_selected_call_indices {
        let call = &data.tool_calls[call_index];
        if let Some(call_id) = call.call_id.as_deref() {
            for result_index in result_indices_by_id
                .get(call_id)
                .into_iter()
                .flatten()
                .filter(|result_index| {
                    compatible_call_result_context(call, &data.tool_results[**result_index])
                })
            {
                selected_result_indices.insert(*result_index);
            }
        } else {
            let context = (call.session_id.as_deref(), call.turn_id.as_deref());
            if call_indices_by_context
                .get(&context)
                .is_some_and(|indices| indices.len() == 1)
                && non_duplicate_result_counts.get(&context).copied() == Some(1)
            {
                if let Some(result_indices) = result_indices_by_context.get(&context) {
                    selected_result_indices.extend(result_indices.iter().copied());
                }
            }
        }
    }
    let tool_calls = data
        .tool_calls
        .iter()
        .enumerate()
        .filter(|(index, _)| selected_call_indices.contains(index))
        .map(|(_, call)| call.clone())
        .collect::<Vec<_>>();
    let tool_results = data
        .tool_results
        .iter()
        .enumerate()
        .filter(|(index, _)| selected_result_indices.contains(index))
        .map(|(_, result)| result.clone())
        .collect::<Vec<_>>();

    let file_operations = select_file_operations(
        &data.file_operations,
        period,
        &record_times,
        &mut selected_session_ids,
        &mut selected_turns,
        &mut observed_times,
        &mut unknown_timestamp_events,
    );
    let token_usage = select_token_usage(
        &data.token_usage,
        period,
        &record_times,
        &mut selected_session_ids,
        &mut selected_turns,
        &mut observed_times,
        &mut unknown_timestamp_events,
    );

    let instruction_snapshots = data
        .instruction_snapshots
        .iter()
        .filter(|snapshot| {
            let timestamp = source_timestamp_with_unknown(
                &snapshot.provenance,
                &record_times,
                &mut unknown_timestamp_events,
            );
            period.is_none_or(|period| timestamp.is_some_and(|value| period.contains(value)))
        })
        .cloned()
        .inspect(|snapshot| {
            let timestamp = source_timestamp(&snapshot.provenance, &record_times);
            add_observed(&mut observed_times, timestamp, period);
            add_session_and_turn(
                &mut selected_session_ids,
                &mut selected_turns,
                snapshot.session_id.as_ref(),
                snapshot.turn_id.as_ref(),
            );
        })
        .collect::<Vec<_>>();

    let mut turns = Vec::new();
    for turn in &data.turns {
        let started =
            timestamp_with_unknown(turn.started_at.as_deref(), &mut unknown_timestamp_events);
        let completed =
            timestamp_with_unknown(turn.completed_at.as_deref(), &mut unknown_timestamp_events);
        for event in &turn.lifecycle {
            let _ = event_timestamp_with_unknown(
                event.timestamp.as_deref(),
                &event.provenance,
                &record_times,
                &mut unknown_timestamp_events,
            );
        }
        let selected = period.is_none_or(|period| {
            selected_turn(
                turn_key(turn).is_some_and(|key| selected_turns.contains(&key)),
                period,
                started,
                completed,
            )
        });
        if !selected {
            continue;
        }
        if let Some(session_id) = &turn.session_id {
            selected_session_ids.insert(session_id.clone());
        }
        add_observed(&mut observed_times, started, period);
        add_observed(&mut observed_times, completed, period);
        let mut selected_turn = turn.clone();
        if let Some(period) = period {
            selected_turn.started_at = in_period_text(
                turn.started_at.as_deref(),
                &turn.provenance,
                &record_times,
                period,
            );
            selected_turn.completed_at = in_period_text(
                turn.completed_at.as_deref(),
                &turn.provenance,
                &record_times,
                period,
            );
            selected_turn.lifecycle = turn
                .lifecycle
                .iter()
                .filter(|event| {
                    event_timestamp(event.timestamp.as_deref(), &event.provenance, &record_times)
                        .is_some_and(|timestamp| period.contains(timestamp))
                })
                .cloned()
                .collect();
        }
        turns.push(selected_turn);
    }

    let sessions = data
        .sessions
        .iter()
        .filter(|session| selected_session_ids.contains(&session.id))
        .cloned()
        .collect::<Vec<_>>();
    let instruction_joins = data
        .instruction_joins
        .iter()
        .filter(|join| selected_session_ids.contains(&join.session_id))
        .cloned()
        .collect::<Vec<_>>();
    let included_sessions = selected_session_ids.len();
    observed_times.sort_unstable();
    let coverage = PeriodCoverage {
        requested_start: period.and_then(|period| period.start_text()),
        requested_end: period.and_then(|period| period.end_text()),
        observed_start: observed_times.first().copied().map(Timestamp::format),
        observed_end: observed_times.last().copied().map(Timestamp::format),
        included_sessions,
        included_records,
        excluded_records,
        unknown_timestamp_records,
        unknown_timestamp_events,
        state: if period.is_some_and(|period| period.is_empty())
            || (observed_times.is_empty()
                && unknown_timestamp_records == 0
                && unknown_timestamp_events == 0)
        {
            PeriodCoverageState::Empty
        } else if unknown_timestamp_records > 0 || unknown_timestamp_events > 0 {
            PeriodCoverageState::Partial
        } else {
            PeriodCoverageState::Complete
        },
    };
    SelectedData {
        data: CanonicalData {
            sessions,
            turns,
            records,
            messages,
            tool_calls,
            tool_results,
            file_operations,
            token_usage,
            diagnostics: data.diagnostics.clone(),
            instruction_snapshots,
            instruction_joins,
        },
        coverage,
    }
}

fn source_key(source: &SourceRef) -> SourceKey {
    (source.path.clone(), source.line)
}

fn source_timestamp(
    source: &SourceRef,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
) -> Option<Timestamp> {
    record_times.get(&source_key(source)).copied().flatten()
}

fn source_timestamp_with_unknown(
    source: &SourceRef,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
    unknown_timestamps: &mut usize,
) -> Option<Timestamp> {
    let timestamp = source_timestamp(source, record_times);
    if timestamp.is_none() {
        *unknown_timestamps += 1;
    }
    timestamp
}

pub(crate) fn event_timestamp(
    value: Option<&str>,
    source: &SourceRef,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
) -> Option<Timestamp> {
    match value {
        Some(value) => Timestamp::parse(value),
        None => source_timestamp(source, record_times),
    }
}

fn event_timestamp_with_unknown(
    value: Option<&str>,
    source: &SourceRef,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
    unknown_timestamps: &mut usize,
) -> Option<Timestamp> {
    let timestamp = event_timestamp(value, source, record_times);
    if timestamp.is_none() {
        *unknown_timestamps += 1;
    }
    timestamp
}

fn timestamp_with_unknown(
    value: Option<&str>,
    unknown_timestamps: &mut usize,
) -> Option<Timestamp> {
    let timestamp = value.and_then(Timestamp::parse);
    if timestamp.is_none() {
        *unknown_timestamps += 1;
    }
    timestamp
}

fn add_observed(
    observed_times: &mut Vec<Timestamp>,
    timestamp: Option<Timestamp>,
    period: Option<&ReportingPeriod>,
) {
    if period.is_none_or(|period| timestamp.is_some_and(|timestamp| period.contains(timestamp))) {
        if let Some(timestamp) = timestamp {
            observed_times.push(timestamp);
        }
    }
}

fn add_turn(turns: &mut HashSet<TurnKey>, session_id: Option<&String>, turn_id: Option<&String>) {
    if let (Some(session_id), Some(turn_id)) = (session_id, turn_id) {
        turns.insert((session_id.clone(), turn_id.clone()));
    }
}

fn add_session_and_turn(
    sessions: &mut HashSet<String>,
    turns: &mut HashSet<TurnKey>,
    session_id: Option<&String>,
    turn_id: Option<&String>,
) {
    if let Some(session_id) = session_id {
        sessions.insert(session_id.clone());
    }
    add_turn(turns, session_id, turn_id);
}

fn session_intersects(
    period: &ReportingPeriod,
    created: Option<Timestamp>,
    updated: Option<Timestamp>,
) -> bool {
    match (created, updated) {
        (Some(created), Some(updated)) => {
            period.overlaps(created.min(updated), created.max(updated))
        }
        (Some(timestamp), None) | (None, Some(timestamp)) => period.contains(timestamp),
        (None, None) => false,
    }
}

fn selected_turn(
    has_selected_child: bool,
    period: &ReportingPeriod,
    started: Option<Timestamp>,
    completed: Option<Timestamp>,
) -> bool {
    has_selected_child
        || match (started, completed) {
            (Some(started), Some(completed)) => {
                period.overlaps(started.min(completed), started.max(completed))
            }
            (Some(timestamp), None) | (None, Some(timestamp)) => period.contains(timestamp),
            (None, None) => false,
        }
}

fn in_period_text(
    value: Option<&str>,
    source: &SourceRef,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
    period: &ReportingPeriod,
) -> Option<String> {
    value
        .filter(|value| {
            event_timestamp(Some(value), source, record_times)
                .is_some_and(|timestamp| period.contains(timestamp))
        })
        .map(str::to_owned)
}

fn select_file_operations(
    operations: &[FileOperation],
    period: Option<&ReportingPeriod>,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
    sessions: &mut HashSet<String>,
    turns: &mut HashSet<TurnKey>,
    observed_times: &mut Vec<Timestamp>,
    unknown_timestamps: &mut usize,
) -> Vec<FileOperation> {
    operations
        .iter()
        .filter(|operation| {
            let timestamp = event_timestamp_with_unknown(
                operation.timestamp.as_deref(),
                &operation.provenance,
                record_times,
                unknown_timestamps,
            );
            let selected = period
                .is_none_or(|period| timestamp.is_some_and(|timestamp| period.contains(timestamp)));
            if selected {
                add_observed(observed_times, timestamp, period);
                add_session_and_turn(
                    sessions,
                    turns,
                    operation.session_id.as_ref(),
                    operation.turn_id.as_ref(),
                );
            }
            selected
        })
        .cloned()
        .collect()
}

fn select_token_usage(
    usage: &[TokenUsage],
    period: Option<&ReportingPeriod>,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
    sessions: &mut HashSet<String>,
    turns: &mut HashSet<TurnKey>,
    observed_times: &mut Vec<Timestamp>,
    unknown_timestamps: &mut usize,
) -> Vec<TokenUsage> {
    usage
        .iter()
        .filter(|usage| {
            let timestamp = event_timestamp_with_unknown(
                usage.timestamp.as_deref(),
                &usage.provenance,
                record_times,
                unknown_timestamps,
            );
            let selected = period
                .is_none_or(|period| timestamp.is_some_and(|timestamp| period.contains(timestamp)));
            if selected {
                add_observed(observed_times, timestamp, period);
                add_session_and_turn(
                    sessions,
                    turns,
                    usage.session_id.as_ref(),
                    usage.turn_id.as_ref(),
                );
            }
            selected
        })
        .cloned()
        .collect()
}

fn turn_key(turn: &Turn) -> Option<TurnKey> {
    Some((turn.session_id.clone()?, turn.id.clone()))
}

fn compatible_call_result_context(call: &ToolCall, result: &ToolResult) -> bool {
    compatible_context(call.session_id.as_ref(), result.session_id.as_ref())
        && compatible_context(call.turn_id.as_ref(), result.turn_id.as_ref())
}

fn compatible_context(left: Option<&String>, right: Option<&String>) -> bool {
    left.is_none() || right.is_none() || left == right
}

fn before_message(
    left: &Message,
    right: &Message,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
) -> bool {
    message_order(left, right, record_times) == Ordering::Less
}

fn message_order(
    left: &Message,
    right: &Message,
    record_times: &HashMap<SourceKey, Option<Timestamp>>,
) -> Ordering {
    event_timestamp(left.timestamp.as_deref(), &left.provenance, record_times)
        .cmp(&event_timestamp(
            right.timestamp.as_deref(),
            &right.provenance,
            record_times,
        ))
        .then_with(|| left.provenance.path.cmp(&right.provenance.path))
        .then_with(|| left.provenance.line.cmp(&right.provenance.line))
}

impl fmt::Display for ReportingPeriod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.start_text(), self.end_text()) {
            (Some(start), Some(end)) => write!(formatter, "[{start}, {end})"),
            (Some(start), None) => write!(formatter, "[{start}, ∞)"),
            (None, Some(end)) => write!(formatter, "(-∞, {end})"),
            (None, None) => formatter.write_str("all"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        FileOperation, Message, MessageRole, Record, RecordKind, SourceKind, TokenUsage,
        TurnLifecycleEvent,
    };

    fn source(line: usize) -> SourceRef {
        SourceRef {
            kind: SourceKind::Rollout,
            path: PathBuf::from("period.jsonl"),
            line: Some(line),
            ingested_at: None,
            parser_schema_version: 1,
        }
    }

    fn record(line: usize, timestamp: Option<&str>) -> Record {
        Record {
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            timestamp: timestamp.map(str::to_owned),
            sequence: line,
            original_record_type: None,
            original_nested_type: None,
            error_category: None,
            is_error: false,
            is_terminal: false,
            kind: RecordKind::ResponseItem,
            provenance: source(line),
        }
    }

    #[test]
    fn parses_offsets_and_renders_utc_without_losing_fraction() {
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T09:00:00.125+09:00"),
            Some("2026-01-03T10:00:00.875+09:00"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            period.start_text().as_deref(),
            Some("2026-01-03T00:00:00.125Z")
        );
        assert_eq!(
            period.end_text().as_deref(),
            Some("2026-01-03T01:00:00.875Z")
        );
    }

    #[test]
    fn accepts_empty_interval_and_rejects_reversed_or_naive_bounds() {
        let empty = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-03T00:00:00Z"),
        )
        .unwrap()
        .unwrap();
        assert!(!empty.contains(Timestamp::parse("2026-01-03T00:00:00Z").unwrap()));
        assert_eq!(
            ReportingPeriod::from_bounds(
                Some("2026-01-04T00:00:00Z"),
                Some("2026-01-03T00:00:00Z"),
            ),
            Err(PeriodError::Reversed)
        );
        assert!(matches!(
            ReportingPeriod::from_bounds(Some("2026-01-03T00:00:00"), None),
            Err(PeriodError::InvalidTimestamp { bound: "since" })
        ));
        assert!(ReportingPeriod::from_bounds(Some("2026-01-03T00:00:00.1Z"), None,).is_ok());
        assert!(
            ReportingPeriod::from_bounds(Some("2026-01-03T00:00:00.123456789Z"), None,).is_ok()
        );
        assert!(
            ReportingPeriod::from_bounds(Some("2026-01-03T00:00:00.1234567890Z"), None,).is_err()
        );
        assert!(ReportingPeriod::from_bounds(Some("2026-01-03T00:00:60Z"), None).is_err());
        assert!(ReportingPeriod::from_bounds(Some("+026-01-03T00:00:00Z"), None,).is_err());
        assert!(ReportingPeriod::from_bounds(Some(" 2026-01-03T00:00:00Z"), None,).is_err());
    }

    #[test]
    fn selects_the_start_boundary_but_excludes_the_end_boundary() {
        let data = CanonicalData {
            records: vec![
                record(1, Some("2026-01-03T00:00:00Z")),
                record(2, Some("2026-01-04T00:00:00Z")),
            ],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap();

        let selected = select_report_data(&data, period.as_ref());

        assert_eq!(selected.data.records.len(), 1);
        assert_eq!(selected.data.records[0].sequence, 1);
    }

    #[test]
    fn excludes_a_span_ending_at_the_since_boundary() {
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap()
        .unwrap();

        assert!(!period.overlaps(
            Timestamp::parse("2026-01-02T00:00:00Z").unwrap(),
            Timestamp::parse("2026-01-03T00:00:00Z").unwrap(),
        ));
    }

    #[test]
    fn keeps_in_range_result_with_its_out_of_range_call_for_correlation() {
        let call = ToolCall {
            id: None,
            call_id: Some("call".to_owned()),
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: None,
            status: None,
            provenance: source(1),
        };
        let result = ToolResult {
            id: None,
            call_id: Some("call".to_owned()),
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            command: Some("cargo test".to_owned()),
            cwd: None,
            stdout: None,
            stderr: Some("synthetic failure".to_owned()),
            duration_ms: None,
            exit_code: Some(1),
            status: Some("failed".to_owned()),
            outcome: crate::model::ToolOutcome::Failed,
            outcome_source: crate::model::OutcomeSource::ExitCode,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance: source(2),
        };
        let data = CanonicalData {
            records: vec![
                record(1, Some("2026-01-02T23:59:59Z")),
                record(2, Some("2026-01-03T00:00:00Z")),
            ],
            tool_calls: vec![call],
            tool_results: vec![result],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap();
        let selected = select_report_data(&data, period.as_ref());
        assert_eq!(selected.data.tool_calls.len(), 1);
        assert_eq!(selected.data.tool_results.len(), 1);
        assert_eq!(selected.data.records.len(), 1);
        assert_eq!(selected.coverage.excluded_records, 1);

        let mut no_id_data = data.clone();
        no_id_data.tool_calls[0].call_id = None;
        no_id_data.tool_results[0].call_id = None;
        let no_id_selected = select_report_data(&no_id_data, period.as_ref());
        assert_eq!(no_id_selected.data.tool_calls.len(), 1);
    }

    #[test]
    fn keeps_in_range_call_with_boundary_result_for_correlation() {
        let call = ToolCall {
            id: None,
            call_id: Some("call".to_owned()),
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            tool_name: Some("exec_command".to_owned()),
            input_summary: None,
            command: Some("cargo test".to_owned()),
            cwd: None,
            status: None,
            provenance: source(1),
        };
        let result = ToolResult {
            id: None,
            call_id: Some("call".to_owned()),
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            command: Some("cargo test".to_owned()),
            cwd: None,
            stdout: Some("synthetic success".to_owned()),
            stderr: None,
            duration_ms: None,
            exit_code: Some(0),
            status: Some("completed".to_owned()),
            outcome: crate::model::ToolOutcome::Succeeded,
            outcome_source: crate::model::OutcomeSource::ExitCode,
            matched_call: true,
            deduplication_key: None,
            equivalent_to: None,
            is_duplicate: false,
            provenance: source(2),
        };
        let data = CanonicalData {
            records: vec![
                record(1, Some("2026-01-03T00:00:00Z")),
                record(2, Some("2026-01-04T00:00:00Z")),
            ],
            tool_calls: vec![call],
            tool_results: vec![result],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap();

        let selected = select_report_data(&data, period.as_ref());

        assert_eq!(selected.data.records.len(), 1);
        assert_eq!(selected.data.tool_calls.len(), 1);
        assert_eq!(selected.data.tool_results.len(), 1);
        assert_eq!(selected.coverage.excluded_records, 1);

        let unfiltered = select_report_data(&data, None);
        assert_eq!(unfiltered.data.records.len(), 2);
        assert_eq!(unfiltered.data.tool_calls.len(), 1);
        assert_eq!(unfiltered.data.tool_results.len(), 1);

        let mut no_id_data = data;
        no_id_data.tool_calls[0].call_id = None;
        no_id_data.tool_results[0].call_id = None;
        let no_id_selected = select_report_data(&no_id_data, period.as_ref());
        assert_eq!(no_id_selected.data.tool_calls.len(), 1);
        assert_eq!(no_id_selected.data.tool_results.len(), 1);
    }

    #[test]
    fn keeps_the_turn_for_a_selected_instruction_snapshot() {
        let data = CanonicalData {
            records: vec![Record {
                session_id: None,
                turn_id: None,
                ..record(2, Some("2026-01-03T00:00:00Z"))
            }],
            turns: vec![Turn {
                id: "turn".to_owned(),
                session_id: Some("session".to_owned()),
                started_at: None,
                completed_at: None,
                cwd: None,
                model: None,
                reasoning_effort: None,
                sequence: 1,
                lifecycle: Vec::new(),
                provenance: source(2),
            }],
            instruction_snapshots: vec![crate::instructions::snapshot_from_rollout(
                Some("session".to_owned()),
                Some("turn".to_owned()),
                Some("synthetic rules"),
                source(2),
            )],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap();

        let selected = select_report_data(&data, period.as_ref());

        assert_eq!(selected.data.instruction_snapshots.len(), 1);
        assert_eq!(selected.data.turns.len(), 1);
    }

    #[test]
    fn keeps_boundary_context_without_claiming_out_of_range_completion() {
        let data = CanonicalData {
            records: vec![
                record(2, Some("2026-01-03T00:00:00Z")),
                Record {
                    is_terminal: true,
                    provenance: source(4),
                    ..record(4, Some("2026-01-03T00:10:00Z"))
                },
            ],
            turns: vec![Turn {
                id: "turn".to_owned(),
                session_id: Some("session".to_owned()),
                started_at: Some("2026-01-02T23:59:59Z".to_owned()),
                completed_at: Some("2026-01-03T00:10:00Z".to_owned()),
                cwd: None,
                model: None,
                reasoning_effort: None,
                sequence: 1,
                lifecycle: vec![TurnLifecycleEvent {
                    kind: "turn_complete".to_owned(),
                    timestamp: Some("2026-01-03T00:10:00Z".to_owned()),
                    sequence: 4,
                    provenance: source(4),
                }],
                provenance: source(2),
            }],
            messages: vec![
                Message {
                    id: None,
                    session_id: Some("session".to_owned()),
                    turn_id: Some("turn".to_owned()),
                    role: Some(MessageRole::Assistant),
                    content: Some("synthetic action".to_owned()),
                    timestamp: Some("2026-01-02T23:59:59Z".to_owned()),
                    provenance: source(1),
                },
                Message {
                    id: None,
                    session_id: Some("session".to_owned()),
                    turn_id: Some("turn".to_owned()),
                    role: Some(MessageRole::User),
                    content: Some("Use the documented command instead.".to_owned()),
                    timestamp: Some("2026-01-03T00:00:01Z".to_owned()),
                    provenance: source(3),
                },
            ],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-03T00:05:00Z"),
        )
        .unwrap();
        let selected = select_report_data(&data, period.as_ref());
        assert_eq!(selected.data.records.len(), 1);
        assert_eq!(selected.data.messages.len(), 2);
        assert_eq!(selected.data.turns[0].completed_at, None);
        assert!(selected.data.turns[0].lifecycle.is_empty());
    }

    #[test]
    fn does_not_replace_invalid_event_timestamps_with_record_timestamps() {
        let provenance = source(1);
        let data = CanonicalData {
            records: vec![record(1, Some("2026-01-03T12:00:00Z"))],
            messages: vec![
                Message {
                    id: None,
                    session_id: Some("session".to_owned()),
                    turn_id: Some("turn".to_owned()),
                    role: Some(MessageRole::Assistant),
                    content: Some("invalid message timestamp".to_owned()),
                    timestamp: Some("not-a-timestamp".to_owned()),
                    provenance: provenance.clone(),
                },
                Message {
                    id: None,
                    session_id: Some("session".to_owned()),
                    turn_id: Some("turn".to_owned()),
                    role: Some(MessageRole::Assistant),
                    content: Some("missing message timestamp".to_owned()),
                    timestamp: None,
                    provenance: provenance.clone(),
                },
            ],
            file_operations: vec![FileOperation {
                session_id: Some("session".to_owned()),
                turn_id: Some("turn".to_owned()),
                path: "src/lib.rs".to_owned(),
                operation: "write".to_owned(),
                timestamp: Some("not-a-timestamp".to_owned()),
                provenance: provenance.clone(),
            }],
            token_usage: vec![TokenUsage {
                session_id: Some("session".to_owned()),
                turn_id: Some("turn".to_owned()),
                timestamp: Some("not-a-timestamp".to_owned()),
                input_tokens: Some(1),
                cached_input_tokens: None,
                output_tokens: Some(1),
                reasoning_output_tokens: None,
                sequence: 1,
                provenance,
            }],
            ..CanonicalData::default()
        };
        let period = ReportingPeriod::from_bounds(
            Some("2026-01-03T00:00:00Z"),
            Some("2026-01-04T00:00:00Z"),
        )
        .unwrap();

        let selected = select_report_data(&data, period.as_ref());

        assert_eq!(selected.data.messages.len(), 1);
        assert_eq!(
            selected.data.messages[0].content.as_deref(),
            Some("missing message timestamp")
        );
        assert!(selected.data.file_operations.is_empty());
        assert!(selected.data.token_usage.is_empty());
        assert_eq!(selected.coverage.unknown_timestamp_events, 3);
    }
}
