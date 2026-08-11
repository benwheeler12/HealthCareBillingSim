//! The A/R Aging pane: a master–detail view of the books **as of one moment
//! in virtual time**. A provider list on the left (whole portfolio first,
//! then each billing organization by open payer A/R) drives the report on
//! the right — the outcome bars (claims funnel + money waterfall) above the
//! aging tables with their colored buckets and mix bars, every number
//! derived from the same as-of snapshot, with the timeline scrubber pinned
//! at the bottom of that same window. The view opens at the end of intake
//! (the mid-flight moment a biller actually lives in). Arrow keys, Enter,
//! and Esc drive everything: ↑/↓ pick a book, Enter steps into the report
//! window — there ↑/↓ scroll the document and ←/→ scrub the as-of day
//! across the whole run (hold an arrow for a second and the scrub
//! fast-forwards to four days per step) — and Esc steps back out.

use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::domain::{Money, PayerId, VirtualTime};
use healthcare_billing_sim::ledger::fold::snapshot_at;
use healthcare_billing_sim::ledger::records::Ledger;
use healthcare_billing_sim::reports::aging::AgingBuckets;
use healthcare_billing_sim::reports::{ar_aging_for, days_in_ar_for, summarize_for};

use super::theme::{
    self, ACCENT, ALERT, BAD, GOOD, WARN, bold, dim, money, pct, swatch, thousands,
};

/// Young → old, the same hues the legend and mix bars use.
const BUCKET_COLORS: [Color; 4] = [GOOD, WARN, ALERT, BAD];
/// Sized so a full table row (label + five money columns + bar) fits the
/// detail panel of a 160-column terminal without clipping.
const MIX_WIDTH: usize = 12;

const VIRTUAL_DAY: Duration = Duration::from_secs(86_400);

/// A held arrow reaches the TUI as a stream of ordinary key events (terminal
/// auto-repeat) — presses in the same direction closer together than this
/// belong to one hold. The bound also clears most terminals' initial repeat
/// delay, so a hold survives its own start.
const REPEAT_GAP: Duration = Duration::from_millis(600);
/// Hold an arrow this long and the scrub shifts into fast-forward…
const TURBO_AFTER: Duration = Duration::from_secs(1);
/// …stepping this many virtual days per repeat instead of one.
const TURBO_DAYS: u32 = 4;

/// Hold detection for the timeline scrubber, inferred purely from key
/// cadence — legacy terminals never send key-up, so the release is the
/// silence after the last repeat.
#[derive(Default)]
struct Hold(Option<HoldState>);

#[derive(Clone, Copy)]
struct HoldState {
    dir: isize,
    started: Instant,
    last: Instant,
}

impl Hold {
    /// Record one press and return how many days it steps: 1, or
    /// `TURBO_DAYS` once the hold is `TURBO_AFTER` old. A direction flip or
    /// a gap longer than auto-repeat starts a fresh hold.
    fn press(&mut self, dir: isize, now: Instant) -> u32 {
        match &mut self.0 {
            Some(h) if h.dir == dir && now.duration_since(h.last) <= REPEAT_GAP => {
                h.last = now;
                if now.duration_since(h.started) >= TURBO_AFTER {
                    TURBO_DAYS
                } else {
                    1
                }
            }
            _ => {
                self.0 = Some(HoldState {
                    dir,
                    started: now,
                    last: now,
                });
                1
            }
        }
    }

    /// Is fast-forward engaged right now? Drives the ×4 badge, which decays
    /// on its own once the repeats stop arriving.
    fn turbo(&self, now: Instant) -> bool {
        self.0.is_some_and(|h| {
            now.duration_since(h.last) <= REPEAT_GAP && now.duration_since(h.started) >= TURBO_AFTER
        })
    }
}

pub struct AgingView {
    /// Row 0 is the whole portfolio; the rest are every organization on the
    /// books, ordered by open payer A/R at the end of intake — the order
    /// never changes while scrubbing, only the numbers do.
    providers: Vec<ProviderRow>,
    pub table: TableState,
    focus: Focus,
    /// The books rebuilt as of `as_of` — every number on this pane (provider
    /// list, outcome bars, aging tables) derives from this one snapshot.
    books: Ledger,
    as_of: VirtualTime,
    /// Cadence tracker for held ←/→ while the timeline is grabbed.
    hold: Hold,
    /// The aging document for the current selection, rebuilt on move.
    doc: Vec<Line<'static>>,
    pub scroll: u16,
}

