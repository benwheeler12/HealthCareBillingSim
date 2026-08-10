use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::watch;

use healthcare_billing_sim::domain::human_virtual;
use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::reports::{
    ChaseList, Denials, Scorecard, ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic,
    payer_scorecard, summarize,
};
use healthcare_billing_sim::scenario::{FaultPatch, PolicyPatch, Scenario};
use healthcare_billing_sim::{RunConfig, RunOutput, run, scenario};

mod tui;

/// Healthcare billing lifecycle simulation: biller ↔ clearinghouse ↔ payers.
///
/// Reads one PayerClaim JSON object per line, drives every claim to a
/// terminal state under seeded faults and virtual time, then prints the
/// practice owner's reports. Same seed, same outcomes.
///
/// Configuration precedence: defaults → --preset → --fault-profile file →
/// individual flags.
#[derive(Parser)]
#[command(version, about, max_term_width = 100)]
struct Cli {
    /// Input file: one PayerClaim JSON object per line.
    input: PathBuf,

    // ---------------- Simulation ----------------
    /// Master seed; a given seed reproduces the same claim outcomes.
    #[arg(long, default_value_t = 42, help_heading = "Simulation")]
    seed: u64,

    /// Ingest rate, claims per VIRTUAL second (wall time is always fast).
    /// The default spreads the 10k sample across ~9.5 virtual months so
    /// receivables genuinely age; raise it to compress the timeline.
    #[arg(long, default_value_t = 0.0004, help_heading = "Simulation")]
    rate: f64,

    /// Named fault preset. The default is 'messy' — real clearinghouses lose
    /// things, so a plain run shows drops, duplicates, and delays being
    /// survived. 'honest' is the lossless baseline; 'chaos' is everything at
    /// once.
    #[arg(long, default_value = "messy", value_parser = ["honest", "messy", "chaos"], help_heading = "Simulation")]
    preset: String,

    /// Scenario file (JSON): fault rates, payer personality overrides,
    /// retry-policy overrides. See data/demo_scenario.json.
    #[arg(long, value_name = "FILE", help_heading = "Simulation")]
    fault_profile: Option<PathBuf>,

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

    /// Worker threads for the simulation runtime (0 = one per CPU core).
    /// Time is computed, not slept, so claim tasks parallelize freely.
    #[arg(long, default_value_t = 0, help_heading = "Simulation")]
    threads: usize,
}

/// Multi-thread tokio runtime for the simulation (Decisions #23: nothing
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
    let (cfg, provenance) = build_config(&cli)?;

    // Interactive UI on a real terminal; plain sequential output for pipes,
    // CI, and --no-tui — that path still satisfies the assessment's
    // "serialize to terminal, then shut down" contract verbatim.
    if interactive {
        let banner = banner_rows(&cli, &cfg, &provenance)
            .into_iter()
            .map(|(label, value)| format!("{label:<20} {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = tui::run(
            cfg,
            tui::TuiOptions {
                banner,
                seed: cli.seed,
                threads: cli.threads,
            },
        )?;
        // Leave a plain-text record in the scrollback after the UI closes.
        let (cfg_again, _) = build_config(&cli)?;
        print_banner(&cli, &cfg_again, &provenance, &style);
        print_reports(&cli, &output, None, &style);
        return Ok(());
    }

    print_banner(&cli, &cfg, &provenance, &style);

    let runtime = sim_runtime(cli.threads)?;
    let wall_start = std::time::Instant::now();
    let output = runtime.block_on(async {
        let mut cfg = cfg;
        let progress_task = spawn_progress_bar(&cli).map(|(tx, task)| {
            cfg.progress = Some(tx);
            task
        });
        let result = run(cfg).await;
        if let Some(task) = progress_task {
            let _ = task.await;
        }
        result
    })?;
    let wall = wall_start.elapsed();

    let t_reports = std::time::Instant::now();
    print_reports(&cli, &output, Some(wall), &style);
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
fn print_reports(cli: &Cli, output: &RunOutput, wall: Option<Duration>, style: &Style) {
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
        &ChaseList(chase_list(books, as_of, cli.chase)).to_string(),
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
        cli.seed,
        style.reset,
    );
}

/// Defaults → preset → scenario file → individual flags. Returns the config
/// plus a human-readable description of where it came from.
fn build_config(cli: &Cli) -> anyhow::Result<(RunConfig, Vec<String>)> {
    let mut cfg = RunConfig::new(cli.input.clone(), cli.seed, cli.rate);
    let mut provenance = vec![format!("preset '{}'", cli.preset)];

    scenario::preset(&cli.preset)
        .expect("clap validated the preset name")
        .apply(&mut cfg);
    if let Some(path) = &cli.fault_profile {
        scenario::load(path)?.apply(&mut cfg);
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
        flags.apply(&mut cfg);
        provenance.push(format!(
            "{overrides} flag override{}",
            if overrides == 1 { "" } else { "s" }
        ));
    }
    Ok((cfg, provenance))
}

