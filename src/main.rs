use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::watch;

use healthcare_billing_sim::claimgen::GenConfig;
use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::reports::{
    ChaseList, Denials, Scorecard, ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic,
    payer_scorecard, summarize,
};
use healthcare_billing_sim::scenario::{FaultPatch, PolicyPatch, Scenario};
use healthcare_billing_sim::simconfig::{Preset, SimConfig};
use healthcare_billing_sim::{RunOutput, run, scenario};

mod tui;

/// Healthcare billing lifecycle simulation: biller ↔ clearinghouse ↔ payers.
///
/// Opens on an interactive configuration screen; Enter generates the claim
/// population in memory, streams it through the simulation under seeded
/// faults and virtual time, and lands on the analysis dashboard — where the
/// configuration pane starts the next run. Same seed, same outcomes.
///
/// Flags set the initial values of that configuration screen. When stdout is
/// not a terminal (pipes, CI) or with --no-tui, one run executes headlessly
/// and the plain sequential reports print instead.
///
/// Configuration precedence: defaults → --preset → --fault-profile file →
/// individual flags.
#[derive(Parser)]
#[command(version, about, max_term_width = 100)]
struct Cli {
    // ---------------- Generation ----------------
    /// Number of claim documents to generate for each run.
    #[arg(long, default_value_t = 10_000, help_heading = "Generation")]
    count: usize,

    /// Fraction of generated documents that are malformed in some way.
    #[arg(
        long,
        default_value_t = 0.02,
        value_name = "0..1",
        help_heading = "Generation"
    )]
    malformed_rate: f64,

    /// Generator seed; defaults to following --seed, so one seed rerolls the
    /// whole world. Pin it to keep the claim population fixed while fault
    /// luck varies.
    #[arg(long, help_heading = "Generation")]
    gen_seed: Option<u64>,

    /// Disable the payer-mix drift (slow payers early, fast payers late)
    /// and generate a uniform mix instead.
    #[arg(long, help_heading = "Generation")]
    no_drift: bool,

    // ---------------- Simulation ----------------
    /// Master seed; a given seed reproduces the same claim outcomes.
    #[arg(long, default_value_t = 42, help_heading = "Simulation")]
    seed: u64,

    /// Ingest rate, claims per VIRTUAL second; the default lets receivables
    /// age over months. Raise it to compress the timeline.
    #[arg(long, default_value_t = 0.0004, help_heading = "Simulation")]
    rate: f64,

    /// Fault preset: 'honest' is lossless, 'messy' has drops, duplicates,
    /// and delays, 'chaos' is everything at once.
    #[arg(long, default_value = "messy", value_parser = ["honest", "messy", "chaos"], help_heading = "Simulation")]
    preset: String,

    /// Scenario file (JSON): fault rates, payer personality overrides,
    /// retry-policy overrides. See data/demo_scenario.json.
    #[arg(long, value_name = "FILE", help_heading = "Simulation")]
    fault_profile: Option<PathBuf>,

    /// Worker threads for the simulation runtime (0 = one per CPU core).
    #[arg(long, default_value_t = 0, help_heading = "Simulation")]
    threads: usize,

    // ---------------- Fault injection ----------------
    /// Probability a claim is dropped on the biller → payer hop.
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    forward_drop_rate: Option<f64>,

    /// Probability a remittance is dropped on the payer → biller hop.
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    return_drop_rate: Option<f64>,

    /// Probability a delivery is duplicated.
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    duplicate_rate: Option<f64>,

    /// Probability a remittance is delayed in transit.
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    delay_rate: Option<f64>,

    /// Upper bound of the injected delay, in virtual seconds.
    #[arg(long, value_name = "SECS", help_heading = "Fault injection")]
    max_delay_secs: Option<f64>,

    /// Probability a payer lies (amounts don't sum — caught by reconciliation).
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    dishonest_rate: Option<f64>,

    /// Per-line probability a service line vanishes from the remittance.
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    line_drop_rate: Option<f64>,

    /// Probability a remittance's claim_id is mangled in transit.
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    corrupt_id_rate: Option<f64>,

    /// Probability a remittance turns to unparseable garbage (== silence).
    #[arg(long, value_name = "0..1", help_heading = "Fault injection")]
    garbage_rate: Option<f64>,

    // ---------------- Retry policy ----------------
    /// Submission attempts before a claim is flagged for review.
    #[arg(long, value_name = "N", help_heading = "Retry policy")]
    max_attempts: Option<u32>,

    /// Virtual seconds of silence before a submission times out.
    #[arg(long, value_name = "SECS", help_heading = "Retry policy")]
    timeout_secs: Option<f64>,

    /// Base of the exponential backoff between retries, virtual seconds.
    #[arg(long, value_name = "SECS", help_heading = "Retry policy")]
    backoff_secs: Option<f64>,

    // ---------------- Output ----------------
    /// Rows of the chase list to print.
    #[arg(long, default_value_t = 10, help_heading = "Output")]
    chase: usize,

    /// Disable ANSI colors (also respects NO_COLOR).
    #[arg(long, help_heading = "Output")]
    no_color: bool,

    /// Suppress the live progress line.
    #[arg(long, help_heading = "Output")]
    no_progress: bool,

    /// Plain sequential output instead of the interactive UI (automatic when
    /// stdout is not a terminal).
    #[arg(long, help_heading = "Output")]
    no_tui: bool,
}

