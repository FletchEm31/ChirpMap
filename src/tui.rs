//! Full-screen ChirpMap window (ratatui): header with the devices in use,
//! an arrow-key menu, and a live view that draws inside the box.
//! Text-driven flows (calibrate, scan) briefly leave the window and come back.

use crate::audio::{self, CpalSonar, SimSonar, Sonar};
use crate::dsp::{self, feet_inches, fmt_len, Echo, RangeProfile};
use crate::{calib, levels, Cmd, DevArgs};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

mod room;

// Blueprint palette.
// Brand: board B, "Blueprint + sonar mark".
const NAVY: Color = Color::Rgb(0x0D, 0x3B, 0x66); // blueprint paper
const PANEL: Color = Color::Rgb(0x12, 0x30, 0x4D); // popups, icon tile
const LINE: Color = Color::Rgb(0xCF, 0xE6, 0xFF); // drafting lines, labels
const ACCENT: Color = Color::Rgb(0x8C, 0xC8, 0xFF); // sonar arcs, data
const DIM: Color = Color::Rgb(0x9D, 0xB8, 0xD6); // secondary text (passes 4.5:1 on navy)
const FAINT: Color = Color::Rgb(0x4E, 0x7A, 0xA6); // grid ticks, outermost arc
const WARN: Color = Color::Rgb(0xFF, 0x9F, 0x6B);
const OK: Color = Color::Rgb(0xA8, 0xE6, 0xB8);
const BAR: Color = ACCENT;
const HIT: Color = Color::Rgb(0xFF, 0xD6, 0x66); // the yellow dot

const MAX_RANGE: f32 = 4.0;

struct Item {
    key: char,
    name: &'static str,
    blurb: &'static str,
    help: &'static str,
}

const ITEMS: &[Item] = &[
    Item { key: '1', name: "Live tracker", blurb: "only what moves",
        help: "Captures the empty room for a few seconds, then shows only what CHANGED. Walk toward or away from the mic and watch the marker follow you." },
    Item { key: '2', name: "Live echoes", blurb: "everything it hears",
        help: "Every reflection the mic picks up: desk, monitor, walls, you. Useful for seeing how cluttered a spot is." },
    Item { key: '3', name: "Calibrate", blurb: "tune speaker + mic",
        help: "Measures room noise, the usable frequency band, volume and speaker ring-down for this speaker + mic. Can also tune system volume and mic sensitivity (asks first, always restores)." },
    Item { key: '4', name: "Scan a room", blurb: "rotate-and-ping plan",
        help: "Turn the speaker+mic through 8 headings to build a blueprint floor plan with measured / assumed / unscanned coverage. Saves JSON + SVG to ~/chirpmap-scans." },
    Item { key: '5', name: "Hardware panel", blurb: "speakers & mics",
        help: "Every speaker and microphone the system exposes, what they can do, and which ones are calibrated." },
    Item { key: '6', name: "Simulator demo", blurb: "fake room, no hardware",
        help: "Scans a simulated 5.2 × 4.1 m room (17' × 13'). Shows what a finished map looks like without making a sound." },
    Item { key: '7', name: "Room designer", blurb: "draw room + devices",
        help: "Draw your room to scale and place every speaker and microphone: where it sits, which way it faces, and which physical device it is. ChirpMap uses this geometry to turn echoes into positions." },
    Item { key: '8', name: "Array tracker", blurb: "2D, all pairs",
        help: "Uses every speaker → mic pair in your room layout at once and shows where things are ON THE FLOOR PLAN as a heat map. More speakers and mics = more pairs = a sharper fix." },
    Item { key: 'u', name: "Update", blurb: "git pull + reinstall",
        help: "Pulls the latest ChirpMap from GitHub into the folder it was built from and reinstalls it. Restart chirpmap afterwards." },
    Item { key: 'q', name: "Quit", blurb: "", help: "Leave ChirpMap. Any audio levels it changed are already put back." },
];

enum Msg {
    Status(String),
    Reading { shown: RangeProfile, echoes: Vec<Echo>, floor: f32, blank: f32 },
    Error(String),
}

struct Live {
    diff: bool,
    stop: Arc<AtomicBool>,
    rx: Receiver<Msg>,
    handle: Option<JoinHandle<()>>,
    status: String,
    last: Option<(RangeProfile, Vec<Echo>, f32, f32)>,
    error: Option<String>,
    readings: u64,
    _levels: Option<levels::Session>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // `_levels` drops after this → system levels restored.
    }
}

