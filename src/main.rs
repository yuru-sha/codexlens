use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use codexlens::advisor::{
    ApplyPlan, ApplyReport, DiffBatch, DoctorOptions, doctor, prepare_apply_proposals,
    proposals_for_findings, render_diffs, render_doctor_with_coverage, render_doctor_with_period,
    render_json_diff, render_json_diff_with_period, render_json_finding_report_with_coverage,
    render_json_finding_report_with_period, render_json_sessions, render_json_sessions_with_period,
    render_proposal_summary, render_report_metadata, render_report_metadata_with_period,
    report_coverage, report_coverage_with_period, report_sessions,
};
use codexlens::analysis::{
    Finding, analyze_default, corrections, failures, instructions, knowledge, rework, verification,
};
use codexlens::discovery::{
    DiscoveredInput, DiscoveryOptions, DiscoveryResult, InputKind, discover,
};
use codexlens::instructions::InstructionCaptureOptions;
use codexlens::model::CanonicalData;
use codexlens::normalize::normalize_rollout;
use codexlens::period::{PeriodCoverage, ReportingPeriod, select_report_data};
use codexlens::rollout::{RolloutParseOptions, parse_rollout};
use codexlens::state::read_state_database;
use codexlens::store::{IngestOptions, SCHEMA_VERSION, Store, StoreFreshness};

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
    Failures {
        #[command(flatten)]
        store: StoreOptions,
    },
    Corrections {
        #[command(flatten)]
        store: StoreOptions,
    },
    #[command(alias = "stuck")]
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
    Optimize {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(long, conflicts_with = "apply")]
        diff: bool,
        #[arg(long, conflicts_with = "diff")]
        apply: bool,
        #[arg(long, requires = "apply")]
        yes: bool,
    },
    Monitor {
        #[command(flatten)]
        store: StoreOptions,
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

#[derive(Debug, Clone, Args)]
struct StoreOptions {
    #[arg(
        long,
        short = 's',
        default_value = ".codexlens.sqlite",
        value_name = "PATH"
    )]
    store: PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
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
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

