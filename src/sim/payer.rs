//! Payer adjudication: a pure function of (seed, claim, config). No payer
//! state anywhere — idempotency via derivation (DESIGN.md Decisions): re-delivering
//! the same claim reproduces the identical remittance.

use std::collections::HashMap;
use std::time::Duration;

use rand::Rng;

use crate::domain::{
    ClaimId, DenialReason, Money, PayerId, RemittanceAdvice, RemittanceLine, SubmittedClaim,
    SubmittedLine,
};
use crate::sim::rng::RngFactory;

/// Per-payer personality. Response-time bounds come from the assessment; the
/// split rates are arbitrary-but-plausible policy (the *shape* is what
/// matters), and the payer scorecard report should rediscover the differences
/// between payers from remittance data alone.
#[derive(Clone, Copy, Debug)]
pub struct PayerConfig {
    pub min_response_time_secs: f64,
    pub max_response_time_secs: f64,
    /// Probability a service line is fully denied.
    pub denial_rate: f64,
    /// Contractual write-off: up to this many basis points of billed.
    pub max_not_allowed_bps: u32,
    /// Applied to the allowed amount: up to this many bps become deductible.
    pub max_deductible_bps: u32,
    /// Fixed coinsurance rate, in bps of the post-deductible amount.
    pub coinsurance_bps: u32,
    /// Flat per-line copay, capped by what remains.
    pub copay: Money,
}

pub fn default_payer_configs() -> HashMap<PayerId, PayerConfig> {
    HashMap::from([
        (
            PayerId::Medicare,
            PayerConfig {
                min_response_time_secs: 1.0,
                max_response_time_secs: 5.0,
                denial_rate: 0.05,
                max_not_allowed_bps: 500,
                max_deductible_bps: 1_000,
                coinsurance_bps: 2_000,
                copay: Money::ZERO,
            },
        ),
        (
            PayerId::UnitedHealthGroup,
            PayerConfig {
                min_response_time_secs: 2.0,
                max_response_time_secs: 10.0,
                denial_rate: 0.10,
                max_not_allowed_bps: 1_500,
                max_deductible_bps: 1_500,
                coinsurance_bps: 1_000,
                copay: Money::from_cents(2_500),
            },
        ),
        (
            PayerId::Anthem,
            PayerConfig {
                min_response_time_secs: 5.0,
                max_response_time_secs: 30.0,
                denial_rate: 0.15,
                max_not_allowed_bps: 2_000,
                max_deductible_bps: 2_000,
                coinsurance_bps: 3_000,
                copay: Money::from_cents(4_000),
            },
        ),
        (
            PayerId::Aetna,
            PayerConfig {
                min_response_time_secs: 2.0,
                max_response_time_secs: 8.0,
                denial_rate: 0.08,
                max_not_allowed_bps: 1_200,
                max_deductible_bps: 1_200,
                coinsurance_bps: 1_500,
                copay: Money::from_cents(2_000),
            },
        ),
        (
            PayerId::Cigna,
            PayerConfig {
                min_response_time_secs: 3.0,
                max_response_time_secs: 12.0,
                denial_rate: 0.10,
                max_not_allowed_bps: 1_400,
                max_deductible_bps: 1_600,
                coinsurance_bps: 2_000,
                copay: Money::from_cents(3_000),
            },
        ),
        (
            PayerId::Humana,
            PayerConfig {
                min_response_time_secs: 3.0,
                max_response_time_secs: 14.0,
                denial_rate: 0.09,
                max_not_allowed_bps: 1_300,
                max_deductible_bps: 1_400,
                coinsurance_bps: 1_800,
                copay: Money::from_cents(2_500),
            },
        ),
        (
            PayerId::BlueCrossBlueShield,
            PayerConfig {
                min_response_time_secs: 4.0,
                max_response_time_secs: 15.0,
                denial_rate: 0.12,
                max_not_allowed_bps: 1_600,
                max_deductible_bps: 1_700,
                coinsurance_bps: 2_200,
                copay: Money::from_cents(3_500),
            },
        ),
        (
            PayerId::KaiserPermanente,
            PayerConfig {
                min_response_time_secs: 1.0,
                max_response_time_secs: 6.0,
                denial_rate: 0.04,
                max_not_allowed_bps: 700,
                max_deductible_bps: 900,
                coinsurance_bps: 1_200,
                copay: Money::from_cents(1_500),
            },
        ),
        (
            PayerId::Centene,
            PayerConfig {
                min_response_time_secs: 5.0,
                max_response_time_secs: 20.0,
                denial_rate: 0.14,
                max_not_allowed_bps: 1_800,
                max_deductible_bps: 1_900,
                coinsurance_bps: 2_500,
                copay: Money::from_cents(3_000),
            },
        ),
        (
            PayerId::MolinaHealthcare,
            PayerConfig {
                min_response_time_secs: 6.0,
                max_response_time_secs: 25.0,
                denial_rate: 0.16,
                max_not_allowed_bps: 2_000,
                max_deductible_bps: 2_100,
                coinsurance_bps: 2_800,
                copay: Money::from_cents(3_500),
            },
        ),
    ])
}