enum Mode {
    Menu,
    AskLevels { diff: bool, spk: u32, mic: u32 },
    Live(Live),
    Designer(room::Designer),
    Array(room::ArrayView),
}

struct App {
    state: ListState,
    speaker: String,
    mic: String,
    mic_warn: Option<&'static str>,
    calib: (String, bool),
    mode: Mode,
    quit: bool,
    /// One-line message shown in the footer (e.g. "Saved room …").
    note: Option<String>,
}

impl App {
    fn new() -> Self {
        let mut a = App {
            state: ListState::default(),
            speaker: String::new(),
            mic: String::new(),
            mic_warn: None,
            calib: (String::new(), false),
            mode: Mode::Menu,
            quit: false,
            note: None,
        };
        a.state.select(Some(0));
        a.refresh();
        a
    }

    fn refresh(&mut self) {
        if sim_mode() {
            self.speaker = "simulated speaker".into();
            self.mic = "simulated mic (CHIRPMAP_SIM)".into();
            self.mic_warn = None;
            self.calib = ("simulator — no calibration needed".into(), true);
            return;
        }
        let (s, m, w) = audio::in_use(None, None);
        self.speaker = s;
        self.mic = m;
        self.mic_warn = w;
        self.calib = crate::calib_summary();
    }
}

fn sim_mode() -> bool {
    std::env::var_os("CHIRPMAP_SIM").is_some()
}

fn open_sonar() -> Result<Box<dyn Sonar>> {
    if sim_mode() {
        Ok(Box::new(SimSonar::new()))
    } else {
        Ok(Box::new(CpalSonar::open(None, None)?))
    }
}

/// Profile without printing anything (stderr would scribble over the window).
fn quiet_profile(sonar: &dyn Sonar) -> calib::Profile {
    match calib::load(&sonar.name()) {
        Some(p) if p.fs == sonar.fs() => p,
        _ => calib::Profile::defaults(sonar),
    }
}

