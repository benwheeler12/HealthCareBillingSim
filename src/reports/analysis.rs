//! Per-provider A/R analysis: the chase list, aging books, and payer
//! signals for one billing organization, composed into a plain-English
//! document a biller can act on. Every figure is computed from the ledger —
//! nothing estimated, nothing generated — and the whole document is a pure
//! function of a snapshot + now, scoped strictly to the one provider.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::domain::{Money, PayerId, VirtualTime, human_virtual};
use crate::ledger::records::{ClaimState, FlagReason, Ledger};
use crate::reports::aging::{AgingBuckets, ar_aging_for, days_in_ar_for};
use crate::reports::chase::{ChaseItem, status_label};

/// Prose wraps at this column; tables may run wider (like the aging pane's).
const WRAP: usize = 76;
const VIRTUAL_DAY_SECS: f64 = 86_400.0;
/// How many chase items the document calls out by name.
const TOP_CHASE: usize = 5;

/// The document, top to bottom: headline, where the money is stuck (aging),
/// what the open claims are doing (status mix), payer signals, the top chase
/// items by risk, and recommended actions. Section rules are deliberately
/// transparent — each line states the numbers that triggered it.
pub fn provider_analysis(ledger: &Ledger, now: VirtualTime, provider: &str) -> String {
    let mut doc = String::new();
    let _ = writeln!(
        doc,
        "{provider} — A/R analysis at run end ({} elapsed)",
        human_virtual(now.as_duration().as_secs_f64())
    );

    let open = open_items(ledger, now, provider);
    if open.is_empty() {
        doc.push('\n');
        doc.push_str("  nothing outstanding — every receivable booked\n");
        return doc;
    }

    let outstanding: Money = open.iter().map(|i| i.outstanding).sum();
    let aging = ar_aging_for(ledger, now, Some(provider)).payer;
    let totals = fold_buckets(&aging);
    let mix = status_mix(ledger, provider);
    let payers = payer_stats(ledger, provider, &aging);

    headline(&mut doc, outstanding, open.len(), ledger, now, provider);
    where_stuck(&mut doc, &aging, &totals, outstanding);
    open_claims_doing(&mut doc, &mix);
    payer_signals(&mut doc, &payers, outstanding);
    top_chase(&mut doc, &open);
    recommendations(&mut doc, &mix, &aging, &totals);
    doc
}

