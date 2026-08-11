//! Interactive terminal UI: four panes on a numbered tab bar (←/→ or 1-4 to
//! move), a live status box pinned to the bottom, and a `?` help overlay.
//! Navigation is layered: Enter steps down (into a provider's A/R analysis
//! or the A/R timeline scrubber), Esc steps back up — the top layer being
//! the pane bar — and Ctrl-C is the only way out of the program (the plain
//! report prints on the way).
//!
//! Pane map — the reports arranged as one instrument, money views first,
//! every pane scrollable by provider:
//!   1 A/R Aging            outcome bars + aging books as of one moment,
//!                          scrubbable a day at a time across the run
//!   2 Provider Insights    per-provider A/R analysis over the drained final
//!                          books — what's still open, and what to chase first
//!   3 Timeline             the run replayed as rate + backlog charts, per book
//!   4 Payer Scorecard      graded payers + denial detail (scorecard ∪ denials)
//!
//! Threading: the TUI event loop owns the main thread on the *wall* clock;
//! the simulation runs on its own multi-thread runtime behind a spawned
//! thread (DESIGN.md 'Virtual time' — time is computed, so claim tasks parallelize).
//! They meet at two channels: the fold's live-progress watch tap, and a
//! oneshot carrying the finished `RunOutput`.

mod aging;
mod insights;
mod payers;
mod theme;
mod timeline;

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Gauge, Paragraph, TableState, Tabs};
use tokio::sync::watch;

use healthcare_billing_sim::domain::{PayerId, human_virtual};
use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::ledger::records::ClaimState;
use healthcare_billing_sim::reports::chase_list;
use healthcare_billing_sim::sim::faults::FaultProfile;
use healthcare_billing_sim::sim::payer::PayerConfig;
use healthcare_billing_sim::{RunConfig, RunOutput};

use theme::{ACCENT, BAD, GOOD, WARN, bold, dim};

pub struct TuiOptions {
    /// Run-configuration banner as (label, value) rows — the same source of
    /// truth main.rs prints to stdout.
    pub banner_rows: Vec<(String, String)>,
    pub seed: u64,
    /// Worker threads for the sim runtime (0 = one per core).
    pub threads: usize,
}