fn spawn_live(diff: bool, stop: Arc<AtomicBool>, tx: Sender<Msg>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let res = (|| -> Result<()> {
            let mut sonar = open_sonar()?;
            let p = quiet_profile(sonar.as_ref());
            let tpl = p.template();
            let bg = if diff {
                let _ = tx.send(Msg::Status("Capturing the empty scene — keep still for a few seconds…".into()));
                let mut shots = Vec::new();
                for _ in 0..8 {
                    if stop.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    shots.push(crate::measure_profile(sonar.as_mut(), &p, &tpl, 4, MAX_RANGE)?.0);
                }
                let _ = tx.send(Msg::Status("Background captured — now move something in front of the mic.".into()));
                Some(dsp::Background::capture(&shots))
            } else {
                let _ = tx.send(Msg::Status("Listening…".into()));
                None
            };
            while !stop.load(Ordering::SeqCst) {
                let (rp, _) = crate::measure_profile(sonar.as_mut(), &p, &tpl, 4, MAX_RANGE)?;
                let (shown, th, floor) = match &bg {
                    Some(b) => {
                        let th = b.threshold(p.fs, p.blank_m, MAX_RANGE);
                        (b.subtract(&rp), th, th * 2.0)
                    }
                    None => {
                        let th = dsp::echo_threshold(p.noise_env, rp.direct_peak);
                        (rp, th, 0.0)
                    }
                };
                let echoes = dsp::find_echoes(&shown, p.blank_m, MAX_RANGE, th);
                if tx.send(Msg::Reading { shown, echoes, floor, blank: p.blank_m }).is_err() {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            let _ = tx.send(Msg::Error(format!("{e:#}")));
        }
    })
}

fn start_live(app: &mut App, diff: bool, sess: Option<levels::Session>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let handle = spawn_live(diff, stop.clone(), tx);
    app.mode = Mode::Live(Live {
        diff,
        stop,
        rx,
        handle: Some(handle),
        status: "Starting…".into(),
        last: None,
        error: None,
        readings: 0,
        _levels: sess,
    });
}

/// Begin live mode, first asking about calibrated levels if there are any.
fn request_live(app: &mut App, diff: bool) {
    if !sim_mode() && levels::available() {
        if let Ok(s) = CpalSonar::open(None, None) {
            if let Some(p) = calib::load(&s.name()) {
                if let (Some(spk), Some(mic)) = (p.speaker_pct, p.mic_pct) {
                    app.mode = Mode::AskLevels { diff, spk, mic };
                    return;
                }
            }
        }
    }
    start_live(app, diff, None);
}

/// Leave the window, run a text-mode command, come back.
fn suspend_run(terminal: &mut DefaultTerminal, cmd: Cmd) {
    ratatui::restore();
    print!("\x1b[H\x1b[J");
    if let Err(e) = crate::run(cmd) {
        println!("\nerror: {e:#}");
    }
    let _ = crate::read_line("\nPress Enter to return to ChirpMap…");
    *terminal = ratatui::init();
}

fn activate(app: &mut App, terminal: &mut DefaultTerminal, key: char) {
    let dev = DevArgs::default();
    let scan = |dev: DevArgs, auto: bool| Cmd::Scan {
        dev,
        steps: 8,
        beam: 30.0,
        avg: 4,
        max_range: 5.0,
        cell: 0.1,
        blank: None,
        positions: None,
        auto,
        out: crate::scans_dir(),
        no_color: false,
    };
    match key {
        '1' => request_live(app, true),
        '2' => request_live(app, false),
        '3' => {
            suspend_run(terminal, Cmd::Calibrate { dev, force: false });
            app.refresh();
        }
        '4' => suspend_run(terminal, scan(dev, false)),
        '5' => suspend_run(terminal, Cmd::Devices),
        '6' => suspend_run(terminal, scan(DevArgs { sim: true, ..DevArgs::default() }, true)),
        'u' => {
            ratatui::restore();
            crate::update();
            let _ = crate::read_line("\nPress Enter, then restart chirpmap to use the new version…");
            app.quit = true;
            *terminal = ratatui::init();
        }
        '7' => app.mode = Mode::Designer(room::Designer::new()),
        '8' => match room::ArrayView::start(true) {
            Ok(v) => app.mode = Mode::Array(v),
            Err(e) => app.note = Some(e),
        },
        'q' => app.quit = true,
        _ => {}
    }
}

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new();
    let res = event_loop(&mut terminal, &mut app);
    app.mode = Mode::Menu; // stops live thread, restores levels
    ratatui::restore();
    res
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        if let Mode::Live(live) = &mut app.mode {
            while let Ok(m) = live.rx.try_recv() {
                match m {
                    Msg::Status(s) => live.status = s,
                    Msg::Reading { shown, echoes, floor, blank } => {
                        live.readings += 1;
                        live.last = Some((shown, echoes, floor, blank));
                    }
                    Msg::Error(e) => live.error = Some(e),
                }
            }
        }
        if let Mode::Array(a) = &mut app.mode {
            a.drain();
        }
        terminal.draw(|f| draw(f, app))?;
        if !event::poll(Duration::from_millis(60))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        if !matches!(app.mode, Mode::Menu) {
            app.note = None;
        }
        if let Mode::Designer(d) = &mut app.mode {
            if ctrl_c {
                app.mode = Mode::Menu;
            } else if let room::Outcome::Done(msg) = d.key(k) {
                app.mode = Mode::Menu;
                app.note = msg;
            }
            continue;
        }
        if let Mode::Array(a) = &app.mode {
            if ctrl_c || matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') | KeyCode::Backspace) {
                app.mode = Mode::Menu;
            } else if k.code == KeyCode::Char('r') || k.code == KeyCode::Char('t') {
                // r: re-capture the empty room · t: toggle tracker / every reflector
                let diff = if k.code == KeyCode::Char('t') { !a.diff } else { a.diff };
                app.mode = Mode::Menu; // stops the old thread first
                match room::ArrayView::start(diff) {
                    Ok(v) => app.mode = Mode::Array(v),
                    Err(e) => app.note = Some(e),
                }
            }
            continue;
        }
        match &app.mode {
            Mode::Live(_) => {
                if ctrl_c || matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') | KeyCode::Backspace) {
                    app.mode = Mode::Menu;
                } else if k.code == KeyCode::Char('r') {
                    let diff = matches!(&app.mode, Mode::Live(l) if l.diff);
                    // Re-capture background: restart the live thread, keep levels.
                    let sess = match std::mem::replace(&mut app.mode, Mode::Menu) {
                        Mode::Live(mut l) => l._levels.take(),
                        _ => None,
                    };
                    start_live(app, diff, sess);
                }
            }
            Mode::AskLevels { diff, spk, mic } => {
                let (diff, spk, mic) = (*diff, *spk, *mic);
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        let sess = levels::Session::begin();
                        if let Some(s) = &sess {
                            s.set_speaker(spk);
                            s.set_mic(mic);
                        }
                        start_live(app, diff, sess);
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => start_live(app, diff, None),
                    KeyCode::Esc => app.mode = Mode::Menu,
                    _ => {}
                }
            }
            Mode::Designer(_) | Mode::Array(_) => {}
            Mode::Menu => {
                app.note = None;
                if ctrl_c {
                    app.quit = true;
                    continue;
                }
                let n = ITEMS.len();
                let i = app.state.selected().unwrap_or(0);
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => app.state.select(Some((i + n - 1) % n)),
                    KeyCode::Down | KeyCode::Char('j') => app.state.select(Some((i + 1) % n)),
                    KeyCode::Enter => activate(app, terminal, ITEMS[i].key),
                    KeyCode::Esc => app.quit = true,
                    KeyCode::Char(c) => {
                        if let Some(pos) = ITEMS.iter().position(|it| it.key == c.to_ascii_lowercase()) {
                            app.state.select(Some(pos));
                            activate(app, terminal, ITEMS[pos].key);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ drawing

fn draw(f: &mut Frame, app: &mut App) {
    let outer = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(LINE))
        .style(Style::new().bg(NAVY).fg(Color::White))
        .title(Line::from(vec![
            Span::styled(" ((", Style::new().fg(FAINT).add_modifier(Modifier::BOLD)),
            Span::styled("(", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled("●", Style::new().fg(HIT).add_modifier(Modifier::BOLD)),
            Span::styled(")", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(")) ", Style::new().fg(FAINT).add_modifier(Modifier::BOLD)),
            Span::styled("CHIRPMAP ", Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!("v{} ", env!("CARGO_PKG_VERSION")), Style::new().fg(DIM)),
        ]))
        .title(Line::from(Span::styled(" acoustic room mapping · measured by sound ", Style::new().fg(DIM))).alignment(Alignment::Right));
    let area = f.area();
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let header_h = if app.mic_warn.is_some() { 6 } else { 5 };
    let rows = Layout::vertical([Constraint::Length(header_h), Constraint::Min(5), Constraint::Length(1)]).split(inner);
    draw_header(f, rows[0], app);

    let body = Layout::horizontal([Constraint::Length(43), Constraint::Min(20)]).split(rows[1]);
    let view = match &app.mode {
        Mode::Designer(_) => 0,
        Mode::Array(_) => 1,
        Mode::Live(_) => 2,
        _ => 3,
    };
    if view != 0 {
        draw_menu(f, body[0], app);
    }
    match &app.mode {
        Mode::Designer(d) => room::draw_designer(f, body[0], body[1], d),
        Mode::Array(a) => room::draw_array(f, body[1], a),
        Mode::Live(live) => draw_live(f, body[1], live),
        _ => draw_info(f, body[1], app),
    }
    if let Mode::AskLevels { spk, mic, .. } = &app.mode {
        draw_levels_popup(f, area, *spk, *mic);
    }

    let keys = match app.mode {
        Mode::Live(_) => " Esc/q back to menu · r re-capture background · Ctrl+C back ",
        Mode::AskLevels { .. } => " y use levels · n keep yours · Esc cancel ",
        Mode::Designer(_) => " Tab device · arrows move · [ ] turn · n device · a/m add · x delete · Enter save · Esc cancel ",
        Mode::Array(_) => " Esc/q back · r re-capture empty room · t tracker ⇄ every reflector ",
        Mode::Menu => " ↑↓ select · Enter run · 1-8 jump · q quit ",
    };
    let footer = match &app.note {
        Some(n) => Line::from(Span::styled(format!(" {n}"), Style::new().fg(HIT).add_modifier(Modifier::BOLD))),
        None => Line::from(Span::styled(keys, Style::new().fg(DIM))),
    };
    f.render_widget(Paragraph::new(footer), rows[2]);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let label = |t: &'static str| Span::styled(format!(" {t:<12}│ "), Style::new().fg(DIM));
    let mut lines = vec![
        Line::from(vec![label("SPEAKER"), Span::raw(app.speaker.clone()).bold()]),
        Line::from(vec![label("MICROPHONE"), Span::raw(app.mic.clone()).bold()]),
        Line::from(vec![
            label("CALIBRATION"),
            Span::styled(app.calib.0.clone(), Style::new().fg(if app.calib.1 { OK } else { WARN })),
        ]),
    ];
    if let Some(w) = app.mic_warn {
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {w} — run: pactl set-default-source <your mic>"),
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        )));
    }
    let block = Block::bordered().border_type(BorderType::Plain).border_style(Style::new().fg(LINE)).title(Span::styled(" SHEET 1 · IN USE ", Style::new().fg(LINE)));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_menu(f: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = ITEMS
        .iter()
        .map(|it| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", it.key), Style::new().fg(DIM)),
                Span::raw(format!("{:<15}", it.name)),
                Span::styled(it.blurb, Style::new().fg(DIM)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().border_type(BorderType::Plain).border_style(Style::new().fg(LINE)).title(Span::styled(" MENU ", Style::new().fg(LINE))))
        .highlight_style(Style::new().fg(NAVY).bg(ACCENT).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶");
    f.render_stateful_widget(list, area, &mut app.state);
}

/// Block-letter wordmark, 5 rows.
const WORDMARK: [&str; 5] = [
    " ███  █  █  ███  ███   ███   █   █   ██   ███ ",
    "█     █  █   █   █  █  █  █  ██ ██  █  █  █  █",
    "█     ████   █   ███   ███   █ █ █  ████  ███ ",
    "█     █  █   █   █ █   █     █   █  █  █  █   ",
    " ███  █  █  ███  █  █  █     █   █  █  █  █   ",
];

fn sonar_mark() -> Vec<Line<'static>> {
    // Three arcs each side of the yellow dot, fading outward, like the logo.
    let arc = |c: &'static str, col: Color| Span::styled(c, Style::new().fg(col).add_modifier(Modifier::BOLD));
    vec![
        Line::from(vec![arc("╭  ", FAINT), arc("╭  ", DIM), arc("╭     ", ACCENT), arc("╮  ", ACCENT), arc("╮  ", DIM), arc("╮", FAINT)]).alignment(Alignment::Center),
        Line::from(vec![arc("│  ", FAINT), arc("│  ", DIM), arc("│  ", ACCENT), arc("●", HIT), arc("  │  ", ACCENT), arc("│  ", DIM), arc("│", FAINT)]).alignment(Alignment::Center),
        Line::from(vec![arc("╰  ", FAINT), arc("╰  ", DIM), arc("╰     ", ACCENT), arc("╯  ", ACCENT), arc("╯  ", DIM), arc("╯", FAINT)]).alignment(Alignment::Center),
    ]
}

fn draw_info(f: &mut Frame, area: Rect, app: &App) {
    let it = &ITEMS[app.state.selected().unwrap_or(0)];
    let mut lines: Vec<Line> = vec![Line::raw("")];
    lines.extend(sonar_mark());
    lines.push(Line::raw(""));
    let pad = " ".repeat((area.width.saturating_sub(2) as usize).saturating_sub(46) / 2);
    for w in WORDMARK {
        lines.push(Line::from(Span::styled(format!("{pad}{w}"), Style::new().fg(Color::White).add_modifier(Modifier::BOLD))));
    }
    lines.push(Line::from(Span::styled("├──── 17' 3\" × 13' 7\" · measured by sound ────┤", Style::new().fg(LINE))).alignment(Alignment::Center));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(format!("▸ {}", it.name.to_uppercase()), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))).alignment(Alignment::Center));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(it.help, Style::new().fg(DIM))).alignment(Alignment::Center));
    let block = Block::bordered().border_type(BorderType::Plain).border_style(Style::new().fg(LINE));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