struct ProviderRow {
    /// None = the whole portfolio.
    name: Option<String>,
    open: usize,
    payer_ar: Money,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The base layer: ↑/↓ pick a book, ←/→ control the pane bar.
    Providers,
    /// Enter stepped into the report window: ↑/↓ scroll the document and
    /// ←/→ scrub the timeline a day at a time, until Esc steps back out.
    Report,
}

pub fn build(output: &RunOutput) -> AgingView {
    // The default view is the end-of-intake snapshot the run already took —
    // rebuilding it via snapshot_at would produce the identical books.
    let as_of = output.intake_finished_at;
    let books = output.intake_ledger.clone();

    // The roster spans the whole run (an organization whose book is empty at
    // one moment may be the biggest at another), ordered by open payer A/R
    // at the default view so the list reads money-first and stays stable.
    let totals = provider_totals(&books);
    let names: BTreeSet<String> = output
        .ledger
        .claims
        .values()
        .filter_map(|r| r.identity.as_ref())
        .map(|id| id.organization_name.clone())
        .collect();
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort_by(|a, b| {
        let ar = |n: &str| totals.get(n).map_or(Money::ZERO, |t| t.1);
        ar(b).cmp(&ar(a)).then_with(|| a.cmp(b))
    });
    let mut providers: Vec<ProviderRow> = names
        .into_iter()
        .map(|name| ProviderRow {
            name: Some(name),
            open: 0,
            payer_ar: Money::ZERO,
        })
        .collect();
    providers.insert(
        0,
        ProviderRow {
            name: None,
            open: 0,
            payer_ar: Money::ZERO,
        },
    );

    let mut table = TableState::default();
    table.select(Some(0));
    let mut view = AgingView {
        providers,
        table,
        focus: Focus::Providers,
        books,
        as_of,
        hold: Hold::default(),
        doc: Vec::new(),
        scroll: 0,
    };
    view.refresh(output);
    view
}

/// Open payer-A/R claims and dollars per organization, over one snapshot. An
/// organization shows up if it has anything open on either book.
fn provider_totals(books: &Ledger) -> HashMap<String, (usize, Money)> {
    let mut totals: HashMap<String, (usize, Money)> = HashMap::new();
    for record in books.claims.values() {
        let Some(identity) = &record.identity else {
            continue;
        };
        let outstanding = record.payer_outstanding();
        let patient: Money = record
            .lines
            .iter()
            .filter(|l| l.is_booked())
            .filter_map(|l| l.adjudication.as_ref())
            .map(|adj| adj.patient_responsibility())
            .sum();
        if outstanding == Money::ZERO && patient == Money::ZERO {
            continue;
        }
        let entry = totals
            .entry(identity.organization_name.clone())
            .or_insert((0, Money::ZERO));
        entry.0 += usize::from(outstanding > Money::ZERO);
        entry.1 += outstanding;
    }
    totals
}

impl AgingView {
    /// ↑/↓: move whichever side has focus — the provider selection (which
    /// rebuilds the report) or the report scroll.
    pub fn key_move(&mut self, delta: isize, output: &RunOutput) {
        match self.focus {
            Focus::Providers => {
                if super::step(&mut self.table, self.providers.len(), delta) {
                    self.doc = self.build_doc(output);
                    self.scroll = 0;
                }
            }
            Focus::Report => {
                self.scroll = self.scroll.saturating_add_signed(delta as i16);
            }
        }
    }

    /// Enter: one layer deeper — step into the report window, where ↑/↓
    /// scroll the document and ←/→ scrub the timeline.
    pub fn enter_report(&mut self) {
        self.focus = Focus::Report;
    }

    /// Esc: one layer back up. Returns false when already at the base layer
    /// (provider list), where ←/→ already control the pane bar.
    pub fn escape(&mut self) -> bool {
        match self.focus {
            Focus::Report => {
                self.focus = Focus::Providers;
                true
            }
            Focus::Providers => false,
        }
    }

    pub fn timeline_grabbed(&self) -> bool {
        self.focus == Focus::Report
    }

    /// ←/→ while the timeline is grabbed: move the as-of moment one virtual
    /// day — four once the key has been held for a second — clamped to
    /// [t+0, run completion], and re-derive everything from the books as
    /// they stood then.
    pub fn step_day(&mut self, delta: isize, output: &RunOutput) {
        let step = VIRTUAL_DAY * self.hold.press(delta.signum(), Instant::now());
        let current = self.as_of.as_duration();
        let next = if delta < 0 {
            current.saturating_sub(step)
        } else {
            (current + step).min(output.finished_at.as_duration())
        };
        if next == current {
            return;
        }
        self.as_of = VirtualTime::default() + next;
        self.books = snapshot_at(&output.ledger, self.as_of);
        self.refresh(output);
    }

