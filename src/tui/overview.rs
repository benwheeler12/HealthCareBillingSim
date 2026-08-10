//! The Overview pane: the whole run on one page. Configuration on the left,
//! outcomes (claims funnel + money waterfall) on the right, and underneath
//! them the table that ties the two together — every fault the clearinghouse
//! injected (ground truth) next to the symptom it left on the biller's books.
//!
//! This pane absorbed the old Overview and Diagnostic panes: the diagnostic's
//! god's-eye comparison IS the link between simulation parameters and outputs,
//! so the two views belong on one screen.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::biller::policy::RetryPolicy;
use healthcare_billing_sim::domain::{PayerId, human_virtual};
use healthcare_billing_sim::reports::{days_in_ar, diagnostic, summarize};
use healthcare_billing_sim::sim::faults::FaultProfile;
use healthcare_billing_sim::sim::sim_truth::FaultKind;

use super::theme::{
    self, ACCENT, ALERT, BAD, GOOD, WARN, bold, dim, money, pct, swatch, thousands,
};

const BAR_WIDTH: usize = 44;

/// The actual configuration values the run executed with, captured from
/// `RunConfig` before the simulation consumed it — the "what went in" panel
/// renders these directly instead of a provenance one-liner.
pub struct ConfigFacts {
    pub seed: u64,
    pub rate_per_sec: f64,
    pub policy: RetryPolicy,
    pub faults: FaultProfile,
    /// Payers whose clearinghouse route overrides the shared fault profile.
    pub route_count: usize,
}

pub struct Overview {
    inputs: Vec<Line<'static>>,
    outcomes: Vec<Line<'static>>,
    links: Vec<Line<'static>>,
}

pub fn build(
    output: &RunOutput,
    banner_rows: &[(String, String)],
    facts: &ConfigFacts,
) -> Overview {
    Overview {
        inputs: input_lines(banner_rows, facts),
        outcomes: outcome_lines(output),
        links: link_lines(output),
    }
}

/// One row of the configuration table; long values wrap on their ` · `
/// separators so nothing is truncated.
fn push_input(lines: &mut Vec<Line<'static>>, label: &str, value: &str) {
    for (i, chunk) in wrap_segments(value, 52).into_iter().enumerate() {
        lines.push(match i {
            0 => Line::from(vec![
                Span::styled(format!(" {label:<13}"), bold()),
                Span::raw(chunk),
            ]),
            _ => Line::from(vec![Span::raw(" ".repeat(14)), Span::styled(chunk, dim())]),
        });
    }
}

/// The configuration breakdown: every value the run executed with, from the
/// captured config itself. Per-payer personalities live in the Payer
/// Scorecard pane, where they sit next to observed behavior.
fn input_lines(banner_rows: &[(String, String)], facts: &ConfigFacts) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some((_, value)) = banner_rows.iter().find(|(label, _)| label == "input") {
        push_input(&mut lines, "input", value);
    }
    push_input(&mut lines, "seed", &facts.seed.to_string());
    let per_min = facts.rate_per_sec * 60.0;
    let interval = if facts.rate_per_sec < 0.1 {
        format!(" (one every {})", human_virtual(1.0 / facts.rate_per_sec))
    } else {
        String::new()
    };
    push_input(
        &mut lines,
        "ingest rate",
        &format!("{} claims per virtual minute{interval}", fmt_rate(per_min)),
    );
    push_input(
        &mut lines,
        "retry policy",
        &format!(
            "{} attempts · {} timeout · {} backoff base",
            facts.policy.max_attempts,
            theme::human_compact(facts.policy.timeout.as_secs_f64()),
            theme::human_compact(facts.policy.backoff_base.as_secs_f64()),
        ),
    );
    push_input(&mut lines, "faults", &facts.faults.summary());
    push_input(
        &mut lines,
        "payers",
        &format!(
            "{} personalities, {} with route-specific faults",
            PayerId::ALL.len(),
            facts.route_count
        ),
    );
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(14)),
        Span::styled(
            "→ configured vs observed in Payer Scorecard [2]",
            theme::accent_bold(),
        ),
    ]));
    lines
}

/// "1.4" / "0.02" / "60" — enough precision to be honest at any rate scale.
fn fmt_rate(per_min: f64) -> String {
    match per_min {
        r if r >= 10.0 => format!("{r:.0}"),
        r if r >= 1.0 => format!("{r:.1}"),
        r => format!("{r:.2}"),
    }
}

