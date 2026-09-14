use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rusqlite::types::ValueRef;

use codexlens::advisor::{
    ApplyPlan, ApplyReport, DiffBatch, DoctorOptions, ReportCoverage, doctor_with_coverage,
    prepare_apply_proposals, proposals_for_findings, render_diffs, render_doctor_with_coverage,
    render_doctor_with_period, render_json_diff, render_json_diff_with_period,
    render_json_finding_report_with_coverage, render_json_finding_report_with_period,
    render_proposal_summary, render_report_metadata_with_period, report_coverage,
    report_coverage_with_period, report_sessions,
};
use codexlens::analysis::views::{
    FailureReport, InventoryReport, OverheadReport, PromptReport, StuckReport, UsageReport,
    ViewOpportunity, WasteReport,
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
    Refresh {
        #[command(flatten)]
        refresh: RefreshOptions,
    },
    Analyze {
        #[command(flatten)]
        store: StoreOptions,
    },
    Sessions {
        #[command(flatten)]
        store: StoreOptions,
    },
    Inventory {
        #[command(flatten)]
        store: StoreOptions,
    },
    Overhead {
        #[command(flatten)]
        store: StoreOptions,
    },
    Usage {
        #[command(flatten)]
        store: StoreOptions,
    },
    Waste {
        #[command(flatten)]
        store: StoreOptions,
    },
    Failures {
        #[command(flatten)]
        store: StoreOptions,
    },
    Stuck {
        #[command(flatten)]
        store: StoreOptions,
    },
    Prompts {
        #[command(flatten)]
        store: StoreOptions,
    },
    Corrections {
        #[command(flatten)]
        store: StoreOptions,
    },
    Rework {
        #[command(flatten)]
        store: StoreOptions,
    },
    Verification {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(alias = "rediscovery")]
    Knowledge {
        #[command(flatten)]
        store: StoreOptions,
    },
    Instructions {
        #[command(flatten)]
        store: StoreOptions,
    },
    Doctor {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(long, value_name = "COUNT")]
        limit: Option<usize>,
    },
    Query {
        #[arg(value_name = "SQL")]
        sql: Option<String>,
        #[command(flatten)]
        options: QueryOptions,
    },
    Optimize {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(long, conflicts_with = "apply")]
        diff: bool,
        #[arg(long, conflicts_with = "diff")]
        apply: bool,
        #[arg(long, requires = "apply")]
        yes: bool,
        #[arg(long, conflicts_with_all = ["diff", "apply"])]
        print: bool,
    },
    Monitor {
        #[command(flatten)]
        store: MonitorStoreOptions,
        #[arg(long, value_name = "PATH")]
        source: PathBuf,
        #[arg(long, value_enum, default_value_t = MonitorKind::Rollout)]
        kind: MonitorKind,
        #[arg(long, value_name = "COUNT")]
        max_polls: Option<usize>,
        #[arg(long, default_value_t = 500, value_name = "MILLISECONDS")]
        interval_ms: u64,
        #[arg(long, value_name = "PATH")]
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
    #[arg(long, short = 's', value_name = "PATH")]
    store: Option<PathBuf>,
    #[arg(long, alias = "home", value_name = "PATH")]
    codex_home: Option<PathBuf>,
    #[arg(long)]
    include_archived: bool,
    #[arg(long)]
    include_subagents: bool,
    #[arg(
        long,
        default_value = "all",
        value_name = "global|project|project:PATH"
    )]
    scope: ScopeFilter,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
    #[arg(long, value_name = "DAYS")]
    days: Option<u64>,
    #[arg(
        long,
        help = "Read exactly this derived store without refreshing raw inputs"
    )]
    frozen: bool,
    #[arg(long, value_name = "RFC3339")]
    since: Option<String>,
    #[arg(long, value_name = "RFC3339")]
    until: Option<String>,
}

impl StoreOptions {
    fn store_path(&self) -> Result<PathBuf> {
        self.store.clone().map_or_else(default_store_path, Ok)
    }
}

