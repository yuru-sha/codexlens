use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use codexlens::advisor::{
    ApplyPlan, ApplyReport, DoctorOptions, doctor, prepare_apply_proposals, proposals_for_findings,
    render_diffs, render_doctor, render_proposal_summary,
};
use codexlens::analysis::{
    Finding, analyze_default, corrections, failures, instructions, knowledge, rework, verification,
};
use codexlens::model::CanonicalData;
use codexlens::store::{SCHEMA_VERSION, Store, StoreFreshness};

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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Analyze { store } => run_finding_report(&store, analyze_default),
        Command::Sessions { store } => {
            let (data, freshness) = load_store(&store)?;
            print!("{}", render_sessions(&data, &freshness));
            Ok(())
        }
        Command::Failures { store } => run_finding_report(&store, failures),
        Command::Corrections { store } => run_finding_report(&store, corrections),
        Command::Rework { store } => run_finding_report(&store, rework),
        Command::Verification { store } => run_finding_report(&store, verification),
        Command::Knowledge { store } => run_finding_report(&store, knowledge),
        Command::Instructions { store } => run_finding_report(&store, instructions),
        Command::Doctor { store, limit } => {
            let (data, findings, freshness) = load_analysis(&store)?;
            let report = doctor(
                &data,
                &findings,
                freshness,
                &DoctorOptions {
                    max_findings_per_scope: limit,
                    ..DoctorOptions::default()
                },
            );
            print!("{}", render_doctor(&report));
            Ok(())
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
            let (data, findings, _) = load_analysis(&store)?;
            let proposal_plan = proposals_for_findings(&data, &findings);
            if diff {
                let batch = proposal_batch(&proposal_plan);
                render_diff_batch(&batch);
            } else {
                run_apply(&data, &proposal_plan.proposals, &proposal_plan.skipped, yes)?;
            }
            Ok(())
        }
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

fn render_diff_batch(batch: &codexlens::advisor::DiffBatch) {
    for rendered in &batch.rendered {
        println!("{}", render_proposal_summary(rendered));
    }
    for skipped in &batch.skipped {
        eprintln!(
            "Skipped {}: {}",
            skipped.target_path.display(),
            skipped.reason
        );
    }
    if batch.rendered.is_empty() && batch.skipped.is_empty() {
        println!("No applicable proposals.");
    }
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

fn bounded_text(value: &str) -> String {
    const MAX_TEXT_BYTES: usize = 256;
    if value.len() <= MAX_TEXT_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_TEXT_BYTES.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

fn load_analysis(options: &StoreOptions) -> Result<(CanonicalData, Vec<Finding>, StoreFreshness)> {
    let (data, freshness) = load_store(options)?;
    let findings = analyze_default(&data);
    Ok((data, findings, freshness))
}

fn load_store(options: &StoreOptions) -> Result<(CanonicalData, StoreFreshness)> {
    if !options.store.is_file() {
        bail!("store does not exist: {}", options.store.display());
    }
    let schema_version = Store::read_schema_version(&options.store).with_context(|| {
        format!(
            "failed to inspect derived store {}; provide a valid SQLite store",
            options.store.display()
        )
    })?;
    let mut migrated_copy = None;
    match schema_version {
        SCHEMA_VERSION => {}
        version if (1..SCHEMA_VERSION).contains(&version) => {
            let copy = TemporaryStoreCopy::create(&options.store).with_context(|| {
                format!(
                    "failed to prepare a temporary copy of legacy derived store {}",
                    options.store.display()
                )
            })?;
            let migrated = Store::open(copy.path()).with_context(|| {
                format!(
                    "failed to migrate legacy derived store {}",
                    options.store.display()
                )
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
            options.store.display()
        )
    })?;
    let data = store
        .load_canonical()
        .with_context(|| format!("failed to load derived store {}", options.store.display()))?;
    let freshness = store
        .freshness()
        .with_context(|| format!("failed to read freshness for {}", options.store.display()))?;
    Ok((data, freshness))
}

fn run_finding_report(
    options: &StoreOptions,
    lens: fn(&CanonicalData) -> Vec<Finding>,
) -> Result<()> {
    let (data, freshness) = load_store(options)?;
    let report = doctor(&data, &lens(&data), freshness, &DoctorOptions::default());
    print!("{}", render_doctor(&report));
    Ok(())
}

fn render_sessions(data: &CanonicalData, freshness: &StoreFreshness) -> String {
    let mut sessions = data.sessions.iter().collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    sessions.dedup_by(|left, right| left.id == right.id);

    let mut output = format!(
        "Store freshness: {} ({} source files)\nSessions: {}\n",
        freshness,
        freshness.source_count,
        sessions.len()
    );
    for session in sessions {
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
    output
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

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
        ] {
            Cli::try_parse_from(["codexlens", command, "--store", "fixture.sqlite"]).unwrap();
        }
    }
}