/// Greedy wrap on ` · ` separators, falling back to word wrap for a segment
/// that is itself longer than the budget.
fn wrap_segments(value: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in value.split(" · ") {
        match out.last_mut() {
            Some(last) if last.chars().count() + seg.chars().count() + 3 <= width => {
                last.push_str(" · ");
                last.push_str(seg);
            }
            _ if seg.chars().count() <= width => out.push(seg.to_string()),
            _ => {
                let mut current = String::new();
                for word in seg.split(' ') {
                    if current.is_empty() {
                        current = word.to_string();
                    } else if current.chars().count() + word.chars().count() < width {
                        current.push(' ');
                        current.push_str(word);
                    } else {
                        out.push(std::mem::take(&mut current));
                        current = word.to_string();
                    }
                }
                if !current.is_empty() {
                    out.push(current);
                }
            }
        }
    }
    out
}

fn outcome_lines(output: &RunOutput) -> Vec<Line<'static>> {
    let s = summarize(&output.ledger);
    let dar = days_in_ar(&output.intake_ledger, output.intake_finished_at);
    let unaccounted = s.billed - s.payer_paid - s.patient_responsibility - s.not_allowed;
    let total = s.total_claims.max(1) as f64;
    let claim_pct = |n: usize| format!("{} ({})", thousands(n as u64), pct(n as f64 / total));
    let money_pct = |m: healthcare_billing_sim::domain::Money| {
        let share = if s.billed.cents() > 0 {
            m.cents() as f64 / s.billed.cents() as f64
        } else {
            0.0
        };
        format!("{} ({})", money(m), pct(share))
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" claims — ", bold()),
            Span::styled(thousands(s.total_claims as u64), theme::accent_bold()),
            Span::styled(" ingested, every one driven terminal", bold()),
        ]),
        bar(&[
            (s.resolved as f64, GOOD),
            (s.rejected as f64, WARN),
            (s.flagged as f64, BAD),
            (s.non_terminal as f64, Color::Magenta),
        ]),
        legend(&[
            (GOOD, "resolved", claim_pct(s.resolved)),
            (WARN, "rejected at ingest", claim_pct(s.rejected)),
        ]),
        legend(&[(BAD, "flagged for review", claim_pct(s.flagged))]),
    ];
    if s.non_terminal > 0 {
        lines.push(legend(&[(
            Color::Magenta,
            "non-terminal (BUG!)",
            claim_pct(s.non_terminal),
        )]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" money — ", bold()),
        Span::styled(money(s.billed), theme::accent_bold()),
        Span::styled(" billed", bold()),
    ]));
    lines.push(bar(&[
        (s.payer_paid.cents() as f64, GOOD),
        (s.patient_responsibility.cents() as f64, ACCENT),
        (s.not_allowed.cents() as f64, Color::Gray),
        (unaccounted.cents() as f64, BAD),
    ]));
    lines.push(legend(&[
        (GOOD, "payer paid", money_pct(s.payer_paid)),
        (ACCENT, "patient owes", money_pct(s.patient_responsibility)),
    ]));
    lines.push(legend(&[(
        Color::Gray,
        "not allowed (write-off)",
        money_pct(s.not_allowed),
    )]));
    if unaccounted.cents() > 0 {
        lines.push(legend(&[(
            BAD,
            "unanswered on flagged claims",
            money_pct(unaccounted),
        )]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" pace — ", bold()),
        Span::raw(format!(
            "intake ended {}, books drained by {}",
            output.intake_finished_at, output.finished_at
        )),
    ]));
    lines.push(Line::from(vec![
        Span::raw("        days in A/R at the intake snapshot: "),
        Span::styled(format!("{dar:.1}"), theme::accent_bold()),
        Span::styled("  (details in A/R Aging [1])", dim()),
    ]));
    lines
}

