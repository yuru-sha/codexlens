use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::types::ValueRef;

use codexlens::advisor::{
    ApplyPlan, ApplyReport, CoverageLimitation, DiffBatch, DoctorOptions, DoctorReport,
    ReportCoverage, coverage_limitations_json, doctor_with_coverage, prepare_apply_proposals,
    proposals_for_findings_and_waste, recommend_scope, render_coverage_limitations, render_diffs,
    render_doctor_with_coverage, render_doctor_with_period, render_json_diff,
    render_json_diff_with_period, render_json_finding_report_with_coverage,
    render_json_finding_report_with_period, render_proposal_summary,
    render_report_metadata_with_period, report_coverage, report_coverage_for_period_selection,
    report_sessions,
};
use codexlens::analysis::views::{
    FailureReport, InventoryReport, OverheadReport, PromptReport, StuckReport, UsageReport,
    ViewOpportunity, WasteReport, doctor_opportunities,
};
use codexlens::analysis::{
    Finding, FindingScope, analyze_default, corrections, instructions, knowledge, rework,
    verification,
};
use codexlens::discovery::{
    DiscoveredInput, DiscoveryOptions, DiscoveryResult, InputKind, discover,
};
use codexlens::instructions::InstructionCaptureOptions;
use codexlens::model::CanonicalData;
use codexlens::normalize::normalize_rollout;
use codexlens::period::{
    DEFAULT_SESSION_DAYS, PeriodCoverage, PeriodCoverageState, ReportingPeriod,
    SessionSelectionOptions, select_eligible_session_data, select_report_data_with_options,
};
use codexlens::rollout::{RolloutParseOptions, parse_rollout};
use codexlens::state::read_state_database;
use codexlens::store::{IngestOptions, IngestReport, SCHEMA_VERSION, Store, StoreFreshness};

#[derive(Debug, Parser)]
#[command(
    name = "codexlens",
    version,
    about = "Turn recurring Codex friction into actionable project guidance"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Build or update the derived SQLite store from Codex inputs")]
    Refresh {
        #[command(flatten)]
        refresh: RefreshOptions,
    },
    #[command(about = "Refresh (unless --frozen) and report all canonical findings")]
    Analyze {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "List bounded session metadata and coverage")]
    Sessions {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Show configured surfaces, ownership, and observed use")]
    Inventory {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Explain always-on context cost and residuals")]
    Overhead {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Show where tool, Skill, model, prompt, and subagent effort goes")]
    Usage {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Rank actionable configuration and workflow opportunities")]
    Waste {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report recurring tool failures with scoped fixes")]
    Failures {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report repeated edit/failure loops and targets")]
    Stuck {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report steering, correction, question, and instruction patterns")]
    Prompts {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report correction-lens findings")]
    Corrections {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report legacy rework findings")]
    Rework {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report verification-lens findings")]
    Verification {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(alias = "rediscovery")]
    #[command(about = "Report knowledge-lens findings")]
    Knowledge {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Report instruction-lens findings")]
    Instructions {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(about = "Show bounded, action-first health fixes by scope")]
    Doctor {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(
            long,
            value_name = "COUNT",
            help = "Limit opportunities per scope to at most five"
        )]
        limit: Option<usize>,
    },
    #[command(about = "Run a bounded read-only SQL query (compatibility alias)")]
    Query {
        #[arg(
            value_name = "SQL",
            help = "One read-only SQL statement; reads stdin when omitted"
        )]
        sql: Option<String>,
        #[command(flatten)]
        options: QueryOptions,
    },
    #[command(about = "Run a bounded read-only SQL query")]
    Sql {
        #[arg(
            value_name = "SQL",
            help = "One read-only SQL statement or stdin; output is limited to 50 columns and 50 rows"
        )]
        sql: Option<String>,
        #[command(flatten)]
        options: QueryOptions,
    },
    #[command(about = "Print/diff a reviewable optimization plan or apply it explicitly")]
    Optimize {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(
            long,
            conflicts_with = "apply",
            help = "Render bounded reviewable diffs without writing"
        )]
        diff: bool,
        #[arg(
            long,
            conflicts_with = "diff",
            help = "Apply the validated plan after explicit confirmation"
        )]
        apply: bool,
        #[arg(
            long,
            requires = "apply",
            help = "Confirm the non-interactive apply operation"
        )]
        yes: bool,
        #[arg(
            long,
            conflicts_with_all = ["diff", "apply"],
            help = "Print the complete bounded advisor briefing without writing"
        )]
        print: bool,
    },
    #[command(about = "Ingest one local source incrementally and optionally write a cursor")]
    Monitor {
        #[command(flatten)]
        store: MonitorStoreOptions,
        #[arg(
            long,
            value_name = "PATH",
            help = "One local rollout JSONL or state SQLite source"
        )]
        source: PathBuf,
        #[arg(
            long,
            value_enum,
            default_value_t = MonitorKind::Rollout,
            help = "Source format to monitor"
        )]
        kind: MonitorKind,
        #[arg(long, value_name = "COUNT", help = "Stop after this many polls")]
        max_polls: Option<usize>,
        #[arg(
            long,
            default_value_t = 500,
            value_name = "MILLISECONDS",
            help = "Delay between polls"
        )]
        interval_ms: u64,
        #[arg(
            long,
            value_name = "PATH",
            help = "Write the bounded cursor at a clean stop"
        )]
        cursor: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum MonitorKind {
    Rollout,
    State,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum ScopeFilter {
    #[default]
    All,
    Global,
    Projects,
    Project(PathBuf),
}

impl FromStr for ScopeFilter {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "all" => Ok(Self::All),
            "global" => Ok(Self::Global),
            "project" => Ok(Self::Projects),
            value if value.starts_with("project:") => {
                let value = value.trim_start_matches("project:");
                if value.is_empty() {
                    return Err("project scope requires a path".to_owned());
                }
                let path = PathBuf::from(value);
                let path = if path.is_absolute() || path.has_root() {
                    path
                } else {
                    std::env::current_dir()
                        .map_err(|error| format!("could not resolve project scope: {error}"))?
                        .join(path)
                };
                Ok(Self::Project(fs::canonicalize(&path).unwrap_or(path)))
            }
            _ => Err("scope must be global, project, project:PATH, or all".to_owned()),
        }
    }
}

#[derive(Debug, Clone, Args)]
struct StoreOptions {
    #[arg(
        long,
        short = 's',
        value_name = "PATH",
        help = "Derived SQLite store (default: per-user state directory)"
    )]
    store: Option<PathBuf>,
    #[arg(
        long,
        alias = "home",
        value_name = "PATH",
        help = "Absolute Codex home to read when refreshing"
    )]
    codex_home: Option<PathBuf>,
    #[arg(long, help = "Include archived sessions in reporting selection")]
    include_archived: bool,
    #[arg(long, help = "Include sub-agent sessions in reporting selection")]
    include_subagents: bool,
    #[arg(
        long,
        default_value = "all",
        value_name = "global|project|project:PATH",
        help = "Render global findings, all projects, or one project path"
    )]
    scope: ScopeFilter,
    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Table,
        help = "Output format; JSON keeps progress on stderr"
    )]
    format: OutputFormat,
    #[arg(
        long,
        value_name = "DAYS",
        help = "Limit selection to this many recent session days"
    )]
    days: Option<u64>,
    #[arg(
        long,
        help = "Read exactly this derived store without refreshing raw inputs; refresh progress goes to stderr and JSON stdout stays one document"
    )]
    frozen: bool,
    #[arg(
        long,
        value_name = "RFC3339",
        help = "Inclusive lower bound for the half-open report period"
    )]
    since: Option<String>,
    #[arg(
        long,
        value_name = "RFC3339",
        help = "Exclusive upper bound for the half-open report period"
    )]
    until: Option<String>,
}

impl StoreOptions {
    fn store_path(&self) -> Result<PathBuf> {
        self.store.clone().map_or_else(default_store_path, Ok)
    }
}

#[derive(Debug, Clone, Args)]
struct QueryOptions {
    #[arg(
        long,
        short = 's',
        value_name = "PATH",
        help = "Existing derived store; never created or refreshed"
    )]
    store: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Table,
        help = "Output format for bounded query rows"
    )]
    format: OutputFormat,
}

impl QueryOptions {
    fn store_path(&self) -> Result<PathBuf> {
        self.store.clone().map_or_else(default_store_path, Ok)
    }
}

fn default_store_path() -> Result<PathBuf> {
    let home = || {
        (if cfg!(windows) {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
        } else {
            std::env::var_os("HOME").map(PathBuf::from)
        })
        .filter(|path| !path.as_os_str().is_empty())
        .context("could not resolve the per-user store home")
    };
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(
            || Ok::<PathBuf, anyhow::Error>(home()?.join(".local/state")),
            |path| {
                if path.is_absolute() {
                    Ok(path)
                } else {
                    Ok(home()?.join(path))
                }
            },
        )?;
    Ok(state_home.join("codexlens/codexlens.db"))
}

#[derive(Debug, Clone, Args)]
struct MonitorStoreOptions {
    #[arg(
        long,
        short = 's',
        default_value = ".codexlens.sqlite",
        value_name = "PATH",
        help = "Writable derived SQLite store for incremental ingestion"
    )]
    store: PathBuf,
    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Table,
        help = "Output format for monitor status"
    )]
    format: OutputFormat,
    #[arg(
        long,
        help = "Read exactly this derived store without refreshing raw inputs"
    )]
    frozen: bool,
    #[arg(long, hide = true, value_name = "RFC3339")]
    since: Option<String>,
    #[arg(long, hide = true, value_name = "RFC3339")]
    until: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct RefreshOptions {
    #[arg(
        long,
        short = 's',
        default_value = ".codexlens.sqlite",
        value_name = "PATH",
        help = "Derived SQLite store to create or update"
    )]
    store: PathBuf,
    #[arg(
        long,
        alias = "home",
        value_name = "PATH",
        help = "Absolute Codex home containing rollout and state inputs"
    )]
    codex_home: Option<PathBuf>,
    #[arg(long, help = "Include archived sessions while ingesting")]
    include_archived: bool,
    #[arg(long, help = "Include sub-agent sessions while ingesting")]
    include_subagents: bool,
    #[arg(
        long,
        value_name = "PATH",
        help = "Optional configuration file to capture"
    )]
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    #[value(name = "table", alias = "human")]
    Table,
    Markdown,
    Json,
}

#[derive(Debug, Clone, Copy)]
enum ViewKind {
    Inventory,
    Overhead,
    Usage,
    Waste,
    Failures,
    Stuck,
    Prompts,
}

impl ViewKind {
    fn command(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Overhead => "overhead",
            Self::Usage => "usage",
            Self::Waste => "waste",
            Self::Failures => "failures",
            Self::Stuck => "stuck",
            Self::Prompts => "prompts",
        }
    }
}

enum ViewReport {
    Inventory(InventoryReport),
    Overhead(OverheadReport),
    Usage(UsageReport),
    Waste(WasteReport),
    Failures(FailureReport),
    Stuck(StuckReport),
    Prompts(PromptReport),
}

impl OutputFormat {
    fn write_report(
        self,
        heading: &str,
        human: impl FnOnce() -> (String, String),
        json: impl FnOnce() -> Result<String, serde_json::Error>,
    ) -> Result<()> {
        match self {
            Self::Table => {
                let (stdout, stderr) = human();
                print!("{stdout}");
                eprint!("{stderr}");
            }
            Self::Markdown => {
                let (stdout, stderr) = human();
                print!("# {heading}\n\n{stdout}");
                eprint!("{stderr}");
            }
            Self::Json => print!("{}", json()?),
        }
        Ok(())
    }
}

struct TemporaryStoreCopy {
    path: PathBuf,
}

