//! Interactive terminal UI: a static screen with the reports arrayed as
//! panes (←/→ to move between them), a live status box pinned to the bottom,
//! and Enter on the chase list drilling into a claim's full audit trail.
//!
//! Threading: the TUI event loop owns the main thread on the *wall* clock;
//! the simulation runs on its own multi-thread runtime behind a spawned
//! thread (Decisions #23 — time is computed, so claim tasks parallelize).
//! They meet at two channels: the fold's live-progress watch tap, and a
//! oneshot carrying the finished `RunOutput`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::{
    Axis, Block, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use tokio::sync::watch;

use healthcare_billing_sim::domain::{Money, VirtualTime, human_virtual};
use healthcare_billing_sim::ledger::events::{ClaimEvent, StampedEvent};
use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::ledger::records::{ClaimRecord, ClaimState, FlagReason};
use healthcare_billing_sim::reports::chase::{ChaseItem, status_label};
use healthcare_billing_sim::reports::{
    Denials, Scorecard, ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic,
    payer_scorecard, summarize,
};
use healthcare_billing_sim::{RunConfig, RunOutput};

pub struct TuiOptions {
    /// Pre-rendered run-configuration banner (uncolored).
    pub banner: String,
    pub seed: u64,
    /// Worker threads for the sim runtime (0 = one per core).
    pub threads: usize,
}

/// Run the simulation under the interactive UI and hand the finished output
/// back so the caller can leave a plain-text record on stdout.
pub fn run(mut cfg: RunConfig, opts: TuiOptions) -> anyhow::Result<RunOutput> {
    let (progress_tx, progress_rx) = watch::channel(Progress::default());
    cfg.progress = Some(progress_tx);
    let (done_tx, done_rx) = mpsc::channel();
    let threads = opts.threads;
    let sim = std::thread::spawn(move || {
        // Multi-thread runtime (Decisions #23): nothing sleeps, so no paused
        // clock — claim tasks execute in true parallel under the UI.
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if threads > 0 {
            builder.worker_threads(threads);
        }
        let result = builder
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(healthcare_billing_sim::run(cfg)));
        let _ = done_tx.send(result);
    });

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, progress_rx, &done_rx, &opts);
    ratatui::restore();
    let _ = sim.join();
    result
}

struct App {
    banner: String,
    seed: u64,
    wall_start: Instant,
    wall_done: Option<Duration>,
    progress: Progress,
    /// None while the simulation is still running.
    done: Option<Done>,
    pane: usize,
    scroll: u16,
    frame: usize,
}

struct Done {
    output: RunOutput,
    panes: Vec<Pane>,
    /// Every open receivable, unordered; the insights view holds sorted
    /// index lists into this.
    chase: Vec<ChaseItem>,
    timeline: Timeline,
    insights: Insights,
    /// Open claim-detail overlay, if any.
    detail: Option<String>,
}

struct Pane {
    title: &'static str,
    content: String,
}

/// Master–detail state for the Provider Insights pane: a provider list on
/// the left drives a claims table on the right; Tab (or Enter on a
/// provider) moves focus right, Tab toggles back.
struct Insights {
    /// Aggregated per organization, sorted by total outstanding desc.
    providers: Vec<ProviderRow>,
    provider_table: TableState,
    /// The selected provider's open claims: indices into `Done::chase`,
    /// ordered by the active sort.
    claims: Vec<usize>,
    claims_table: TableState,
    focus: InsightsFocus,
    sort: SortKey,
    sort_desc: bool,
}

struct ProviderRow {
    name: String,
    open: usize,
    outstanding: Money,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InsightsFocus {
    Providers,
    Claims,
}

/// Sort order for the selected provider's claims table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Cost,
    Age,
    Risk,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Cost => "cost",
            SortKey::Age => "age",
            SortKey::Risk => "risk",
        }
    }
}

const TIMELINE_PANE: usize = 0;
const INSIGHTS_PANE: usize = 5;

/// One keys line, everywhere, always — the status box never rewords itself.
/// Pane-specific instructions live at the top of the pane they belong to.
const KEYS_HINT: &str =
    "←/→ panes · ↑/↓ scroll or select · enter drill in · q quit (prints plain report)";

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut progress_rx: watch::Receiver<Progress>,
    done_rx: &mpsc::Receiver<anyhow::Result<RunOutput>>,
    opts: &TuiOptions,
) -> anyhow::Result<RunOutput> {
    let mut app = App {
        banner: opts.banner.clone(),
        seed: opts.seed,
        wall_start: Instant::now(),
        wall_done: None,
        progress: Progress::default(),
        done: None,
        pane: 0,
        scroll: 0,
        frame: 0,
    };

    loop {
        if app.done.is_none() {
            app.progress = *progress_rx.borrow_and_update();
            if let Ok(result) = done_rx.try_recv() {
                let output = result?;
                app.wall_done = Some(app.wall_start.elapsed());
                app.progress = final_progress(&output);
                app.done = Some(Done::build(output));
            }
        }
        app.frame = app.frame.wrapping_add(1);
        terminal.draw(|frame| draw(frame, &mut app))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if handle_key(&mut app, key.code, key.modifiers) {
            match app.done {
                Some(done) => return Ok(done.output),
                None => anyhow::bail!("interrupted before the simulation finished"),
            }
        }
    }
}