impl OutputFormat {
    fn write_report(
        self,
        human: impl FnOnce() -> (String, String),
        json: impl FnOnce() -> Result<String, serde_json::Error>,
    ) -> Result<()> {
        match self {
            Self::Human => {
                let (stdout, stderr) = human();
                print!("{stdout}");
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
        Command::Sessions { store } => {
            let (data, freshness, selection) = load_reporting(&store)?;
            if let Some(selection) = selection.as_ref() {
                let coverage = report_coverage_with_period(&data, &selection.period);
                store.format.write_report(
                    || {
                        (
                            render_sessions_with_period(
                                &data,
                                &freshness,
                                &coverage,
                                &selection.coverage,
                            ),
                            String::new(),
                        )
                    },
                    || {
                        render_json_sessions_with_period(
                            &data,
                            &freshness,
                            &coverage,
                            &selection.coverage,
                        )
                    },
                )
            } else {
                store.format.write_report(
                    || (render_sessions(&data, &freshness), String::new()),
                    || render_json_sessions(&data, &freshness),
                )
            }
        }
        Command::Failures { store } => run_finding_report(&store, failures, "failures"),
        Command::Corrections { store } => run_finding_report(&store, corrections, "corrections"),
        Command::Rework { store } => run_finding_report(&store, rework, "rework"),
        Command::Verification { store } => run_finding_report(&store, verification, "verification"),
        Command::Knowledge { store } => run_finding_report(&store, knowledge, "knowledge"),
        Command::Instructions { store } => run_finding_report(&store, instructions, "instructions"),
        Command::Doctor { store, limit } => {
            let (data, findings, freshness, selection) = load_analysis(&store)?;
            let mut report = doctor(
                &data,
                &findings,
                freshness,
                &DoctorOptions {
                    max_findings_per_scope: limit,
                    ..DoctorOptions::default()
                },
            );
            if let Some(selection) = selection.as_ref() {
                report.period_start = selection.coverage.observed_start.clone();
                report.period_end = selection.coverage.observed_end.clone();
            }
            let coverage = selection.as_ref().map_or_else(
                || report_coverage(&data),
                |selection| report_coverage_with_period(&data, &selection.period),
            );
            if let Some(selection) = selection.as_ref() {
                store.format.write_report(
                    || {
                        (
                            render_doctor_with_period(&report, &coverage, &selection.coverage),
                            String::new(),
                        )
                    },
                    || {
                        render_json_finding_report_with_period(
                            "doctor",
                            &report,
                            &coverage,
                            &selection.coverage,
                        )
                    },
                )
            } else {
                store.format.write_report(
                    || {
                        (
                            render_doctor_with_coverage(&report, &coverage),
                            String::new(),
                        )
                    },
                    || render_json_finding_report_with_coverage("doctor", &report, &coverage),
                )
            }
        }
        Command::Optimize {
            store,
            diff,
            apply,
            yes,
        } => {
            if !diff && !apply {
                bail!("optimize requires --diff or --apply");
            }
            if apply && (store.since.is_some() || store.until.is_some()) {
                bail!(
                    "reporting period filters are supported by optimize --diff only; optimize --apply requires the unfiltered store"
                );
            }
            let (data, findings, freshness, selection) = load_analysis(&store)?;
            let proposal_plan = proposals_for_findings(&data, &findings);
            if diff {
                let batch = proposal_batch(&proposal_plan);
                if let Some(selection) = selection.as_ref() {
                    let coverage = report_coverage_with_period(&data, &selection.period);
                    store.format.write_report(
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
    store_options: &StoreOptions,
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

fn run_refresh(options: &RefreshOptions) -> Result<()> {
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
        .ingest_inputs_with_instructions(&discovery.inputs, &IngestOptions::default(), &capture)
        .context("refresh failed while ingesting discovered inputs")?;
    let freshness = store.freshness()?;
    drop(store);
    staged.commit()?;

    for diagnostic in &discovery.diagnostics {
        eprintln!(
            "Discovery diagnostic {}: {}",
            bounded_display(&diagnostic.path),
            bounded_text(&diagnostic.message)
        );
    }
    println!("Refreshed store: {}", bounded_display(&options.store));
    for file in report.files {
        let status = if file.skipped { "skipped" } else { "ingested" };
        println!(
            "- {}: {status} ({} sessions, {} records, {} diagnostics)",
            bounded_display(&file.source),
            file.sessions,
            file.records,
            file.diagnostics
        );
    }
    println!("Store freshness: {freshness}");
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

#[derive(Debug, Clone)]
struct ReportingSelection {
    period: ReportingPeriod,
    coverage: PeriodCoverage,
}

fn load_reporting(
    options: &StoreOptions,
) -> Result<(CanonicalData, StoreFreshness, Option<ReportingSelection>)> {
    let period = ReportingPeriod::from_bounds(options.since.as_deref(), options.until.as_deref())
        .map_err(anyhow::Error::new)?;
    let (data, freshness) = load_store(options)?;
    let Some(period) = period else {
        return Ok((data, freshness, None));
    };
    let selected = select_report_data(&data, Some(&period));
    Ok((
        selected.data,
        freshness,
        Some(ReportingSelection {
            period,
            coverage: selected.coverage,
        }),
    ))
}

fn load_store(options: &StoreOptions) -> Result<(CanonicalData, StoreFreshness)> {
    let store_display = bounded_display(&options.store);
    if !options.store.is_file() {
        bail!("store does not exist: {store_display}");
    }
    let schema_version = Store::read_schema_version(&options.store).with_context(|| {
        format!(
            "failed to inspect derived store {}; provide a valid SQLite store",
            store_display
        )
    })?;
    let mut migrated_copy = None;
    match schema_version {
        SCHEMA_VERSION => {}
        version if (1..SCHEMA_VERSION).contains(&version) => {
            let copy = TemporaryStoreCopy::create(&options.store).with_context(|| {
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
        .unwrap_or(options.store.as_path());
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
    let mut report = doctor(&data, &lens(&data), freshness, &DoctorOptions::default());
    if let Some(selection) = selection.as_ref() {
        report.period_start = selection.coverage.observed_start.clone();
        report.period_end = selection.coverage.observed_end.clone();
    }
    let coverage = selection.as_ref().map_or_else(
        || report_coverage(&data),
        |selection| report_coverage_with_period(&data, &selection.period),
    );
    if let Some(selection) = selection.as_ref() {
        options.format.write_report(
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

fn render_sessions(data: &CanonicalData, freshness: &StoreFreshness) -> String {
    let mut output = render_report_metadata(&report_coverage(data), freshness);
    append_sessions(&mut output, data);
    output
}

fn render_sessions_with_period(
    data: &CanonicalData,
    freshness: &StoreFreshness,
    coverage: &codexlens::advisor::ReportCoverage,
    period: &PeriodCoverage,
) -> String {
    let mut output = render_report_metadata_with_period(coverage, freshness, period);
    append_sessions(&mut output, data);
    output
}

fn append_sessions(output: &mut String, data: &CanonicalData) {
    for session in report_sessions(data) {
        output.push_str("- ");
        output.push_str(&session.id);
        output.push('\n');
        output.push_str("  created: ");
        output.push_str(session.created_at.as_deref().unwrap_or("unknown"));
        output.push('\n');
        output.push_str("  updated: ");
        output.push_str(session.updated_at.as_deref().unwrap_or("unknown"));
        output.push('\n');
        output.push_str("  cwd: ");
        output.push_str(session.cwd.as_deref().unwrap_or("unknown"));
        output.push('\n');
        output.push_str("  project: ");
        output.push_str(session.project.as_deref().unwrap_or("unknown"));
        output.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, OutputFormat};
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
    fn reporting_commands_accept_json_format() {
        let cli = Cli::try_parse_from(["codexlens", "analyze", "--format", "json"]).unwrap();
        let Command::Analyze { store } = cli.command else {
            panic!("expected analyze command");
        };
        assert_eq!(store.format, OutputFormat::Json);
    }
}
