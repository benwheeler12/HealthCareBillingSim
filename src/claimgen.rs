//! Seeded in-memory claim generator: yields one PayerClaim JSON document per
//! line, straight into ingest — no file on disk anywhere. `malformed_rate`
//! mixes in every ingest-fault flavor (invalid JSON, bad NPI, unknown payer,
//! negative charge, duplicate claim_id) so a generated run exercises
//! fault-table class 1 end to end.
//!
//! The interface is deliberately the *text line*, not a parsed `Claim`:
//! malformed documents are corrupted at the JSON level, and ingest's
//! validation is the system under test — generated lines flow through the
//! exact same parallel-validation path a file's lines did.

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde_json::json;

/// Everything the generator needs. `seed: None` follows the run's master
/// seed — one keystroke rerolls the whole world; pin it to keep the claim
/// population fixed while fault luck varies.
#[derive(Clone, Debug, PartialEq)]
pub struct GenConfig {
    /// Number of claim documents to generate.
    pub count: usize,
    /// Generator seed; `None` follows the simulation's master seed.
    pub seed: Option<u64>,
    /// Fraction of lines that are malformed in some way.
    pub malformed_rate: f64,
    /// Drift the payer mix across the stream: anthem-heavy early,
    /// medicare-heavy late, giving each payer a distinct A/R aging profile
    /// when claims are ingested over virtual months.
    pub drift: bool,
}

impl Default for GenConfig {
    fn default() -> GenConfig {
        GenConfig {
            count: 10_000,
            seed: None,
            malformed_rate: 0.02,
            drift: true,
        }
    }
}

impl GenConfig {
    pub fn resolved_seed(&self, master_seed: u64) -> u64 {
        self.seed.unwrap_or(master_seed)
    }

    /// One-line human summary for banners and the configuration form.
    pub fn summary(&self, master_seed: u64) -> String {
        format!(
            "{} generated claims · {:.0}% malformed · drift {} · gen seed {}",
            self.count,
            self.malformed_rate * 100.0,
            if self.drift { "on" } else { "off" },
            match self.seed {
                Some(seed) => seed.to_string(),
                None => format!("follows master ({master_seed})"),
            },
        )
    }
}

/// The generated stream, lazily: each `next()` mints one JSON line, so
/// claims reach ingest — and their claim tasks — batch by batch as they are
/// produced. Same config, same seed, same lines.
pub fn stream(cfg: &GenConfig, master_seed: u64) -> impl Iterator<Item = String> + Send + use<> {
    let mut rng = ChaCha8Rng::seed_from_u64(cfg.resolved_seed(master_seed));
    let count = cfg.count;
    let malformed_rate = cfg.malformed_rate;
    let drift = cfg.drift;
    (0..count).map(move |i| {
        let position = i as f64 / count.max(1) as f64;
        if rng.random_bool(malformed_rate) {
            malformed_line(&mut rng, i, drift, position)
        } else {
            valid_line(&mut rng, i, drift, position)
        }
    })
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

/// (payer_id, weight at stream start, weight at stream end). Weights drift
/// linearly across the stream: the slow, lossy payers (anthem, molina)
/// dominate early and the fast, clean ones (medicare, kaiser) late, so the
/// A/R aging report shows a distinct profile per payer when claims are
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

/// Payer for this line. Uniform by default; with drift, the slow payers
/// dominate the start of the stream and the fast ones the end.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_config_same_lines() {
        let cfg = GenConfig {
            count: 200,
            malformed_rate: 0.1,
            ..GenConfig::default()
        };
        let a: Vec<String> = stream(&cfg, 7).collect();
        let b: Vec<String> = stream(&cfg, 7).collect();
        assert_eq!(a.len(), 200);
        assert_eq!(a, b, "the stream must be a pure function of its seeds");
        // A different master seed rerolls everything when gen seed follows.
        let c: Vec<String> = stream(&cfg, 8).collect();
        assert_ne!(a, c);
    }

    #[test]
    fn pinned_gen_seed_ignores_the_master_seed() {
        let cfg = GenConfig {
            count: 50,
            seed: Some(11),
            ..GenConfig::default()
        };
        let a: Vec<String> = stream(&cfg, 1).collect();
        let b: Vec<String> = stream(&cfg, 2).collect();
        assert_eq!(a, b, "a pinned generator seed fixes the claim population");
    }

    #[test]
    fn malformed_rate_zero_yields_only_parseable_documents() {
        let cfg = GenConfig {
            count: 300,
            malformed_rate: 0.0,
            ..GenConfig::default()
        };
        for line in stream(&cfg, 42) {
            assert!(
                serde_json::from_str::<serde_json::Value>(&line).is_ok(),
                "unparseable line from a zero-malformed stream: {line}"
            );
        }
    }
}