/// Multi-thread tokio runtime for the simulation (DESIGN.md 'Virtual time': nothing
/// sleeps, so no paused clock — claim tasks execute in true parallel).
fn sim_runtime(threads: usize) -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if threads > 0 {
        builder.worker_threads(threads);
    }
    builder.enable_all().build()
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(cli.rate > 0.0, "--rate must be positive");
    anyhow::ensure!(
        (0.0..=1.0).contains(&cli.malformed_rate),
        "--malformed-rate must be in [0,1]"
    );
    let interactive = std::io::stdout().is_terminal() && !cli.no_tui;

    // Logs to stderr; stdout is reserved for the reports. While the
    // interactive UI owns the screen, stray stderr lines would corrupt it,
    // so logging defaults to off there — an explicit RUST_LOG still wins.
    let default_filter = if interactive {
        "healthcare_billing_sim=off"
    } else {
        "healthcare_billing_sim=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let style = Style::detect(cli.no_color);
    let (cfg, provenance) = build_sim_config(&cli)?;

    // Interactive UI on a real terminal: configure → run → explore → rerun.
    // Plain sequential output for pipes, CI, and --no-tui: one headless run
    // of the flag-built configuration, then the reports.
    if interactive {
        match tui::run(cfg)? {
            // Leave a plain-text record of the last completed run in the
            // scrollback after the UI closes — banner from the snapshot that
            // actually ran, edits made afterwards notwithstanding.
            Some((ran_cfg, output)) => {
                print_banner(&ran_cfg, &["interactive session".to_string()], &style);
                print_reports(cli.chase, &ran_cfg, &output, None, &style);
            }
            None => println!("no simulation run completed — nothing to report"),
        }
        return Ok(());
    }

    print_banner(&cfg, &provenance, &style);

    let runtime = sim_runtime(cfg.threads)?;
    let wall_start = std::time::Instant::now();
    let output = runtime.block_on(async {
        let mut run_cfg = cfg.to_run_config();
        let progress_task = spawn_progress_bar(&cli).map(|(tx, task)| {
            run_cfg.progress = Some(tx);
            task
        });
        let result = run(run_cfg).await;
        if let Some(task) = progress_task {
            let _ = task.await;
        }
        result
    })?;
    let wall = wall_start.elapsed();

    let t_reports = std::time::Instant::now();
    print_reports(cli.chase, &cfg, &output, Some(wall), &style);
    if std::env::var_os("SIM_PHASES").is_some() {
        eprintln!(
            "SIM_PHASES reports={:.2}s",
            t_reports.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// The full report suite on stdout. A/R views are a mid-flight snapshot by
/// nature: on the final books every claim has been driven terminal, so young
/// receivables would be empty by construction. Aging/chase/days-in-A/R
/// report over the books as of the moment intake ended; everything else
/// reads the final ledger.
fn print_reports(
    chase: usize,
    cfg: &SimConfig,
    output: &RunOutput,
    wall: Option<Duration>,
    style: &Style,
) {
    let ledger = &output.ledger;
    let (books, as_of) = (&output.intake_ledger, output.intake_finished_at);
    print_styled(&summarize(ledger).to_string(), style);
    println!();
    println!(
        "{}the A/R views below are a snapshot as of end of intake ({}), mid-flight —\nthe books as a biller would see them; the run then drained to terminal by {}{}\n",
        style.dim, as_of, output.finished_at, style.reset,
    );
    print_styled(&ar_aging(books, as_of).to_string(), style);
    println!(
        "{}days in A/R:{} {:.1}\n",
        style.bold,
        style.reset,
        days_in_ar(books, as_of)
    );
    print_styled(&Scorecard(payer_scorecard(ledger)).to_string(), style);
    println!();
    print_styled(&Denials(denial_breakdown(ledger)).to_string(), style);
    println!();
    print_styled(
        &ChaseList(chase_list(books, as_of, chase)).to_string(),
        style,
    );
    println!();
    print_styled(&diagnostic(ledger, &output.sim_truth).to_string(), style);

    let virtual_days = output.finished_at.as_duration().as_secs_f64() / 86_400.0;
    let wall_note = wall
        .map(|w| format!(" in {:.2}s of wall time", w.as_secs_f64()))
        .unwrap_or_default();
    println!(
        "\n{}simulated {} claims across {virtual_days:.1} virtual days{wall_note} · seed {}{}",
        style.dim,
        output.ledger.claims.len(),
        cfg.seed,
        style.reset,
    );
}

/// Defaults → preset → scenario file → individual flags, landing in the one
/// editable SimConfig the whole session shares. Returns the config plus a
/// human-readable description of where it came from.
fn build_sim_config(cli: &Cli) -> anyhow::Result<(SimConfig, Vec<String>)> {
    let mut cfg = SimConfig {
        seed: cli.seed,
        rate_per_sec: cli.rate,
        threads: cli.threads,
        generator: GenConfig {
            count: cli.count,
            seed: cli.gen_seed,
            malformed_rate: cli.malformed_rate,
            drift: !cli.no_drift,
        },
        ..SimConfig::default()
    };
    cfg.apply_preset(Preset::parse(&cli.preset).expect("clap validated the preset name"));
    let mut provenance = vec![format!("preset '{}'", cli.preset)];

    if let Some(path) = &cli.fault_profile {
        cfg.apply_scenario(&scenario::load(path)?);
        cfg.preset = Preset::Custom;
        provenance.push(format!("scenario file {}", path.display()));
    }

    let flags = Scenario {
        faults: FaultPatch {
            forward_drop_rate: cli.forward_drop_rate,
            return_drop_rate: cli.return_drop_rate,
            duplicate_rate: cli.duplicate_rate,
            extra_delay_rate: cli.delay_rate,
            max_extra_delay_secs: cli.max_delay_secs,
            dishonest_adjudication_rate: cli.dishonest_rate,
            line_drop_rate: cli.line_drop_rate,
            corrupt_claim_id_rate: cli.corrupt_id_rate,
            corrupt_remittance_rate: cli.garbage_rate,
        },
        payers: Default::default(),
        policy: PolicyPatch {
            max_attempts: cli.max_attempts,
            timeout_secs: cli.timeout_secs,
            backoff_base_secs: cli.backoff_secs,
        },
    };
    let overrides = flags.count_set();
    if overrides > 0 {
        flags.validated()?;
        cfg.apply_scenario(&flags);
        cfg.preset = Preset::Custom;
        provenance.push(format!(
            "{overrides} flag override{}",
            if overrides == 1 { "" } else { "s" }
        ));
    }
    Ok((cfg, provenance))
}

fn print_banner(cfg: &SimConfig, provenance: &[String], style: &Style) {
    let Style {
        bold,
        dim,
        cyan,
        reset,
        ..
    } = style;
    println!("{bold}{cyan}Healthcare Billing Lifecycle Simulation{reset}");
    println!("{dim}{}{reset}", "─".repeat(72));
    let mut rows = cfg.banner_rows();
    rows.push(("config from".to_string(), provenance.join(" → ")));
    for (label, value) in rows {
        println!("  {bold}{label:<22}{reset} {value}");
    }
    println!("{dim}{}{reset}\n", "─".repeat(72));
}

/// Live progress line on stderr, tty-only, throttled by wall clock. Reads the
/// fold's best-effort watch tap; the run itself never waits on it.
fn spawn_progress_bar(cli: &Cli) -> Option<(watch::Sender<Progress>, tokio::task::JoinHandle<()>)> {
    if cli.no_progress || !std::io::stderr().is_terminal() {
        return None;
    }
    let (tx, mut rx) = watch::channel(Progress::default());
    let task = tokio::spawn(async move {
        const FRAMES: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];
        let mut last_draw = std::time::Instant::now();
        let mut frame = 0usize;
        while rx.changed().await.is_ok() {
            if last_draw.elapsed() < Duration::from_millis(50) {
                continue;
            }
            last_draw = std::time::Instant::now();
            let p = *rx.borrow();
            frame = (frame + 1) % FRAMES.len();
            eprint!(
                "\r\x1b[2K{} {} claims · {} resolved · {} rejected · {} flagged · t+{:.1} virtual days",
                FRAMES[frame],
                p.claims,
                p.resolved,
                p.rejected,
                p.flagged,
                p.now.as_duration().as_secs_f64() / 86_400.0,
            );
            let _ = std::io::stderr().flush();
        }
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });
    Some((tx, task))
}

/// Minimal ANSI styling; every field is empty when color is off.
struct Style {
    bold: &'static str,
    dim: &'static str,
    cyan: &'static str,
    reset: &'static str,
}

impl Style {
    fn detect(no_color_flag: bool) -> Style {
        let enabled = !no_color_flag
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal();
        if enabled {
            Style {
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                cyan: "\x1b[36m",
                reset: "\x1b[0m",
            }
        } else {
            Style {
                bold: "",
                dim: "",
                cyan: "",
                reset: "",
            }
        }
    }
}

/// Reports render plain (tests compare text); the shell styles their
/// `=== title ===` section markers on the way out.
fn print_styled(text: &str, style: &Style) {
    for line in text.lines() {
        match line
            .strip_prefix("=== ")
            .and_then(|l| l.strip_suffix(" ==="))
        {
            Some(title) => println!("{}{}▌ {title}{}", style.bold, style.cyan, style.reset),
            None => println!("{line}"),
        }
    }
}