/// Apply one keypress; returns true when the user asked to leave the UI.
fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let ctrl_c = code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL);
    let Some(done) = &mut app.done else {
        return ctrl_c || code == KeyCode::Char('q') || code == KeyCode::Esc;
    };

    // The overlay swallows everything: any of these close it, nothing quits.
    if done.detail.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) || ctrl_c {
            done.detail = None;
        }
        return false;
    }

    let panes = done.panes.len();
    match code {
        _ if ctrl_c => return true,
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Left => {
            app.pane = (app.pane + panes - 1) % panes;
            app.scroll = 0;
        }
        KeyCode::Right => {
            app.pane = (app.pane + 1) % panes;
            app.scroll = 0;
        }
        KeyCode::Up if app.pane == INSIGHTS_PANE => done.move_selection(-1),
        KeyCode::Down if app.pane == INSIGHTS_PANE => done.move_selection(1),
        KeyCode::PageUp if app.pane == INSIGHTS_PANE => done.move_selection(-10),
        KeyCode::PageDown if app.pane == INSIGHTS_PANE => done.move_selection(10),
        KeyCode::Up => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Down => app.scroll = app.scroll.saturating_add(1),
        KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(10),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_add(10),
        KeyCode::Tab if app.pane == INSIGHTS_PANE => {
            done.insights.focus = match done.insights.focus {
                InsightsFocus::Providers => InsightsFocus::Claims,
                InsightsFocus::Claims => InsightsFocus::Providers,
            };
        }
        KeyCode::Enter if app.pane == INSIGHTS_PANE => match done.insights.focus {
            // Enter on a provider steps into their claims.
            InsightsFocus::Providers => done.insights.focus = InsightsFocus::Claims,
            // Enter on a claim opens the full breakdown.
            InsightsFocus::Claims => {
                let selected = done
                    .insights
                    .claims_table
                    .selected()
                    .and_then(|i| done.insights.claims.get(i))
                    .and_then(|&idx| done.chase.get(idx));
                if let Some(item) = selected {
                    done.detail = Some(claim_detail(&done.output, &item.claim_id.0));
                }
            }
        },
        // Sort keys for the claims table: pressing the active key flips the
        // direction, pressing another switches to it (descending first).
        KeyCode::Char('c') if app.pane == INSIGHTS_PANE => done.set_sort(SortKey::Cost),
        KeyCode::Char('a') if app.pane == INSIGHTS_PANE => done.set_sort(SortKey::Age),
        KeyCode::Char('r') if app.pane == INSIGHTS_PANE => done.set_sort(SortKey::Risk),
        _ => {}
    }
    false
}

fn final_progress(output: &RunOutput) -> Progress {
    let mut p = Progress {
        claims: output.ledger.claims.len(),
        now: output.finished_at,
        ..Progress::default()
    };
    for record in output.ledger.claims.values() {
        match record.state {
            ClaimState::Resolved => p.resolved += 1,
            ClaimState::Rejected { .. } => p.rejected += 1,
            ClaimState::Flagged { .. } => p.flagged += 1,
            _ => {}
        }
    }
    p
}

impl Done {
    fn build(output: RunOutput) -> Done {
        let (books, as_of) = (&output.intake_ledger, output.intake_finished_at);

        let overview = format!(
            "{}\n{}\n\nA/R panes are a snapshot as of end of intake ({as_of}) — the books\nas a biller sees them mid-flight. The run then drained every claim to a\nterminal state by {} (the correctness guarantee).",
            output_banner(&output),
            summarize(&output.ledger),
            output.finished_at,
        );
        // Both aging sections — payer A/R and patient responsibility — as one
        // pane; ar_aging's Display already renders them as two sections.
        let aging_pane = format!(
            "{}\ndays in A/R: {:.1}   (as of {as_of})",
            ar_aging(books, as_of),
            days_in_ar(books, as_of)
        );
        let chase = chase_list(books, as_of, usize::MAX);
        let timeline = timeline(
            &output.ledger.event_log,
            output.intake_finished_at,
            output.finished_at,
            TIMELINE_BUCKETS,
        );

        let panes = vec![
            Pane {
                title: "Timeline",
                content: String::new(),
            },
            Pane {
                title: "Overview",
                content: overview,
            },
            Pane {
                title: "Aging",
                content: aging_pane,
            },
            Pane {
                title: "Scorecard",
                content: Scorecard(payer_scorecard(&output.ledger)).to_string(),
            },
            Pane {
                title: "Denials",
                content: Denials(denial_breakdown(&output.ledger)).to_string(),
            },
            Pane {
                title: "Provider Insights",
                content: String::new(),
            },
            Pane {
                title: "Diagnostic",
                content: diagnostic(&output.ledger, &output.sim_truth).to_string(),
            },
        ];
        let insights = Insights::build(&chase);
        Done {
            output,
            panes,
            chase,
            timeline,
            insights,
            detail: None,
        }
    }