impl TemporaryStoreCopy {
    fn create(source: &Path) -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        for attempt in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "codexlens-reporting-{}-{nonce}-{attempt}.sqlite",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    drop(file);
                    if let Err(error) = fs::copy(source, &path) {
                        let _ = fs::remove_file(&path);
                        return Err(error.into());
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not allocate a temporary derived-store copy")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryStoreCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct AtomicRefreshStore {
    target: PathBuf,
    path: PathBuf,
    committed: bool,
}

#[derive(Debug)]
struct RefreshOutcome {
    discovery: DiscoveryResult,
    report: IngestReport,
    freshness: StoreFreshness,
}

impl AtomicRefreshStore {
    fn create(target: &Path) -> Result<Self> {
        let parent = target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        for attempt in 0..100 {
            let path = parent.join(format!(
                ".codexlens-refresh-{}-{nonce}-{attempt}.sqlite",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    drop(file);
                    if target.exists() {
                        if let Err(error) = fs::copy(target, &path) {
                            let _ = fs::remove_file(&path);
                            return Err(error.into());
                        }
                    }
                    return Ok(Self {
                        target: target.to_path_buf(),
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not allocate an atomic refresh store")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(mut self) -> Result<()> {
        fs::rename(&self.path, &self.target).with_context(|| {
            format!(
                "failed to atomically replace derived store {}",
                bounded_display(&self.target)
            )
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for AtomicRefreshStore {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Refresh { refresh } => run_refresh(&refresh),
        Command::Analyze { store } => run_analyze(&store),
        Command::Sessions { store } => run_sessions_report(&store),
        Command::Inventory { store } => run_view_report(&store, ViewKind::Inventory),
        Command::Overhead { store } => run_view_report(&store, ViewKind::Overhead),
        Command::Usage { store } => run_view_report(&store, ViewKind::Usage),
        Command::Waste { store } => run_view_report(&store, ViewKind::Waste),
        Command::Failures { store } => run_view_report(&store, ViewKind::Failures),
        Command::Stuck { store } => run_view_report(&store, ViewKind::Stuck),
        Command::Prompts { store } => run_view_report(&store, ViewKind::Prompts),
        Command::Corrections { store } => run_finding_report(&store, corrections, "corrections"),
        Command::Rework { store } => run_finding_report(&store, rework, "rework"),
        Command::Verification { store } => run_finding_report(&store, verification, "verification"),
        Command::Knowledge { store } => run_finding_report(&store, knowledge, "knowledge"),
        Command::Instructions { store } => run_finding_report(&store, instructions, "instructions"),
        Command::Doctor { store, limit } => run_doctor_report(&store, limit),
        Command::Query { sql, options } => run_query(sql, &options, "query"),
        Command::Sql { sql, options } => run_query(sql, &options, "sql"),
        Command::Optimize {
            store,
            diff,
            apply,
            yes,
            print,
        } => {
            if !diff && !apply && !print {
                bail!("optimize requires --diff, --print, or --apply");
            }
            if apply && (store.since.is_some() || store.until.is_some()) {
                bail!(
                    "reporting period filters are supported by optimize --diff only; optimize --apply requires the unfiltered store"
                );
            }
            let (data, freshness, selection) = load_reporting(&store)?;
            let scoped_data = data_for_scope(&data, &store.scope);
            let findings = analyze_default(&scoped_data)
                .into_iter()
                .filter(|finding| scope_matches(&store.scope, &finding.scope))
                .collect::<Vec<_>>();
            let waste = match filter_view_report(
                ViewReport::Waste(codexlens::analysis::views::waste(&scoped_data)),
                &store.scope,
            ) {
                ViewReport::Waste(report) => report,
                _ => unreachable!("waste report variant"),
            };
            let overhead = match filter_view_report(
                ViewReport::Overhead(codexlens::analysis::views::overhead(&scoped_data)),
                &store.scope,
            ) {
                ViewReport::Overhead(report) => report,
                _ => unreachable!("overhead report variant"),
            };
            let opportunities = doctor_opportunities(&scoped_data, &findings, &waste, &overhead);
            let proposal_plan =
                proposals_for_findings_and_waste(&scoped_data, &findings, &opportunities);
            if print {
                run_optimize_print(
                    &scoped_data,
                    &findings,
                    &waste,
                    &proposal_plan,
                    &freshness,
                    &store,
                    selection.as_ref(),
                )
            } else if diff {
                let batch = proposal_batch(&proposal_plan);
                if let Some(selection) = selection.as_ref() {
                    let coverage = coverage_for_scope(&data, Some(selection), &store.scope);
                    let period = period_for_scope(selection, &store.scope);
                    store.format.write_report(
                        "OPTIMIZE",
                        || {
                            render_optimize_human_with_period(
                                &batch, &coverage, &freshness, &period,
                            )
                        },
                        || render_json_diff_with_period(&batch, &freshness, &coverage, &period),
                    )
                } else {
                    store.format.write_report(
                        "OPTIMIZE",
                        || render_optimize_human(&batch),
                        || render_json_diff(&batch),
                    )
                }
            } else {
                run_apply(&data, &proposal_plan.proposals, &proposal_plan.skipped, yes)?;
                Ok(())
            }
        }
        Command::Monitor {
            store,
            source,
            kind,
            max_polls,
            interval_ms,
            cursor,
        } => {
            if store.since.is_some() || store.until.is_some() {
                bail!("--since and --until are reporting filters; monitor does not accept them");
            }
            run_monitor(
                &store,
                &source,
                kind,
                max_polls,
                interval_ms,
                cursor.as_deref(),
            )
        }
    }
}

fn run_monitor(
    store_options: &MonitorStoreOptions,
    source: &Path,
    kind: MonitorKind,
    max_polls: Option<usize>,
    interval_ms: u64,
    cursor_path: Option<&Path>,
) -> Result<()> {
    let options = codexlens::monitor::MonitorOptions {
        poll_interval: std::time::Duration::from_millis(interval_ms),
        ..codexlens::monitor::MonitorOptions::default()
    };
    let mut store = Store::open(&store_options.store).with_context(|| {
        format!(
            "failed to open derived store {}; monitoring requires a writable local store",
            store_options.store.display()
        )
    })?;
    if let Some(path) = cursor_path {
        reject_monitor_cursor_path(path, source, &store_options.store)?;
    }
    let cursor = cursor_path.map(load_monitor_cursor).transpose()?.flatten();
    let mut monitor = match kind {
        MonitorKind::Rollout => codexlens::monitor::LocalMonitor::rollout(source, cursor, options)?,
        MonitorKind::State => codexlens::monitor::LocalMonitor::state(source, cursor, options)?,
    };
    let mut clock = codexlens::monitor::SystemMonitorClock;
    let mut polls = 0usize;
    let mut cursor_error = None;
    monitor.run(&mut store, &mut clock, |poll| {
        polls = polls.saturating_add(1);
        println!(
            "Monitor {:?}: {:?} ({} records, {} skipped duplicates, offset {})",
            kind, poll.status, poll.records, poll.skipped_duplicates, poll.cursor.offset,
        );
        for diagnostic in &poll.diagnostics {
            eprintln!(
                "Monitor diagnostic {:?}: {}",
                diagnostic.kind, diagnostic.message
            );
        }
        if let Some(path) = cursor_path {
            if let Err(error) = write_monitor_cursor(path, &poll.cursor) {
                cursor_error = Some(error);
                return true;
            }
        }
        max_polls.is_some_and(|limit| polls >= limit)
    })?;
    if let Some(error) = cursor_error {
        return Err(error);
    }
    Ok(())
}

fn load_monitor_cursor(path: &Path) -> Result<Option<codexlens::monitor::MonitorCursor>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read monitor cursor {}", bounded_display(path)))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("failed to parse monitor cursor {}", bounded_display(path)))
}

fn write_monitor_cursor(path: &Path, cursor: &codexlens::monitor::MonitorCursor) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(cursor)?;
    fs::write(path, bytes)
        .with_context(|| format!("failed to write monitor cursor {}", bounded_display(path)))
}

fn reject_monitor_cursor_path(cursor: &Path, source: &Path, store: &Path) -> Result<()> {
    for (label, protected) in [("source", source), ("derived store", store)] {
        if cursor.exists() && same_file_identity(cursor, protected)? {
            bail!(
                "monitor cursor must not be the {label}: {}",
                bounded_display(cursor)
            );
        }
    }
    Ok(())
}

fn refresh_store(options: &RefreshOptions) -> Result<RefreshOutcome> {
    if let Some(home) = options.codex_home.as_deref() {
        if !home.is_absolute() {
            bail!(
                "--codex-home must be an absolute directory: {}",
                bounded_display(home)
            );
        }
    }
    let discovery = discover(&DiscoveryOptions {
        explicit_home: options.codex_home.clone(),
        include_archived: options.include_archived,
    });
    if discovery.inputs.is_empty() {
        return Err(refresh_discovery_error(&discovery));
    }

    let home = discovery
        .home
        .as_ref()
        .context("refresh could not resolve the Codex home")?;
    let codex_home = fs::canonicalize(&home.path).with_context(|| {
        format!(
            "refresh could not resolve Codex home {}",
            home.path.display()
        )
    })?;
    let all_raw_inputs = if options.include_archived {
        discovery.inputs.clone()
    } else {
        discover(&DiscoveryOptions {
            explicit_home: options.codex_home.clone(),
            include_archived: true,
        })
        .inputs
    };
    let (capture, config) =
        InstructionCaptureOptions::from_codex_home(&codex_home, options.config.as_deref());
    let instruction_paths =
        protected_instruction_paths(&codex_home, &discovery.inputs, &capture, &config.path);
    reject_protected_store_path(&options.store, &all_raw_inputs, &instruction_paths)?;
    ensure_store_parent(&options.store)?;
    let staged = AtomicRefreshStore::create(&options.store).with_context(|| {
        format!(
            "failed to prepare derived store {} for refresh",
            bounded_display(&options.store)
        )
    })?;
    let mut store = Store::open(staged.path()).with_context(|| {
        format!(
            "failed to open staged derived store {} for refresh",
            bounded_display(&options.store)
        )
    })?;
    let report = store
        .ingest_inputs_with_instructions_and_subagents(
            &discovery.inputs,
            &IngestOptions {
                ..IngestOptions::default()
            },
            &capture,
            options.include_subagents,
        )
        .context("refresh failed while ingesting discovered inputs")?;
    let freshness = store.freshness()?;
    drop(store);
    staged.commit()?;

    Ok(RefreshOutcome {
        discovery,
        report,
        freshness,
    })
}

fn ensure_store_parent(target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if parent.exists() {
        if !parent.is_dir() {
            bail!(
                "derived store parent is not a directory: {}",
                bounded_display(parent)
            );
        }
        return Ok(());
    }
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create derived store directory {}",
            bounded_display(parent)
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn run_refresh(options: &RefreshOptions) -> Result<()> {
    let outcome = refresh_store(options)?;
    for diagnostic in &outcome.discovery.diagnostics {
        eprintln!(
            "Discovery diagnostic {}: {}",
            bounded_display(&diagnostic.path),
            bounded_text(&diagnostic.message)
        );
    }
    println!("Refreshed store: {}", bounded_display(&options.store));
    for file in outcome.report.files {
        let status = if file.skipped { "skipped" } else { "ingested" };
        println!(
            "- {}: {status} ({} sessions, {} records, {} diagnostics)",
            bounded_display(&file.source),
            file.sessions,
            file.records,
            file.diagnostics
        );
    }
    println!("Store freshness: {}", outcome.freshness);
    Ok(())
}

fn reject_protected_store_path(
    store: &Path,
    inputs: &[DiscoveredInput],
    protected_paths: &[PathBuf],
) -> Result<()> {
    let Ok(identity) = fs::canonicalize(store) else {
        return Ok(());
    };
    let mut matches_raw_input = false;
    for input in inputs {
        if input.identity == identity || same_file_identity(&input.path, store)? {
            matches_raw_input = true;
            break;
        }
    }
    if !matches_raw_input {
        for path in protected_paths {
            if path.is_file() && same_file_identity(path, store)? {
                matches_raw_input = true;
                break;
            }
        }
    }
    if matches_raw_input {
        bail!(
            "derived store must not be a raw or instruction input: {}",
            bounded_display(store)
        );
    }
    Ok(())
}

fn protected_instruction_paths(
    codex_home: &Path,
    inputs: &[DiscoveredInput],
    capture: &InstructionCaptureOptions,
    config_path: &Path,
) -> Vec<PathBuf> {
    let mut paths = BTreeSet::from([
        config_path.to_path_buf(),
        codex_home.join("AGENTS.override.md"),
        codex_home.join("AGENTS.md"),
    ]);
    let mut names = vec!["AGENTS.override.md".to_owned(), "AGENTS.md".to_owned()];
    names.extend(
        capture
            .config
            .project_doc_fallback_filenames
            .iter()
            .cloned(),
    );

    for input in inputs {
        let sessions = match input.kind {
            InputKind::StateDatabase => read_state_database(&input.path).sessions,
            InputKind::Rollout { .. } => Vec::new(),
        };
        for session in sessions {
            add_instruction_candidate_paths(
                &mut paths,
                &names,
                session.project.as_deref(),
                session.cwd.as_deref(),
            );
        }
        if matches!(input.kind, InputKind::Rollout { .. }) {
            let parsed = parse_rollout(&input.path, &RolloutParseOptions::default());
            for session in normalize_rollout(&parsed).sessions {
                add_instruction_candidate_paths(
                    &mut paths,
                    &names,
                    session.project.as_deref(),
                    session.cwd.as_deref(),
                );
            }
            for record in parsed.records {
                if let Some(context) = record.instruction_context {
                    add_instruction_candidate_paths(
                        &mut paths,
                        &names,
                        context.project_root.as_deref(),
                        context.cwd.as_deref(),
                    );
                }
            }
        }
    }
    paths.into_iter().collect()
}

fn add_instruction_candidate_paths(
    paths: &mut BTreeSet<PathBuf>,
    names: &[String],
    project: Option<&str>,
    cwd: Option<&str>,
) {
    for base in [project, cwd].into_iter().flatten() {
        let path = Path::new(base);
        if !path.is_absolute() {
            continue;
        }
        let mut directory = Some(path);
        while let Some(directory_path) = directory {
            for name in names {
                paths.insert(directory_path.join(name));
            }
            directory = directory_path.parent();
        }
    }
}

#[cfg(unix)]
fn same_file_identity(left: &Path, right: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = fs::metadata(left)?;
    let right = fs::metadata(right)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(windows)]
fn same_file_identity(left: &Path, right: &Path) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    fn identity(path: &Path) -> std::io::Result<(u32, u64)> {
        let file = std::fs::File::open(path)?;
        let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let information = unsafe { information.assume_init() };
        Ok((
            information.dwVolumeSerialNumber,
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        ))
    }

    Ok(identity(left)? == identity(right)?)
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &Path, right: &Path) -> std::io::Result<bool> {
    Ok(fs::canonicalize(left)? == fs::canonicalize(right)?)
}

fn refresh_discovery_error(discovery: &DiscoveryResult) -> anyhow::Error {
    let detail = discovery
        .diagnostics
        .first()
        .map(|diagnostic| {
            format!(
                "{}: {}",
                bounded_display(&diagnostic.path),
                bounded_text(&diagnostic.message)
            )
        })
        .unwrap_or_else(|| "no supported rollout or state inputs were discovered".to_owned());
    anyhow::anyhow!("refresh could not discover inputs: {detail}")
}

fn bounded_display(path: &Path) -> String {
    bounded_text(&path.display().to_string())
}

fn bounded_text(value: &str) -> String {
    const MAX_BYTES: usize = 256;
    if value.len() <= MAX_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_BYTES - 3;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

const QUERY_SQL_MAX_BYTES: usize = 64 * 1024;
const QUERY_COLUMN_LIMIT: usize = 50;

struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    omitted_columns: usize,
    omitted_rows: usize,
}

fn run_query(sql: Option<String>, options: &QueryOptions, command: &str) -> Result<()> {
    let sql = match sql {
        Some(sql) => sql,
        None => read_query_from_stdin()?,
    };
    validate_query_input(&sql)?;
    let path = options.store_path()?;
    let display = bounded_display(&path);
    if !path.is_file() {
        bail!("store does not exist: {display}");
    }
    let store = Store::open_read_only(&path).with_context(|| {
        format!("failed to open query store {display}; provide a valid SQLite store")
    })?;
    store
        .connection()
        .authorizer(Some(read_only_sql_authorizer));
    let result = execute_query(store.connection(), &sql)?;
    match options.format {
        OutputFormat::Table => print!("{}", render_query_table(&result, command)),
        OutputFormat::Markdown => print!("{}", render_query_markdown(&result, command)),
        OutputFormat::Json => {
            let document = serde_json::json!({
                "schema_version": 1,
                "command": command,
                "data": {
                    "columns": result.columns,
                    "rows": result.rows,
                    "omitted_column_count": result.omitted_columns,
                    "omitted_count": result.omitted_rows,
                },
            });
            println!("{}", serde_json::to_string_pretty(&document)?);
        }
    }
    Ok(())
}

fn read_query_from_stdin() -> Result<String> {
    let mut sql = String::new();
    io::stdin()
        .take((QUERY_SQL_MAX_BYTES + 1) as u64)
        .read_to_string(&mut sql)
        .context("could not read query from stdin")?;
    Ok(sql)
}

fn validate_query_input(sql: &str) -> Result<()> {
    if sql.is_empty() {
        bail!("query requires SQL as an argument or on stdin");
    }
    if sql.len() > QUERY_SQL_MAX_BYTES {
        bail!("query exceeds the {QUERY_SQL_MAX_BYTES}-byte limit");
    }
    if sql.as_bytes().contains(&0) {
        bail!("query contains an unsupported NUL byte");
    }
    Ok(())
}

fn execute_query(connection: &rusqlite::Connection, sql: &str) -> Result<QueryResult> {
    let mut statement = connection.prepare(sql).map_err(|error| {
        if error.sqlite_error_code() == Some(rusqlite::ErrorCode::AuthorizationForStatementDenied) {
            anyhow::anyhow!("query must be a single read-only SQL statement")
        } else {
            anyhow::anyhow!("query must contain one valid SQL statement")
        }
    })?;
    if !statement.readonly() || is_mutating_pragma(sql) {
        bail!("query must be a single read-only SQL statement");
    }

    let total_columns = statement.column_count();
    let columns = statement
        .column_names()
        .into_iter()
        .take(QUERY_COLUMN_LIMIT)
        .map(bounded_text)
        .collect::<Vec<_>>();
    let omitted_columns = total_columns.saturating_sub(columns.len());
    let mut rows = statement
        .query([])
        .map_err(|_| anyhow::anyhow!("query could not start"))?;
    let mut result_rows = Vec::new();
    let mut omitted_rows = 0;
    while let Some(row) = rows
        .next()
        .map_err(|_| anyhow::anyhow!("query could not read result rows"))?
    {
        if result_rows.len() >= CLI_VIEW_ROW_LIMIT {
            omitted_rows += 1;
            continue;
        }
        let mut result_row = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            let value = row
                .get_ref(index)
                .map_err(|_| anyhow::anyhow!("query returned an unreadable value"))?;
            result_row.push(query_value(value));
        }
        result_rows.push(result_row);
    }
    Ok(QueryResult {
        columns,
        rows: result_rows,
        omitted_columns,
        omitted_rows,
    })
}

fn read_only_sql_authorizer(context: rusqlite::hooks::AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Read { .. } | AuthAction::Select | AuthAction::Recursive => {
            Authorization::Allow
        }
        AuthAction::Function { function_name }
            if !function_name.eq_ignore_ascii_case("load_extension") =>
        {
            Authorization::Allow
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if pragma_value.is_none() && read_only_pragma_without_argument(pragma_name) => {
            Authorization::Allow
        }
        AuthAction::Pragma { pragma_name, .. }
            if READ_ONLY_PRAGMA_ARGUMENTS
                .iter()
                .any(|candidate| pragma_name.eq_ignore_ascii_case(candidate)) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }
}

fn read_only_pragma_without_argument(name: &str) -> bool {
    [
        "application_id",
        "compile_options",
        "data_version",
        "encoding",
        "foreign_keys",
        "freelist_count",
        "page_count",
        "page_size",
        "recursive_triggers",
        "schema_version",
        "user_version",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn query_value(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(value) => serde_json::json!(value),
        ValueRef::Real(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ValueRef::Text(value) => {
            serde_json::Value::String(bounded_text(&String::from_utf8_lossy(value)))
        }
        ValueRef::Blob(value) => serde_json::Value::String(format!("<blob {} bytes>", value.len())),
    }
}

fn query_human_value(value: &serde_json::Value) -> String {
    let value = match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::String(value) => value.clone(),
        value => value.to_string(),
    };
    value.replace(['\r', '\n'], " ")
}

fn render_query_table(result: &QueryResult, command: &str) -> String {
    let mut output = format!("{}\n", command.to_ascii_uppercase());
    if result.columns.is_empty() {
        output.push_str("No columns.\n");
        return output;
    }
    output.push_str("Columns: ");
    output.push_str(&result.columns.join(" | "));
    output.push('\n');
    for row in &result.rows {
        output.push_str("- ");
        output.push_str(
            &row.iter()
                .map(query_human_value)
                .collect::<Vec<_>>()
                .join(" | "),
        );
        output.push('\n');
    }
    output.push_str(&format!(
        "Rows: {}\nOmitted rows: {}\nOmitted columns: {}\n",
        result.rows.len(),
        result.omitted_rows,
        result.omitted_columns
    ));
    output
}

fn markdown_cell(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

fn render_query_markdown(result: &QueryResult, command: &str) -> String {
    let mut output = format!("# {}\n\n", command.to_ascii_uppercase());
    if result.columns.is_empty() {
        output.push_str("No columns.\n");
        return output;
    }
    output.push('|');
    for column in &result.columns {
        output.push(' ');
        output.push_str(&markdown_cell(column));
        output.push_str(" |");
    }
    output.push('\n');
    output.push('|');
    for _ in &result.columns {
        output.push_str(" --- |");
    }
    output.push('\n');
    for row in &result.rows {
        output.push('|');
        for value in row {
            output.push(' ');
            output.push_str(&markdown_cell(&query_human_value(value)));
            output.push_str(" |");
        }
        output.push('\n');
    }
    output.push_str(&format!(
        "\nRows: {}\nOmitted rows: {}\nOmitted columns: {}\n",
        result.rows.len(),
        result.omitted_rows,
        result.omitted_columns
    ));
    output
}

// ponytail: keep the built-in read-only argument list explicit; extend it when SQLite adds one.
const READ_ONLY_PRAGMA_ARGUMENTS: &[&str] = &[
    "foreign_key_check",
    "foreign_key_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "quick_check",
    "table_list",
    "table_info",
    "table_xinfo",
];

const MUTATING_PRAGMA_WITHOUT_ARGUMENTS: &[&str] = &[
    "incremental_vacuum",
    "optimize",
    "shrink_memory",
    "wal_checkpoint",
];

fn is_mutating_pragma(sql: &str) -> bool {
    let Some(mut rest) = after_sql_keyword(sql, "pragma") else {
        return false;
    };
    rest = skip_sql_space_and_comments(rest);
    let Some((mut after_name, mut name)) = take_sql_identifier(rest) else {
        return true;
    };
    after_name = skip_sql_space_and_comments(after_name);
    if let Some(after_schema) = after_name.strip_prefix('.') {
        let Some((qualified_rest, qualified_name)) =
            take_sql_identifier(skip_sql_space_and_comments(after_schema))
        else {
            return true;
        };
        after_name = qualified_rest;
        name = qualified_name;
    }
    after_name = skip_sql_space_and_comments(after_name);
    if after_name.starts_with('=') {
        return true;
    }
    if after_name.starts_with('(') {
        return !READ_ONLY_PRAGMA_ARGUMENTS
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate));
    }
    MUTATING_PRAGMA_WITHOUT_ARGUMENTS
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn after_sql_keyword<'a>(sql: &'a str, keyword: &str) -> Option<&'a str> {
    let sql = skip_sql_space_and_comments(sql);
    let prefix = sql.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    if sql
        .get(keyword.len()..)
        .and_then(|tail| tail.chars().next())
        .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return None;
    }
    sql.get(keyword.len()..)
}

fn skip_sql_space_and_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(comment) = sql.strip_prefix("--") {
            let Some(end) = comment.find('\n') else {
                return "";
            };
            sql = &comment[end + 1..];
            continue;
        }
        if let Some(comment) = sql.strip_prefix("/*") {
            let Some(end) = comment.find("*/") else {
                return "";
            };
            sql = &comment[end + 2..];
            continue;
        }
        return sql;
    }
}

fn take_sql_identifier(sql: &str) -> Option<(&str, &str)> {
    let end = sql
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(sql.len());
    (end > 0).then(|| (&sql[end..], &sql[..end]))
}

fn proposal_batch(plan: &codexlens::advisor::ProposalPlan) -> codexlens::advisor::DiffBatch {
    let mut batch = render_diffs(&plan.proposals);
    batch.skipped.extend(plan.skipped.clone());
    batch.skipped.sort_by(|left, right| {
        left.target_path
            .cmp(&right.target_path)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    batch
}

fn run_apply(
    data: &CanonicalData,
    proposals: &[codexlens::advisor::Proposal],
    skipped: &[codexlens::advisor::SkippedProposal],
    yes: bool,
) -> Result<()> {
    for skipped in skipped {
        eprintln!(
            "Skipped {}: {}",
            bounded_path(&skipped.target_path),
            bounded_text(&skipped.reason)
        );
    }
    if proposals.is_empty() {
        require_confirmation_mode(yes)?;
        println!("No applicable proposals.");
        return Ok(());
    }
    let plan = prepare_apply_proposals(data, proposals)
        .context("optimize --apply rejected the proposal batch before writing")?;
    let confirmed = confirm_apply(&plan, yes)?;
    let report = plan
        .apply(confirmed)
        .context("optimize --apply could not complete the transaction")?;
    print_apply_report(&report);
    Ok(())
}

fn confirm_apply(plan: &ApplyPlan, yes: bool) -> Result<bool> {
    require_confirmation_mode(yes)?;
    if yes {
        return Ok(true);
    }
    eprintln!("Validated write set:");
    for path in plan.write_set() {
        eprintln!("- {}", bounded_path(path));
    }
    eprint!("Apply these files? Type 'yes' to continue: ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("yes") {
        Ok(true)
    } else {
        bail!("optimize --apply cancelled; no files were written")
    }
}

fn require_confirmation_mode(yes: bool) -> Result<()> {
    if !yes && !io::stdin().is_terminal() {
        bail!(
            "optimize --apply requires explicit confirmation; rerun with --yes after reviewing optimize --diff"
        );
    }
    Ok(())
}

fn print_apply_report(report: &ApplyReport) {
    println!("Applied {} file(s).", report.changed_files.len());
    for path in &report.changed_files {
        println!("Changed: {}", bounded_path(path));
    }
    println!("Backups retained at {}.", bounded_path(&report.backup_dir));
    println!("Recovery: {}.", report.recovery);
}

fn bounded_path(path: &Path) -> String {
    bounded_text(&path.display().to_string())
}

fn run_analyze(options: &StoreOptions) -> Result<()> {
    validate_reporting_options(options)?;
    if !options.frozen {
        let store_path = options.store_path()?;
        refresh_reporting_store(options, &store_path)?;
    }
    run_finding_report(options, analyze_default, "analyze")
}

fn refresh_reporting_store(options: &StoreOptions, store_path: &Path) -> Result<()> {
    let refresh = RefreshOptions {
        store: store_path.to_path_buf(),
        codex_home: options.codex_home.clone(),
        include_archived: options.include_archived,
        include_subagents: options.include_subagents,
        config: None,
    };
    let outcome = refresh_store(&refresh).with_context(|| {
        format!(
            "analyze could not refresh derived store {}",
            bounded_display(store_path)
        )
    })?;
    report_auto_refresh(&outcome, store_path);
    Ok(())
}

fn run_view_report(options: &StoreOptions, kind: ViewKind) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = coverage_for_scope(&data, selection.as_ref(), &options.scope);
    let scoped_data = data_for_scope(&data, &options.scope);
    let period = selection
        .as_ref()
        .map(|selection| period_for_scope(selection, &options.scope));
    let report = match kind {
        ViewKind::Inventory => {
            ViewReport::Inventory(codexlens::analysis::views::inventory(&scoped_data))
        }
        ViewKind::Overhead => {
            ViewReport::Overhead(codexlens::analysis::views::overhead(&scoped_data))
        }
        ViewKind::Usage => ViewReport::Usage(codexlens::analysis::views::usage(&scoped_data)),
        ViewKind::Waste => ViewReport::Waste(codexlens::analysis::views::waste(&scoped_data)),
        ViewKind::Failures => {
            ViewReport::Failures(codexlens::analysis::views::failures(&scoped_data))
        }
        ViewKind::Stuck => ViewReport::Stuck(codexlens::analysis::views::stuck(&scoped_data)),
        ViewKind::Prompts => ViewReport::Prompts(codexlens::analysis::views::prompts(&scoped_data)),
    };
    let report = filter_view_report(report, &options.scope);
    match options.format {
        OutputFormat::Table => {
            let (stdout, stderr) = render_view_table(
                kind,
                &report,
                &freshness,
                &coverage,
                &options.scope,
                period.as_ref(),
            );
            print!("{stdout}");
            eprint!("{stderr}");
            Ok(())
        }
        OutputFormat::Markdown => {
            print!(
                "{}",
                render_view_markdown(
                    kind,
                    &report,
                    &freshness,
                    &coverage,
                    &options.scope,
                    period.as_ref(),
                )
            );
            Ok(())
        }
        OutputFormat::Json => print_view_json(
            kind,
            &report,
            &scoped_data,
            &freshness,
            &coverage,
            options,
            period.as_ref(),
        ),
    }
}

fn run_sessions_report(options: &StoreOptions) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = coverage_for_scope(&data, selection.as_ref(), &options.scope);
    let scoped_data = data_for_scope(&data, &options.scope);
    let period = selection
        .as_ref()
        .map(|selection| period_for_scope(selection, &options.scope));
    let sessions = report_sessions(&scoped_data)
        .into_iter()
        .filter(|session| session_scope_matches(&options.scope, session))
        .collect::<Vec<_>>();
    let omitted_count = sessions.len().saturating_sub(CLI_VIEW_ROW_LIMIT);
    match options.format {
        OutputFormat::Table => {
            print!(
                "{}",
                render_sessions_table(
                    &sessions,
                    &freshness,
                    &coverage,
                    &options.scope,
                    period.as_ref(),
                )
            );
            Ok(())
        }
        OutputFormat::Markdown => {
            print!(
                "{}",
                render_sessions_markdown(
                    &sessions,
                    &freshness,
                    &coverage,
                    &options.scope,
                    period.as_ref(),
                )
            );
            Ok(())
        }
        OutputFormat::Json => {
            let rows = sessions
                .iter()
                .take(CLI_VIEW_ROW_LIMIT)
                .map(|session| {
                    serde_json::json!({
                        "id": bounded_text(&session.id),
                        "created_at": session.created_at.as_deref().map(bounded_text),
                        "updated_at": session.updated_at.as_deref().map(bounded_text),
                        "cwd": session.cwd.as_deref().map(|path| bounded_path(Path::new(path))),
                        "project": session.project.as_deref().map(|path| bounded_path(Path::new(path))),
                    })
                })
                .collect::<Vec<_>>();
            let document = serde_json::json!({
                "schema_version": 1,
                "command": "sessions",
                "scope": scope_filter_json(&options.scope),
                "coverage": cli_coverage_json(&scoped_data, &coverage, options, period.as_ref()),
                "freshness": freshness_json(&freshness),
                "data": {
                    "rows": rows,
                    "omitted_count": omitted_count,
                },
            });
            println!("{}", serde_json::to_string_pretty(&document)?);
            Ok(())
        }
    }
}

fn render_sessions_table(
    sessions: &[codexlens::advisor::SessionSummary],
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    period: Option<&PeriodCoverage>,
) -> String {
    let mut output = format!(
        "SESSIONS\nScope: {}\nStore freshness: {}\n",
        scope_label(scope),
        freshness,
    );
    append_period_metadata(&mut output, period);
    output.push_str(&format!(
        "Coverage: {} ({} sessions)\n",
        coverage.status, coverage.session_count
    ));
    output.push_str(&render_coverage_limitations(coverage));
    if sessions.is_empty() {
        output.push_str("No selected sessions.\n");
        output.push_str("Omitted sessions: 0\n");
        return output;
    }
    output.push('\n');
    for session in sessions.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!("- {}\n", bounded_text(&session.id)));
        append_field(
            &mut output,
            "created",
            session.created_at.as_deref().map(bounded_text),
        );
        append_field(
            &mut output,
            "updated",
            session.updated_at.as_deref().map(bounded_text),
        );
        append_field(
            &mut output,
            "cwd",
            session
                .cwd
                .as_deref()
                .map(|path| bounded_path(Path::new(path))),
        );
        append_field(
            &mut output,
            "project",
            session
                .project
                .as_deref()
                .map(|path| bounded_path(Path::new(path))),
        );
    }
    output.push_str(&format!(
        "Omitted sessions: {}\n",
        sessions.len().saturating_sub(CLI_VIEW_ROW_LIMIT)
    ));
    output
}

