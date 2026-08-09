use std::path::PathBuf;

use clap::Parser;

use healthcare_billing_sim::reports::summarize;
use healthcare_billing_sim::{RunConfig, run};

/// Healthcare billing lifecycle simulation: biller ↔ clearinghouse ↔ payers.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Input file: one PayerClaim JSON object per line.
    input: PathBuf,

    /// Master seed; a given seed reproduces the same claim outcomes.
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Ingest rate, claims per (virtual) second.
    #[arg(long, default_value_t = 1.0)]
    rate: f64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(cli.rate > 0.0, "--rate must be positive");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "healthcare_billing_sim=info".into()),
        )
        .init();

    // Virtual time: paused clock + auto-advance needs a current-thread
    // runtime (DESIGN.md Decisions #12). The sim runs as fast as compute allows.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()?;
    let ledger = runtime.block_on(run(RunConfig::new(cli.input, cli.seed, cli.rate)))?;

    println!("{}", summarize(&ledger));
    Ok(())
}
