//! Seeded input generator for demos and load tests: writes one PayerClaim
//! JSON object per line. `--malformed-rate` mixes in every ingest-fault
//! flavor (invalid JSON, bad NPI, unknown payer, negative charge, duplicate
//! claim_id) so a generated file exercises fault-table class 1 end to end.
//!
//! Usage: cargo run --bin generate-claims -- 500 --seed 7 > claims.jsonl

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde_json::json;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Number of lines to generate.
    count: usize,

    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Fraction of lines that are malformed in some way.
    #[arg(long, default_value_t = 0.0)]
    malformed_rate: f64,

    /// Output path; stdout if omitted.
    #[arg(long)]
    out: Option<PathBuf>,
}

const FIRST_NAMES: &[&str] = &[
    "Ada",
    "Alan",
    "Grace",
    "Katherine",
    "Annie",
    "Mary",
    "Dorothy",
];
const LAST_NAMES: &[&str] = &[
    "Lovelace", "Turing", "Hopper", "Johnson", "Easley", "Jackson",
];
const PAYERS: &[&str] = &["medicare", "united_health_group", "anthem"];
const PROCEDURES: &[&str] = &[
    "99213", "99214", "36415", "73030", "93000", "97110", "99396",
];

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(
        (0.0..=1.0).contains(&cli.malformed_rate),
        "--malformed-rate must be in [0,1]"
    );
    let mut rng = ChaCha8Rng::seed_from_u64(cli.seed);
    let mut out: Box<dyn Write> = match &cli.out {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout().lock()),
    };

    for i in 0..cli.count {
        let line = if rng.random_bool(cli.malformed_rate) {
            malformed_line(&mut rng, i)
        } else {
            valid_line(&mut rng, i)
        };
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn valid_line(rng: &mut ChaCha8Rng, i: usize) -> String {
    let lines: Vec<serde_json::Value> = (0..rng.random_range(1..=4))
        .map(|l| {
            json!({
                "service_line_id": format!("L{}", l + 1),
                "procedure_code": PROCEDURES[rng.random_range(0..PROCEDURES.len())],
                "units": rng.random_range(1..=4),
                "details": "generated service line",
                "unit_charge_currency": "USD",
                // Whole cents by construction.
                "unit_charge_amount": rng.random_range(500..=50_000) as f64 / 100.0,
                "do_not_bill": rng.random_bool(0.05),
            })
        })
        .collect();
    json!({
        "claim_id": format!("gen-{i:06}"),
        "place_of_service_code": 11,
        "insurance": {
            "payer_id": PAYERS[rng.random_range(0..PAYERS.len())],
            "patient_member_id": format!("M-{:08}", rng.random_range(0..100_000_000u64)),
        },
        "patient": {
            "first_name": FIRST_NAMES[rng.random_range(0..FIRST_NAMES.len())],
            "last_name": LAST_NAMES[rng.random_range(0..LAST_NAMES.len())],
            "gender": if rng.random_bool(0.5) { "f" } else { "m" },
            "dob": format!("19{:02}-{:02}-{:02}",
                rng.random_range(40..99), rng.random_range(1..=12), rng.random_range(1..=28)),
        },
        "organization": {"name": "Riverside Family Practice"},
        "rendering_provider": {
            "first_name": "Grace",
            "last_name": "Hopper",
            "npi": format!("{:010}", rng.random_range(1_000_000_000u64..=9_999_999_999)),
        },
        "service_lines": lines,
    })
    .to_string()
}

fn malformed_line(rng: &mut ChaCha8Rng, i: usize) -> String {
    let valid = valid_line(rng, i);
    match rng.random_range(0..5) {
        // 1.1: not JSON at all.
        0 => format!("{{corrupted {}", &valid[1..valid.len().min(40)]),
        // 1.2: NPI fails the ten-digit pattern.
        1 => {
            let npi_start = valid.find("\"npi\":\"").expect("npi field") + 7;
            format!("{}bad{}", &valid[..npi_start], &valid[npi_start + 3..])
        }
        // 1.2: unknown payer.
        2 => valid
            .replacen(PAYERS[0], "aetna", 1)
            .replacen(PAYERS[1], "aetna", 1)
            .replacen(PAYERS[2], "aetna", 1),
        // 1.2: negative charge.
        3 => {
            let amt = valid.find("\"unit_charge_amount\":").expect("amount") + 21;
            format!("{}-{}", &valid[..amt], &valid[amt..])
        }
        // 1.3: duplicate of an earlier claim_id.
        _ => valid.replace(
            &format!("gen-{i:06}"),
            &format!("gen-{:06}", i.saturating_sub(1)),
        ),
    }
}