fn render_sessions_markdown(
    sessions: &[codexlens::advisor::SessionSummary],
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    period: Option<&PeriodCoverage>,
) -> String {
    let table = render_sessions_table(sessions, freshness, coverage, scope, period);
    let mut lines = table.lines();
    let heading = lines.next().unwrap_or("SESSIONS");
    format!("# {heading}\n\n{}\n", lines.collect::<Vec<_>>().join("\n"))
}

fn session_scope_matches(
    scope: &ScopeFilter,
    session: &codexlens::advisor::SessionSummary,
) -> bool {
    let finding_scope = session
        .project
        .as_deref()
        .or(session.cwd.as_deref())
        .filter(|path| Path::new(path).is_absolute() || Path::new(path).has_root())
        .map(|path| {
            FindingScope::Project(fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
        })
        .unwrap_or(FindingScope::Global);
    scope_matches(scope, &finding_scope)
}

fn filter_view_report(report: ViewReport, scope: &ScopeFilter) -> ViewReport {
    match report {
        ViewReport::Inventory(mut report) => {
            report.rows.retain(|row| scope_matches(scope, &row.scope));
            ViewReport::Inventory(report)
        }
        ViewReport::Overhead(mut report) => {
            report.rows.retain(|row| scope_matches(scope, &row.scope));
            ViewReport::Overhead(report)
        }
        ViewReport::Usage(mut report) => {
            report.rows.retain(|row| scope_matches(scope, &row.scope));
            ViewReport::Usage(report)
        }
        ViewReport::Waste(mut report) => {
            report
                .opportunities
                .retain(|row| scope_matches(scope, &row.scope));
            ViewReport::Waste(report)
        }
        ViewReport::Failures(mut report) => {
            report
                .rows
                .retain(|row| scope_matches(scope, &row.opportunity.scope));
            ViewReport::Failures(report)
        }
        ViewReport::Stuck(mut report) => {
            report
                .rows
                .retain(|row| scope_matches(scope, &row.opportunity.scope));
            ViewReport::Stuck(report)
        }
        ViewReport::Prompts(mut report) => {
            report.rows.retain(|row| scope_matches(scope, &row.scope));
            ViewReport::Prompts(report)
        }
    }
}

struct DoctorView {
    top_fixes: Vec<ViewOpportunity>,
    top_fixes_omitted_count: usize,
    cost: OverheadReport,
    pruning: InventoryReport,
    finding_report: DoctorReport,
    has_opportunities: bool,
    coverage_sufficient: bool,
}

fn run_doctor_report(options: &StoreOptions, limit: Option<usize>) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = coverage_for_scope(&data, selection.as_ref(), &options.scope);
    let scoped_data = data_for_scope(&data, &options.scope);
    let period = selection
        .as_ref()
        .map(|selection| period_for_scope(selection, &options.scope));
    let waste = match filter_view_report(
        ViewReport::Waste(codexlens::analysis::views::waste(&scoped_data)),
        &options.scope,
    ) {
        ViewReport::Waste(report) => report,
        _ => unreachable!("waste report variant"),
    };
    let cost = match filter_view_report(
        ViewReport::Overhead(codexlens::analysis::views::overhead(&scoped_data)),
        &options.scope,
    ) {
        ViewReport::Overhead(report) => report,
        _ => unreachable!("overhead report variant"),
    };
    let pruning = match filter_view_report(
        ViewReport::Inventory(codexlens::analysis::views::inventory(&scoped_data)),
        &options.scope,
    ) {
        ViewReport::Inventory(report) => report,
        _ => unreachable!("inventory report variant"),
    };
    let max_per_scope = limit.unwrap_or(5).min(5);
    let findings = analyze_default(&scoped_data)
        .into_iter()
        .filter(|finding| scope_matches(&options.scope, &finding.scope))
        .collect::<Vec<_>>();
    let mut finding_report = doctor_with_coverage(
        &scoped_data,
        &findings,
        freshness.clone(),
        &DoctorOptions {
            max_findings_per_scope: Some(CLI_VIEW_ROW_LIMIT),
            ..DoctorOptions::default()
        },
        &coverage,
    );
    if let Some(period) = period.as_ref() {
        finding_report.period_start = period.observed_start.clone();
        finding_report.period_end = period.observed_end.clone();
    }
    let all_opportunities = doctor_opportunities(&scoped_data, &findings, &waste, &cost);
    let has_opportunities = !all_opportunities.is_empty();
    let (top_fixes, top_fixes_omitted_count) =
        bound_doctor_opportunities(all_opportunities, max_per_scope);
    let unknown_cost = cost.rows.iter().any(|row| row.unknown_cost);
    let unknown_inventory = pruning
        .rows
        .iter()
        .any(|row| row.usage_state == codexlens::model::SurfaceUsageState::Unknown);
    let coverage_sufficient = coverage.status == "observed"
        && coverage.limitations.is_empty()
        && coverage.limitations_omitted == 0
        && !unknown_cost
        && !unknown_inventory;
    let doctor = DoctorView {
        top_fixes,
        top_fixes_omitted_count,
        cost,
        pruning: InventoryReport {
            measure: pruning.measure,
            rows: pruning
                .rows
                .into_iter()
                .filter(|row| row.action.is_some())
                .collect(),
        },
        finding_report,
        has_opportunities,
        coverage_sufficient,
    };
    match options.format {
        OutputFormat::Table => {
            let (stdout, stderr) = render_doctor_table(
                &doctor,
                &freshness,
                &coverage,
                &options.scope,
                period.as_ref(),
            );
            print!("{stdout}");
            eprint!("{stderr}");
            Ok(())
        }
        OutputFormat::Markdown => {
            print!(
                "{}",
                render_doctor_markdown(
                    &doctor,
                    &freshness,
                    &coverage,
                    &options.scope,
                    period.as_ref(),
                )
            );
            Ok(())
        }
        OutputFormat::Json => print_doctor_json(
            &doctor,
            &scoped_data,
            &freshness,
            &coverage,
            &options.scope,
            options,
            period.as_ref(),
        ),
    }
}