/// Run the simulation under the interactive UI and hand the finished output
/// back so the caller can leave a plain-text record on stdout.
pub fn run(mut cfg: RunConfig, opts: TuiOptions) -> anyhow::Result<RunOutput> {
    // The Payer Scorecard pane shows configured personalities next to the
    // observed statistics — capture them before cfg moves to the sim.
    let payer_configs = cfg.payers.clone();
    let payer_routes = cfg.payer_faults.clone();

    let (progress_tx, progress_rx) = watch::channel(Progress::default());
    cfg.progress = Some(progress_tx);
    let (done_tx, done_rx) = mpsc::channel();
    let threads = opts.threads;
    let sim = std::thread::spawn(move || {
        // Multi-thread runtime (DESIGN.md 'Virtual time'): nothing sleeps, so no paused
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
    let result = event_loop(
        &mut terminal,
        progress_rx,
        &done_rx,
        &opts,
        payer_configs,
        payer_routes,
    );
    ratatui::restore();
    let _ = sim.join();
    result
}

const AGING: usize = 0;
const PROVIDERS: usize = 1;
const PAYERS: usize = 3;
// Pane 2, Timeline, is the draw dispatch's wildcard arm.
const PANE_TITLES: [&str; 4] = [
    "A/R Aging",
    "Provider Insights",
    "Timeline",
    "Payer Scorecard",
];

/// One keys line, everywhere, always — the status box never rewords itself.
/// Pane-specific instructions live in the hint row of the pane they belong
/// to, and the full map is one `?` away.
const KEYS_HINT: &str =
    "←/→ panes · ↑/↓ select · enter steps in · esc steps out · ? keys · ctrl-c quit";

struct App {
    banner_rows: Vec<(String, String)>,
    seed: u64,
    wall_start: Instant,
    wall_done: Option<Duration>,
    progress: Progress,
    /// None while the simulation is still running.
    done: Option<Done>,
    pane: usize,
    frame: usize,
    help: bool,
}

struct Done {
    output: RunOutput,
    timeline: timeline::TimelineView,
    payers: payers::PayersView,
    aging: aging::AgingView,
    insights: insights::Insights,
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut progress_rx: watch::Receiver<Progress>,
    done_rx: &mpsc::Receiver<anyhow::Result<RunOutput>>,
    opts: &TuiOptions,
    payer_configs: HashMap<PayerId, PayerConfig>,
    payer_routes: HashMap<PayerId, FaultProfile>,
) -> anyhow::Result<RunOutput> {
    let mut app = App {
        banner_rows: opts.banner_rows.clone(),
        seed: opts.seed,
        wall_start: Instant::now(),
        wall_done: None,
        progress: Progress::default(),
        done: None,
        pane: 0,
        frame: 0,
        help: false,
    };

    loop {
        if app.done.is_none() {
            app.progress = *progress_rx.borrow_and_update();
            if let Ok(result) = done_rx.try_recv() {
                let output = result?;
                app.wall_done = Some(app.wall_start.elapsed());
                app.progress = final_progress(&output);
                app.done = Some(Done::build(
                    output,
                    payer_configs.clone(),
                    payer_routes.clone(),
                ));
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
        // Terminals speaking an enhanced keyboard protocol report auto-repeat
        // as Repeat instead of a stream of Presses — treat both as a press so
        // holding a key (the timeline scrubber's fast-forward included)
        // behaves the same everywhere. Releases are noise.
        if key.kind == KeyEventKind::Release {
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
/// Only Ctrl-C quits — Enter steps down a layer, Esc steps back up, and at
/// the top layer (the pane bar) Esc is a no-op.
fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    // Ctrl-C always leaves, overlays or not — and nothing else does.
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    // The help overlay swallows everything: any key closes it, nothing quits.
    if app.help {
        app.help = false;
        return false;
    }
    if code == KeyCode::Char('?') {
        app.help = true;
        return false;
    }
    let Some(done) = &mut app.done else {
        return false;
    };

    let panes = PANE_TITLES.len();
    match code {
        // While the A/R timeline is grabbed, ←/→ scrub the as-of day; the
        // pane bar gets them back when Esc lets go.
        KeyCode::Left if app.pane == AGING && done.aging.timeline_grabbed() => {
            done.aging.step_day(-1, &done.output)
        }
        KeyCode::Right if app.pane == AGING && done.aging.timeline_grabbed() => {
            done.aging.step_day(1, &done.output)
        }
        KeyCode::Left => app.pane = (app.pane + panes - 1) % panes,
        KeyCode::Right => app.pane = (app.pane + 1) % panes,
        KeyCode::Char(c @ '1'..='4') => app.pane = c as usize - '1' as usize,
        KeyCode::Up if app.pane == PROVIDERS => done.insights.move_selection(-1, &done.output),
        KeyCode::Down if app.pane == PROVIDERS => done.insights.move_selection(1, &done.output),
        KeyCode::PageUp if app.pane == PROVIDERS => {
            done.insights.move_selection(-10, &done.output)
        }
        KeyCode::PageDown if app.pane == PROVIDERS => {
            done.insights.move_selection(10, &done.output)
        }
        KeyCode::Up if app.pane == PAYERS => done.payers.move_selection(-1),
        KeyCode::Down if app.pane == PAYERS => done.payers.move_selection(1),
        KeyCode::PageUp if app.pane == PAYERS => done.payers.move_selection(-10),
        KeyCode::PageDown if app.pane == PAYERS => done.payers.move_selection(10),
        KeyCode::Up if app.pane == AGING => done.aging.key_move(-1, &done.output),
        KeyCode::Down if app.pane == AGING => done.aging.key_move(1, &done.output),
        KeyCode::PageUp if app.pane == AGING => done.aging.key_move(-10, &done.output),
        KeyCode::PageDown if app.pane == AGING => done.aging.key_move(10, &done.output),
        KeyCode::Up => done.timeline.move_selection(-1, &done.output),
        KeyCode::Down => done.timeline.move_selection(1, &done.output),
        KeyCode::PageUp => done.timeline.move_selection(-10, &done.output),
        KeyCode::PageDown => done.timeline.move_selection(10, &done.output),
        // Enter steps down a layer; Esc steps back up, stopping at the pane
        // bar (it never quits — that's Ctrl-C's job alone).
        KeyCode::Enter if app.pane == AGING => done.aging.enter_report(),
        KeyCode::Esc if app.pane == AGING => {
            done.aging.escape();
        }
        KeyCode::Esc if app.pane == PROVIDERS => {
            done.insights.escape();
        }
        // Enter steps down into the analysis document — the terminal layer;
        // ↑/↓ then scroll it, Esc steps back to the provider list.
        KeyCode::Enter if app.pane == PROVIDERS => {
            done.insights.focus = insights::Focus::Analysis;
        }
        _ => {}
    }
    false
}

/// Advance a table selection by `delta`, clamped; returns true if it moved.
pub(crate) fn step(table: &mut TableState, len: usize, delta: isize) -> bool {
    if len == 0 {
        return false;
    }
    let current = table.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, len as isize - 1) as usize;
    let moved = table.selected() != Some(next);
    table.select(Some(next));
    moved
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
    fn build(
        output: RunOutput,
        payer_configs: HashMap<PayerId, PayerConfig>,
        payer_routes: HashMap<PayerId, FaultProfile>,
    ) -> Done {
        // The chase list feeds Provider Insights, which reads the drained
        // final books: what's still open at run completion is exactly the
        // pile a human has to work. (The A/R Aging pane keeps its own as-of
        // snapshot — mid-flight by default, scrubbable across the run.)
        let chase = chase_list(&output.ledger, output.finished_at, usize::MAX);
        let timeline = timeline::build(&output);
        let payers = payers::build(&output, payer_configs, payer_routes);
        let aging = aging::build(&output);
        let insights = insights::Insights::build(&output, &chase);
        Done {
            output,
            timeline,
            payers,
            aging,
            insights,
        }
    }
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [tabs_area, content, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .areas(frame.area());

    match &mut app.done {
        None => draw_running(frame, tabs_area, content, app),
        Some(done) => {
            // The active tab is a solid block, not just a tinted word: padded
            // numbered title, black-on-cyan, with the rest of the bar dimmed.
            let titles: Vec<Line> = PANE_TITLES
                .iter()
                .enumerate()
                .map(|(i, title)| Line::from(format!(" {} {title} ", i + 1)))
                .collect();
            let tabs = Tabs::new(titles)
                .select(app.pane)
                .style(dim())
                .highlight_style(theme::bold().fg(Color::Black).bg(ACCENT))
                .divider("");
            frame.render_widget(tabs, tabs_area);

            match app.pane {
                AGING => aging::draw(frame, content, &mut done.aging, &done.output),
                PAYERS => payers::draw(frame, content, &mut done.payers),
                PROVIDERS => insights::draw(frame, content, &mut done.insights),
                _ => timeline::draw(frame, content, &mut done.timeline),
            }
        }
    }

    draw_status(frame, status_area, app);
    if app.help {
        draw_help(frame, frame.area());
    }
}

/// The waiting room: the run configuration under a live drain gauge, so the
/// screen is telling the story before the first pane exists.
fn draw_running(frame: &mut ratatui::Frame, tabs_area: Rect, content: Rect, app: &App) {
    frame.render_widget(
        Line::from(vec![
            Span::styled(
                " healthcare billing simulation — running ",
                theme::accent_bold(),
            ),
            Span::styled("· the dashboard opens when the books drain", dim()),
        ]),
        tabs_area,
    );
    let [banner_area, gauge_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(content);
    let banner_lines: Vec<Line> = app
        .banner_rows
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!(" {label:<22} "), bold()),
                Span::raw(value.clone()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(banner_lines).block(theme::panel("run configuration")),
        banner_area,
    );

    let p = &app.progress;
    let settled = p.resolved + p.rejected + p.flagged;
    let ratio = if p.claims > 0 {
        (settled as f64 / p.claims as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .block(theme::panel("claims driven to a terminal state"))
        .gauge_style(Style::default().fg(ACCENT).bg(Color::DarkGray))
        .ratio(ratio)
        .label(format!(
            "{} / {} settled",
            theme::thousands(settled as u64),
            theme::thousands(p.claims as u64)
        ));
    frame.render_widget(gauge, gauge_area);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    const FRAMES: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];
    let p = &app.progress;
    let counts = vec![
        Span::styled(theme::thousands(p.claims as u64), bold()),
        Span::raw(" claims · "),
        Span::styled(
            theme::thousands(p.resolved as u64),
            Style::default().fg(GOOD),
        ),
        Span::raw(" resolved · "),
        Span::styled(
            theme::thousands(p.rejected as u64),
            Style::default().fg(WARN),
        ),
        Span::raw(" rejected · "),
        Span::styled(theme::thousands(p.flagged as u64), Style::default().fg(BAD)),
        Span::raw(" flagged · "),
        Span::raw(human_virtual(p.now.as_duration().as_secs_f64())),
    ];
    // The headline carries live state; the keys line is deliberately the
    // same on every pane and in every phase — pane-specific instructions
    // live in the hint row of the pane they belong to.
    let mut headline = match app.wall_done {
        None => vec![Span::styled(
            format!(" {} running · ", FRAMES[app.frame % FRAMES.len()]),
            theme::accent_bold(),
        )],
        Some(_) => vec![Span::styled(" ✓ complete · ", theme::bold().fg(GOOD))],
    };
    headline.extend(counts);
    match app.wall_done {
        None => headline.push(Span::styled(
            format!(" · {:.1}s wall", app.wall_start.elapsed().as_secs_f64()),
            dim(),
        )),
        Some(wall) => headline.push(Span::styled(
            format!(" · {:.2}s wall · seed {}", wall.as_secs_f64(), app.seed),
            dim(),
        )),
    }
    let status = Paragraph::new(vec![
        Line::from(headline),
        Line::from(Span::styled(format!(" {KEYS_HINT}"), dim())),
    ])
    .block(theme::panel("simulation"));
    frame.render_widget(status, area);
}

/// The `?` overlay: every key in the app on one card.
fn draw_help(frame: &mut ratatui::Frame, area: Rect) {
    let key = |k: &str, action: &str| {
        Line::from(vec![
            Span::styled(format!("   {k:<12}"), Style::default().fg(ACCENT)),
            Span::raw(action.to_string()),
        ])
    };
    let note = |text: &str| Line::from(Span::styled(format!("   {text}"), dim()));
    let section = |title: &str| Line::from(Span::styled(format!(" {title}"), theme::accent_bold()));
    let lines = vec![
        section("Everywhere"),
        key("←/→", "move between panes"),
        key("1-4", "jump straight to a pane"),
        key("↑/↓", "scroll, or move the selection"),
        key("pgup/pgdn", "the same, ten at a time"),
        key("enter", "step down a layer — select, grab, drill in"),
        key(
            "esc",
            "step back up a layer — the top layer is the pane bar",
        ),
        key("ctrl-c", "quit — the plain report prints to stdout"),
        Line::default(),
        section("1 A/R Aging"),
        key("↑/↓", "pick a book — all providers first, then each one"),
        key(
            "enter",
            "into the report: ↑/↓ scroll, ←/→ move the books a day (hold 1s: ×4)",
        ),
        key("esc", "back out to the provider list"),
        note("buckets shade green → red as receivables age past 90 days"),
        Line::default(),
        section("2 Provider Insights"),
        key("↑/↓", "pick a provider; the analysis follows"),
        key("enter", "into the analysis document; ↑/↓ scroll it"),
        key("esc", "back up to the provider list"),
        note("every figure computed from the drained final books — nothing generated"),
        Line::default(),
        section("3 Timeline"),
        key("↑/↓", "pick a book — the charts re-bucket to its claims"),
        note("rate charts share one y-scale; the backlog must drain to zero"),
        Line::default(),
        section("4 Payer Scorecard"),
        key("↑/↓", "pick a payer; the detail below follows"),
        note("grades are on the curve: denial rate + response time + paid share"),
        Line::default(),
        Line::from(Span::styled("   any key closes this card", dim())),
    ];
    let popup = theme::popup(area, 74, lines.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(theme::panel("keyboard map").border_style(Style::default().fg(ACCENT))),
        popup,
    );
}

/// Render smoke tests: run a real (small, messy) simulation and draw every
/// pane and overlay into a TestBackend — the closest an automated check gets
/// to pressing the keys. A panic anywhere in layout/formatting fails here.
#[cfg(test)]
mod render_tests {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;

    static INPUT_ID: AtomicUsize = AtomicUsize::new(0);

    fn claim_line(i: usize) -> String {
        let payers = ["medicare", "anthem", "aetna", "cigna", "humana"];
        json!({
            "claim_id": format!("c-{i:03}"),
            "place_of_service_code": 11,
            "insurance": {"payer_id": payers[i % payers.len()], "patient_member_id": format!("M-{i}")},
            "patient": {"first_name": "Ada", "last_name": "Lovelace", "gender": "f", "dob": "1985-12-10"},
            "organization": {"name": format!("Clinic {}", i % 4)},
            "rendering_provider": {"first_name": "Grace", "last_name": "Hopper", "npi": "1234567890"},
            "service_lines": [{
                "service_line_id": "L1",
                "procedure_code": "99213",
                "units": 1,
                "details": "office visit",
                "unit_charge_currency": "USD",
                "unit_charge_amount": 100.0 + i as f64,
                "do_not_bill": false,
            }],
        })
        .to_string()
    }

    /// A finished App over a real messy run: 40 claims across five payers,
    /// ingested fast enough that receivables are still open at intake end.
    fn done_app() -> App {
        let unique = INPUT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tui-render-test-{}-{unique}.jsonl",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).expect("create input");
        for i in 0..40 {
            writeln!(file, "{}", claim_line(i)).expect("write input");
        }
        let mut cfg = RunConfig::new(path, 7, 10.0);
        healthcare_billing_sim::scenario::preset("messy")
            .expect("preset exists")
            .apply(&mut cfg);
        let configs = cfg.payers.clone();
        let routes = cfg.payer_faults.clone();
        let output = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(healthcare_billing_sim::run(cfg))
            .expect("run");
        App {
            banner_rows: vec![
                ("input".to_string(), "test.jsonl".to_string()),
                ("seed".to_string(), "7".to_string()),
            ],
            seed: 7,
            wall_start: Instant::now(),
            wall_done: Some(Duration::from_secs(1)),
            progress: final_progress(&output),
            done: Some(Done::build(output, configs, routes)),
            pane: 0,
            frame: 0,
            help: false,
        }
    }

    fn render(app: &mut App) -> String {
        let backend = TestBackend::new(140, 44);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn every_pane_and_overlay_renders() {
        let mut app = done_app();
        let markers = [
            "A/R Aging — all providers",
            "Providers —",
            "Backlog",
            "Configured —",
        ];
        for (pane, marker) in markers.iter().enumerate() {
            app.pane = pane;
            let text = render(&mut app);
            assert!(
                text.contains(marker),
                "pane {pane} is missing {marker:?}:\n{text}"
            );
        }

        // The help overlay sits on top of whatever pane is active.
        handle_key(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(render(&mut app).contains("keyboard map"));
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        // The providers pane renders the selected provider's analysis
        // document — headline first, every figure computed from the ledger.
        app.pane = PROVIDERS;
        let text = render(&mut app);
        assert!(text.contains("A/R analysis"), "no analysis doc:\n{text}");
        assert!(text.contains("HEADLINE"), "{text}");
        assert!(text.contains("outstanding across"), "{text}");
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        // The aging pane re-derives its report — outcome bars included —
        // for a single provider.
        app.pane = AGING;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let selected = app
            .done
            .as_ref()
            .expect("done")
            .aging
            .selected_name()
            .expect("a provider row")
            .to_string();
        let text = render(&mut app);
        assert!(
            text.contains(&format!("A/R Aging — {selected}")),
            "per-provider aging title missing:\n{text}"
        );
        assert!(
            text.contains("claims — ") && text.contains("money — "),
            "outcome bars missing from the aging report:\n{text}"
        );

        // The timeline pane re-buckets its charts for a single provider.
        app.pane = 2;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let selected = app
            .done
            .as_ref()
            .expect("done")
            .timeline
            .selected_name()
            .expect("a provider row")
            .to_string();
        let text = render(&mut app);
        assert!(
            text.contains(&format!("{selected} — ingested")),
            "per-provider timeline title missing:\n{text}"
        );
    }

    #[test]
    fn running_screen_renders_banner_and_gauge() {
        let mut app = done_app();
        app.done = None;
        app.wall_done = None;
        let text = render(&mut app);
        assert!(text.contains("run configuration"));
        assert!(text.contains("terminal state"));
    }

    #[test]
    fn keys_route_between_panes_and_quit() {
        let mut app = done_app();
        handle_key(&mut app, KeyCode::Char('4'), KeyModifiers::NONE);
        assert_eq!(app.pane, PAYERS);
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            app.done.as_ref().expect("done").payers.table.selected(),
            Some(1)
        );
        // Right from the last pane wraps around to the first.
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.pane, AGING);
        handle_key(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(app.pane, PROVIDERS);
        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        // Neither q nor Esc terminates anymore — only Ctrl-C does.
        assert!(!handle_key(
            &mut app,
            KeyCode::Char('q'),
            KeyModifiers::NONE
        ));
        assert!(!handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE));
        assert!(handle_key(
            &mut app,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn enter_grabs_the_aging_timeline_and_esc_lets_go() {
        let mut app = done_app();
        app.pane = AGING;
        let grabbed = |app: &App| app.done.as_ref().expect("done").aging.timeline_grabbed();

        // Enter grabs the timeline: ←/→ scrub the as-of day, not the panes.
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(grabbed(&app));
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.pane, AGING, "← must scrub the day, not switch panes");
        let text = render(&mut app);
        assert!(text.contains("timeline"), "no scrubber row:\n{text}");
        assert!(
            text.contains("books as of"),
            "report must carry its as-of moment:\n{text}"
        );

        // Esc lets go; the arrows control the pane bar again.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!grabbed(&app));
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.pane, PANE_TITLES.len() - 1);
    }

    #[test]
    fn insights_enter_steps_down_and_esc_steps_up() {
        let mut app = done_app();
        app.pane = PROVIDERS;
        let focus = |app: &App| app.done.as_ref().expect("done").insights.focus;
        let scroll = |app: &App| app.done.as_ref().expect("done").insights.scroll;

        assert!(matches!(focus(&app), insights::Focus::Providers));
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(focus(&app), insights::Focus::Analysis));

        // With the document focused, ↑/↓ scroll it instead of moving the
        // provider selection.
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 1);
        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 0);

        // Esc steps back up; the analysis is the terminal layer, so one Esc
        // lands on the providers, where Esc becomes a no-op — never a quit.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(focus(&app), insights::Focus::Providers));
        assert!(!handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(focus(&app), insights::Focus::Providers));
    }
}

/// Dev preview, not a check: `cargo test -- --ignored dump_frames` renders
/// every pane and overlay over the real 10k sample into target/frames/*.txt
/// — the fastest way to eyeball a layout change without a terminal session.
#[cfg(test)]
mod frame_dump {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn dump(app: &mut App, dir: &std::path::Path, name: &str) {
        let backend = TestBackend::new(160, 45);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            text.push('\n');
        }
        std::fs::write(dir.join(format!("{name}.txt")), text).expect("write frame");
    }

    #[test]
    #[ignore]
    fn dump_frames() {
        let mut cfg = RunConfig::new("data/sample_claims_10k.jsonl".into(), 42, 0.0004);
        healthcare_billing_sim::scenario::preset("messy")
            .expect("preset")
            .apply(&mut cfg);
        let configs = cfg.payers.clone();
        let routes = cfg.payer_faults.clone();
        let output = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt")
            .block_on(healthcare_billing_sim::run(cfg))
            .expect("run");
        let mut app = App {
            banner_rows: vec![(
                "input".to_string(),
                "data/sample_claims_10k.jsonl (3.2 MB)".to_string(),
            )],
            seed: 42,
            wall_start: Instant::now(),
            wall_done: Some(Duration::from_secs(2)),
            progress: final_progress(&output),
            done: Some(Done::build(output, configs, routes)),
            pane: 0,
            frame: 0,
            help: false,
        };
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/frames");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for pane in 0..PANE_TITLES.len() {
            app.pane = pane;
            dump(&mut app, &dir, &format!("pane{pane}"));
        }
        // Per-provider variants of the two master–detail report panes.
        app.pane = AGING;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        dump(&mut app, &dir, "pane0-provider");
        // The aging books scrubbed 60 days back from the end of intake.
        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..60 {
            handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        dump(&mut app, &dir, "pane0-scrubbed");
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.pane = 2;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        dump(&mut app, &dir, "pane2-provider");
        app.help = true;
        dump(&mut app, &dir, "help");
        app.help = false;
        app.pane = PROVIDERS;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        dump(&mut app, &dir, "insights-analysis");
    }
}