    /// Move the selection in whichever insights list has focus; moving the
    /// provider selection re-derives that provider's claims.
    fn move_selection(&mut self, delta: isize) {
        let ins = &mut self.insights;
        match ins.focus {
            InsightsFocus::Providers => {
                if step(&mut ins.provider_table, ins.providers.len(), delta) {
                    ins.rebuild_claims(&self.chase);
                }
            }
            InsightsFocus::Claims => {
                step(&mut ins.claims_table, ins.claims.len(), delta);
            }
        }
    }

    /// Toggle direction when the active key is pressed again; switch keys
    /// descending-first otherwise.
    fn set_sort(&mut self, key: SortKey) {
        let ins = &mut self.insights;
        if ins.sort == key {
            ins.sort_desc = !ins.sort_desc;
        } else {
            ins.sort = key;
            ins.sort_desc = true;
        }
        ins.rebuild_claims(&self.chase);
    }
}

/// Advance a table selection by `delta`, clamped; returns true if it moved.
fn step(table: &mut TableState, len: usize, delta: isize) -> bool {
    if len == 0 {
        return false;
    }
    let current = table.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, len as isize - 1) as usize;
    let moved = table.selected() != Some(next);
    table.select(Some(next));
    moved
}

impl Insights {
    fn build(chase: &[ChaseItem]) -> Insights {
        // Aggregate per organization: open-claim count and total outstanding.
        let mut totals: std::collections::HashMap<&str, (usize, Money)> =
            std::collections::HashMap::new();
        for item in chase {
            let entry = totals
                .entry(item.provider.as_str())
                .or_insert((0, Money::ZERO));
            entry.0 += 1;
            entry.1 += item.outstanding;
        }
        let mut providers: Vec<ProviderRow> = totals
            .into_iter()
            .map(|(name, (open, outstanding))| ProviderRow {
                name: name.to_string(),
                open,
                outstanding,
            })
            .collect();
        providers.sort_by(|a, b| {
            b.outstanding
                .cmp(&a.outstanding)
                .then_with(|| a.name.cmp(&b.name))
        });

        let mut provider_table = TableState::default();
        if !providers.is_empty() {
            provider_table.select(Some(0));
        }
        let mut insights = Insights {
            providers,
            provider_table,
            claims: Vec::new(),
            claims_table: TableState::default(),
            focus: InsightsFocus::Providers,
            sort: SortKey::Risk,
            sort_desc: true,
        };
        insights.rebuild_claims(chase);
        insights
    }

    fn selected_provider(&self) -> Option<&ProviderRow> {
        self.provider_table
            .selected()
            .and_then(|i| self.providers.get(i))
    }