fn bound_doctor_opportunities(
    opportunities: Vec<ViewOpportunity>,
    max_per_scope: usize,
) -> (Vec<ViewOpportunity>, usize) {
    let total = opportunities.len();
    let mut seen_per_scope = BTreeMap::<String, usize>::new();
    let mut visible = Vec::new();
    for opportunity in opportunities {
        let count = seen_per_scope
            .entry(opportunity.scope.to_string())
            .or_default();
        if *count >= max_per_scope || visible.len() >= CLI_VIEW_ROW_LIMIT {
            continue;
        }
        *count += 1;
        visible.push(opportunity);
    }
    let omitted = total.saturating_sub(visible.len());
    (visible, omitted)
}

fn render_doctor_table(
    doctor: &DoctorView,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    period: Option<&PeriodCoverage>,
) -> (String, String) {
    let mut output = format!(
        "WHAT TO FIX FIRST\nScope: {}\nStore freshness: {}\n",
        scope_label(scope),
        freshness,
    );
    append_period_metadata(&mut output, period);
    output.push_str(&format!(
        "Coverage: {} ({} sessions)\n",
        coverage.status, coverage.session_count
    ));
    output.push_str(&render_coverage_limitations(coverage));
    if doctor.top_fixes.is_empty() {
        if doctor.top_fixes_omitted_count > 0 {
            output.push_str(
                "Top fixes were omitted by the selected doctor limit; inspect the focused views for the remaining opportunities.\n",
            );
        } else if doctor.coverage_sufficient {
            output.push_str("No top fixes with bounded evidence.\n");
        } else {
            output.push_str(
                "No actionable finding was observed with sufficient evidence.\nAnalysis is incomplete; inspect coverage limitations before treating this as healthy.\n",
            );
        }
    } else {
        for (index, opportunity) in doctor.top_fixes.iter().take(CLI_VIEW_ROW_LIMIT).enumerate() {
            output.push_str(&format!(
                "\n{}. {}\n",
                index + 1,
                bounded_text(&opportunity.title)
            ));
            append_opportunity_fields(&mut output, opportunity);
        }
        if doctor.top_fixes_omitted_count > 0 {
            output.push_str(&format!(
                "Omitted {} additional top-fix opportunit{} due to the per-scope/summary limit.\n",
                doctor.top_fixes_omitted_count,
                if doctor.top_fixes_omitted_count == 1 {
                    "y"
                } else {
                    "ies"
                },
            ));
        }
    }
    if !doctor.cost.rows.is_empty() {
        output.push_str("\nCOST\n");
        render_overhead_table(&mut output, &doctor.cost);
    }
    if !doctor.pruning.rows.is_empty() {
        output.push_str("\nCONFIG WORTH PRUNING\n");
        render_inventory_table(&mut output, &doctor.pruning);
    }
    if !doctor.has_opportunities && doctor.coverage_sufficient {
        output.push_str("\nLOOKS HEALTHY\nNo actionable opportunities were observed.\n");
    }
    (output, String::new())
}