#[derive(Debug, Clone, Args)]
struct QueryOptions {
    #[arg(long, short = 's', value_name = "PATH")]
    store: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
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
        value_name = "PATH"
    )]
    store: PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
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
        value_name = "PATH"
    )]
    store: PathBuf,
    #[arg(long, alias = "home", value_name = "PATH")]
    codex_home: Option<PathBuf>,
    #[arg(long)]
    include_archived: bool,
    #[arg(long)]
    include_subagents: bool,
    #[arg(long, value_name = "PATH")]
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
        Command::Analyze { store } => run_finding_report(&store, analyze_default, "analyze"),
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
        Command::Query { sql, options } => run_query(sql, &options),
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
            let (data, findings, freshness, selection) = load_analysis(&store)?;
            let findings = findings
                .into_iter()
                .filter(|finding| scope_matches(&store.scope, &finding.scope))
                .collect::<Vec<_>>();
            let proposal_plan = proposals_for_findings(&data, &findings);
            if diff || print {
                let batch = proposal_batch(&proposal_plan);
                if let Some(selection) = selection.as_ref() {
                    let coverage = selection.report_coverage.clone();
                    store.format.write_report(
                        "OPTIMIZE",
                        || {
                            render_optimize_human_with_period(
                                &batch,
                                &coverage,
                                &freshness,
                                &selection.coverage,
                            )
                        },
                        || {
                            render_json_diff_with_period(
                                &batch,
                                &freshness,
                                &coverage,
                                &selection.coverage,
                            )
                        },
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

fn run_query(sql: Option<String>, options: &QueryOptions) -> Result<()> {
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
    let result = execute_query(store.connection(), &sql)?;
    match options.format {
        OutputFormat::Table => print!("{}", render_query_table(&result)),
        OutputFormat::Markdown => print!("{}", render_query_markdown(&result)),
        OutputFormat::Json => {
            let document = serde_json::json!({
                "schema_version": 1,
                "command": "query",
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
    let mut statement = connection
        .prepare(sql)
        .map_err(|_| anyhow::anyhow!("query must contain one valid SQL statement"))?;
    if starts_with_sql_keyword(sql, "pragma") || !statement.readonly() {
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

fn render_query_table(result: &QueryResult) -> String {
    let mut output = String::from("QUERY\n");
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

fn render_query_markdown(result: &QueryResult) -> String {
    let mut output = String::from("# QUERY\n\n");
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

fn starts_with_sql_keyword(mut sql: &str, keyword: &str) -> bool {
    loop {
        sql = sql.trim_start();
        if let Some(comment) = sql.strip_prefix("--") {
            let Some(end) = comment.find('\n') else {
                return false;
            };
            sql = &comment[end + 1..];
            continue;
        }
        if let Some(comment) = sql.strip_prefix("/*") {
            let Some(end) = comment.find("*/") else {
                return false;
            };
            sql = &comment[end + 2..];
            continue;
        }
        let Some(prefix) = sql.get(..keyword.len()) else {
            return false;
        };
        if !prefix.eq_ignore_ascii_case(keyword) {
            return false;
        }
        return !sql
            .get(keyword.len()..)
            .and_then(|tail| tail.chars().next())
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_');
    }
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

fn load_analysis(
    options: &StoreOptions,
) -> Result<(
    CanonicalData,
    Vec<Finding>,
    StoreFreshness,
    Option<ReportingSelection>,
)> {
    let (data, freshness, selection) = load_reporting(options)?;
    let findings = analyze_default(&data);
    Ok((data, findings, freshness, selection))
}

fn run_view_report(options: &StoreOptions, kind: ViewKind) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = report_coverage_for_selection(&data, selection.as_ref());
    let period = selection.as_ref().map(|selection| &selection.coverage);
    let report = match kind {
        ViewKind::Inventory => ViewReport::Inventory(codexlens::analysis::views::inventory(&data)),
        ViewKind::Overhead => ViewReport::Overhead(codexlens::analysis::views::overhead(&data)),
        ViewKind::Usage => ViewReport::Usage(codexlens::analysis::views::usage(&data)),
        ViewKind::Waste => ViewReport::Waste(codexlens::analysis::views::waste(&data)),
        ViewKind::Failures => ViewReport::Failures(codexlens::analysis::views::failures(&data)),
        ViewKind::Stuck => ViewReport::Stuck(codexlens::analysis::views::stuck(&data)),
        ViewKind::Prompts => ViewReport::Prompts(codexlens::analysis::views::prompts(&data)),
    };
    let report = filter_view_report(report, &options.scope);
    match options.format {
        OutputFormat::Table => {
            let (stdout, stderr) =
                render_view_table(kind, &report, &freshness, &coverage, &options.scope, period);
            print!("{stdout}");
            eprint!("{stderr}");
            Ok(())
        }
        OutputFormat::Markdown => {
            print!(
                "{}",
                render_view_markdown(kind, &report, &freshness, &coverage, &options.scope, period,)
            );
            Ok(())
        }
        OutputFormat::Json => {
            print_view_json(kind, &report, &data, &freshness, &coverage, options, period)
        }
    }
}

fn run_sessions_report(options: &StoreOptions) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = report_coverage_for_selection(&data, selection.as_ref());
    let period = selection.as_ref().map(|selection| &selection.coverage);
    let sessions = report_sessions(&data)
        .into_iter()
        .filter(|session| session_scope_matches(&options.scope, session))
        .collect::<Vec<_>>();
    let omitted_count = sessions.len().saturating_sub(CLI_VIEW_ROW_LIMIT);
    match options.format {
        OutputFormat::Table => {
            print!(
                "{}",
                render_sessions_table(&sessions, &freshness, &coverage, &options.scope, period,)
            );
            Ok(())
        }
        OutputFormat::Markdown => {
            print!(
                "{}",
                render_sessions_markdown(&sessions, &freshness, &coverage, &options.scope, period,)
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
                "coverage": cli_coverage_json(&data, &coverage, options, period),
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
        .filter(|path| Path::new(path).is_absolute())
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
    cost: OverheadReport,
    pruning: InventoryReport,
    has_opportunities: bool,
}

fn run_doctor_report(options: &StoreOptions, limit: Option<usize>) -> Result<()> {
    let (data, freshness, selection) = load_reporting(options)?;
    let coverage = report_coverage_for_selection(&data, selection.as_ref());
    let period = selection.as_ref().map(|selection| &selection.coverage);
    let waste = match filter_view_report(
        ViewReport::Waste(codexlens::analysis::views::waste(&data)),
        &options.scope,
    ) {
        ViewReport::Waste(report) => report,
        _ => unreachable!("waste report variant"),
    };
    let cost = match filter_view_report(
        ViewReport::Overhead(codexlens::analysis::views::overhead(&data)),
        &options.scope,
    ) {
        ViewReport::Overhead(report) => report,
        _ => unreachable!("overhead report variant"),
    };
    let pruning = match filter_view_report(
        ViewReport::Inventory(codexlens::analysis::views::inventory(&data)),
        &options.scope,
    ) {
        ViewReport::Inventory(report) => report,
        _ => unreachable!("inventory report variant"),
    };
    let max_per_scope = limit.unwrap_or(5).min(5);
    let mut seen_per_scope = BTreeMap::<String, usize>::new();
    let top_fixes = waste
        .opportunities
        .iter()
        .filter(|opportunity| {
            let count = seen_per_scope
                .entry(opportunity.scope.to_string())
                .or_default();
            if *count >= max_per_scope {
                return false;
            }
            *count += 1;
            true
        })
        .cloned()
        .collect();
    let has_opportunities = !waste.opportunities.is_empty();
    let doctor = DoctorView {
        top_fixes,
        cost,
        pruning: InventoryReport {
            measure: pruning.measure,
            rows: pruning
                .rows
                .into_iter()
                .filter(|row| row.action.is_some())
                .collect(),
        },
        has_opportunities,
    };
    match options.format {
        OutputFormat::Table => {
            let (stdout, stderr) =
                render_doctor_table(&doctor, &freshness, &coverage, &options.scope, period);
            print!("{stdout}");
            eprint!("{stderr}");
            Ok(())
        }
        OutputFormat::Markdown => {
            print!(
                "{}",
                render_doctor_markdown(&doctor, &freshness, &coverage, &options.scope, period)
            );
            Ok(())
        }
        OutputFormat::Json => print_doctor_json(
            &doctor,
            &data,
            &freshness,
            &coverage,
            &options.scope,
            options,
            period,
        ),
    }
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
    if doctor.top_fixes.is_empty() {
        output.push_str("No top fixes with bounded evidence.\n");
    } else {
        for (index, opportunity) in doctor.top_fixes.iter().enumerate() {
            output.push_str(&format!(
                "\n{}. {}\n",
                index + 1,
                bounded_text(&opportunity.title)
            ));
            append_opportunity_fields(&mut output, opportunity);
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
    if !doctor.has_opportunities {
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
    let document = serde_json::json!({
        "schema_version": 1,
        "command": "doctor",
        "scope": scope_filter_json(scope),
        "coverage": cli_coverage_json(data, coverage, options, period),
        "freshness": freshness_json(freshness),
        "data": {
            "top_fixes": doctor.top_fixes.iter().map(opportunity_json).collect::<Vec<_>>(),
            "cost": {
                "measure": doctor.cost.measure,
                "rows": doctor.cost.rows.iter().map(overhead_row_json).collect::<Vec<_>>(),
            },
            "config_pruning": {
                "measure": doctor.pruning.measure,
                "rows": doctor.pruning.rows.iter().take(CLI_VIEW_ROW_LIMIT).map(inventory_row_json).collect::<Vec<_>>(),
                "omitted_count": doctor.pruning.rows.len().saturating_sub(CLI_VIEW_ROW_LIMIT),
            },
            "looks_healthy": !doctor.has_opportunities,
        },
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
        "Coverage: {} ({} sessions)\n\n",
        coverage.status, coverage.session_count
    ));
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
            "  observed minimum: {} bytes\n  readable startup: {} bytes\n  residual: {} bytes\n  unknown cost: {}\n",
            optional_number(row.observed_min_startup_bytes),
            optional_number(row.readable_startup_bytes),
            optional_number(row.residual_bytes),
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
        "  scope: {}\n  target: {}\n  impact: {}\n  severity: {}\n  confidence: {}\n  counts: {} occurrences, {} sessions\n  action: {}\n",
        opportunity.scope,
        bounded_text(&opportunity.target),
        bounded_text(&opportunity.impact),
        opportunity.severity.as_str(),
        opportunity.confidence.as_str(),
        opportunity.occurrences,
        opportunity.distinct_sessions,
        bounded_text(&opportunity.action),
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
        "target": bounded_text(&opportunity.target),
        "impact": bounded_text(&opportunity.impact),
        "severity": opportunity.severity.as_str(),
        "confidence": opportunity.confidence.as_str(),
        "occurrences": opportunity.occurrences,
        "distinct_sessions": opportunity.distinct_sessions,
        "action": bounded_text(&opportunity.action),
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
}

fn report_coverage_for_selection(
    data: &CanonicalData,
    selection: Option<&ReportingSelection>,
) -> ReportCoverage {
    selection.map_or_else(
        || report_coverage(data),
        |selection| selection.report_coverage.clone(),
    )
}

fn load_reporting(
    options: &StoreOptions,
) -> Result<(CanonicalData, StoreFreshness, Option<ReportingSelection>)> {
    let period = ReportingPeriod::from_bounds(options.since.as_deref(), options.until.as_deref())
        .map_err(anyhow::Error::new)?;
    let selection_options = session_selection_options(options)?;
    let store_path = options.store_path()?;
    if !options.frozen {
        let refresh = RefreshOptions {
            store: store_path.clone(),
            codex_home: options.codex_home.clone(),
            include_archived: options.include_archived,
            include_subagents: options.include_subagents,
            config: None,
        };
        let outcome = refresh_store(&refresh).with_context(|| {
            format!(
                "automatic analysis could not refresh derived store {}",
                bounded_display(&store_path)
            )
        })?;
        report_auto_refresh(&outcome, &store_path);
    }
    let (data, freshness) = load_store(&store_path)?;
    let selected = select_report_data_with_options(&data, period.as_ref(), &selection_options);
    let Some(period) = period else {
        return Ok((selected.data, freshness, None));
    };
    let eligible_data = select_eligible_session_data(&data, &selection_options);
    let mut report_coverage = report_coverage_with_period(&eligible_data, &period);
    report_coverage.session_count = selected.coverage.included_sessions;
    report_coverage.record_count = selected.coverage.included_records;
    report_coverage.status = match selected.coverage.state {
        PeriodCoverageState::Empty => "empty",
        PeriodCoverageState::Complete => "observed",
        PeriodCoverageState::Partial => "partial",
    }
    .to_owned();
    Ok((
        selected.data,
        freshness,
        Some(ReportingSelection {
            coverage: selected.coverage,
            report_coverage,
        }),
    ))
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
    let coverage = report_coverage_for_selection(&data, selection.as_ref());
    let findings = lens(&data)
        .into_iter()
        .filter(|finding| scope_matches(&options.scope, &finding.scope))
        .collect::<Vec<_>>();
    let mut report = doctor_with_coverage(
        &data,
        &findings,
        freshness,
        &DoctorOptions {
            max_findings_per_scope: Some(CLI_VIEW_ROW_LIMIT),
            ..DoctorOptions::default()
        },
        &coverage,
    );
    if let Some(selection) = selection.as_ref() {
        report.period_start = selection.coverage.observed_start.clone();
        report.period_end = selection.coverage.observed_end.clone();
    }
    if let Some(selection) = selection.as_ref() {
        options.format.write_report(
            command,
            || {
                (
                    render_doctor_with_period(&report, &coverage, &selection.coverage),
                    String::new(),
                )
            },
            || {
                render_json_finding_report_with_period(
                    command,
                    &report,
                    &coverage,
                    &selection.coverage,
                )
            },
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

fn render_optimize_human(batch: &DiffBatch) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    for rendered in &batch.rendered {
        stdout.push_str(&render_proposal_summary(rendered));
        stdout.push('\n');
    }
    for skipped in &batch.skipped {
        stderr.push_str(&format!(
            "Skipped {}: {}\n",
            skipped.target_path.display(),
            skipped.reason
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