    /// Re-derive the provider rows and the report document from the current
    /// snapshot. Selection and scroll survive — the row order is fixed, so
    /// scrubbing the timeline animates the numbers under a steady cursor.
    fn refresh(&mut self, output: &RunOutput) {
        let totals = provider_totals(&self.books);
        let mut all = (0usize, Money::ZERO);
        for &(open, ar) in totals.values() {
            all.0 += open;
            all.1 += ar;
        }
        for row in &mut self.providers {
            let (open, payer_ar) = match &row.name {
                Some(name) => totals.get(name).copied().unwrap_or((0, Money::ZERO)),
                None => all,
            };
            row.open = open;
            row.payer_ar = payer_ar;
        }
        self.doc = self.build_doc(output);
    }

    fn build_doc(&self, output: &RunOutput) -> Vec<Line<'static>> {
        build_doc(&self.books, self.as_of, output, self.selected_name())
    }

    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.table
            .selected()
            .and_then(|i| self.providers.get(i))
            .and_then(|p| p.name.as_deref())
    }
}

/// The report for one selection, every section derived from the same as-of
/// snapshot: the outcome bars on top, the aging tables below.
fn build_doc(
    books: &Ledger,
    as_of: VirtualTime,
    output: &RunOutput,
    provider: Option<&str>,
) -> Vec<Line<'static>> {
    let aging = ar_aging_for(books, as_of, provider);
    let dar = days_in_ar_for(books, as_of, provider);

    let scope = provider.unwrap_or("all providers");
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {scope} — days in A/R: "), bold()),
        Span::styled(format!("{dar:.1}"), theme::accent_bold()),
        Span::styled(
            format!("   books as of {as_of}{}", moment_label(as_of, output)),
            dim(),
        ),
    ])];
    lines.push(Line::default());
    lines.extend(outcome_lines(books, provider));
    lines.push(Line::default());
    lines.push(bucket_legend());
    lines.push(Line::default());
    section(
        &mut lines,
        "Payer A/R — ages from first submission (retries never reset the clock)",
        &aging.payer,
    );
    lines.push(Line::default());
    section(
        &mut lines,
        "Patient responsibility — ages from adjudication, when it became patient debt",
        &aging.patient,
    );
    lines
}

/// What this moment on the timeline is, when it's one of the two landmarks.
fn moment_label(as_of: VirtualTime, output: &RunOutput) -> &'static str {
    if as_of == output.intake_finished_at {
        " (end of intake)"
    } else if as_of >= output.finished_at {
        " (run complete — every claim terminal)"
    } else {
        ""
    }
}

const BAR_WIDTH: usize = 44;

/// The claims funnel and money waterfall for this book, computed over the
/// same as-of snapshot as the aging tables below — where every claim stood
/// at this moment, next to how its money was aging.
fn outcome_lines(books: &Ledger, provider: Option<&str>) -> Vec<Line<'static>> {
    let s = summarize_for(books, provider);
    let outstanding = s.billed - s.payer_paid - s.patient_responsibility - s.not_allowed;
    let total = s.total_claims.max(1) as f64;
    let claim_pct = |n: usize| format!("{} ({})", thousands(n as u64), pct(n as f64 / total));
    let money_pct = |m: Money| {
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
            Span::styled(" ingested by this moment", bold()),
        ]),
        outcome_bar(&[
            (s.resolved as f64, GOOD),
            (s.rejected as f64, WARN),
            (s.flagged as f64, BAD),
            (s.non_terminal as f64, Color::Magenta),
        ]),
        outcome_legend(&[
            (GOOD, "resolved", claim_pct(s.resolved)),
            (WARN, "rejected at ingest", claim_pct(s.rejected)),
            (BAD, "flagged", claim_pct(s.flagged)),
            (Color::Magenta, "in flight", claim_pct(s.non_terminal)),
        ]),
    ];
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" money — ", bold()),
        Span::styled(money(s.billed), theme::accent_bold()),
        Span::styled(" billed by this moment", bold()),
    ]));
    lines.push(outcome_bar(&[
        (s.payer_paid.cents() as f64, GOOD),
        (s.patient_responsibility.cents() as f64, ACCENT),
        (s.not_allowed.cents() as f64, Color::Gray),
        (outstanding.cents() as f64, BAD),
    ]));
    lines.push(outcome_legend(&[
        (GOOD, "payer paid", money_pct(s.payer_paid)),
        (ACCENT, "patient owes", money_pct(s.patient_responsibility)),
    ]));
    lines.push(outcome_legend(&[
        (
            Color::Gray,
            "not allowed (write-off)",
            money_pct(s.not_allowed),
        ),
        (BAD, "outstanding (unanswered)", money_pct(outstanding)),
    ]));
    lines
}

