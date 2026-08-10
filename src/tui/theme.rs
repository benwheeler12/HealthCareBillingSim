//! Shared visual language for the TUI: one accent color, one hint style,
//! one bar vocabulary. Every pane draws from here so the whole app reads as
//! a single instrument instead of seven ad-hoc screens.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use healthcare_billing_sim::domain::Money;

pub const ACCENT: Color = Color::Cyan;
pub const GOOD: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;
pub const ALERT: Color = Color::LightRed;

pub fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn accent_bold() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// The standard frame: rounded corners, bold padded title. Every pane uses
/// this so borders read as one family.
pub fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(format!(" {} ", title.into()), bold()))
}

/// The keys→action hint line every pane opens with: keys in accent, actions
/// dim, always in the same place (the first row of the pane).
pub fn keys_hint(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, action)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", dim()));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(ACCENT),
        ));
        spans.push(Span::styled(format!(" {action}"), dim()));
    }
    Line::from(spans)
}

/// A plain explanatory hint line (no keys to call out).
pub fn hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" {text}"), dim()))
}

/// Proportional multi-segment bar (largest-remainder apportionment, so the
/// cells always sum to `width`). Zero-weight everything renders as an empty
/// track rather than nothing.
pub fn seg_bar(width: usize, segments: &[(f64, Color)]) -> Vec<Span<'static>> {
    let total: f64 = segments.iter().map(|(w, _)| w.max(0.0)).sum();
    if width == 0 {
        return Vec::new();
    }
    if total <= 0.0 {
        return vec![Span::styled("░".repeat(width), dim())];
    }
    let mut cells = Vec::with_capacity(segments.len());
    let mut remainders = Vec::with_capacity(segments.len());
    let mut used = 0;
    for (i, (w, _)) in segments.iter().enumerate() {
        let exact = w.max(0.0) / total * width as f64;
        let floor = exact.floor() as usize;
        cells.push(floor);
        used += floor;
        remainders.push((i, exact - floor as f64));
    }
    remainders.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (i, _) in remainders.into_iter().take(width.saturating_sub(used)) {
        cells[i] += 1;
    }
    segments
        .iter()
        .zip(cells)
        .filter(|(_, n)| *n > 0)
        .map(|((_, color), n)| Span::styled("█".repeat(n), Style::default().fg(*color)))
        .collect()
}

/// Single-value fill meter: `frac` of `width` in `color`, rest a dim track.
pub fn meter(frac: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(width - filled), dim()),
    ]
}

/// Colored legend swatch + label + emphasized value, for bar legends.
pub fn swatch(color: Color, label: &str, value: String) -> Vec<Span<'static>> {
    vec![
        Span::styled("■ ", Style::default().fg(color)),
        Span::styled(format!("{label} "), dim()),
        Span::styled(value, bold()),
    ]
}

/// 1,234,567 — counts get thousands separators everywhere in the UI.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        // No `is_multiple_of` — the repo builds on rustc 1.85, which predates it.
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// $1,234,567.89 — Money with separators for headline figures.
pub fn money(m: Money) -> String {
    let cents = m.cents();
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}${}.{:02}", thousands(abs / 100), abs % 100)
}

pub fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

/// Compact virtual-duration for dense table cells ("6.1d", "4.5h", "12m").
pub fn human_compact(secs: f64) -> String {
    match secs {
        s if s >= 86_400.0 => format!("{:.1}d", s / 86_400.0),
        s if s >= 3_600.0 => format!("{:.1}h", s / 3_600.0),
        s if s >= 60.0 => format!("{:.1}m", s / 60.0),
        s => format!("{s:.0}s"),
    }
}

/// Centered popup rect clamped to the containing area.
pub fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Render a document of lines inside `block` with clamped scrolling and a
/// position indicator in the bottom border whenever there is more to see.
pub fn scrolled_paragraph(
    frame: &mut ratatui::Frame,
    area: Rect,
    block: Block<'static>,
    lines: &[Line<'static>],
    scroll: &mut u16,
) {
    let visible = area.height.saturating_sub(2);
    let max = (lines.len() as u16).saturating_sub(visible);
    *scroll = (*scroll).min(max);
    let block = if max > 0 {
        let last = (*scroll + visible).min(lines.len() as u16);
        block.title_bottom(
            Line::from(format!(
                " ↑/↓ · lines {}–{last} of {} ",
                *scroll + 1,
                lines.len()
            ))
            .right_aligned()
            .style(dim()),
        )
    } else {
        block
    };
    let body = Paragraph::new(lines.to_vec())
        .block(block)
        .scroll((*scroll, 0));
    frame.render_widget(body, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn money_formats_with_separators() {
        assert_eq!(money(Money::from_cents(123_456_789)), "$1,234,567.89");
        assert_eq!(money(Money::from_cents(-505)), "-$5.05");
    }

    #[test]
    fn seg_bar_cells_always_sum_to_width() {
        for width in [1usize, 7, 24, 40] {
            let spans = seg_bar(width, &[(1.0, GOOD), (2.0, WARN), (0.3, BAD)]);
            let cells: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            assert_eq!(cells, width);
        }
    }

    #[test]
    fn seg_bar_empty_weights_render_a_track() {
        let spans = seg_bar(10, &[(0.0, GOOD)]);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.chars().count(), 10);
    }
}
