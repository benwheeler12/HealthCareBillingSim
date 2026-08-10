//! The A/R Aging pane: the plain aging tables re-set with color — each
//! bucket column keeps a fixed hue (young → old runs green → red) and every
//! row carries a mix bar so the shape of a payer's book reads at a glance.

use std::collections::BTreeMap;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};

use healthcare_billing_sim::domain::{Money, PayerId, VirtualTime};
use healthcare_billing_sim::ledger::records::Ledger;
use healthcare_billing_sim::reports::aging::AgingBuckets;
use healthcare_billing_sim::reports::{ar_aging, days_in_ar};

use super::theme::{self, ALERT, BAD, GOOD, WARN, bold, dim, money};

/// Young → old, the same hues the legend and mix bars use.
const BUCKET_COLORS: [Color; 4] = [GOOD, WARN, ALERT, BAD];
const MIX_WIDTH: usize = 18;

pub fn build(books: &Ledger, as_of: VirtualTime) -> Vec<Line<'static>> {
    let aging = ar_aging(books, as_of);
    let dar = days_in_ar(books, as_of);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" days in A/R: ", bold()),
            Span::styled(format!("{dar:.1}"), theme::accent_bold()),
            Span::styled(
                format!(
                    "   snapshot as of {as_of} (end of intake) — the books as a biller sees them mid-flight"
                ),
                dim(),
            ),
        ]),
        legend(),
        Line::default(),
    ];
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

fn legend() -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (color, label) in BUCKET_COLORS
        .iter()
        .zip(["0–30d", "31–60d", "61–90d", "90+d"])
    {
        spans.push(Span::styled(
            "■ ",
            ratatui::style::Style::default().fg(*color),
        ));
        spans.push(Span::styled(format!("{label}  "), dim()));
    }
    spans.push(Span::styled(
        "· each row's mix bar shows how that payer's book skews",
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
    lines.push(row("TOTAL", &grand, true));
}

fn row(label: &str, b: &AgingBuckets, emphasize: bool) -> Line<'static> {
    let cell = |amount: Money, color: Color| {
        let style = if amount == Money::ZERO {
            dim()
        } else {
            ratatui::style::Style::default().fg(color)
        };
        Span::styled(format!(" {:>13}", money(amount)), style)
    };
    let mut spans = vec![Span::styled(
        format!("   {label:<22}"),
        if emphasize {
            bold()
        } else {
            ratatui::style::Style::default()
        },
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

pub fn draw(frame: &mut ratatui::Frame, area: Rect, lines: &[Line<'static>], scroll: &mut u16) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[(
            "↑/↓",
            "scroll · buckets run green → red as receivables age past 90 days",
        )]),
        hint_area,
    );
    theme::scrolled_paragraph(
        frame,
        body,
        theme::panel("A/R Aging — who's sitting on the money, and for how long"),
        lines,
        scroll,
    );
}