fn outcome_bar(segments: &[(f64, Color)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(theme::seg_bar(BAR_WIDTH, segments));
    Line::from(spans)
}

fn outcome_legend(entries: &[(Color, &str, String)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (color, label, value)) in entries.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.extend(swatch(*color, label, value.clone()));
    }
    Line::from(spans)
}

fn bucket_legend() -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (color, label) in BUCKET_COLORS
        .iter()
        .zip(["0–30d", "31–60d", "61–90d", "90+d"])
    {
        spans.push(Span::styled("■ ", Style::default().fg(*color)));
        spans.push(Span::styled(format!("{label}  "), dim()));
    }
    spans.push(Span::styled(
        "· each row's mix bar shows how that book skews",
        dim(),
    ));
    Line::from(spans)
}

fn section(lines: &mut Vec<Line<'static>>, title: &str, rows: &BTreeMap<PayerId, AgingBuckets>) {
    lines.push(Line::from(Span::styled(
        format!(" ▌ {title}"),
        theme::accent_bold(),
    )));
    if rows.is_empty() {
        lines.push(Line::from(Span::styled("   nothing outstanding", dim())));
        return;
    }
    lines.push(Line::from(Span::styled(
        format!(
            "   {:<22} {:>13} {:>13} {:>13} {:>13} {:>14}  {:<MIX_WIDTH$}",
            "payer", "0–30d", "31–60d", "61–90d", "90+d", "total", "mix"
        ),
        bold(),
    )));
    let mut grand = AgingBuckets::default();
    for (payer, b) in rows {
        lines.push(row(payer.as_str(), b, false));
        grand.d0_30 += b.d0_30;
        grand.d31_60 += b.d31_60;
        grand.d61_90 += b.d61_90;
        grand.d90_plus += b.d90_plus;
    }
    if rows.len() > 1 {
        lines.push(row("TOTAL", &grand, true));
    }
}