fn render_doctor_markdown(
    doctor: &DoctorView,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    period: Option<&PeriodCoverage>,
) -> String {
    let (table, _) = render_doctor_table(doctor, freshness, coverage, scope, period);
    let mut lines = table.lines();
    let heading = lines.next().unwrap_or("WHAT TO FIX FIRST");
    format!("# {heading}\n\n{}\n", lines.collect::<Vec<_>>().join("\n"))
}

fn print_doctor_json(
    doctor: &DoctorView,
    data: &CanonicalData,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    options: &StoreOptions,
    period: Option<&PeriodCoverage>,
) -> Result<()> {
    let finding_document = if let Some(period) = period {
        render_json_finding_report_with_period("doctor", &doctor.finding_report, coverage, period)?
    } else {
        render_json_finding_report_with_coverage("doctor", &doctor.finding_report, coverage)?
    };
    let mut doctor_data = serde_json::from_str::<serde_json::Value>(&finding_document)?
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("doctor finding report did not contain data"))?;
    doctor_data["top_fixes"] = serde_json::json!(
        doctor
            .top_fixes
            .iter()
            .take(CLI_VIEW_ROW_LIMIT)
            .map(opportunity_json)
            .collect::<Vec<_>>()
    );
    doctor_data["top_fixes_omitted_count"] = serde_json::json!(doctor.top_fixes_omitted_count);
    doctor_data["cost"] = serde_json::json!({
        "measure": doctor.cost.measure,
        "rows": doctor.cost.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(overhead_row_json).collect::<Vec<_>>(),
        "omitted_count": doctor.cost.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
    });
    doctor_data["config_pruning"] = serde_json::json!({
        "measure": doctor.pruning.measure,
        "rows": doctor.pruning.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(inventory_row_json).collect::<Vec<_>>(),
        "omitted_count": doctor.pruning.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
    });
    doctor_data["looks_healthy"] =
        serde_json::json!(!doctor.has_opportunities && doctor.coverage_sufficient);
    doctor_data["analysis_sufficient"] = serde_json::json!(doctor.coverage_sufficient);
    let document = serde_json::json!({
        "schema_version": 1,
        "command": "doctor",
        "scope": scope_filter_json(scope),
        "coverage": cli_coverage_json(data, coverage, options, period),
        "freshness": freshness_json(freshness),
        "data": doctor_data,
    });
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}

fn append_period_metadata(output: &mut String, period: Option<&PeriodCoverage>) {
    let Some(period) = period else {
        return;
    };
    let requested = match (&period.requested_start, &period.requested_end) {
        (Some(start), Some(end)) => format!("[{start}, {end})"),
        (Some(start), None) => format!("[{start}, ∞)"),
        (None, Some(end)) => format!("(-∞, {end})"),
        (None, None) => "all".to_owned(),
    };
    output.push_str(&format!(
        "Requested period: {requested}\nObserved records: {} (excluded: {}, unknown timestamps: {} records, {} events)\nPeriod coverage: {}\n",
        period.included_records,
        period.excluded_records,
        period.unknown_timestamp_records,
        period.unknown_timestamp_events,
        period.state.as_str(),
    ));
}

fn scope_matches(filter: &ScopeFilter, scope: &FindingScope) -> bool {
    match filter {
        ScopeFilter::All => true,
        ScopeFilter::Global => matches!(scope, FindingScope::Global),
        ScopeFilter::Projects => !matches!(scope, FindingScope::Global),
        ScopeFilter::Project(root) => match scope {
            FindingScope::Global => false,
            FindingScope::Project(path) | FindingScope::Instruction(path) => {
                scope_path_matches(root, path)
            }
            FindingScope::Path(path) => scope_path_matches(root, Path::new(path)),
        },
    }
}

fn scope_path_matches(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
        || fs::canonicalize(path)
            .ok()
            .is_some_and(|canonical| canonical.starts_with(root))
}

const CLI_VIEW_ROW_LIMIT: usize = 50;

fn view_heading(kind: ViewKind) -> &'static str {
    match kind {
        ViewKind::Inventory => "CONFIGURATION INVENTORY",
        ViewKind::Overhead => "CONTEXT COST",
        ViewKind::Usage => "WHERE EFFORT GOES",
        ViewKind::Waste => "OPPORTUNITIES",
        ViewKind::Failures => "RECURRING FAILURES",
        ViewKind::Stuck => "STUCK WORK",
        ViewKind::Prompts => "HOW YOU STEER CODEX",
    }
}

fn scope_label(scope: &ScopeFilter) -> String {
    match scope {
        ScopeFilter::All => "global + projects".to_owned(),
        ScopeFilter::Global => "global".to_owned(),
        ScopeFilter::Projects => "projects".to_owned(),
        ScopeFilter::Project(path) => format!("project:{}", path.display()),
    }
}

fn render_view_table(
    kind: ViewKind,
    report: &ViewReport,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    period: Option<&PeriodCoverage>,
) -> (String, String) {
    let mut output = format!(
        "{}\nScope: {}\nStore freshness: {}\n",
        view_heading(kind),
        scope_label(scope),
        freshness,
    );
    append_period_metadata(&mut output, period);
    output.push_str(&format!(
        "Coverage: {} ({} sessions)\n",
        coverage.status, coverage.session_count
    ));
    output.push_str(&render_coverage_limitations(coverage));
    output.push('\n');
    match report {
        ViewReport::Inventory(report) => render_inventory_table(&mut output, report),
        ViewReport::Overhead(report) => render_overhead_table(&mut output, report),
        ViewReport::Usage(report) => render_usage_table(&mut output, report),
        ViewReport::Waste(report) => render_waste_table(&mut output, report),
        ViewReport::Failures(report) => render_failures_table(&mut output, report),
        ViewReport::Stuck(report) => render_stuck_table(&mut output, report),
        ViewReport::Prompts(report) => render_prompts_table(&mut output, report),
    }
    (output, String::new())
}

fn render_inventory_table(output: &mut String, report: &InventoryReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    if report.rows.is_empty() {
        output.push_str("No configured surfaces.\n");
        return;
    }
    for row in report.rows.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!(
            "- {} {} [{}]\n",
            row.kind.as_str(),
            bounded_text(&row.name),
            row.scope
        ));
        output.push_str(&format!("  owner: {}\n", bounded_text(&row.owner)));
        append_field(output, "path", row.path.as_deref().map(bounded_path));
        output.push_str(&format!(
            "  usage: {} ({} uses, {} sessions)\n",
            row.usage_state.as_str(),
            row.observed_uses,
            row.observed_sessions
        ));
        output.push_str(&format!("  load: {}\n", row.load_mode.as_str()));
        output.push_str(&format!(
            "  estimate: static={} bytes, startup={} bytes\n",
            optional_number(row.static_bytes),
            optional_number(row.startup_bytes)
        ));
        append_field(output, "action", row.action.as_deref().map(str::to_owned));
        append_evidence(output, &row.evidence);
        append_limitations(output, &row.limitations);
    }
    append_omitted(output, report.rows.len());
}

fn render_overhead_table(output: &mut String, report: &OverheadReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    if report.rows.is_empty() {
        output.push_str("No context-cost rows.\n");
        return;
    }
    for row in report.rows.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!("- {}\n", row.scope));
        append_field(output, "project", row.project.as_deref().map(bounded_path));
        output.push_str(&format!("  sessions: {}\n", row.session_count));
        output.push_str(&format!(
            "  observed minimum: {} bytes\n  readable startup: {} bytes (user-controlled configuration)\n  residual ({}): {} bytes\n  cost control: {}\n  unknown cost: {}\n",
            optional_number(row.observed_min_startup_bytes),
            optional_number(row.readable_startup_bytes),
            if row.residual_bytes.is_some() {
                "system/tool"
            } else {
                "unknown"
            },
            optional_number(row.residual_bytes),
            if row.unknown_cost { "unknown" } else { "user-controlled" },
            row.unknown_cost
        ));
        append_evidence(output, &row.evidence);
        append_limitations(output, &row.limitations);
    }
    append_omitted(output, report.rows.len());
}

fn render_usage_table(output: &mut String, report: &UsageReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    output.push_str(&format!(
        "Coverage: {} ({} known of {} events, {} observed sessions)\n",
        report.coverage.status,
        report.coverage.known_events,
        report.coverage.total_events,
        report.coverage.observed_sessions
    ));
    if report.rows.is_empty() {
        output.push_str("No observed usage rows.\n");
        return;
    }
    for row in report.rows.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!(
            "- {} {} [{}]\n",
            row.kind.as_str(),
            bounded_text(&row.name),
            row.scope
        ));
        append_field(
            output,
            "usage state",
            row.usage_state.map(|state| state.as_str().to_owned()),
        );
        output.push_str(&format!(
            "  counts: {} occurrences, {} sessions\n  tokens: input={}, cached_input={}, output={}, reasoning={}\n  duration: {} ms ({} observations)\n",
            row.occurrences,
            row.distinct_sessions,
            row.input_tokens,
            row.cached_input_tokens,
            row.output_tokens,
            row.reasoning_output_tokens,
            row.duration_ms,
            row.duration_observations
        ));
        append_evidence(output, &row.evidence);
        append_limitations(output, &row.limitations);
    }
    append_omitted(output, report.rows.len());
}

fn render_waste_table(output: &mut String, report: &WasteReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    if report.opportunities.is_empty() {
        output.push_str("No actionable opportunities.\n");
        return;
    }
    for opportunity in report.opportunities.iter().take(CLI_VIEW_ROW_LIMIT) {
        append_opportunity_table(output, opportunity);
    }
    append_omitted(output, report.opportunities.len());
}

fn render_failures_table(output: &mut String, report: &FailureReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    if report.rows.is_empty() {
        output.push_str("No recurring non-transient failures.\n");
        return;
    }
    for row in report.rows.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!(
            "- {} / {} / {}\n",
            bounded_text(&row.category),
            bounded_text(&row.tool),
            bounded_text(&row.command_family)
        ));
        append_opportunity_fields(output, &row.opportunity);
    }
    append_omitted(output, report.rows.len());
}

fn render_stuck_table(output: &mut String, report: &StuckReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    if report.rows.is_empty() {
        output.push_str("No stuck edit or failure loops.\n");
        return;
    }
    for row in report.rows.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!("- {}\n", bounded_text(&row.path)));
        append_field(
            output,
            "session",
            row.session_id.as_deref().map(str::to_owned),
        );
        output.push_str(&format!("  sequence: {}\n", bounded_list(&row.sequence)));
        output.push_str(&format!(
            "  observed commands: {}\n",
            bounded_list(&row.observed_commands)
        ));
        append_opportunity_fields(output, &row.opportunity);
    }
    append_omitted(output, report.rows.len());
}

