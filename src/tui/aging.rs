//! The A/R Aging pane: a master–detail view. A provider list on the left
//! (whole portfolio first, then each billing organization by open payer A/R)
//! drives the report on the right — the outcome bars (claims funnel + money
//! waterfall, from the drained final books) above the aging tables with
//! their colored buckets and mix bars, all recomputed for whichever book is
//! selected. Tab hops focus so ↑/↓ can either change the selection or
//! scroll the report.

use std::collections::BTreeMap;
use std::collections::HashMap;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table, TableState};

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::domain::{Money, PayerId};
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

pub struct AgingView {
    /// Row 0 is the whole portfolio; the rest are organizations, biggest
    /// open payer A/R first.
    providers: Vec<ProviderRow>,
    pub table: TableState,
    focus: Focus,
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
    Providers,
    Tables,
}

pub fn build(output: &RunOutput) -> AgingView {
    let books = &output.intake_ledger;
    // Aggregate per organization: open payer-A/R claims and dollars. An
    // organization appears if it has anything on either book.
    let mut totals: HashMap<&str, (usize, Money)> = HashMap::new();
    let mut all = (0usize, Money::ZERO);
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
            .entry(identity.organization_name.as_str())
            .or_insert((0, Money::ZERO));
        let open = usize::from(outstanding > Money::ZERO);
        entry.0 += open;
        entry.1 += outstanding;
        all.0 += open;
        all.1 += outstanding;
    }
    let mut providers: Vec<ProviderRow> = totals
        .into_iter()
        .map(|(name, (open, payer_ar))| ProviderRow {
            name: Some(name.to_string()),
            open,
            payer_ar,
        })
        .collect();
    providers.sort_by(|a, b| {
        b.payer_ar
            .cmp(&a.payer_ar)
            .then_with(|| a.name.cmp(&b.name))
    });
    providers.insert(
        0,
        ProviderRow {
            name: None,
            open: all.0,
            payer_ar: all.1,
        },
    );

    let mut table = TableState::default();
    table.select(Some(0));
    let doc = build_doc(output, None);
    AgingView {
        providers,
        table,
        focus: Focus::Providers,
        doc,
        scroll: 0,
    }
}

impl AgingView {
    /// ↑/↓: move whichever side has focus — the provider selection (which
    /// rebuilds the report) or the report scroll.
    pub fn key_move(&mut self, delta: isize, output: &RunOutput) {
        match self.focus {
            Focus::Providers => {
                if super::step(&mut self.table, self.providers.len(), delta) {
                    let provider = self.selected_name().map(str::to_string);
                    self.doc = build_doc(output, provider.as_deref());
                    self.scroll = 0;
                }
            }
            Focus::Tables => {
                self.scroll = self.scroll.saturating_add_signed(delta as i16);
            }
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Providers => Focus::Tables,
            Focus::Tables => Focus::Providers,
        };
    }

    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.table
            .selected()
            .and_then(|i| self.providers.get(i))
            .and_then(|p| p.name.as_deref())
    }
}

/// The report for one selection: the outcome bars (final books) on top, the
/// aging tables (intake snapshot) below.
fn build_doc(output: &RunOutput, provider: Option<&str>) -> Vec<Line<'static>> {
    let (books, as_of) = (&output.intake_ledger, output.intake_finished_at);
    let aging = ar_aging_for(books, as_of, provider);
    let dar = days_in_ar_for(books, as_of, provider);

    let scope = provider.unwrap_or("all providers");
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {scope} — days in A/R: "), bold()),
        Span::styled(format!("{dar:.1}"), theme::accent_bold()),
        Span::styled(format!("   snapshot as of {as_of} (end of intake)"), dim()),
    ])];
    lines.push(Line::default());
    lines.extend(outcome_lines(output, provider));
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

const BAR_WIDTH: usize = 44;

/// The claims funnel and money waterfall for this book, computed over the
/// drained final ledger — where every claim ended up, next to how its money
/// was aging mid-flight below.
fn outcome_lines(output: &RunOutput, provider: Option<&str>) -> Vec<Line<'static>> {
    let s = summarize_for(&output.ledger, provider);
    let unaccounted = s.billed - s.payer_paid - s.patient_responsibility - s.not_allowed;
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
            Span::styled(" ingested, every one driven terminal", bold()),
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
        ]),
    ];
    if s.non_terminal > 0 {
        lines.push(outcome_legend(&[(
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
    lines.push(outcome_bar(&[
        (s.payer_paid.cents() as f64, GOOD),
        (s.patient_responsibility.cents() as f64, ACCENT),
        (s.not_allowed.cents() as f64, Color::Gray),
        (unaccounted.cents() as f64, BAD),
    ]));
    lines.push(outcome_legend(&[
        (GOOD, "payer paid", money_pct(s.payer_paid)),
        (ACCENT, "patient owes", money_pct(s.patient_responsibility)),
    ]));
    let mut writeoffs = vec![(
        Color::Gray,
        "not allowed (write-off)",
        money_pct(s.not_allowed),
    )];
    if unaccounted.cents() > 0 {
        writeoffs.push((BAD, "unanswered on flagged", money_pct(unaccounted)));
    }
    lines.push(outcome_legend(&writeoffs));
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

pub fn draw(frame: &mut ratatui::Frame, area: Rect, view: &mut AgingView) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[
            ("↑/↓", "pick a provider (all providers first)"),
            ("tab", "hop over to scroll the tables"),
            ("green → red", "receivables aging past 90 days"),
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
    let doc = view.doc.clone();
    theme::scrolled_paragraph(
        frame,
        right,
        theme::panel(format!("A/R Aging — {scope}")).border_style(if list_focused {
            blur_style
        } else {
            focus_style
        }),
        &doc,
        &mut view.scroll,
    );
}