    /// Re-derive the claims list for the selected provider under the active
    /// sort, and reset the claim selection to the top.
    fn rebuild_claims(&mut self, chase: &[ChaseItem]) {
        let Some(provider) = self.selected_provider().map(|p| p.name.clone()) else {
            self.claims.clear();
            self.claims_table.select(None);
            return;
        };
        self.claims = chase
            .iter()
            .enumerate()
            .filter(|(_, item)| item.provider == provider)
            .map(|(idx, _)| idx)
            .collect();
        let key = self.sort;
        self.claims.sort_by(|&a, &b| {
            let (a, b) = (&chase[a], &chase[b]);
            let ordering = match key {
                SortKey::Cost => a.outstanding.cmp(&b.outstanding),
                SortKey::Age => a.age.cmp(&b.age),
                SortKey::Risk => a
                    .risk()
                    .partial_cmp(&b.risk())
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            // Deterministic tiebreak keeps equal rows stable across resorts.
            ordering.then_with(|| a.claim_id.cmp(&b.claim_id))
        });
        if self.sort_desc {
            self.claims.reverse();
        }
        self.claims_table
            .select((!self.claims.is_empty()).then_some(0));
    }
}

/// The banner belongs to the caller (it knows the CLI); this trailer names
/// what the panes were computed from.
fn output_banner(output: &RunOutput) -> String {
    format!(
        "{} claims on the books · intake ended {} · drained by {}",
        output.ledger.claims.len(),
        output.intake_finished_at,
        output.finished_at
    )
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [tabs_area, content, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .areas(frame.area());

    match &mut app.done {
        None => {
            frame.render_widget(
                Line::from(" healthcare billing simulation — running ")
                    .bold()
                    .cyan(),
                tabs_area,
            );
            let body = Paragraph::new(app.banner.clone())
                .block(Block::bordered().title(" run configuration "));
            frame.render_widget(body, content);
        }
        Some(done) => {
            // The active tab is a solid block, not just a tinted word: padded
            // title, black-on-cyan, with the rest of the bar dimmed.
            let titles: Vec<String> = done
                .panes
                .iter()
                .map(|p| format!(" {} ", p.title))
                .collect();
            let tabs = Tabs::new(titles)
                .select(app.pane)
                .style(Style::default().fg(Color::DarkGray))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .divider("");
            frame.render_widget(tabs, tabs_area);

            if app.pane == INSIGHTS_PANE {
                draw_insights(frame, content, done);
            } else if app.pane == TIMELINE_PANE {
                draw_timeline(frame, content, done);
            } else {
                let pane = &done.panes[app.pane];
                let body = Paragraph::new(pane.content.clone())
                    .block(Block::bordered().title(format!(" {} ", pane.title)))
                    .scroll((app.scroll, 0));
                frame.render_widget(body, content);
            }

            if let Some(detail) = &done.detail {
                draw_detail(frame, content, detail);
            }
        }
    }

    draw_status(frame, status_area, app);
}

/// The Timeline pane's data: virtual time bucketed into fixed windows, each
/// lifecycle series normalized to events per virtual day, plus the in-flight
/// backlog curve (cumulative ingested minus settled). Built once from the
/// retained event log — the audit trail replayed as a picture.
#[derive(Default)]
struct Timeline {
    end_day: f64,
    intake_end_day: f64,
    bucket_secs: f64,
    ingested: Vec<(f64, f64)>,
    submitted: Vec<(f64, f64)>,
    remitted: Vec<(f64, f64)>,
    settled: Vec<(f64, f64)>,
    in_flight: Vec<(f64, f64)>,
    /// Event totals per series, in the same order as the fields above.
    totals: [usize; 4],
    max_rate: f64,
    max_in_flight: f64,
}

const TIMELINE_BUCKETS: usize = 120;
const VDAY: f64 = 86_400.0;

fn timeline(
    log: &[StampedEvent],
    intake_end: VirtualTime,
    end: VirtualTime,
    buckets: usize,
) -> Timeline {
    let end_secs = end.as_duration().as_secs_f64().max(f64::EPSILON);
    let bucket_secs = end_secs / buckets as f64;
    let mut counts = vec![[0usize; 4]; buckets];
    for stamped in log {
        // Rejected/duplicate rows never take flight, and quarantine/garbage/
        // late notes are epistemics, not flow — the chart tracks claim traffic.
        let series = match &stamped.event {
            ClaimEvent::Ingested { .. } => 0,
            ClaimEvent::Submitted { .. } => 1,
            ClaimEvent::RemittanceApplied { .. } => 2,
            ClaimEvent::Resolved | ClaimEvent::Flagged { .. } => 3,
            _ => continue,
        };
        let at = stamped.at.as_duration().as_secs_f64();
        let idx = ((at / bucket_secs) as usize).min(buckets - 1);
        counts[idx][series] += 1;
    }

    let mut tl = Timeline {
        end_day: end_secs / VDAY,
        intake_end_day: intake_end.as_duration().as_secs_f64() / VDAY,
        bucket_secs,
        ..Timeline::default()
    };
    let per_day = VDAY / bucket_secs;
    let mut open = 0.0;
    for (i, bucket) in counts.iter().enumerate() {
        let x = (i as f64 + 0.5) * bucket_secs / VDAY;
        tl.ingested.push((x, bucket[0] as f64 * per_day));
        tl.submitted.push((x, bucket[1] as f64 * per_day));
        tl.remitted.push((x, bucket[2] as f64 * per_day));
        tl.settled.push((x, bucket[3] as f64 * per_day));
        open += bucket[0] as f64 - bucket[3] as f64;
        tl.in_flight.push((x, open));
        tl.max_in_flight = tl.max_in_flight.max(open);
        for (series, &n) in bucket.iter().enumerate() {
            tl.totals[series] += n;
            tl.max_rate = tl.max_rate.max(n as f64 * per_day);
        }
    }
    tl
}

fn draw_timeline(frame: &mut ratatui::Frame, area: Rect, done: &Done) {
    let tl = &done.timeline;
    let half = [Constraint::Percentage(50), Constraint::Percentage(50)];
    let [rates_area, flight_area] = Layout::vertical(half).areas(area);
    let [row1, row2] = Layout::vertical(half).areas(rates_area);
    let [q_ingested, q_submitted] = Layout::horizontal(half).areas(row1);
    let [q_remitted, q_settled] = Layout::horizontal(half).areas(row2);

    let dim = Style::default().fg(Color::DarkGray);

    // Small multiples on one shared y scale, so rates compare across charts
    // at a glance — submitted riding above ingested is the retry traffic.
    let rate_top = (tl.max_rate * 1.15).max(1.0);
    let quads = [
        (
            q_ingested,
            format!(
                " ingested {} · each point ≈ {} ",
                tl.totals[0],
                human_virtual(tl.bucket_secs)
            ),
            Color::Cyan,
            &tl.ingested,
        ),
        (
            q_submitted,
            format!(" submitted {} (incl. retries) ", tl.totals[1]),
            Color::Yellow,
            &tl.submitted,
        ),
        (
            q_remitted,
            format!(" remitted {} ", tl.totals[2]),
            Color::Green,
            &tl.remitted,
        ),
        (
            q_settled,
            format!(" settled {} (resolved + flagged) ", tl.totals[3]),
            Color::Magenta,
            &tl.settled,
        ),
    ];
    for (quad_area, title, color, data) in quads {
        let datasets = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(data),
        ];
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(title))
            .x_axis(
                Axis::default()
                    .style(dim)
                    .bounds([0.0, tl.end_day])
                    .labels(vec!["0".to_string(), format!("{:.0}d", tl.end_day)]),
            )
            .y_axis(
                Axis::default()
                    .title("per day")
                    .style(dim)
                    .bounds([0.0, rate_top])
                    .labels(vec!["0".to_string(), fmt_axis(rate_top)]),
            );
        frame.render_widget(chart, quad_area);
    }

