//! Report-suite tests. The centerpiece is the closed loop: configure a payer
//! slow and denial-happy in the *simulation*, then watch the scorecard
//! rediscover exactly that from remittance data alone — the reports never see
//! the sim's config.

mod common;

use std::time::Duration;

use common::{run_sim_with, simple_claim, write_input};
use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::domain::{Money, PayerId};
use healthcare_billing_sim::reports::{
    ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic, payer_scorecard,
};
use healthcare_billing_sim::sim::sim_truth::FaultKind;

#[test]
fn scorecard_rediscovers_injected_payer_personalities() {
    let input: Vec<String> = (0..90).map(simple_claim).collect();
    let path = write_input("scorecard.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    {
        let anthem = cfg.payers.get_mut(&PayerId::Anthem).expect("config");
        anthem.min_response_time_secs = 300.0;
        anthem.max_response_time_secs = 900.0;
        anthem.denial_rate = 0.6;
    }
    cfg.policy.timeout = Duration::from_secs(1000);

    let output = run_sim_with(cfg);
    let scores = payer_scorecard(&output.ledger);
    let anthem = &scores[&PayerId::Anthem];
    let medicare = &scores[&PayerId::Medicare];

    // Slow: rediscovered response time sits inside the injected bounds and
    // dwarfs medicare's.
    assert!(anthem.avg_response_secs >= 300.0 && anthem.avg_response_secs <= 900.0);
    assert!(medicare.avg_response_secs <= 5.0);
    // Denial-happy: rediscovered rate is in the injected neighborhood.
    assert!(
        (0.4..=0.8).contains(&anthem.denial_rate),
        "anthem denial rate {} should reflect the injected 0.6",
        anthem.denial_rate
    );
    assert!(medicare.denial_rate <= 0.2);
    // Denials pay less of billed.
    assert!(anthem.paid_to_billed < medicare.paid_to_billed);

    // Denial breakdown agrees: anthem dominates denied dollars.
    let denials = denial_breakdown(&output.ledger);
    let denied_dollars = |payer: PayerId| -> Money {
        denials
            .iter()
            .filter(|((p, _), _)| *p == payer)
            .map(|(_, (_, money))| *money)
            .sum()
    };
    assert!(denied_dollars(PayerId::Anthem) > denied_dollars(PayerId::Medicare));
}

#[test]
fn aging_buckets_spread_when_submissions_span_months() {
    // 200 claims over ~115 virtual days (one claim per ~14 virtual hours),
    // with enough drops and a tight budget that flagged claims accumulate
    // across the whole timeline.
    let input: Vec<String> = (0..200).map(simple_claim).collect();
    let path = write_input("aging.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 1.0 / 50_000.0);
    cfg.faults.forward_drop_rate = 0.45;
    cfg.policy.max_attempts = 2;

    let output = run_sim_with(cfg);
    let aging = ar_aging(&output.ledger, output.finished_at);

    let mut grand = healthcare_billing_sim::reports::aging::AgingBuckets::default();
    for buckets in aging.payer.values() {
        grand.d0_30 += buckets.d0_30;
        grand.d31_60 += buckets.d31_60;
        grand.d61_90 += buckets.d61_90;
        grand.d90_plus += buckets.d90_plus;
    }
    // Flagged (unbooked) receivables exist in every age bucket.
    assert!(grand.d0_30 > Money::ZERO, "young receivables missing");
    assert!(grand.d31_60 > Money::ZERO, "31-60d receivables missing");
    assert!(grand.d61_90 > Money::ZERO, "61-90d receivables missing");
    assert!(grand.d90_plus > Money::ZERO, "90+d receivables missing");

    // The aging total is exactly the sum of open claims' outstanding.
    let outstanding: Money = output
        .ledger
        .claims
        .values()
        .map(|r| r.payer_outstanding())
        .sum();
    assert_eq!(grand.total(), outstanding);

    // Patient responsibility exists and aged from adjudication.
    let patient_total: Money = aging.patient.values().map(|b| b.total()).sum::<Money>();
    assert!(patient_total > Money::ZERO);

    // Days in A/R is positive and sane (can't exceed elapsed days).
    let dar = days_in_ar(&output.ledger, output.finished_at);
    let elapsed_days = output.finished_at.as_duration().as_secs_f64() / 86_400.0;
    assert!(dar > 0.0 && dar <= elapsed_days);

    // Chase list: sorted by outstanding desc then age desc, only open claims.
    let chase = chase_list(&output.ledger, output.finished_at, 25);
    assert!(!chase.is_empty());
    for pair in chase.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(
            a.outstanding > b.outstanding || (a.outstanding == b.outstanding && a.age >= b.age),
            "chase list out of order"
        );
    }
    for item in &chase {
        assert!(item.outstanding > Money::ZERO);
    }
}

#[test]
fn diagnostic_squares_ledger_view_with_ground_truth() {
    let input: Vec<String> = (0..40).map(simple_claim).collect();
    let path = write_input("diag.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.forward_drop_rate = 0.3;
    cfg.faults.dishonest_adjudication_rate = 0.15;

    let output = run_sim_with(cfg);
    let diag = diagnostic(&output.ledger, &output.sim_truth);

    // Biller's view: dishonesty count matches ground truth exactly (each lie
    // is per-claim and always caught).
    let liars = output.sim_truth.claims_with(FaultKind::Dishonest).len();
    assert_eq!(diag.flagged_reconciliation, liars);

    // Every claim that retried really was struck by a transport fault — the
    // biller never times out spuriously in an honest-latency world.
    let struck = output.sim_truth.claims_with(FaultKind::ForwardDrop);
    let retried: Vec<_> = output
        .ledger
        .claims
        .values()
        .filter(|r| r.attempts > 1)
        .collect();
    assert_eq!(diag.retried_claims, retried.len());
    for record in retried {
        assert!(
            struck.contains(&record.claim_id),
            "claim {} retried without a ground-truth cause",
            record.claim_id
        );
    }
    assert!(diag.truth.iter().any(|(k, _)| *k == FaultKind::ForwardDrop));
}