fn row(label: &str, b: &AgingBuckets, emphasize: bool) -> Line<'static> {
    let cell = |amount: Money, color: Color| {
        let style = if amount == Money::ZERO {
            dim()
        } else {
            Style::default().fg(color)
        };
        Span::styled(format!(" {:>13}", money(amount)), style)
    };
    let mut spans = vec![Span::styled(
        format!("   {label:<22}"),
        if emphasize { bold() } else { Style::default() },
    )];
    for (amount, color) in [b.d0_30, b.d31_60, b.d61_90, b.d90_plus]
        .into_iter()
        .zip(BUCKET_COLORS)
    {
        spans.push(cell(amount, color));
    }
    spans.push(Span::styled(format!(" {:>14}", money(b.total())), bold()));
    spans.push(Span::raw("  "));
    spans.extend(theme::seg_bar(
        MIX_WIDTH,
        &[
            (b.d0_30.cents() as f64, BUCKET_COLORS[0]),
            (b.d31_60.cents() as f64, BUCKET_COLORS[1]),
            (b.d61_90.cents() as f64, BUCKET_COLORS[2]),
            (b.d90_plus.cents() as f64, BUCKET_COLORS[3]),
        ],
    ));
    Line::from(spans)
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, view: &mut AgingView, output: &RunOutput) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[
            ("↑/↓", "pick a provider"),
            ("enter", "step into the report"),
        ]),
        hint_area,
    );

    let [left, right] =
        Layout::horizontal([Constraint::Length(48), Constraint::Min(40)]).areas(body);

    let focus_style = Style::default().fg(ACCENT);
    let blur_style = theme::dim();
    let list_focused = view.focus == Focus::Providers;

    let provider_rows: Vec<Row> = view
        .providers
        .iter()
        .map(|p| {
            let (name, style) = match &p.name {
                Some(name) => (name.clone(), Style::default()),
                None => ("ALL PROVIDERS".to_string(), bold()),
            };
            Row::new(vec![
                Cell::from(Line::from(Span::styled(name, style))),
                Cell::from(p.open.to_string()),
                Cell::from(Line::from(Span::styled(money(p.payer_ar), bold()))),
            ])
        })
        .collect();
    let position = Line::from(Span::styled(
        format!(
            " {}/{} ",
            view.table.selected().map_or(0, |i| i + 1),
            view.providers.len()
        ),
        dim(),
    ))
    .right_aligned();
    let providers = Table::new(
        provider_rows,
        [
            Constraint::Min(24),
            Constraint::Length(4),
            Constraint::Length(13),
        ],
    )
    .header(Row::new(["provider", "open", "payer A/R"]).style(bold()))
    .block(
        theme::panel(format!("Books — {} rows", view.providers.len()))
            .title_bottom(position)
            .border_style(if list_focused {
                focus_style
            } else {
                blur_style
            }),
    )
    .row_highlight_style(theme::bold().bg(Color::DarkGray))
    .highlight_symbol(if list_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(providers, left, &mut view.table);

    let scope = view
        .selected_name()
        .map(str::to_string)
        .unwrap_or_else(|| "all providers".to_string());
    let mut block =
        theme::panel(format!("A/R Aging — {scope}")).border_style(if view.focus == Focus::Report {
            focus_style
        } else {
            blur_style
        });
    // The timeline scrubber lives inside this window, pinned under the
    // report it drives: the inner area splits into the scrolling document
    // and one always-visible scrubber row at the bottom.
    let inner = block.inner(right);
    let [doc_area, scrubber_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let visible = doc_area.height;
    let max = (view.doc.len() as u16).saturating_sub(visible);
    view.scroll = view.scroll.min(max);
    if max > 0 {
        let last = (view.scroll + visible).min(view.doc.len() as u16);
        block = block.title_bottom(
            Line::from(format!(
                " ↑/↓ · lines {}–{last} of {} ",
                view.scroll + 1,
                view.doc.len()
            ))
            .right_aligned()
            .style(dim()),
        );
    }
    frame.render_widget(block, right);
    frame.render_widget(
        Paragraph::new(view.doc.clone()).scroll((view.scroll, 0)),
        doc_area,
    );
    frame.render_widget(timeline_line(view, output), scrubber_area);
}

/// The scrubber row pinned at the bottom of the report window: a track
/// spanning the run with the as-of day marked, lit in accent while the
/// timeline is grabbed, with a ×4 badge while fast-forward is engaged.
fn timeline_line(view: &AgingView, output: &RunOutput) -> Line<'static> {
    const TRACK: usize = 32;
    let total = output.finished_at.as_duration().as_secs_f64();
    let frac = if total > 0.0 {
        (view.as_of.as_duration().as_secs_f64() / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let grabbed = view.timeline_grabbed();
    let day = view.as_of.as_duration().as_secs_f64() / VIRTUAL_DAY.as_secs_f64();
    let last_day = total / VIRTUAL_DAY.as_secs_f64();

    let mut spans = vec![Span::styled(
        " timeline ",
        if grabbed { theme::accent_bold() } else { dim() },
    )];
    spans.extend(theme::meter(
        frac,
        TRACK,
        if grabbed { ACCENT } else { Color::DarkGray },
    ));
    spans.push(Span::styled(
        format!("  day {day:.1} of {last_day:.1}"),
        if grabbed {
            theme::accent_bold()
        } else {
            bold()
        },
    ));
    if grabbed && view.hold.turbo(Instant::now()) {
        spans.push(Span::styled(" ×4", theme::accent_bold()));
    }
    // No moment label here — the report headline above already carries it.
    spans.push(Span::styled(
        format!(
            "  ·  {}",
            if grabbed {
                "←/→ ±1 day (hold 1s: ×4) · esc backs out"
            } else {
                "enter steps into the report"
            }
        ),
        dim(),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_shifts_into_turbo_after_a_second_and_resets_on_flip_or_gap() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut hold = Hold::default();

        assert_eq!(hold.press(1, t0), 1, "a first press steps one day");
        assert_eq!(hold.press(1, at(500)), 1, "a young hold stays 1×");
        assert_eq!(
            hold.press(1, at(1_100)),
            TURBO_DAYS,
            "a second-old hold fast-forwards"
        );
        assert!(hold.turbo(at(1_150)), "the ×4 badge shows mid-hold");
        assert_eq!(
            hold.press(-1, at(1_300)),
            1,
            "reversing direction starts a fresh hold"
        );
        assert_eq!(hold.press(-1, at(1_500)), 1, "still young the other way");
        assert_eq!(
            hold.press(-1, at(2_500)),
            1,
            "a gap past auto-repeat is a new press, not a hold"
        );
        assert!(
            !hold.turbo(at(2_600)),
            "the badge decays once repeats stop arriving"
        );
    }
}