    // The default legend hides itself above 1/4 of the chart area; the
    // legend is the intake-end marker's only key, so give it headroom.
    let legend_room = (Constraint::Ratio(1, 2), Constraint::Ratio(1, 2));
    let flight_top = (tl.max_in_flight * 1.15).max(1.0);
    let intake_marker = [(tl.intake_end_day, 0.0), (tl.intake_end_day, flight_top)];
    let flight_sets = vec![
        Dataset::default()
            .name("in flight")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::LightRed))
            .data(&tl.in_flight),
        Dataset::default()
            .name(format!("intake ends {:.0}d", tl.intake_end_day))
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(dim)
            .data(&intake_marker),
    ];
    let flight = Chart::new(flight_sets)
        .hidden_legend_constraints(legend_room)
        .block(
            Block::bordered()
                .title(" Backlog — claims in flight (ingested − settled) · drains to zero "),
        )
        .x_axis(
            Axis::default()
                .title("virtual days")
                .style(dim)
                .bounds([0.0, tl.end_day])
                .labels(vec![
                    "0".to_string(),
                    format!("{:.0}", tl.end_day / 2.0),
                    format!("{:.0}d", tl.end_day),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("open")
                .style(dim)
                .bounds([0.0, flight_top])
                .labels(vec![
                    "0".to_string(),
                    fmt_axis(flight_top / 2.0),
                    fmt_axis(flight_top),
                ]),
        );
    frame.render_widget(flight, flight_area);
}

fn fmt_axis(v: f64) -> String {
    if v >= 10.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn draw_insights(frame: &mut ratatui::Frame, area: Rect, done: &mut Done) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    frame.render_widget(
        Line::from(
            " ↑/↓ select provider · tab or enter → their claims (tab back) · \
             c/a/r sort claims by cost/age/risk, press again to flip · \
             enter on a claim: full breakdown ",
        )
        .style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );

    let ins = &mut done.insights;
    if ins.providers.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing outstanding — all receivables booked")
                .block(Block::bordered().title(" Provider insights ")),
            body,
        );
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(40), Constraint::Min(40)]).areas(body);

    let focus_style = Style::default().fg(Color::Cyan);
    let blur_style = Style::default().fg(Color::DarkGray);
    let highlight = Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    // Left: the providers, biggest book of open money first.
    let provider_rows: Vec<Row> = ins
        .providers
        .iter()
        .map(|p| {
            Row::new([
                p.name.clone(),
                p.open.to_string(),
                p.outstanding.to_string(),
            ])
        })
        .collect();
    let provider_focused = ins.focus == InsightsFocus::Providers;
    let providers = Table::new(
        provider_rows,
        [
            Constraint::Min(20),
            Constraint::Length(4),
            Constraint::Length(11),
        ],
    )
    .header(Row::new(["provider", "open", "outstanding"]).style(Modifier::BOLD))
    .block(
        Block::bordered()
            .title(format!(
                " Providers — {} with open A/R ",
                ins.providers.len()
            ))
            .border_style(if provider_focused {
                focus_style
            } else {
                blur_style
            }),
    )
    .row_highlight_style(highlight)
    .highlight_symbol(if provider_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(providers, left, &mut ins.provider_table);

    // Right: the selected provider's open claims under the active sort.
    let arrow = if ins.sort_desc { "↓" } else { "↑" };
    let sort_header = |name: &str, key: SortKey| {
        if ins.sort == key {
            format!("{name} {arrow}")
        } else {
            name.to_string()
        }
    };
    let header = Row::new([
        "claim".to_string(),
        "payer".to_string(),
        sort_header("outstanding", SortKey::Cost),
        sort_header("age", SortKey::Age),
        sort_header("risk", SortKey::Risk),
        "att".to_string(),
        "status".to_string(),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let claim_rows: Vec<Row> = ins
        .claims
        .iter()
        .filter_map(|&idx| done.chase.get(idx))
        .map(|item| {
            Row::new([
                item.claim_id.0.clone(),
                item.payer_id.as_str().to_string(),
                item.outstanding.to_string(),
                format!("{:.1}d", item.age.as_secs_f64() / 86_400.0),
                format!("{:.0}", item.risk()),
                item.attempts.to_string(),
                item.status.clone(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(4),
        Constraint::Min(14),
    ];
    let title = match ins.selected_provider() {
        Some(p) => format!(
            " {} — {} open · {} outstanding · sorted by {} {} (risk = $ × days) ",
            p.name,
            p.open,
            p.outstanding,
            ins.sort.label(),
            arrow,
        ),
        None => " claims ".to_string(),
    };
    let claims_focused = ins.focus == InsightsFocus::Claims;
    let claims = Table::new(claim_rows, widths)
        .header(header)
        .block(
            Block::bordered()
                .title(title)
                .border_style(if claims_focused {
                    focus_style
                } else {
                    blur_style
                }),
        )
        .row_highlight_style(highlight)
        .highlight_symbol(if claims_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(claims, right, &mut ins.claims_table);
}

fn draw_detail(frame: &mut ratatui::Frame, area: Rect, detail: &str) {
    let width = area.width.saturating_sub(6).min(96);
    let height = area.height.saturating_sub(2);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let body = Paragraph::new(detail.to_string())
        .wrap(Wrap { trim: false })
        .block(
            Block::bordered()
                .title(" claim audit trail — esc to close ")
                .border_style(Style::default().fg(Color::Cyan)),
        );
    frame.render_widget(body, popup);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    const FRAMES: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];
    let p = &app.progress;
    let counts = format!(
        "{} claims · {} resolved · {} rejected · {} flagged · {}",
        p.claims,
        p.resolved,
        p.rejected,
        p.flagged,
        human_virtual(p.now.as_duration().as_secs_f64()),
    );
    // The headline carries live state; the keys line is deliberately the
    // same on every pane and in every phase — pane-specific instructions
    // live at the top of the pane they belong to.
    let headline = match app.wall_done {
        None => format!(
            "{} running · {counts} · {:.1}s wall",
            FRAMES[app.frame % FRAMES.len()],
            app.wall_start.elapsed().as_secs_f64()
        ),
        Some(wall) => format!(
            "✓ complete · {counts} · {:.2}s wall · seed {}",
            wall.as_secs_f64(),
            app.seed
        ),
    };
    let status = Paragraph::new(format!("{headline}\n{KEYS_HINT}"))
        .block(Block::bordered().title(" simulation "));
    frame.render_widget(status, area);
}

/// Everything the ledger knows about one claim, straight from the audit
/// trail: identity, both states (at the intake snapshot and final), money
/// lines, and the full event history — the event-sourced ledger on display.
fn claim_detail(output: &RunOutput, claim_id: &str) -> String {
    let key = healthcare_billing_sim::domain::ClaimId(claim_id.to_string());
    let Some(record) = output.ledger.claims.get(&key) else {
        return format!("claim {claim_id} not found");
    };
    let snapshot = output.intake_ledger.claims.get(&key);
    let snapshot_state = snapshot
        .map(state_label)
        .unwrap_or_else(|| "not yet ingested".to_string());

    let mut out = String::new();
    out.push_str(&format!("claim {claim_id}\n"));
    if let Some(identity) = &record.identity {
        out.push_str(&format!(
            "payer {} · member {} · npi {}\n",
            identity.payer_id, identity.patient_member_id, identity.provider_npi
        ));
    }
    out.push_str(&format!(
        "at intake snapshot: {snapshot_state}\nfinal state:        {} · {} attempts\n",
        state_label(record),
        record.attempts
    ));
    out.push_str("\nwhat this means:\n");
    match snapshot.filter(|s| s.state != record.state) {
        Some(snap) => out.push_str(&format!(
            "• at the A/R snapshot — {}: {}\n• by the end of the run — {}: {}\n",
            status_label(snap),
            state_explanation(snap),
            status_label(record),
            state_explanation(record),
        )),
        None => out.push_str(&format!("{}\n", state_explanation(record))),
    }
    out.push_str(&format!(
        "\noutstanding {} of {} billed\n\nservice lines:\n",
        record.payer_outstanding(),
        record.lines.iter().map(|l| l.billed()).sum::<Money>()
    ));
    for line in &record.lines {
        let status = match &line.adjudication {
            _ if line.do_not_bill => "do_not_bill".to_string(),
            Some(adj) => format!(
                "paid {} · patient {} · not allowed {}{}",
                adj.payer_paid,
                adj.patient_responsibility(),
                adj.not_allowed,
                adj.denial_reason
                    .map(|r| format!(" · denied: {r}"))
                    .unwrap_or_default()
            ),
            None => "unanswered".to_string(),
        };
        out.push_str(&format!(
            "  {} {} ×{} @ {} — {status}\n",
            line.service_line_id, line.procedure_code, line.units, line.unit_charge
        ));
    }
    out.push_str("\nevent history:\n");
    for stamped in &record.history {
        out.push_str(&format!(
            "  {:<26} {}\n",
            stamped.at.to_string(),
            event_label(&stamped.event)
        ));
    }
    out
}

/// The chase list's plain-English status, with the armed deadline appended
/// while the claim is still waiting.
fn state_label(record: &ClaimRecord) -> String {
    match &record.state {
        ClaimState::AwaitingResponse { timeout_at } => {
            format!("{} (deadline {timeout_at})", status_label(record))
        }
        _ => status_label(record),
    }
}

/// What the state means in this simulation, in operator language — the
/// claim-detail overlay's decoder ring for the status column.
fn state_explanation(record: &ClaimRecord) -> String {
    let answered = record
        .lines
        .iter()
        .any(|l| !l.do_not_bill && l.adjudication.is_some());
    match &record.state {
        ClaimState::Pending => "ingested but not yet submitted — claims spend only \
            milliseconds here between the ingest event and the first submission."
            .into(),
        ClaimState::AwaitingResponse { .. } if answered => "a remittance arrived, but some \
            service lines were missing — lost on the way back from the payer. The lines that \
            made it are balanced and booked; the unanswered ones keep the claim open and \
            aging. At the deadline the biller resubmits, and the payer's re-derived answer \
            can fill in the gaps."
            .into(),
        ClaimState::AwaitingResponse { .. } if record.attempts > 1 => "an earlier submission \
            went unanswered past its follow-up deadline, so the biller sent the claim again. \
            All silence looks the same from here: the claim may have been dropped on the way \
            to the payer, the answer dropped or delayed on the way back, or the payer may \
            simply be slow. Resubmitting is safe — a payer that already adjudicated re-issues \
            the identical remittance."
            .into(),
        ClaimState::AwaitingResponse { .. } => "submitted to the payer through the \
            clearinghouse; no answer yet, and the follow-up deadline hasn't passed. The \
            biller cannot tell a slow payer from a dropped claim or a lost answer — silence \
            is all it sees. If the deadline passes it resubmits, a bounded number of times."
            .into(),
        ClaimState::Resolved => "every billable line has an adjudication and the books \
            balance to the cent — payer paid + patient responsibility + not allowed exactly \
            equals what was billed. Nothing left outstanding."
            .into(),
        ClaimState::Rejected { .. } => "failed validation at ingest (bad JSON, schema \
            violation, or billing policy) and was never submitted to any payer. Kept as a \
            ledger row so no input line is silently dropped."
            .into(),
        ClaimState::Flagged {
            reason: FlagReason::RetriesExhausted,
        } => format!(
            "the biller submitted this claim {} times — its whole retry budget — and every \
            deadline passed in silence: on each attempt the claim or its answer was dropped \
            (or delayed past the timeout) in transit. Unlike a claim still awaiting or \
            retrying, the biller has stopped chasing: the claim is parked for human \
            follow-up and its unanswered billed amount stays in A/R. A remittance that \
            straggles in later can still flip it to resolved.",
            record.attempts
        ),
        ClaimState::Flagged {
            reason: FlagReason::ReconciliationFailed { billed, accounted },
        } => {
            let delta = if accounted < billed {
                format!("short by {}", *billed - *accounted)
            } else {
                format!("over by {}", *accounted - *billed)
            };
            format!(
                "the payer answered, but the money doesn't add up. Every line must satisfy \
                billed = payer paid + patient responsibility (copay/coinsurance/deductible) \
                + not allowed, exactly, in cents. Here the payer accounted for {accounted} \
                against {billed} billed ({delta}) — the simulation's dishonest-payer fault. \
                The biller refuses to book figures that don't reconcile, so the disputed \
                amount stays outstanding for a human to take up with the payer."
            )
        }
        ClaimState::Flagged {
            reason: FlagReason::MalformedRemittance,
        } => "a remittance correlated to this claim but referenced service lines the claim \
            doesn't have (or repeated one) — not usable as an answer, so the claim is parked \
            for human review."
            .into(),
    }
}

fn event_label(event: &ClaimEvent) -> String {
    match event {
        ClaimEvent::Ingested { .. } => "ingested".into(),
        ClaimEvent::Rejected { reason } => format!("rejected: {reason:?}"),
        ClaimEvent::DuplicateIngest { line_no } => {
            format!("duplicate claim_id on input line {line_no} — first document wins")
        }
        ClaimEvent::Submitted {
            attempt,
            timeout_at,
        } => {
            format!("submitted, attempt {attempt} (deadline {timeout_at})")
        }
        ClaimEvent::RemittanceApplied { lines } => {
            format!("remittance applied: {} line(s) adjudicated", lines.len())
        }
        ClaimEvent::Resolved => "resolved — books balance exactly".into(),
        ClaimEvent::Flagged { reason } => format!("flagged for review: {reason:?}"),
        ClaimEvent::RemittanceQuarantined => "remittance quarantined (unknown claim)".into(),
        ClaimEvent::GarbageRemittance => "garbage remittance received — treated as silence".into(),
        ClaimEvent::LateRemittance { .. } => "late remittance after terminal state".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use healthcare_billing_sim::domain::{ClaimId, PayerId};
    use healthcare_billing_sim::ledger::records::{ClaimIdentity, FlagReason};

    use super::*;

    fn at(days: f64) -> VirtualTime {
        VirtualTime::default() + Duration::from_secs_f64(days * VDAY)
    }

    fn ev(days: f64, event: ClaimEvent) -> StampedEvent {
        StampedEvent {
            at: at(days),
            claim_id: ClaimId("c1".into()),
            event,
        }
    }

    fn ingested() -> ClaimEvent {
        ClaimEvent::Ingested {
            identity: ClaimIdentity {
                payer_id: PayerId::Medicare,
                provider_npi: String::new(),
                organization_name: String::new(),
                patient_member_id: String::new(),
            },
            lines: Vec::new(),
        }
    }

    #[test]
    fn timeline_buckets_events_and_normalizes_to_per_day_rates() {
        // 10 virtual days into 10 buckets -> 1-day buckets, per_day factor 1.
        let log = vec![
            ev(0.2, ingested()),
            ev(0.7, ingested()),
            ev(3.5, ClaimEvent::Resolved),
            ev(9.99, ClaimEvent::Resolved),
        ];
        let tl = timeline(&log, at(5.0), at(10.0), 10);
        assert_eq!(tl.totals, [2, 0, 0, 2]);
        assert_eq!(tl.ingested[0].1, 2.0);
        assert_eq!(tl.settled[3].1, 1.0);
        assert_eq!(tl.settled[9].1, 1.0);
        assert!((tl.end_day - 10.0).abs() < 1e-9);
        assert!((tl.intake_end_day - 5.0).abs() < 1e-9);
    }

    #[test]
    fn in_flight_tracks_ingested_minus_settled_and_drains_to_zero() {
        let log = vec![
            ev(1.0, ingested()),
            ev(2.0, ingested()),
            ev(
                2.1,
                ClaimEvent::Submitted {
                    attempt: 1,
                    timeout_at: at(4.0),
                },
            ),
            ev(6.0, ClaimEvent::Resolved),
            ev(
                8.0,
                ClaimEvent::Flagged {
                    reason: FlagReason::RetriesExhausted,
                },
            ),
        ];
        let tl = timeline(&log, at(2.5), at(10.0), 10);
        assert_eq!(tl.in_flight[1].1, 1.0);
        assert_eq!(tl.in_flight[2].1, 2.0);
        assert_eq!(tl.in_flight[6].1, 1.0);
        assert_eq!(tl.in_flight[9].1, 0.0);
        // Submissions are traffic, never backlog: retries must not inflate it.
        assert_eq!(tl.totals[1], 1);
        assert!(tl.in_flight.iter().all(|&(_, y)| y >= 0.0));
    }
}
