use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use codexlens::advisor::{
    DoctorOptions, doctor, proposals_for_findings, render_diffs, render_doctor,
    render_proposal_summary,
};
use codexlens::analysis::analyze_default;
use codexlens::store::Store;

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
    Analyze,
    Sessions,
    Failures,
    Corrections,
    #[command(alias = "stuck")]
    Rework,
    Verification,
    #[command(alias = "rediscovery")]
    Knowledge,
    Instructions,
    Doctor {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(long, value_name = "COUNT")]
        limit: Option<usize>,
    },
    Optimize {
        #[command(flatten)]
        store: StoreOptions,
        #[arg(long)]
        diff: bool,
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

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Analyze => "analyze",
            Self::Sessions => "sessions",
            Self::Failures => "failures",
            Self::Corrections => "corrections",
            Self::Rework => "rework",
            Self::Verification => "verification",
            Self::Knowledge => "knowledge",
            Self::Instructions => "instructions",
            Self::Doctor { .. } => "doctor",
            Self::Optimize { .. } => "optimize",
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
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
        Command::Optimize { store, diff } => {
            if !diff {
                bail!("optimize requires --diff; proposals are advisory and read-only");
            }
            let (data, findings, _) = load_analysis(&store)?;
            let plan = proposals_for_findings(&data, &findings);
            let mut batch = render_diffs(&plan.proposals);
            batch.skipped.extend(plan.skipped);
            batch.skipped.sort_by(|left, right| {
                left.target_path
                    .cmp(&right.target_path)
                    .then_with(|| left.reason.cmp(&right.reason))
            });
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
            Ok(())
        }
        command => bail!(
            "codexlens {} is not implemented yet; see docs/specs/ for the implementation contract",
            command.name()
        ),
    }
}

fn load_analysis(
    options: &StoreOptions,
) -> Result<(
    codexlens::model::CanonicalData,
    Vec<codexlens::analysis::Finding>,
    codexlens::store::StoreFreshness,
)> {
    if !options.store.is_file() {
        bail!("store does not exist: {}", options.store.display());
    }
    let store = Store::open(&options.store)?;
    let data = store.load_canonical()?;
    let freshness = store.freshness()?;
    let findings = analyze_default(&data);
    Ok((data, findings, freshness))
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn rediscovery_is_an_alias_for_knowledge() {
        let cli = Cli::try_parse_from(["codexlens", "rediscovery"]).unwrap();
        assert!(matches!(cli.command, Command::Knowledge));
    }
}
