use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::watch;

use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::reports::{
    ChaseList, Denials, Scorecard, ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic,
    payer_scorecard, summarize,
};
use healthcare_billing_sim::scenario::{FaultPatch, PolicyPatch, Scenario};
use healthcare_billing_sim::{RunConfig, run, scenario};

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
    /// Low rates spread submissions across virtual months, which is what
    /// makes the AR-aging buckets interesting.
    #[arg(long, default_value_t = 1.0, help_heading = "Simulation")]
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

    let style = Style::detect(cli.no_color);
    let (cfg, provenance) = build_config(&cli)?;
    print_banner(&cli, &cfg, &provenance, &style);

    // Virtual time: paused clock + auto-advance needs a current-thread
    // runtime (DESIGN.md Decisions #12). The sim runs as fast as compute allows.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()?;
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

    let (ledger, now) = (&output.ledger, output.finished_at);
    print_styled(&summarize(ledger).to_string(), &style);
    println!();
    print_styled(&ar_aging(ledger, now).to_string(), &style);
    println!(
        "{}days in A/R:{} {:.1}\n",
        style.bold,
        style.reset,
        days_in_ar(ledger, now)
    );
    print_styled(&Scorecard(payer_scorecard(ledger)).to_string(), &style);
    println!();
    print_styled(&Denials(denial_breakdown(ledger)).to_string(), &style);
    println!();
    print_styled(
        &ChaseList(chase_list(ledger, now, cli.chase)).to_string(),
        &style,
    );
    println!();
    print_styled(&diagnostic(ledger, &output.sim_truth).to_string(), &style);

    let virtual_days = now.as_duration().as_secs_f64() / 86_400.0;
    println!(
        "\n{}simulated {} claims across {:.1} virtual days in {:.2}s of wall time · seed {}{}",
        style.dim,
        output.ledger.claims.len(),
        virtual_days,
        wall.as_secs_f64(),
        cli.seed,
        style.reset,
    );
    Ok(())
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

fn print_banner(cli: &Cli, cfg: &RunConfig, provenance: &[String], style: &Style) {
    let Style {
        bold,
        dim,
        cyan,
        reset,
        ..
    } = style;
    let file_size = std::fs::metadata(&cli.input)
        .map(|m| format!(" ({})", human_bytes(m.len())))
        .unwrap_or_default();

    println!("{bold}{cyan}Healthcare Billing Lifecycle Simulation{reset}");
    println!("{dim}{}{reset}", "─".repeat(72));
    println!(
        "  {bold}input{reset}                {}{file_size}",
        cli.input.display()
    );
    println!("  {bold}seed{reset}                 {}", cfg.seed);
    let interval_hint = if cfg.rate_per_sec < 0.1 {
        format!(" (one every {})", human_virtual(1.0 / cfg.rate_per_sec))
    } else {
        String::new()
    };
    println!(
        "  {bold}ingest rate{reset}          {} claims per virtual second{interval_hint}",
        cfg.rate_per_sec
    );
    println!(
        "  {bold}retry policy{reset}         {} attempts · {:.0}s timeout · {:.0}s backoff base",
        cfg.policy.max_attempts,
        cfg.policy.timeout.as_secs_f64(),
        cfg.policy.backoff_base.as_secs_f64(),
    );
    println!("  {bold}faults{reset}               {}", fault_summary(cfg));
    for payer in healthcare_billing_sim::domain::PayerId::ALL {
        let p = &cfg.payers[&payer];
        println!(
            "  {bold}{:<20}{reset} responds {:.0}-{:.0}s · denies {:.0}% · copay {}",
            payer.as_str(),
            p.min_response_time_secs,
            p.max_response_time_secs,
            p.denial_rate * 100.0,
            p.copay,
        );
    }
    println!(
        "  {bold}config from{reset}          {}",
        provenance.join(" → ")
    );
    println!("{dim}{}{reset}\n", "─".repeat(72));
}

fn fault_summary(cfg: &RunConfig) -> String {
    let f = &cfg.faults;
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
            human_virtual(f.max_extra_delay_secs)
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

fn human_virtual(secs: f64) -> String {
    match secs {
        s if s >= 86_400.0 => format!("{:.1} virtual days", s / 86_400.0),
        s if s >= 3_600.0 => format!("{:.1} virtual hours", s / 3_600.0),
        s if s >= 60.0 => format!("{:.1} virtual minutes", s / 60.0),
        s => format!("{s:.0} virtual seconds"),
    }
}

fn human_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b if b >= 1_000 => format!("{:.1} kB", b as f64 / 1e3),
        b => format!("{b} B"),
    }
}