fn bar(segments: &[(f64, Color)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(theme::seg_bar(BAR_WIDTH, segments));
    Line::from(spans)
}

fn legend(entries: &[(Color, &str, String)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(pad_legend(entries));
    Line::from(spans)
}

fn pad_legend(entries: &[(Color, &str, String)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (color, label, value)) in entries.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.extend(swatch(*color, label, value.clone()));
    }
    spans
}

/// The cause→effect table: injected faults (ground truth the biller can never
/// see) against the symptoms they left on the books (which the biller lives
/// in). One row per fault kind that actually fired this run.
fn link_lines(output: &RunOutput) -> Vec<Line<'static>> {
    let d = diagnostic(&output.ledger, &output.sim_truth);

    let mut lines = vec![Line::from(vec![
        Span::styled(" injected (ground truth)", theme::accent_bold()),
        Span::styled("  →  ", dim()),
        Span::styled("what the biller's books show", theme::accent_bold()),
    ])];
    if d.truth.is_empty() {
        lines.push(Line::from(Span::styled(
            "   no faults injected — an honest, lossless run; the symptoms below should read zero",
            dim(),
        )));
    }
    for (kind, count) in &d.truth {
        let (name, symptom) = fault_row(*kind);
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>6} × ", thousands(*count as u64)), bold()),
            Span::raw(format!("{name:<16}")),
            Span::styled(symptom, dim()),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" on the books: ", bold()),
        Span::styled(thousands(d.retried_claims as u64), bold()),
        Span::styled(" claims needed retries · ", dim()),
        Span::styled(thousands(d.flagged_retries_exhausted as u64), bold()),
        Span::styled(" flagged retries-exhausted · ", dim()),
        Span::styled(thousands(d.flagged_reconciliation as u64), bold()),
        Span::styled(" flagged by reconciliation", dim()),
    ]));
    lines.push(Line::from(vec![
        Span::raw("               "),
        Span::styled(thousands(d.flagged_malformed as u64), bold()),
        Span::styled(" flagged malformed · ", dim()),
        Span::styled(thousands(d.quarantined as u64), bold()),
        Span::styled(" remittances quarantined · ", dim()),
        Span::styled(thousands(d.late_resolved as u64), bold()),
        Span::styled(" resolved late, after flagging", dim()),
    ]));

    let s = summarize(&output.ledger);
    lines.push(match s.non_terminal {
        0 => Line::from(Span::styled(
            " ✓ every claim still reached a terminal state, and the books balance to the cent",
            theme::bold().fg(GOOD),
        )),
        n => Line::from(Span::styled(
            format!(" ✗ {n} claims never reached a terminal state — this is a bug"),
            theme::bold().fg(ALERT),
        )),
    });
    lines
}

fn fault_row(kind: FaultKind) -> (&'static str, &'static str) {
    match kind {
        FaultKind::ForwardDrop => (
            "forward drop",
            "claim vanished en route — the biller saw silence, timed out, retried",
        ),
        FaultKind::ReturnDrop => (
            "return drop",
            "remittance lost coming back — same silence, another attempt burned",
        ),
        FaultKind::ExtraDelay => (
            "extra delay",
            "remittance dawdled in transit — timeouts emerge, none were injected",
        ),
        FaultKind::Duplicate => (
            "duplicate",
            "delivered twice — idempotent replay, the books didn't blink",
        ),
        FaultKind::Dishonest => (
            "dishonest math",
            "amounts don't sum — per-line reconciliation caught it, claim flagged",
        ),
        FaultKind::CorruptClaimId => (
            "corrupt id",
            "claim_id mangled in transit — remittance quarantined as a stray",
        ),
        FaultKind::LineDrop => (
            "line drop",
            "a service line vanished — partial adjudication, money left unanswered",
        ),
        FaultKind::CorruptRemittance => (
            "garbage remit",
            "unparseable bytes — treated as silence, the timeout machinery owns it",
        ),
    }
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, ov: &Overview, scroll: &mut u16) {
    let [hint_area, top, links_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(ov.inputs.len().max(ov.outcomes.len()) as u16 + 2),
        Constraint::Min(5),
    ])
    .areas(area);

    frame.render_widget(
        theme::hint(
            "the whole run on one page — what went in, what came out, and the fault \
             ledger linking the two · ↑/↓ scrolls the fault table",
        ),
        hint_area,
    );

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(top);
    frame.render_widget(
        Paragraph::new(ov.inputs.clone()).block(theme::panel("What went in — configuration")),
        left,
    );
    frame.render_widget(
        Paragraph::new(ov.outcomes.clone()).block(theme::panel("What came out — outcomes")),
        right,
    );

    theme::scrolled_paragraph(
        frame,
        links_area,
        theme::panel("Cause → effect — injected faults vs the books"),
        &ov.links,
        scroll,
    );
}