/// The banner as (label, value) rows — one source of truth for both the
/// styled stdout print and the TUI's configuration pane.
fn banner_rows(cli: &Cli, cfg: &RunConfig, provenance: &[String]) -> Vec<(String, String)> {
    let file_size = std::fs::metadata(&cli.input)
        .map(|m| format!(" ({})", human_bytes(m.len())))
        .unwrap_or_default();
    let interval_hint = if cfg.rate_per_sec < 0.1 {
        format!(" (one every {})", human_virtual(1.0 / cfg.rate_per_sec))
    } else {
        String::new()
    };
    let mut rows = vec![
        (
            "input".to_string(),
            format!("{}{file_size}", cli.input.display()),
        ),
        ("seed".to_string(), cfg.seed.to_string()),
        (
            "ingest rate".to_string(),
            format!(
                "{} claims per virtual second{interval_hint}",
                cfg.rate_per_sec
            ),
        ),
        (
            "retry policy".to_string(),
            format!(
                "{} attempts · {} timeout · {} backoff base",
                cfg.policy.max_attempts,
                human_virtual(cfg.policy.timeout.as_secs_f64()),
                human_virtual(cfg.policy.backoff_base.as_secs_f64()),
            ),
        ),
        ("faults".to_string(), fault_summary(&cfg.faults)),
    ];
    for payer in healthcare_billing_sim::domain::PayerId::ALL {
        let p = &cfg.payers[&payer];
        let route = match cfg.payer_faults.get(&payer) {
            Some(profile) => format!(" · route: {}", fault_summary(profile)),
            None => String::new(),
        };
        rows.push((
            payer.as_str().to_string(),
            format!(
                "responds in {} to {} · denies {:.0}% · copay {}{route}",
                human_short(p.min_response_time_secs),
                human_short(p.max_response_time_secs),
                p.denial_rate * 100.0,
                p.copay,
            ),
        ));
    }
    rows.push(("config from".to_string(), provenance.join(" → ")));
    rows
}

fn print_banner(cli: &Cli, cfg: &RunConfig, provenance: &[String], style: &Style) {
    let Style {
        bold,
        dim,
        cyan,
        reset,
        ..
    } = style;
    println!("{bold}{cyan}Healthcare Billing Lifecycle Simulation{reset}");
    println!("{dim}{}{reset}", "─".repeat(72));
    for (label, value) in banner_rows(cli, cfg, provenance) {
        println!("  {bold}{label:<20}{reset} {value}");
    }
    println!("{dim}{}{reset}\n", "─".repeat(72));
}

fn fault_summary(f: &healthcare_billing_sim::sim::faults::FaultProfile) -> String {
    let pct = |x: f64| format!("{:.0}%", x * 100.0);
    let mut parts = Vec::new();
    if f.forward_drop_rate > 0.0 {
        parts.push(format!("forward drops {}", pct(f.forward_drop_rate)));
    }
    if f.return_drop_rate > 0.0 {
        parts.push(format!("return drops {}", pct(f.return_drop_rate)));
    }
    if f.duplicate_rate > 0.0 {
        parts.push(format!("duplicates {}", pct(f.duplicate_rate)));
    }
    if f.extra_delay_rate > 0.0 {
        parts.push(format!(
            "delays {} (≤{})",
            pct(f.extra_delay_rate),
            human_short(f.max_extra_delay_secs)
        ));
    }
    if f.dishonest_adjudication_rate > 0.0 {
        parts.push(format!("dishonest {}", pct(f.dishonest_adjudication_rate)));
    }
    if f.line_drop_rate > 0.0 {
        parts.push(format!("line drops {}", pct(f.line_drop_rate)));
    }
    if f.corrupt_claim_id_rate > 0.0 {
        parts.push(format!("corrupt ids {}", pct(f.corrupt_claim_id_rate)));
    }
    if f.corrupt_remittance_rate > 0.0 {
        parts.push(format!("garbage {}", pct(f.corrupt_remittance_rate)));
    }
    if parts.is_empty() {
        "none — honest, lossless transport".to_string()
    } else {
        parts.join(" · ")
    }
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

/// Compact virtual-duration for dense table/banner lines ("3d", "4.5h").
fn human_short(secs: f64) -> String {
    match secs {
        s if s >= 86_400.0 => format!("{:.0}d", s / 86_400.0),
        s if s >= 3_600.0 => format!("{:.1}h", s / 3_600.0),
        s if s >= 60.0 => format!("{:.1}m", s / 60.0),
        s => format!("{s:.0}s"),
    }
}

fn human_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b if b >= 1_000 => format!("{:.1} kB", b as f64 / 1e3),
        b => format!("{b} B"),
    }
}
