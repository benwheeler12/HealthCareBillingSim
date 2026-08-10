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

    /// Drift the payer mix across the file: anthem-heavy early, medicare-heavy
    /// late (uhg steady). Gives each payer a distinct A/R aging profile when
    /// the file is ingested over virtual months.
    #[arg(long)]
    drift: bool,
}

const FIRST_NAMES: &[&str] = &[
    "Ada",
    "Alan",
    "Grace",
    "Katherine",
    "Annie",
    "Mary",
    "Dorothy",
    "Edsger",
    "Barbara",
    "Donald",
];
const LAST_NAMES: &[&str] = &[
    "Lovelace", "Turing", "Hopper", "Johnson", "Easley", "Jackson", "Liskov", "Knuth", "Dijkstra",
    "Hamilton",
];

/// (payer_id, weight at file start, weight at file end). Weights drift
/// linearly across the file: the slow, lossy payers (anthem, molina)
/// dominate early and the fast, clean ones (medicare, kaiser) late, so the
/// A/R aging report shows a distinct profile per payer when the file is
/// ingested over virtual months. Each column sums to 1.0.
const PAYERS_DRIFT: &[(&str, f64, f64)] = &[
    ("anthem", 0.30, 0.04),
    ("molina_healthcare", 0.14, 0.04),
    ("centene", 0.10, 0.06),
    ("humana", 0.08, 0.08),
    ("blue_cross_blue_shield", 0.08, 0.08),
    ("cigna", 0.08, 0.08),
    ("united_health_group", 0.08, 0.08),
    ("aetna", 0.06, 0.10),
    ("kaiser_permanente", 0.04, 0.14),
    ("medicare", 0.04, 0.30),
];

/// 100 distinct billing organizations, each with its own rendering provider
/// and stable NPI: ORG_PLACES[i / 10] × ORG_KINDS[i % 10].
const ORG_PLACES: &[&str] = &[
    "Riverside",
    "Lakeside",
    "Summit",
    "Cedar Grove",
    "Harborview",
    "Prairie",
    "Blue Ridge",
    "Sunrise",
    "Willow Creek",
    "Maple Leaf",
];
const ORG_KINDS: &[&str] = &[
    "Family Practice",
    "Medical Group",
    "Health Associates",
    "Clinic",
    "Physicians",
    "Care Center",
    "Orthopedics",
    "Pediatrics",
    "Internal Medicine",
    "Wellness Center",
];
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
        let position = i as f64 / cli.count.max(1) as f64;
        let line = if rng.random_bool(cli.malformed_rate) {
            malformed_line(&mut rng, i, cli.drift, position)
        } else {
            valid_line(&mut rng, i, cli.drift, position)
        };
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Payer for this line. Uniform by default; with drift, the slow payers
/// dominate the start of the file and the fast ones the end.
fn pick_payer(rng: &mut ChaCha8Rng, drift: bool, position: f64) -> &'static str {
    if !drift {
        return PAYERS_DRIFT[rng.random_range(0..PAYERS_DRIFT.len())].0;
    }
    let mut draw: f64 = rng.random_range(0.0..1.0);
    for (payer, early, late) in PAYERS_DRIFT {
        let weight = early + (late - early) * position;
        if draw < weight {
            return payer;
        }
        draw -= weight;
    }
    PAYERS_DRIFT.last().expect("non-empty").0
}

/// One of 100 distinct (organization, rendering provider, NPI) triples.
/// The draw is squared so volume concentrates on the low indices — a few
/// high-volume practices and a long tail, which is what makes the provider
/// insights view worth sorting.
fn pick_provider(rng: &mut ChaCha8Rng) -> (String, &'static str, &'static str, String) {
    let uniform: f64 = rng.random_range(0.0..1.0);
    let index = ((uniform * uniform) * 100.0) as usize % 100;
    let org = format!("{} {}", ORG_PLACES[index / 10], ORG_KINDS[index % 10]);
    // Stable per-organization provider and NPI, derived from the index.
    let first = FIRST_NAMES[index % FIRST_NAMES.len()];
    let last = LAST_NAMES[(index / 10 + index) % LAST_NAMES.len()];
    let npi = format!("19{:08}", 1_234_567 + index * 731);
    (org, first, last, npi)
}

fn valid_line(rng: &mut ChaCha8Rng, i: usize, drift: bool, position: f64) -> String {
    let (org, provider_first, provider_last, npi) = pick_provider(rng);
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
            "payer_id": pick_payer(rng, drift, position),
            "patient_member_id": format!("M-{:08}", rng.random_range(0..100_000_000u64)),
        },
        "patient": {
            "first_name": FIRST_NAMES[rng.random_range(0..FIRST_NAMES.len())],
            "last_name": LAST_NAMES[rng.random_range(0..LAST_NAMES.len())],
            "gender": if rng.random_bool(0.5) { "f" } else { "m" },
            "dob": format!("19{:02}-{:02}-{:02}",
                rng.random_range(40..99), rng.random_range(1..=12), rng.random_range(1..=28)),
        },
        "organization": {"name": org},
        "rendering_provider": {
            "first_name": provider_first,
            "last_name": provider_last,
            "npi": npi,
        },
        "service_lines": lines,
    })
    .to_string()
}

fn malformed_line(rng: &mut ChaCha8Rng, i: usize, drift: bool, position: f64) -> String {
    let valid = valid_line(rng, i, drift, position);
    match rng.random_range(0..5) {
        // 1.1: not JSON at all.
        0 => format!("{{corrupted {}", &valid[1..valid.len().min(40)]),
        // 1.2: NPI fails the ten-digit pattern.
        1 => {
            let npi_start = valid.find("\"npi\":\"").expect("npi field") + 7;
            format!("{}bad{}", &valid[..npi_start], &valid[npi_start + 3..])
        }
        // 1.2: unknown payer.
        2 => PAYERS_DRIFT.iter().fold(valid, |line, (payer, _, _)| {
            line.replacen(payer, "acme_health", 1)
        }),
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