fn render_prompts_table(output: &mut String, report: &PromptReport) {
    output.push_str(&format!("Measure: {}\n", report.measure));
    if report.rows.is_empty() {
        output.push_str("No user prompts.\n");
        return;
    }
    for row in report.rows.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!(
            "- {} [{}] ({} occurrences, {} sessions)\n  verdict: {}\n",
            row.class.as_str(),
            row.scope,
            row.occurrences,
            row.distinct_sessions,
            row.verdict
        ));
        append_evidence(output, &row.evidence);
        append_limitations(output, &row.limitations);
    }
    append_omitted(output, report.rows.len());
}

fn append_opportunity_table(output: &mut String, opportunity: &ViewOpportunity) {
    output.push_str(&format!("- {}\n", bounded_text(&opportunity.title)));
    append_opportunity_fields(output, opportunity);
}

fn append_opportunity_fields(output: &mut String, opportunity: &ViewOpportunity) {
    output.push_str(&format!(
        "  scope: {}\n  owner: {}\n  target: {}\n  impact: {}\n  severity: {}\n  confidence: {}\n  counts: {} occurrences, {} sessions\n  action: {}\n  follow-up: {}\n",
        opportunity.scope,
        bounded_text(&opportunity.owner),
        bounded_text(&opportunity.target),
        bounded_text(&opportunity.impact),
        opportunity.severity.as_str(),
        opportunity.confidence.as_str(),
        opportunity.occurrences,
        opportunity.distinct_sessions,
        bounded_text(&opportunity.action),
        bounded_text(&opportunity.follow_up),
    ));
    append_evidence(output, &opportunity.evidence);
    append_limitations(output, &opportunity.limitations);
}

fn append_field(output: &mut String, name: &str, value: Option<String>) {
    if let Some(value) = value {
        output.push_str(&format!("  {name}: {value}\n"));
    }
}

fn append_evidence(output: &mut String, evidence: &[codexlens::analysis::EvidenceRef]) {
    for evidence in evidence.iter().take(3) {
        let source = format_source(&evidence.source.path, evidence.source.line);
        let excerpt = evidence
            .excerpt
            .as_deref()
            .map(bounded_text)
            .unwrap_or_else(|| "(no excerpt)".to_owned());
        output.push_str(&format!("  evidence: {source} — {excerpt}\n"));
    }
}

fn append_limitations(output: &mut String, limitations: &[String]) {
    for limitation in limitations {
        output.push_str(&format!("  limitation: {}\n", bounded_text(limitation)));
    }
}

fn append_omitted(output: &mut String, total: usize) {
    if total > CLI_VIEW_ROW_LIMIT {
        output.push_str(&format!(
            "Omitted {} additional rows.\n",
            total - CLI_VIEW_ROW_LIMIT
        ));
    }
}

fn optional_number(value: Option<usize>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn bounded_list(values: &[String]) -> String {
    values
        .iter()
        .take(3)
        .map(|value| bounded_text(value))
        .collect::<Vec<_>>()
        .join("; ")
}

fn format_source(path: &Path, line: Option<usize>) -> String {
    line.map_or_else(
        || bounded_path(path),
        |line| format!("{}:{line}", bounded_path(path)),
    )
}

fn render_view_markdown(
    kind: ViewKind,
    report: &ViewReport,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    scope: &ScopeFilter,
    period: Option<&PeriodCoverage>,
) -> String {
    let (table, _) = render_view_table(kind, report, freshness, coverage, scope, period);
    let mut lines = table.lines();
    let heading = lines.next().unwrap_or(view_heading(kind));
    format!("# {heading}\n\n{}\n", lines.collect::<Vec<_>>().join("\n"))
}

fn print_view_json(
    kind: ViewKind,
    report: &ViewReport,
    data: &CanonicalData,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    options: &StoreOptions,
    period: Option<&PeriodCoverage>,
) -> Result<()> {
    let document = serde_json::json!({
        "schema_version": 1,
        "command": kind.command(),
        "scope": scope_filter_json(&options.scope),
        "coverage": cli_coverage_json(data, coverage, options, period),
        "freshness": freshness_json(freshness),
        "data": view_report_json(report),
    });
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}

fn cli_coverage_json(
    data: &CanonicalData,
    coverage: &ReportCoverage,
    options: &StoreOptions,
    period: Option<&PeriodCoverage>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "session_count": coverage.session_count,
        "included_session_count": report_sessions(data).len(),
        "record_count": coverage.record_count,
        "archived_included": options.include_archived,
        "subagents_included": options.include_subagents,
        "status": coverage.status,
        "activity_start": coverage.activity_start,
        "activity_end": coverage.activity_end,
        "valid_activity_timestamps": coverage.valid_activity_timestamps,
        "missing_activity_timestamps": coverage.missing_activity_timestamps,
        "invalid_activity_timestamps": coverage.invalid_activity_timestamps,
    });
    let limitations = coverage_limitations_json(coverage);
    value["limitations"] = limitations["limitations"].clone();
    value["limitations_omitted"] = limitations["limitations_omitted"].clone();
    if let Some(period) = period {
        value["requested_start"] = serde_json::json!(period.requested_start);
        value["requested_end"] = serde_json::json!(period.requested_end);
        value["included_records"] = serde_json::json!(period.included_records);
        value["excluded_records"] = serde_json::json!(period.excluded_records);
        value["unknown_timestamp_records"] = serde_json::json!(period.unknown_timestamp_records);
        value["unknown_timestamp_events"] = serde_json::json!(period.unknown_timestamp_events);
        value["period_status"] = serde_json::json!(period.state.as_str());
    }
    value
}

fn scope_filter_json(scope: &ScopeFilter) -> serde_json::Value {
    match scope {
        ScopeFilter::All => serde_json::json!({"kind": "all"}),
        ScopeFilter::Global => serde_json::json!({"kind": "global"}),
        ScopeFilter::Projects => serde_json::json!({"kind": "project"}),
        ScopeFilter::Project(path) => serde_json::json!({
            "kind": "project",
            "value": bounded_path(path),
        }),
    }
}

fn finding_scope_json(scope: &FindingScope) -> serde_json::Value {
    match scope {
        FindingScope::Global => serde_json::json!({"kind": "global"}),
        FindingScope::Project(path) => serde_json::json!({
            "kind": "project",
            "value": bounded_path(path),
        }),
        FindingScope::Instruction(path) => serde_json::json!({
            "kind": "instruction",
            "value": bounded_path(path),
        }),
        FindingScope::Path(path) => serde_json::json!({
            "kind": "path",
            "value": bounded_text(path),
        }),
    }
}

fn freshness_json(freshness: &StoreFreshness) -> serde_json::Value {
    serde_json::json!({
        "state": match freshness.state {
            codexlens::store::FreshnessState::Empty => "empty",
            codexlens::store::FreshnessState::Recorded => "recorded",
        },
        "source_count": freshness.source_count,
        "latest_ingested_at": freshness.latest_ingested_at,
    })
}

fn evidence_json(evidence: &codexlens::analysis::EvidenceRef) -> serde_json::Value {
    serde_json::json!({
        "session_id": evidence.session_id.as_deref().map(bounded_text),
        "source": {
            "kind": match evidence.source.kind {
                codexlens::model::SourceKind::Rollout => "rollout",
                codexlens::model::SourceKind::State => "state",
            },
            "path": bounded_path(&evidence.source.path),
            "line": evidence.source.line,
            "ingested_at": evidence.source.ingested_at,
            "parser_schema_version": evidence.source.parser_schema_version,
        },
        "role": evidence_role_name(&evidence.role),
        "excerpt": evidence.excerpt.as_deref().map(bounded_text),
    })
}

fn evidence_role_name(role: &codexlens::analysis::EvidenceRole) -> &'static str {
    match role {
        codexlens::analysis::EvidenceRole::Observation => "observation",
        codexlens::analysis::EvidenceRole::PrecedingAction => "preceding_action",
        codexlens::analysis::EvidenceRole::FileOperation => "file_operation",
        codexlens::analysis::EvidenceRole::VerificationCommand => "verification_command",
        codexlens::analysis::EvidenceRole::InstructionSnapshot => "instruction_snapshot",
        codexlens::analysis::EvidenceRole::InstructionFile => "instruction_file",
    }
}

fn opportunity_json(opportunity: &ViewOpportunity) -> serde_json::Value {
    serde_json::json!({
        "id": bounded_text(&opportunity.id),
        "title": bounded_text(&opportunity.title),
        "scope": finding_scope_json(&opportunity.scope),
        "owner": bounded_text(&opportunity.owner),
        "target": bounded_text(&opportunity.target),
        "impact": bounded_text(&opportunity.impact),
        "severity": opportunity.severity.as_str(),
        "confidence": opportunity.confidence.as_str(),
        "occurrences": opportunity.occurrences,
        "distinct_sessions": opportunity.distinct_sessions,
        "action": bounded_text(&opportunity.action),
        "follow_up": bounded_text(&opportunity.follow_up),
        "evidence": opportunity.evidence.iter().take(3).map(evidence_json).collect::<Vec<_>>(),
        "limitations": opportunity
            .limitations
            .iter()
            .map(|value| bounded_text(value))
            .collect::<Vec<_>>(),
    })
}

