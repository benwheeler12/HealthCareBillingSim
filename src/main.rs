use std::path::PathBuf;

use clap::Parser;

use healthcare_billing_sim::reports::{
    ChaseList, Denials, Scorecard, ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic,
    payer_scorecard, summarize,
};
use healthcare_billing_sim::{RunConfig, run, scenario};

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

    /// Scenario file (JSON): fault rates, payer personality overrides,
    /// retry-policy overrides. Omit for an honest, lossless run.
    #[arg(long)]
    fault_profile: Option<PathBuf>,

    /// How many rows of the chase list to print.
    #[arg(long, default_value_t = 10)]
    chase: usize,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(cli.rate > 0.0, "--rate must be positive");

    // Logs to stderr; stdout is reserved for the reports.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "healthcare_billing_sim=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut cfg = RunConfig::new(cli.input, cli.seed, cli.rate);
    if let Some(path) = &cli.fault_profile {
        scenario::load(path)?.apply(&mut cfg);
        tracing::info!(scenario = %path.display(), "scenario applied");
    }

    // Virtual time: paused clock + auto-advance needs a current-thread
    // runtime (DESIGN.md Decisions #12). The sim runs as fast as compute allows.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()?;
    let output = runtime.block_on(run(cfg))?;

    let (ledger, now) = (&output.ledger, output.finished_at);
    println!("{}\n", summarize(ledger));
    println!("{}", ar_aging(ledger, now));
    println!("days in A/R: {:.1}\n", days_in_ar(ledger, now));
    println!("{}", Scorecard(payer_scorecard(ledger)));
    println!("{}", Denials(denial_breakdown(ledger)));
    println!("{}", ChaseList(chase_list(ledger, now, cli.chase)));
    println!("{}", diagnostic(ledger, &output.sim_truth));
    Ok(())
}
