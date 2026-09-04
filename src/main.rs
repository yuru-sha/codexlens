use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

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
    Doctor,
    Optimize,
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
            Self::Doctor => "doctor",
            Self::Optimize => "optimize",
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    bail!(
        "codexlens {} is not implemented yet; see docs/specs/ for the implementation contract",
        cli.command.name()
    );
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