/// This provider's open receivables, riskiest first — the same rows the
/// chase list derives, filtered before the portfolio-wide sort.
fn open_items(ledger: &Ledger, now: VirtualTime, provider: &str) -> Vec<ChaseItem> {
    let mut items: Vec<ChaseItem> = ledger
        .claims
        .values()
        .filter(|r| r.payer_outstanding() > Money::ZERO)
        .filter_map(|r| {
            let identity = r.identity.as_ref()?;
            if identity.organization_name != provider {
                return None;
            }
            Some(ChaseItem {
                claim_id: r.claim_id.clone(),
                payer_id: identity.payer_id,
                provider: identity.organization_name.clone(),
                outstanding: r.payer_outstanding(),
                age: r.age(now).unwrap_or_default(),
                attempts: r.attempts,
                status: status_label(r),
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b.risk()
            .partial_cmp(&a.risk())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.claim_id.cmp(&b.claim_id))
    });
    items
}

/// Open-claim counts by what the claim is doing right now, mirroring the
/// chase list's plain-English labels (partial wins over retrying).
#[derive(Default)]
struct StatusMix {
    pending: usize,
    awaiting_first: usize,
    retrying: usize,
    partial: usize,
    /// Outstanding money hanging on partially-answered claims.
    partial_money: Money,
    gave_up: usize,
    gave_up_money: Money,
    unreconciled: usize,
    unreconciled_money: Money,
    malformed: usize,
    malformed_money: Money,
}

impl StatusMix {
    fn flagged(&self) -> usize {
        self.gave_up + self.unreconciled + self.malformed
    }
}

fn status_mix(ledger: &Ledger, provider: &str) -> StatusMix {
    let mut mix = StatusMix::default();
    for record in ledger.claims.values() {
        let open = record.payer_outstanding();
        let ours = record
            .identity
            .as_ref()
            .is_some_and(|id| id.organization_name == provider);
        if !ours || open == Money::ZERO {
            continue;
        }
        let answered = record
            .lines
            .iter()
            .any(|l| !l.do_not_bill && l.adjudication.is_some());
        match &record.state {
            ClaimState::Pending => mix.pending += 1,
            ClaimState::AwaitingResponse { .. } if answered => {
                mix.partial += 1;
                mix.partial_money += open;
            }
            ClaimState::AwaitingResponse { .. } if record.attempts > 1 => mix.retrying += 1,
            ClaimState::AwaitingResponse { .. } => mix.awaiting_first += 1,
            ClaimState::Flagged {
                reason: FlagReason::RetriesExhausted,
            } => {
                mix.gave_up += 1;
                mix.gave_up_money += open;
            }
            ClaimState::Flagged {
                reason: FlagReason::ReconciliationFailed { .. },
            } => {
                mix.unreconciled += 1;
                mix.unreconciled_money += open;
            }
            ClaimState::Flagged {
                reason: FlagReason::MalformedRemittance,
            } => {
                mix.malformed += 1;
                mix.malformed_money += open;
            }
            // Resolved and Rejected claims carry no payer outstanding.
            ClaimState::Resolved | ClaimState::Rejected { .. } => {}
        }
    }
    mix
}

/// Per-payer facts over this provider's book alone — open money plus the
/// response/denial experience, so signals never lean on portfolio data.
struct PayerFacts {
    payer: PayerId,
    open: Money,
    buckets: AgingBuckets,
    flagged_open: usize,
    response_sum: f64,
    responses: usize,
    adjudicated_lines: usize,
    denied_lines: usize,
}

fn payer_stats(
    ledger: &Ledger,
    provider: &str,
    aging: &BTreeMap<PayerId, AgingBuckets>,
) -> Vec<PayerFacts> {
    let mut facts: BTreeMap<PayerId, PayerFacts> = BTreeMap::new();
    for record in ledger.claims.values() {
        let Some(identity) = &record.identity else {
            continue;
        };
        if identity.organization_name != provider {
            continue;
        }
        let f = facts.entry(identity.payer_id).or_insert(PayerFacts {
            payer: identity.payer_id,
            open: Money::ZERO,
            buckets: AgingBuckets::default(),
            flagged_open: 0,
            response_sum: 0.0,
            responses: 0,
            adjudicated_lines: 0,
            denied_lines: 0,
        });
        let open = record.payer_outstanding();
        f.open += open;
        if open > Money::ZERO && matches!(record.state, ClaimState::Flagged { .. }) {
            f.flagged_open += 1;
        }
        let first_answer = record
            .lines
            .iter()
            .filter_map(|l| l.adjudication.as_ref())
            .map(|adj| adj.adjudicated_at)
            .min();
        if let (Some(submitted), Some(answered)) = (record.first_submitted_at, first_answer) {
            f.response_sum += answered.saturating_since(submitted).as_secs_f64();
            f.responses += 1;
        }
        for line in record.lines.iter().filter(|l| !l.do_not_bill) {
            if let Some(adj) = &line.adjudication {
                f.adjudicated_lines += 1;
                f.denied_lines += usize::from(adj.denial_reason.is_some());
            }
        }
    }
    let mut facts: Vec<PayerFacts> = facts
        .into_values()
        .filter(|f| f.open > Money::ZERO)
        .map(|mut f| {
            f.buckets = aging.get(&f.payer).copied().unwrap_or_default();
            f
        })
        .collect();
    // Biggest open balance first; name breaks the tie deterministically.
    facts.sort_by(|a, b| b.open.cmp(&a.open).then_with(|| a.payer.cmp(&b.payer)));
    facts
}

impl PayerFacts {
    /// Concern rules, stated with the numbers that trip them: a quarter of
    /// the balance past 90 days (or 40% past 60), any open flags, or a
    /// denial rate at or above 15%.
    fn concerns(&self) -> Vec<String> {
        let mut concerns = Vec::new();
        let total = self.buckets.total();
        if total > Money::ZERO {
            let over_90 = frac(self.buckets.d90_plus, total);
            let over_60 = frac(self.buckets.d61_90 + self.buckets.d90_plus, total);
            if over_90 >= 0.25 {
                concerns.push(format!("{} of its balance is past 90 days", pct(over_90)));
            } else if over_60 >= 0.40 {
                concerns.push(format!("{} of its balance is past 60 days", pct(over_60)));
            }
        }
        if self.flagged_open > 0 {
            concerns.push(format!(
                "{} open claim{} flagged for review",
                self.flagged_open,
                plural(self.flagged_open)
            ));
        }
        let denial_rate = if self.adjudicated_lines > 0 {
            self.denied_lines as f64 / self.adjudicated_lines as f64
        } else {
            0.0
        };
        if denial_rate >= 0.15 {
            concerns.push(format!("denial rate {}", pct(denial_rate)));
        }
        concerns
    }
}

fn headline(
    doc: &mut String,
    outstanding: Money,
    open: usize,
    ledger: &Ledger,
    now: VirtualTime,
    provider: &str,
) {
    doc.push_str("\nHEADLINE\n");
    let days = days_in_ar_for(ledger, now, Some(provider));
    wrapped(
        doc,
        &format!(
            "{} outstanding across {open} open claim{} · days in A/R: {days:.1}",
            money(outstanding),
            plural(open)
        ),
    );
}

fn where_stuck(
    doc: &mut String,
    aging: &BTreeMap<PayerId, AgingBuckets>,
    totals: &AgingBuckets,
    outstanding: Money,
) {
    doc.push_str("\nWHERE THE MONEY IS STUCK\n");
    let mut prose = String::new();
    if totals.d90_plus > Money::ZERO {
        let _ = write!(
            prose,
            "{} ({}) has already aged past 90 days{}.",
            money(totals.d90_plus),
            pct(frac(totals.d90_plus, outstanding)),
            concentration(aging)
        );
    }
    if totals.d61_90 > Money::ZERO {
        if !prose.is_empty() {
            prose.push(' ');
        }
        let _ = write!(
            prose,
            "{} ({}) sits at 61–90 days and crosses the 90-day line without action.",
            money(totals.d61_90),
            pct(frac(totals.d61_90, outstanding))
        );
    }
    if prose.is_empty() {
        prose = format!(
            "Nothing has aged past 60 days: {} is under 30 days old and {} at 31–60 days.",
            money(totals.d0_30),
            money(totals.d31_60)
        );
    }
    wrapped(doc, &prose);
    doc.push('\n');

    let cell = |m: Money| {
        if m == Money::ZERO {
            "–".to_string()
        } else {
            money(m)
        }
    };
    let _ = writeln!(
        doc,
        "  {:<22} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "payer", "0-30d", "31-60d", "61-90d", "90+d", "total"
    );
    for (payer, b) in aging {
        let _ = writeln!(
            doc,
            "  {:<22} {:>12} {:>12} {:>12} {:>12} {:>12}",
            payer.as_str(),
            cell(b.d0_30),
            cell(b.d31_60),
            cell(b.d61_90),
            cell(b.d90_plus),
            money(b.total()),
        );
    }
    let _ = writeln!(
        doc,
        "  {:<22} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "TOTAL",
        cell(totals.d0_30),
        cell(totals.d31_60),
        cell(totals.d61_90),
        cell(totals.d90_plus),
        money(totals.total()),
    );
}

/// "— concentrated with cigna ($44,210.00) and humana ($16,794.00)" for the
/// one or two payers holding the most 90+ money; empty when nothing is there.
fn concentration(aging: &BTreeMap<PayerId, AgingBuckets>) -> String {
    let mut holders: Vec<(&PayerId, Money)> = aging
        .iter()
        .filter(|(_, b)| b.d90_plus > Money::ZERO)
        .map(|(p, b)| (p, b.d90_plus))
        .collect();
    holders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    match holders.as_slice() {
        [] => String::new(),
        [(p, m)] => format!(" — all of it with {} ({})", p.as_str(), money(*m)),
        [(p1, m1), (p2, m2), ..] => format!(
            " — concentrated with {} ({}) and {} ({})",
            p1.as_str(),
            money(*m1),
            p2.as_str(),
            money(*m2)
        ),
    }
}

fn open_claims_doing(doc: &mut String, mix: &StatusMix) {
    doc.push_str("\nWHAT THE OPEN CLAIMS ARE DOING\n");
    let mut parts = Vec::new();
    if mix.pending > 0 {
        parts.push(format!("{} pending submission", mix.pending));
    }
    if mix.awaiting_first > 0 {
        parts.push(format!("{} awaiting a first response", mix.awaiting_first));
    }
    if mix.retrying > 0 {
        parts.push(format!("{} retrying", mix.retrying));
    }
    if mix.partial > 0 {
        parts.push(format!(
            "{} partial remit{} (some lines answered, some lost)",
            mix.partial,
            plural(mix.partial)
        ));
    }
    if mix.flagged() > 0 {
        parts.push(format!("{} flagged for human review", mix.flagged()));
    }
    wrapped(doc, &parts.join(" · "));
    if mix.flagged() > 0 {
        let mut flags = Vec::new();
        if mix.gave_up > 0 {
            flags.push(format!("{} gave up after exhausting retries", mix.gave_up));
        }
        if mix.unreconciled > 0 {
            flags.push(format!("{} don't reconcile to the cent", mix.unreconciled));
        }
        if mix.malformed > 0 {
            flags.push(format!(
                "{} unusable remittance{}",
                mix.malformed,
                plural(mix.malformed)
            ));
        }
        wrapped(doc, &format!("flagged breakdown: {}", flags.join(", ")));
    }
}

fn payer_signals(doc: &mut String, payers: &[PayerFacts], outstanding: Money) {
    doc.push_str("\nPAYER SIGNALS\n");
    for f in payers {
        let concerns = f.concerns();
        let marker = if concerns.is_empty() { '●' } else { '▲' };
        let mut line = format!(
            "{marker} {} — {} open ({} of open A/R)",
            f.payer.as_str(),
            money(f.open),
            pct(frac(f.open, outstanding))
        );
        if f.responses > 0 {
            let _ = write!(
                line,
                " · avg response {}",
                human_virtual(f.response_sum / f.responses as f64)
            );
        } else {
            line.push_str(" · no answer received yet");
        }
        if concerns.is_empty() {
            line.push_str(" · balance young, nothing flagged — normal follow-up cadence");
        } else {
            for concern in concerns {
                let _ = write!(line, " · {concern}");
            }
        }
        wrapped(doc, &line);
    }
}

fn top_chase(doc: &mut String, open: &[ChaseItem]) {
    doc.push_str("\nTOP CHASE ITEMS (risk = dollars stuck × days stuck)\n");
    for (i, item) in open.iter().take(TOP_CHASE).enumerate() {
        wrapped(
            doc,
            &format!(
                "{}. {} · {} · {} · {:.1}d · {}",
                i + 1,
                item.claim_id,
                item.payer_id.as_str(),
                money(item.outstanding),
                item.age.as_secs_f64() / VIRTUAL_DAY_SECS,
                item.status
            ),
        );
    }
    if open.len() > TOP_CHASE {
        wrapped(
            doc,
            &format!(
                "… and {} more open claims below them",
                open.len() - TOP_CHASE
            ),
        );
    }
}

fn recommendations(
    doc: &mut String,
    mix: &StatusMix,
    aging: &BTreeMap<PayerId, AgingBuckets>,
    totals: &AgingBuckets,
) {
    doc.push_str("\nRECOMMENDED ACTIONS\n");
    let mut recs = Vec::new();
    if mix.gave_up > 0 {
        recs.push(format!(
            "Escalate the {} claim{} flagged \"no response, gave up\" ({}) by phone \
             or portal — the biller's retry budget is spent, so nothing further \
             happens without a human.",
            mix.gave_up,
            plural(mix.gave_up),
            money(mix.gave_up_money)
        ));
    }
    if totals.d61_90 > Money::ZERO {
        let holder = aging
            .iter()
            .filter(|(_, b)| b.d61_90 > Money::ZERO)
            .max_by(|a, b| a.1.d61_90.cmp(&b.1.d61_90).then_with(|| b.0.cmp(a.0)));
        let holder = holder
            .map(|(p, b)| format!("; {} holds the most ({})", p.as_str(), money(b.d61_90)))
            .unwrap_or_default();
        recs.push(format!(
            "Work the {} aged 61–90 days before it crosses the 90-day line{holder}.",
            money(totals.d61_90)
        ));
    }
    if mix.unreconciled > 0 {
        recs.push(format!(
            "Dispute the {} reconciliation failure{} ({}) with the payer — the books \
             refuse figures that don't sum to the cent.",
            mix.unreconciled,
            plural(mix.unreconciled),
            money(mix.unreconciled_money)
        ));
    }
    if mix.malformed > 0 {
        recs.push(format!(
            "Request corrected remittances for the {} claim{} whose remittance was \
             unusable ({}).",
            mix.malformed,
            plural(mix.malformed),
            money(mix.malformed_money)
        ));
    }
    if mix.partial > 0 {
        recs.push(format!(
            "{} partial remit{} {} {} hanging on unanswered lines — resubmission \
             is already scheduled, but verify the missing lines with the payer.",
            mix.partial,
            plural(mix.partial),
            if mix.partial == 1 { "has" } else { "have" },
            money(mix.partial_money)
        ));
    }
    if recs.is_empty() {
        recs.push(
            "No urgent action — every open receivable is young and still inside \
             normal follow-up."
                .to_string(),
        );
    }
    for (i, rec) in recs.iter().enumerate() {
        wrapped(doc, &format!("{}. {rec}", i + 1));
    }
}

fn fold_buckets(aging: &BTreeMap<PayerId, AgingBuckets>) -> AgingBuckets {
    let mut totals = AgingBuckets::default();
    for b in aging.values() {
        totals.d0_30 += b.d0_30;
        totals.d31_60 += b.d31_60;
        totals.d61_90 += b.d61_90;
        totals.d90_plus += b.d90_plus;
    }
    totals
}

/// $1,234,567.89 — the document quotes money with thousands separators, the
/// way the pane's provider table (and the sample the layout was approved
/// against) prints headline figures.
fn money(m: Money) -> String {
    let cents = m.cents();
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let digits = (abs / 100).to_string();
    let mut whole = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        // No `is_multiple_of` — the repo builds on rustc 1.85, which predates it.
        if i > 0 && (digits.len() - i) % 3 == 0 {
            whole.push(',');
        }
        whole.push(c);
    }
    format!("{sign}${whole}.{:02}", abs % 100)
}