fn draw_live(f: &mut Frame, area: Rect, live: &Live) {
    let title = if live.diff { " LIVE TRACKER · change vs background " } else { " LIVE ECHOES " };
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(LINE))
        .title(Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title(Line::from(Span::styled(format!(" {} readings ", live.readings), Style::new().fg(DIM))).alignment(Alignment::Right));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(e) = &live.error {
        let p = Paragraph::new(vec![
            Line::raw(""),
            Line::from(Span::styled("Audio error", Style::new().fg(WARN).add_modifier(Modifier::BOLD))),
            Line::raw(e.clone()),
            Line::raw(""),
            Line::raw("Esc to go back. Check the mic in the header, or try Calibrate."),
        ])
        .wrap(Wrap { trim: true });
        f.render_widget(p, inner);
        return;
    }

    let parts = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(inner);
    // Big readout.
    let (headline, colour) = match &live.last {
        Some((_, echoes, _, _)) if !echoes.is_empty() => {
            let e = &echoes[0];
            (
                format!("▶ {}   {:.0} dB{}", fmt_len(e.dist_m), e.db(), if echoes.len() > 1 { format!("   (+{} more)", echoes.len() - 1) } else { String::new() }),
                HIT,
            )
        }
        Some(_) => (if live.diff { "· nothing moving ·".to_string() } else { "· no echoes above threshold ·".to_string() }, DIM),
        None => (live.status.clone(), DIM),
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(headline, Style::new().fg(colour).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
            Line::from(Span::styled(live.status.clone(), Style::new().fg(DIM))).alignment(Alignment::Center),
        ]),
        parts[0],
    );

    let Some((rp, echoes, floor, blank)) = &live.last else { return };
    let rows = parts[1].height.max(1) as usize;
    // Pick a bin so the whole range fits on screen (multiples of 5 cm).
    let bin = ((MAX_RANGE / rows as f32) / 0.05).ceil().max(1.0) * 0.05;
    let nb = (MAX_RANGE / bin).ceil() as usize;
    let mut vals = vec![0.0f32; nb];
    for (i, &v) in rp.env.iter().enumerate() {
        let d = rp.dist(i);
        if d < *blank || d >= MAX_RANGE {
            continue;
        }
        let b = (d / bin) as usize;
        if b < nb {
            vals[b] = vals[b].max(v);
        }
    }
    let top = vals.iter().cloned().fold(0.0, f32::max).max(*floor).max(1e-12);
    let label_w = 17usize;
    let bar_w = (parts[1].width as usize).saturating_sub(label_w + 4).max(5);
    let lines: Vec<Line> = vals
        .iter()
        .enumerate()
        .take(rows)
        .map(|(k, &v)| {
            let lo = k as f32 * bin;
            let hit = echoes.iter().any(|e| e.dist_m >= lo && e.dist_m < lo + bin);
            let len = ((v / top) * bar_w as f32) as usize;
            let label = format!("{lo:5.2} m {:>7} ", feet_inches(lo));
            let lbl_style = if hit { Style::new().fg(HIT).add_modifier(Modifier::BOLD) } else { Style::new().fg(DIM) };
            let bar_style = if hit { Style::new().fg(HIT) } else if lo < *blank { Style::new().fg(DIM) } else { Style::new().fg(BAR) };
            let mut spans = vec![Span::styled(label, lbl_style), Span::styled("┤", Style::new().fg(LINE)), Span::styled("█".repeat(len.min(bar_w)), bar_style)];
            if hit {
                spans.push(Span::styled(" ◀", Style::new().fg(HIT).add_modifier(Modifier::BOLD)));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), parts[1]);
}

fn draw_levels_popup(f: &mut Frame, area: Rect, spk: u32, mic: u32) {
    let w = 62.min(area.width.saturating_sub(4));
    let h = 7;
    let r = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + (area.height.saturating_sub(h)) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let p = Paragraph::new(vec![
        Line::raw(""),
        Line::raw(format!("Use your calibrated levels: speaker {spk}%, mic {mic}%?")).alignment(Alignment::Center),
        Line::from(Span::styled("Your own levels are put back when you leave the live view.", Style::new().fg(DIM))).alignment(Alignment::Center),
        Line::raw(""),
        Line::from(Span::styled("[y] yes    [n] keep mine    [Esc] cancel", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
    ])
    .style(Style::new().bg(PANEL))
    .block(Block::bordered().border_type(BorderType::Plain).border_style(Style::new().fg(LINE)).title(" AUDIO LEVELS "));
    f.render_widget(p, r);
}