/// Latency draw for one adjudication. Adjudication-family key (no attempt):
/// a re-delivered claim takes the same time and yields the same answer.
pub fn latency(cfg: &PayerConfig, rng: &RngFactory, claim_id: &ClaimId) -> Duration {
    let secs = rng
        .adjudication(claim_id, "latency")
        .random_range(cfg.min_response_time_secs..=cfg.max_response_time_secs);
    Duration::from_secs_f64(secs)
}

/// Honest adjudication. Splits are carved with integer floors and the payer
/// takes the exact remainder, so the reconciliation equation holds by
/// construction: billed == paid + coinsurance + copay + deductible + not_allowed.
pub fn adjudicate(claim: &SubmittedClaim, cfg: &PayerConfig, rng: &RngFactory) -> RemittanceAdvice {
    let lines = claim
        .lines
        .iter()
        .map(|line| adjudicate_line(&claim.claim_id, line, cfg, rng))
        .collect();
    RemittanceAdvice {
        claim_id: claim.claim_id.clone(),
        payer_id: claim.payer_id,
        lines,
    }
}

fn adjudicate_line(
    claim_id: &ClaimId,
    line: &SubmittedLine,
    cfg: &PayerConfig,
    rng: &RngFactory,
) -> RemittanceLine {
    let mut rng = rng.adjudication(claim_id, &format!("line/{}", line.service_line_id));
    let billed = line.billed();

    if rng.random_bool(cfg.denial_rate) {
        return RemittanceLine {
            service_line_id: line.service_line_id.clone(),
            payer_paid: Money::ZERO,
            coinsurance: Money::ZERO,
            copay: Money::ZERO,
            deductible: Money::ZERO,
            not_allowed: billed,
            denial_reason: Some(draw_denial_reason(&mut rng)),
        };
    }

    let not_allowed = billed.bps(rng.random_range(0..=cfg.max_not_allowed_bps));
    let allowed = billed - not_allowed;
    let deductible = allowed.bps(rng.random_range(0..=cfg.max_deductible_bps));
    let after_deductible = allowed - deductible;
    let coinsurance = after_deductible.bps(cfg.coinsurance_bps);
    let copay = cfg.copay.min(after_deductible - coinsurance);
    let payer_paid = after_deductible - coinsurance - copay;

    RemittanceLine {
        service_line_id: line.service_line_id.clone(),
        payer_paid,
        coinsurance,
        copay,
        deductible,
        not_allowed,
        denial_reason: None,
    }
}

fn draw_denial_reason(rng: &mut impl Rng) -> DenialReason {
    match rng.random_range(0..4) {
        0 => DenialReason::NotCovered,
        1 => DenialReason::PriorAuthRequired,
        2 => DenialReason::MedicalNecessity,
        _ => DenialReason::OutOfNetwork,
    }
}

/// Fault 3.1: dishonest mode. Keyed by the adjudication family (no attempt),
/// so a re-delivered claim reproduces the identical *lie* — duplicates stay
/// consistent, and the biller's only defense is the reconciliation equation.
/// Inflating not_allowed guarantees the books cannot sum, whatever the line's
/// honest split was.
pub fn apply_dishonesty(remit: &mut RemittanceAdvice, rng: &RngFactory) {
    let mut rng = rng.adjudication(&remit.claim_id, "dishonest/skim");
    let Some(line) = remit.lines.first_mut() else {
        return;
    };
    line.not_allowed += Money::from_cents(rng.random_range(1..=99));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submitted(claim_id: &str, cents: i64, units: u32) -> SubmittedClaim {
        SubmittedClaim {
            claim_id: ClaimId(claim_id.into()),
            payer_id: PayerId::Anthem,
            lines: vec![SubmittedLine {
                service_line_id: "L1".into(),
                procedure_code: "99213".into(),
                units,
                unit_charge: Money::from_cents(cents),
            }],
        }
    }

    #[test]
    fn honest_adjudication_balances_exactly_for_many_seeds_and_amounts() {
        let configs = default_payer_configs();
        for seed in 0..50 {
            let rng = RngFactory::new(seed);
            for (i, cents) in [1, 3, 99, 12_345, 1_000_000].into_iter().enumerate() {
                let claim = submitted(&format!("c-{seed}-{i}"), cents, 3);
                for cfg in configs.values() {
                    let remit = adjudicate(&claim, cfg, &rng);
                    for line in &remit.lines {
                        assert_eq!(line.accounted(), Money::from_cents(cents * 3));
                        assert!(line.payer_paid >= Money::ZERO);
                    }
                }
            }
        }
    }

    #[test]
    fn redelivery_reproduces_the_identical_remittance() {
        let rng = RngFactory::new(42);
        let cfg = default_payer_configs()[&PayerId::Anthem];
        let claim = submitted("c-1", 12_345, 2);
        let a = adjudicate(&claim, &cfg, &rng);
        let b = adjudicate(&claim, &cfg, &rng);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn latency_stays_within_configured_bounds() {
        let rng = RngFactory::new(42);
        let cfg = default_payer_configs()[&PayerId::Medicare];
        for i in 0..100 {
            let d = latency(&cfg, &rng, &ClaimId(format!("c-{i}")));
            assert!(d >= Duration::from_secs_f64(cfg.min_response_time_secs));
            assert!(d <= Duration::from_secs_f64(cfg.max_response_time_secs));
        }
    }
}