fn frac(part: Money, whole: Money) -> f64 {
    if whole == Money::ZERO {
        0.0
    } else {
        part.cents() as f64 / whole.cents() as f64
    }
}

fn pct(f: f64) -> String {
    format!("{:.0}%", f * 100.0)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Word-wrap one paragraph at WRAP columns, two-space indent throughout —
/// numbered items and bullets hang their continuation lines one step deeper.
fn wrapped(doc: &mut String, text: &str) {
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return;
    };
    let hanging = first.chars().count() <= 2 && (first.ends_with('.') || "▲●…".contains(first));
    let rest_indent = if hanging { "     " } else { "  " };
    let mut line = format!("  {first}");
    for word in words {
        if line.chars().count() + 1 + word.chars().count() > WRAP {
            doc.push_str(&line);
            doc.push('\n');
            line = format!("{rest_indent}{word}");
        } else {
            line.push(' ');
            line.push_str(word);
        }
    }
    doc.push_str(&line);
    doc.push('\n');
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::ClaimId;
    use crate::ledger::records::{Adjudication, ClaimIdentity, ClaimRecord, LineRecord};

    const DAY: Duration = Duration::from_secs(86_400);

    /// The fixture's single "now": 100 virtual days into the run.
    fn now() -> VirtualTime {
        VirtualTime::default() + DAY * 100
    }

    fn line(cents: i64, answered: bool) -> LineRecord {
        LineRecord {
            service_line_id: "L1".into(),
            procedure_code: "99213".into(),
            units: 1,
            unit_charge: Money::from_cents(cents),
            do_not_bill: false,
            adjudication: answered.then(|| Adjudication {
                payer_paid: Money::from_cents(cents),
                coinsurance: Money::ZERO,
                copay: Money::ZERO,
                deductible: Money::ZERO,
                not_allowed: Money::ZERO,
                denial_reason: None,
                adjudicated_at: VirtualTime::default() + DAY * 60,
            }),
        }
    }

    fn record(
        id: &str,
        provider: &str,
        payer: PayerId,
        state: ClaimState,
        attempts: u32,
        age_days: u32,
        lines: Vec<LineRecord>,
    ) -> ClaimRecord {
        let submitted = VirtualTime::default()
            + (now().as_duration().checked_sub(DAY * age_days)).expect("age fits inside the run");
        ClaimRecord {
            claim_id: ClaimId(id.into()),
            identity: Some(ClaimIdentity {
                payer_id: payer,
                provider_npi: "1234567890".into(),
                organization_name: provider.into(),
                patient_member_id: "M-1".into(),
            }),
            state,
            attempts,
            ingested_at: VirtualTime::default(),
            first_submitted_at: Some(submitted),
            resolved_at: None,
            lines,
            history: Vec::new(),
        }
    }

    fn awaiting() -> ClaimState {
        ClaimState::AwaitingResponse {
            timeout_at: VirtualTime::default(),
        }
    }

    /// Prose wraps mid-sentence; collapse whitespace so assertions can match
    /// full phrases regardless of where the wrap falls.
    fn flat(doc: &str) -> String {
        doc.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// One provider with four open claims across two payers plus a second
    /// provider that must never leak into the document.
    fn ledger() -> (Ledger, VirtualTime) {
        let mut ledger = Ledger::default();
        let records = vec![
            // $500 × 100d with cigna, still waiting on the first answer.
            record(
                "c-1",
                "Clinic A",
                PayerId::Cigna,
                awaiting(),
                1,
                100,
                vec![line(50_000, false)],
            ),
            // $300 × 95d with cigna, retries exhausted — the escalation case.
            record(
                "c-2",
                "Clinic A",
                PayerId::Cigna,
                ClaimState::Flagged {
                    reason: FlagReason::RetriesExhausted,
                },
                3,
                95,
                vec![line(30_000, false)],
            ),
            // $200 × 10d with aetna, on its third attempt.
            record(
                "c-3",
                "Clinic A",
                PayerId::Aetna,
                awaiting(),
                3,
                10,
                vec![line(20_000, false)],
            ),
            // $100 open × 45d with aetna: one line booked, one unanswered.
            record(
                "c-4",
                "Clinic A",
                PayerId::Aetna,
                awaiting(),
                2,
                45,
                vec![line(15_000, true), line(10_000, false)],
            ),
            // A different organization entirely.
            record(
                "c-9",
                "Clinic B",
                PayerId::Humana,
                awaiting(),
                1,
                30,
                vec![line(5_000, false)],
            ),
        ];
        for r in records {
            ledger.claims.insert(r.claim_id.clone(), r);
        }
        (ledger, now())
    }

    #[test]
    fn headline_totals_the_open_book() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        assert!(doc.contains("Clinic A — A/R analysis"), "{doc}");
        assert!(
            flat(&doc).contains("$1,100.00 outstanding across 4 open claims"),
            "{doc}"
        );
    }

    #[test]
    fn aging_names_the_90_plus_holder_and_the_61_90_pile() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        // c-1 + c-2 = $800 past 90 days, all with cigna.
        assert!(
            flat(&doc).contains("$800.00 (73%) has already aged past 90 days"),
            "{doc}"
        );
        assert!(
            flat(&doc).contains("all of it with cigna ($800.00)"),
            "{doc}"
        );
        // c-4's open line is 45 days old — nothing sits at 61–90.
        assert!(!flat(&doc).contains("61–90 days and crosses"), "{doc}");
    }

    #[test]
    fn status_mix_reads_like_the_chase_list() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        assert!(flat(&doc).contains("1 awaiting a first response"), "{doc}");
        assert!(flat(&doc).contains("1 retrying"), "{doc}");
        assert!(flat(&doc).contains("1 partial remit"), "{doc}");
        assert!(flat(&doc).contains("1 flagged for human review"), "{doc}");
        assert!(
            flat(&doc).contains("1 gave up after exhausting retries"),
            "{doc}"
        );
    }

    #[test]
    fn payer_signals_flag_cigna_and_clear_aetna() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        // cigna: $800 of $1100, 100% past 90 days, one flagged claim.
        assert!(
            flat(&doc).contains("▲ cigna — $800.00 open (73% of open A/R)"),
            "{doc}"
        );
        assert!(
            flat(&doc).contains("100% of its balance is past 90 days"),
            "{doc}"
        );
        assert!(
            flat(&doc).contains("1 open claim flagged for review"),
            "{doc}"
        );
        // aetna's balance is young and unflagged.
        assert!(
            flat(&doc).contains("● aetna — $300.00 open (27% of open A/R)"),
            "{doc}"
        );
        assert!(flat(&doc).contains("normal follow-up cadence"), "{doc}");
    }

    #[test]
    fn chase_items_rank_by_risk() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        // risk: c-1 50,000 > c-2 28,500 > c-4 4,500 > c-3 2,000.
        let pos = |needle: &str| {
            doc.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing:\n{doc}"))
        };
        assert!(pos("1. c-1") < pos("2. c-2"), "{doc}");
        assert!(pos("2. c-2") < pos("3. c-4"), "{doc}");
        assert!(pos("3. c-4") < pos("4. c-3"), "{doc}");
    }

    #[test]
    fn recommendations_escalate_flags_and_verify_partials() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        assert!(
            flat(&doc).contains("Escalate the 1 claim flagged \"no response, gave up\" ($300.00)"),
            "{doc}"
        );
        assert!(
            flat(&doc).contains("verify the missing lines with the payer"),
            "{doc}"
        );
    }

    #[test]
    fn document_is_scoped_to_the_one_provider() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        assert!(!doc.contains("humana"), "{doc}");
        assert!(!doc.contains("c-9"), "{doc}");
        assert!(!doc.contains("Clinic B"), "{doc}");
    }

    #[test]
    fn empty_book_reads_as_such() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic Z");
        assert!(doc.contains("nothing outstanding"), "{doc}");
    }

    #[test]
    fn prose_wraps_inside_the_pane() {
        let (ledger, now) = ledger();
        let doc = provider_analysis(&ledger, now, "Clinic A");
        for line in doc.lines().filter(|l| !l.contains("payer")) {
            // Tables may run wider; prose must not.
            if !line.starts_with("  $") && !line.contains(">") {
                assert!(
                    line.chars().count() <= 90,
                    "line too wide ({}): {line}",
                    line.chars().count()
                );
            }
        }
    }
}