fn view_report_json(report: &ViewReport) -> serde_json::Value {
    match report {
        ViewReport::Inventory(report) => serde_json::json!({
            "measure": report.measure,
            "rows": report.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(inventory_row_json).collect::<Vec<_>>(),
            "omitted_count": report.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
        ViewReport::Overhead(report) => serde_json::json!({
            "measure": report.measure,
            "rows": report.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(overhead_row_json).collect::<Vec<_>>(),
            "omitted_count": report.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
        ViewReport::Usage(report) => serde_json::json!({
            "measure": report.measure,
            "coverage": usage_coverage_json(&report.coverage),
            "rows": report.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(usage_row_json).collect::<Vec<_>>(),
            "omitted_count": report.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
        ViewReport::Waste(report) => serde_json::json!({
            "measure": report.measure,
            "opportunities": report.opportunities.iter().take(CLI_VIEW_ROW_LIMIT).map(opportunity_json).collect::<Vec<_>>(),
            "omitted_count": report.opportunities.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
        ViewReport::Failures(report) => serde_json::json!({
            "measure": report.measure,
            "rows": report.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(|row| serde_json::json!({
                "category": bounded_text(&row.category),
                "tool": bounded_text(&row.tool),
                "command_family": bounded_text(&row.command_family),
                "opportunity": opportunity_json(&row.opportunity),
            })).collect::<Vec<_>>(),
            "omitted_count": report.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
        ViewReport::Stuck(report) => serde_json::json!({
            "measure": report.measure,
            "rows": report.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(|row| serde_json::json!({
                "path": bounded_text(&row.path),
                "session_id": row.session_id.as_deref().map(bounded_text),
                "sequence": row.sequence.iter().take(10).map(|value| bounded_text(value)).collect::<Vec<_>>(),
                "observed_commands": row.observed_commands.iter().take(10).map(|value| bounded_text(value)).collect::<Vec<_>>(),
                "opportunity": opportunity_json(&row.opportunity),
            })).collect::<Vec<_>>(),
            "omitted_count": report.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
        ViewReport::Prompts(report) => serde_json::json!({
            "measure": report.measure,
            "rows": report.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(|row| serde_json::json!({
                "class": row.class.as_str(),
                "scope": finding_scope_json(&row.scope),
                "occurrences": row.occurrences,
                "distinct_sessions": row.distinct_sessions,
                "verdict": bounded_text(&row.verdict),
                "evidence": row.evidence.iter().take(3).map(evidence_json).collect::<Vec<_>>(),
                "limitations": row.limitations.iter().map(|value| bounded_text(value)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "omitted_count": report.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
        }),
    }
}

fn inventory_row_json(row: &codexlens::analysis::views::InventoryRow) -> serde_json::Value {
    serde_json::json!({
        "id": bounded_text(&row.id),
        "scope": finding_scope_json(&row.scope),
        "owner": bounded_text(&row.owner),
        "kind": row.kind.as_str(),
        "name": bounded_text(&row.name),
        "path": row.path.as_deref().map(bounded_path),
        "load_mode": row.load_mode.as_str(),
        "static_bytes": row.static_bytes,
        "startup_bytes": row.startup_bytes,
        "usage_state": row.usage_state.as_str(),
        "observed_uses": row.observed_uses,
        "observed_sessions": row.observed_sessions,
        "action": row.action.as_deref().map(bounded_text),
        "evidence": row.evidence.iter().take(3).map(evidence_json).collect::<Vec<_>>(),
        "limitations": row.limitations.iter().map(|value| bounded_text(value)).collect::<Vec<_>>(),
    })
}

fn overhead_row_json(row: &codexlens::analysis::views::OverheadRow) -> serde_json::Value {
    serde_json::json!({
        "scope": finding_scope_json(&row.scope),
        "project": row.project.as_deref().map(bounded_path),
        "session_count": row.session_count,
        "observed_min_startup_bytes": row.observed_min_startup_bytes,
        "readable_startup_bytes": row.readable_startup_bytes,
        "residual_bytes": row.residual_bytes,
        "residual_source": if row.residual_bytes.is_some() { "system_or_tool" } else { "unknown" },
        "cost_control": if row.unknown_cost { "unknown" } else { "user_controlled" },
        "unknown_cost": row.unknown_cost,
        "evidence": row.evidence.iter().take(3).map(evidence_json).collect::<Vec<_>>(),
        "limitations": row.limitations.iter().map(|value| bounded_text(value)).collect::<Vec<_>>(),
    })
}

fn usage_coverage_json(coverage: &codexlens::analysis::views::UsageCoverage) -> serde_json::Value {
    serde_json::json!({
        "total_sessions": coverage.total_sessions,
        "observed_sessions": coverage.observed_sessions,
        "known_events": coverage.known_events,
        "total_events": coverage.total_events,
        "status": coverage.status,
    })
}

fn usage_row_json(row: &codexlens::analysis::views::UsageRow) -> serde_json::Value {
    serde_json::json!({
        "kind": row.kind.as_str(),
        "name": bounded_text(&row.name),
        "scope": finding_scope_json(&row.scope),
        "usage_state": row.usage_state.map(|state| state.as_str()),
        "occurrences": row.occurrences,
        "distinct_sessions": row.distinct_sessions,
        "input_tokens": row.input_tokens,
        "cached_input_tokens": row.cached_input_tokens,
        "output_tokens": row.output_tokens,
        "reasoning_output_tokens": row.reasoning_output_tokens,
        "duration_ms": row.duration_ms,
        "duration_observations": row.duration_observations,
        "coverage": usage_coverage_json(&row.coverage),
        "evidence": row.evidence.iter().take(3).map(evidence_json).collect::<Vec<_>>(),
        "limitations": row.limitations.iter().map(|value| bounded_text(value)).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone)]
struct ReportingSelection {
    coverage: PeriodCoverage,
    report_coverage: ReportCoverage,
    period: ReportingPeriod,
    selection_options: SessionSelectionOptions,
    source_data: CanonicalData,
}

fn coverage_for_scope(
    data: &CanonicalData,
    selection: Option<&ReportingSelection>,
    scope: &ScopeFilter,
) -> ReportCoverage {
    if let Some(selection) = selection {
        if matches!(scope, ScopeFilter::All | ScopeFilter::Global) {
            return selection.report_coverage.clone();
        }
        let (mut scoped_coverage, _) = period_report_coverage(selection, scope);
        if scoped_coverage.session_count == 0 && selection.report_coverage.status != "empty" {
            scoped_coverage.status = "partial".to_owned();
            scoped_coverage.limitations.push(CoverageLimitation {
                kind: "scope_selection".to_owned(),
                source: codexlens::model::SourceRef::state(PathBuf::from("<scope>")),
                message: "No selected project session was available for this scope".to_owned(),
                selected_sessions: 0,
                selected_records: 0,
                affected_lenses: vec!["all".to_owned()],
            });
        }
        return scoped_coverage;
    }
    if matches!(scope, ScopeFilter::All | ScopeFilter::Global) {
        return report_coverage(data);
    }

    let scoped = data_for_scope(data, scope);
    let mut scoped_coverage = report_coverage(&scoped);
    if scoped.sessions.is_empty() && scoped_coverage.status != "empty" {
        scoped_coverage.status = "partial".to_owned();
        scoped_coverage.limitations.push(CoverageLimitation {
            kind: "scope_selection".to_owned(),
            source: codexlens::model::SourceRef::state(PathBuf::from("<scope>")),
            message: "No selected project session was available for this scope".to_owned(),
            selected_sessions: 0,
            selected_records: 0,
            affected_lenses: vec!["all".to_owned()],
        });
    }
    scoped_coverage
}

fn data_for_scope<'a>(data: &'a CanonicalData, scope: &ScopeFilter) -> Cow<'a, CanonicalData> {
    if data.surfaces.is_empty() && matches!(scope, ScopeFilter::All | ScopeFilter::Global) {
        return Cow::Borrowed(data);
    }
    let session_ids = data
        .sessions
        .iter()
        .filter(|session| session_matches_filter(data, &session.id, scope))
        .map(|session| session.id.clone())
        .collect::<BTreeSet<_>>();
    let mut scoped = data.clone();
    scoped
        .sessions
        .retain(|session| session_ids.contains(&session.id));
    scoped.turns.retain(|turn| {
        turn.session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.records.retain(|record| {
        record
            .session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.messages.retain(|message| {
        message
            .session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.tool_calls.retain(|call| {
        call.session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.tool_results.retain(|result| {
        result
            .session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.file_operations.retain(|operation| {
        operation
            .session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.token_usage.retain(|usage| {
        usage
            .session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped.instruction_snapshots.retain(|snapshot| {
        snapshot
            .session_id
            .as_ref()
            .is_some_and(|id| session_ids.contains(id))
    });
    scoped
        .instruction_joins
        .retain(|join| session_ids.contains(&join.session_id));
    scoped.diagnostics.retain(|diagnostic| {
        diagnostic
            .session_id
            .as_ref()
            .is_none_or(|id| session_ids.contains(id))
    });
    codexlens::config::recompute_surface_usage(&mut scoped);
    Cow::Owned(scoped)
}

fn period_for_scope(selection: &ReportingSelection, scope: &ScopeFilter) -> PeriodCoverage {
    if matches!(scope, ScopeFilter::All | ScopeFilter::Global) {
        return selection.coverage.clone();
    }
    period_report_coverage(selection, scope).1
}

fn period_report_coverage(
    selection: &ReportingSelection,
    scope: &ScopeFilter,
) -> (ReportCoverage, PeriodCoverage) {
    period_report_coverage_from_data(
        &selection.source_data,
        &selection.period,
        &selection.selection_options,
        scope,
    )
}

fn period_report_coverage_from_data(
    data: &CanonicalData,
    period: &ReportingPeriod,
    selection_options: &SessionSelectionOptions,
    scope: &ScopeFilter,
) -> (ReportCoverage, PeriodCoverage) {
    let scoped_source = data_for_scope(data, scope);
    let selected = select_report_data_with_options(&scoped_source, Some(period), selection_options);
    let eligible_data = select_eligible_session_data(&scoped_source, selection_options);
    let mut report_coverage =
        report_coverage_for_period_selection(&eligible_data, period, &selected.data);
    report_coverage.session_count = selected.coverage.included_sessions;
    report_coverage.record_count = selected.coverage.included_records;
    if matches!(selected.coverage.state, PeriodCoverageState::Empty) {
        report_coverage.status = "empty".to_owned();
    }
    (report_coverage, selected.coverage)
}

fn session_matches_filter(data: &CanonicalData, session_id: &str, filter: &ScopeFilter) -> bool {
    let project = data
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.project.as_deref().or(session.cwd.as_deref()))
        .filter(|path| Path::new(path).is_absolute() || Path::new(path).has_root())
        .map(PathBuf::from);
    match filter {
        ScopeFilter::Projects => project.is_some(),
        ScopeFilter::Project(root) => project
            .as_deref()
            .is_some_and(|path| scope_path_matches(root, path)),
        ScopeFilter::All | ScopeFilter::Global => true,
    }
}

fn load_reporting(
    options: &StoreOptions,
) -> Result<(CanonicalData, StoreFreshness, Option<ReportingSelection>)> {
    let period = ReportingPeriod::from_bounds(options.since.as_deref(), options.until.as_deref())
        .map_err(anyhow::Error::new)?;
    let selection_options = session_selection_options(options)?;
    let store_path = options.store_path()?;
    let (data, freshness) = load_store(&store_path)?;
    let selected = select_report_data_with_options(&data, period.as_ref(), &selection_options);
    let Some(period) = period else {
        return Ok((selected.data, freshness, None));
    };
    let source_data = data.clone();
    let (report_coverage, _) = period_report_coverage_from_data(
        &source_data,
        &period,
        &selection_options,
        &ScopeFilter::All,
    );
    Ok((
        selected.data,
        freshness,
        Some(ReportingSelection {
            coverage: selected.coverage,
            report_coverage,
            period,
            selection_options,
            source_data,
        }),
    ))
}

fn validate_reporting_options(options: &StoreOptions) -> Result<()> {
    ReportingPeriod::from_bounds(options.since.as_deref(), options.until.as_deref())
        .map_err(anyhow::Error::new)?;
    session_selection_options(options)?;
    Ok(())
}

fn report_auto_refresh(outcome: &RefreshOutcome, store: &Path) {
    eprintln!("Refreshed store: {}", bounded_display(store));
    for diagnostic in &outcome.discovery.diagnostics {
        eprintln!(
            "Discovery diagnostic {}: {}",
            bounded_display(&diagnostic.path),
            bounded_text(&diagnostic.message)
        );
    }
    for file in &outcome.report.files {
        let status = if file.skipped { "skipped" } else { "ingested" };
        eprintln!(
            "- {}: {status} ({} sessions, {} records, {} diagnostics)",
            bounded_display(&file.source),
            file.sessions,
            file.records,
            file.diagnostics
        );
    }
    eprintln!("Store freshness: {}", outcome.freshness);
}

fn session_selection_options(options: &StoreOptions) -> Result<SessionSelectionOptions> {
    if options.days.is_some() && (options.since.is_some() || options.until.is_some()) {
        bail!("--days cannot be combined with --since or --until");
    }
    Ok(SessionSelectionOptions {
        days: options.days.unwrap_or(DEFAULT_SESSION_DAYS),
        include_archived: options.include_archived,
        include_subagents: options.include_subagents,
    })
}

fn load_store(path: &Path) -> Result<(CanonicalData, StoreFreshness)> {
    let store_display = bounded_display(path);
    if !path.is_file() {
        bail!("store does not exist: {store_display}");
    }
    let schema_version = Store::read_schema_version(path).with_context(|| {
        format!(
            "failed to inspect derived store {}; provide a valid SQLite store",
            store_display
        )
    })?;
    let mut migrated_copy = None;
    match schema_version {
        SCHEMA_VERSION => {}
        version if (1..SCHEMA_VERSION).contains(&version) => {
            let copy = TemporaryStoreCopy::create(path).with_context(|| {
                format!(
                    "failed to prepare a temporary copy of legacy derived store {}",
                    store_display
                )
            })?;
            let migrated = Store::open(copy.path()).with_context(|| {
                format!("failed to migrate legacy derived store {}", store_display)
            })?;
            drop(migrated);
            migrated_copy = Some(copy);
        }
        version => bail!(
            "store schema version {version} is not supported for reporting (expected {SCHEMA_VERSION})"
        ),
    }
    let report_path = migrated_copy
        .as_ref()
        .map(TemporaryStoreCopy::path)
        .unwrap_or(path);
    let store = Store::open_read_only(report_path).with_context(|| {
        format!(
            "failed to open derived store {}; provide a valid SQLite store",
            store_display
        )
    })?;
    let data = store
        .load_canonical()
        .with_context(|| format!("failed to load derived store {store_display}"))?;
    let freshness = store
        .freshness()
        .with_context(|| format!("failed to read freshness for {store_display}"))?;
    Ok((data, freshness))
}

fn run_finding_report(
    options: &StoreOptions,
    lens: fn(&CanonicalData) -> Vec<Finding>,
    command: &str,
) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = coverage_for_scope(&data, selection.as_ref(), &options.scope);
    let scoped_data = data_for_scope(&data, &options.scope);
    let period = selection
        .as_ref()
        .map(|selection| period_for_scope(selection, &options.scope));
    let findings = lens(&scoped_data)
        .into_iter()
        .filter(|finding| scope_matches(&options.scope, &finding.scope))
        .collect::<Vec<_>>();
    let mut report = doctor_with_coverage(
        &scoped_data,
        &findings,
        freshness,
        &DoctorOptions {
            max_findings_per_scope: Some(CLI_VIEW_ROW_LIMIT),
            ..DoctorOptions::default()
        },
        &coverage,
    );
    if let Some(period) = period.as_ref() {
        report.period_start = period.observed_start.clone();
        report.period_end = period.observed_end.clone();
    }
    if let Some(period) = period.as_ref() {
        options.format.write_report(
            command,
            || {
                (
                    render_doctor_with_period(&report, &coverage, period),
                    String::new(),
                )
            },
            || render_json_finding_report_with_period(command, &report, &coverage, period),
        )
    } else {
        options.format.write_report(
            command,
            || {
                (
                    render_doctor_with_coverage(&report, &coverage),
                    String::new(),
                )
            },
            || render_json_finding_report_with_coverage(command, &report, &coverage),
        )
    }
}

fn run_optimize_print(
    data: &CanonicalData,
    findings: &[Finding],
    waste: &WasteReport,
    proposal_plan: &codexlens::advisor::ProposalPlan,
    freshness: &StoreFreshness,
    options: &StoreOptions,
    selection: Option<&ReportingSelection>,
) -> Result<()> {
    let coverage = coverage_for_scope(data, selection, &options.scope);
    let period = selection.map(|selection| period_for_scope(selection, &options.scope));
    let batch = proposal_batch(proposal_plan);
    options.format.write_report(
        "OPTIMIZE",
        || {
            (
                render_optimize_briefing_human(
                    data,
                    findings,
                    waste,
                    &batch,
                    freshness,
                    &coverage,
                    options,
                    period.as_ref(),
                ),
                String::new(),
            )
        },
        || {
            render_optimize_briefing_json(
                data,
                findings,
                waste,
                &batch,
                freshness,
                &coverage,
                options,
                period.as_ref(),
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn render_optimize_briefing_human(
    data: &CanonicalData,
    findings: &[Finding],
    waste: &WasteReport,
    batch: &DiffBatch,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    options: &StoreOptions,
    period: Option<&PeriodCoverage>,
) -> String {
    let mut output = format!(
        "OPTIMIZATION BRIEFING\nScope: {}\nStore freshness: {}\nCoverage: {} ({} sessions)\n",
        scope_label(&options.scope),
        freshness,
        coverage.status,
        coverage.session_count,
    );
    append_period_metadata(&mut output, period);
    output.push_str(&render_coverage_limitations(coverage));

    output.push_str("\nFINDINGS\n");
    if findings.is_empty() {
        output.push_str("No selected findings were observed.\n");
    } else {
        for finding in findings.iter().take(CLI_VIEW_ROW_LIMIT) {
            output.push_str(&format!(
                "- {}\n  problem: {}\n  impact: {} occurrences across {} sessions\n  owner: {}\n  target: {}\n  action: {}\n  follow-up: {}\n",
                finding.kind.as_str(),
                bounded_text(&finding.summary),
                finding.occurrences,
                finding.distinct_sessions,
                briefing_owner(&finding.scope),
                briefing_finding_target(data, finding),
                bounded_text(&finding.suggested_action),
                briefing_follow_up(data, finding),
            ));
            if !finding.affected_paths.is_empty() {
                output.push_str(&format!(
                    "  affected paths: {}\n",
                    bounded_list(&finding.affected_paths)
                ));
            }
            append_evidence(&mut output, &finding.evidence);
            append_limitations(&mut output, &finding.limitations);
        }
        append_omitted(&mut output, findings.len());
    }

    output.push_str("\nCONFIGURATION WASTE\n");
    if waste.opportunities.is_empty() {
        output.push_str("No actionable configuration waste was observed.\n");
    } else {
        for opportunity in waste.opportunities.iter().take(CLI_VIEW_ROW_LIMIT) {
            append_opportunity_table(&mut output, opportunity);
        }
        append_omitted(&mut output, waste.opportunities.len());
    }

    let overhead = codexlens::analysis::views::overhead(data);
    output.push_str("\nOVERHEAD\n");
    render_overhead_table(&mut output, &filter_overhead(overhead, &options.scope));

    output.push_str("\nREVIEWABLE PROPOSALS\n");
    if batch.rendered.is_empty() {
        output.push_str("No reviewable diffs were rendered.\n");
    } else {
        for rendered in batch.rendered.iter().take(CLI_VIEW_ROW_LIMIT) {
            output.push_str(&render_proposal_summary(rendered));
            output.push('\n');
        }
        append_omitted(&mut output, batch.rendered.len());
    }
    for skipped in batch.skipped.iter().take(CLI_VIEW_ROW_LIMIT) {
        output.push_str(&format!(
            "Skipped {}: {}\n",
            bounded_path(&skipped.target_path),
            bounded_text(&skipped.reason)
        ));
    }
    if batch.skipped.len() > CLI_VIEW_ROW_LIMIT {
        output.push_str(&format!(
            "Skipped {} additional proposal(s).\n",
            batch.skipped.len() - CLI_VIEW_ROW_LIMIT
        ));
    }

    output.push_str("\nNEXT WORKFLOW\n");
    output.push_str("1. Review the bounded evidence and targets above.\n");
    output.push_str("2. Run codexlens optimize --diff for the applicable scope.\n");
    output.push_str("3. Apply only an inspected plan with codexlens optimize --apply --yes.\n");
    output.push_str("4. Run the project's documented verification command.\n");
    output
}

#[allow(clippy::too_many_arguments)]
fn render_optimize_briefing_json(
    data: &CanonicalData,
    findings: &[Finding],
    waste: &WasteReport,
    batch: &DiffBatch,
    freshness: &StoreFreshness,
    coverage: &ReportCoverage,
    options: &StoreOptions,
    period: Option<&PeriodCoverage>,
) -> Result<String, serde_json::Error> {
    let overhead = filter_overhead(codexlens::analysis::views::overhead(data), &options.scope);
    let findings = findings
        .iter()
        .take(CLI_VIEW_ROW_LIMIT)
        .map(|finding| briefing_finding_json(data, finding))
        .collect::<Vec<_>>();
    let safe_diff_document: serde_json::Value = serde_json::from_str(&render_json_diff(batch)?)?;
    let proposals = safe_diff_document["data"].clone();
    let limitations = coverage
        .limitations
        .iter()
        .take(CLI_VIEW_ROW_LIMIT)
        .map(|limitation| bounded_text(&limitation.message))
        .chain(
            batch
                .skipped
                .iter()
                .take(CLI_VIEW_ROW_LIMIT)
                .map(|skipped| bounded_text(&skipped.reason)),
        )
        .chain(
            proposals["skipped"]
                .as_array()
                .into_iter()
                .flatten()
                .take(CLI_VIEW_ROW_LIMIT)
                .filter_map(|skipped| skipped["reason"].as_str().map(str::to_owned)),
        )
        .collect::<Vec<_>>();
    let document = serde_json::json!({
        "schema_version": 1,
        "command": "optimize",
        "scope": scope_filter_json(&options.scope),
        "coverage": cli_coverage_json(data, coverage, options, period),
        "freshness": freshness_json(freshness),
        "data": {
            "findings": findings,
            "configuration_waste": waste.opportunities.iter().take(CLI_VIEW_ROW_LIMIT).map(opportunity_json).collect::<Vec<_>>(),
            "overhead": overhead.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(overhead_row_json).collect::<Vec<_>>(),
            "proposals": proposals,
            "next_steps": [
                "Review the bounded evidence and targets above",
                "Run codexlens optimize --diff for the applicable scope",
                "Apply only an inspected plan with codexlens optimize --apply --yes",
                "Run the project's documented verification command",
            ],
            "limitations": limitations,
        },
    });
    let mut output = serde_json::to_string_pretty(&document)?;
    output.push('\n');
    Ok(output)
}

fn filter_overhead(mut report: OverheadReport, scope: &ScopeFilter) -> OverheadReport {
    report.rows.retain(|row| scope_matches(scope, &row.scope));
    report
}

fn briefing_finding_target(data: &CanonicalData, finding: &Finding) -> String {
    recommend_scope(data, finding)
        .map(|recommendation| bounded_path(&recommendation.target_path))
        .or_else(|| {
            finding
                .affected_paths
                .first()
                .map(|path| bounded_text(path))
        })
        .unwrap_or_else(|| match &finding.scope {
            FindingScope::Global => "AGENTS.md".to_owned(),
            FindingScope::Project(path) | FindingScope::Instruction(path) => bounded_path(path),
            FindingScope::Path(path) => bounded_text(path),
        })
}

fn briefing_owner(scope: &FindingScope) -> String {
    match scope {
        FindingScope::Global => "global instruction owner".to_owned(),
        FindingScope::Project(path) | FindingScope::Instruction(path) => bounded_path(path),
        FindingScope::Path(path) => bounded_text(path),
    }
}

fn briefing_follow_up(data: &CanonicalData, finding: &Finding) -> String {
    codexlens::analysis::views::follow_up_for_finding(data, finding)
}

fn briefing_finding_json(data: &CanonicalData, finding: &Finding) -> serde_json::Value {
    serde_json::json!({
        "kind": finding.kind.as_str(),
        "severity": finding.severity.as_str(),
        "confidence": finding.confidence.as_str(),
        "scope": finding_scope_json(&finding.scope),
        "key": bounded_text(&finding.key),
        "problem": bounded_text(&finding.summary),
        "impact": format!("{} occurrences across {} sessions", finding.occurrences, finding.distinct_sessions),
        "occurrences": finding.occurrences,
        "distinct_sessions": finding.distinct_sessions,
        "owner": briefing_owner(&finding.scope),
        "target": briefing_finding_target(data, finding),
        "affected_paths": finding
            .affected_paths
            .iter()
            .take(3)
            .map(|path| bounded_text(path))
            .collect::<Vec<_>>(),
        "evidence": finding.evidence.iter().take(3).map(evidence_json).collect::<Vec<_>>(),
        "action": bounded_text(&finding.suggested_action),
        "follow_up": briefing_follow_up(data, finding),
        "limitations": finding.limitations.iter().take(3).map(|value| bounded_text(value)).collect::<Vec<_>>(),
    })
}

fn render_optimize_human(batch: &DiffBatch) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    for rendered in batch.rendered.iter().take(CLI_VIEW_ROW_LIMIT) {
        stdout.push_str(&render_proposal_summary(rendered));
        stdout.push('\n');
    }
    append_omitted(&mut stdout, batch.rendered.len());
    for skipped in batch.skipped.iter().take(CLI_VIEW_ROW_LIMIT) {
        stderr.push_str(&format!(
            "Skipped {}: {}\n",
            bounded_path(&skipped.target_path),
            bounded_text(&skipped.reason)
        ));
    }
    if batch.skipped.len() > CLI_VIEW_ROW_LIMIT {
        stderr.push_str(&format!(
            "Skipped {} additional proposal(s).\n",
            batch.skipped.len() - CLI_VIEW_ROW_LIMIT
        ));
    }
    if batch.rendered.is_empty() && batch.skipped.is_empty() {
        stdout.push_str("No applicable proposals.\n");
    }
    (stdout, stderr)
}

fn render_optimize_human_with_period(
    batch: &DiffBatch,
    coverage: &codexlens::advisor::ReportCoverage,
    freshness: &StoreFreshness,
    period: &PeriodCoverage,
) -> (String, String) {
    let (mut stdout, stderr) = render_optimize_human(batch);
    let mut metadata = render_report_metadata_with_period(coverage, freshness, period);
    metadata.push_str(&stdout);
    stdout = metadata;
    (stdout, stderr)
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, OutputFormat, session_selection_options};
    use clap::Parser;

    #[test]
    fn refresh_accepts_input_and_store_options() {
        let cli = Cli::try_parse_from([
            "codexlens",
            "refresh",
            "--codex-home",
            "/synthetic/codex",
            "--include-archived",
            "--config",
            "/synthetic/config.toml",
            "--store",
            "/synthetic/store.sqlite",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Refresh { .. }));
    }

    #[test]
    fn rediscovery_is_an_alias_for_knowledge() {
        let cli = Cli::try_parse_from(["codexlens", "rediscovery"]).unwrap();
        assert!(matches!(cli.command, Command::Knowledge { .. }));
    }

    #[test]
    fn reporting_commands_accept_store_options() {
        for command in [
            "analyze",
            "sessions",
            "sql",
            "failures",
            "corrections",
            "rework",
            "verification",
            "knowledge",
            "instructions",
            "monitor",
        ] {
            let args = if command == "monitor" {
                vec![
                    "codexlens",
                    command,
                    "--source",
                    "fixture.jsonl",
                    "--store",
                    "fixture.sqlite",
                    "--max-polls",
                    "1",
                ]
            } else {
                vec!["codexlens", command, "--store", "fixture.sqlite"]
            };
            Cli::try_parse_from(args).unwrap();
        }
        Cli::try_parse_from(["codexlens", "doctor", "--frozen"]).unwrap();
    }

    #[test]
    fn ingestion_commands_keep_workflow_specific_options() {
        assert!(Cli::try_parse_from(["codexlens", "refresh", "--format", "json"]).is_err());
        assert!(
            Cli::try_parse_from([
                "codexlens",
                "monitor",
                "--source",
                "fixture.jsonl",
                "--scope",
                "project"
            ])
            .is_err()
        );
    }

    #[test]
    fn reporting_commands_accept_session_selection_options() {
        let Command::Sessions { store } = Cli::try_parse_from([
            "codexlens",
            "sessions",
            "--include-archived",
            "--include-subagents",
            "--days",
            "7",
        ])
        .unwrap()
        .command
        else {
            panic!("expected sessions command");
        };
        assert!(store.include_archived);
        assert!(store.include_subagents);
        assert_eq!(store.days, Some(7));
        let Command::Sessions { store } = Cli::try_parse_from([
            "codexlens",
            "sessions",
            "--days",
            "7",
            "--since",
            "2026-01-01T00:00:00Z",
        ])
        .unwrap()
        .command
        else {
            panic!("expected sessions command");
        };
        assert!(session_selection_options(&store).is_err());
    }

    #[test]
    fn reporting_commands_accept_json_format() {
        let cli = Cli::try_parse_from(["codexlens", "analyze", "--format", "json"]).unwrap();
        let Command::Analyze { store } = cli.command else {
            panic!("expected analyze command");
        };
        assert_eq!(store.format, OutputFormat::Json);
    }
}
