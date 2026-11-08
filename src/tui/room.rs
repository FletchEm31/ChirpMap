//! Room designer (draw the room, place speakers and mics) and the 2D array view.

use super::*;
use crate::array::{self, ArrayBackground, ArrayIo, CpalArray, Image, SimArray};
use crate::tracker::{Track, Tracker};
use crate::layout::{self as plan, Kind, Node};
use ratatui::crossterm::event::KeyEvent;
use std::collections::VecDeque;
use std::time::Instant;

// ------------------------------------------------------------------ shared floor-plan drawing

fn ramp(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0).powf(0.75);
    let lerp = |a: (f32, f32, f32), b: (f32, f32, f32), u: f32| {
        Color::Rgb((a.0 + (b.0 - a.0) * u) as u8, (a.1 + (b.1 - a.1) * u) as u8, (a.2 + (b.2 - a.2) * u) as u8)
    };
    let navy = (13.0, 59.0, 102.0);
    let blue = (140.0, 200.0, 255.0);
    let yellow = (255.0, 214.0, 102.0);
    if t < 0.6 {
        lerp(navy, blue, t / 0.6)
    } else {
        lerp(blue, yellow, (t - 0.6) / 0.4)
    }
}

fn arrow(deg: f32) -> char {
    ['↑', '↗', '→', '↘', '↓', '↙', '←', '↖'][((deg.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}

/// Draw the room to scale like an architect's plan: wall dimensions on the
/// walls, optional heat image, devices, guide lines for the selected device,
/// and a target marker.
/// A movement detection on the plan: position and age in seconds.
#[derive(Clone, Copy)]
pub(super) struct Ping {
    pub x: f32,
    pub y: f32,
    pub age: f32,
    /// Which person (0 = untracked).
    pub id: u32,
}

const RING_S: f32 = 1.2; // how long a ping's ring keeps expanding
const TRAIL_S: f32 = 4.0; // how long the fading trail stays on the plan
const TRAIL_N: usize = 8; // at most this many trail dots
const PING_GAP_S: f32 = 0.33; // minimum time between pings

fn fade(to: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (n0, n1, n2) = (13.0, 59.0, 102.0); // navy
    Color::Rgb((n0 + (to.0 as f32 - n0) * t) as u8, (n1 + (to.1 as f32 - n1) * t) as u8, (n2 + (to.2 as f32 - n2) * t) as u8)
}

fn draw_plan(f: &mut Frame, area: Rect, l: &plan::Layout, img: Option<&Image>, pings: &[Ping], sel: Option<usize>, title: String, alert: bool) {
    const LEFT: u16 = 11; // room for the depth dimension down the left wall
    const TOP: u16 = 1; // room for the width dimension along the top wall
    let aw = area.width.saturating_sub(2 + LEFT).max(4) as f32;
    let ah = area.height.saturating_sub(2 + TOP).max(2) as f32;
    // One text row = s metres; one column = s/2 metres (terminal cells are ~2:1).
    let s = (l.width_m / (aw / 2.0)).max(l.depth_m / ah);
    let cols = ((l.width_m / s) * 2.0).round().max(2.0) as usize;
    let rows = (l.depth_m / s).round().max(1.0) as usize;
    let used_w = cols as u16 + 2 + LEFT;
    let used_h = rows as u16 + 2 + TOP;
    let ox = area.x + area.width.saturating_sub(used_w) / 2;
    let oy = area.y + area.height.saturating_sub(used_h) / 2;
    let rect = Rect {
        x: ox + LEFT,
        y: oy + TOP,
        width: (cols as u16 + 2).min(area.width.saturating_sub(LEFT)),
        height: (rows as u16 + 2).min(area.height.saturating_sub(TOP)),
    };
    let to_cell = |x: f32, y: f32| -> (usize, usize) {
        let c = ((x / s) * 2.0).floor().clamp(0.0, cols as f32 - 1.0) as usize;
        let r = ((l.depth_m - y) / s).floor().clamp(0.0, rows as f32 - 1.0) as usize;
        (r, c)
    };

    // Width dimension along the top wall: ├──── 11' 6" · 3.50 m ────┤
    let wtxt = format!(" {} · {:.2} m ", feet_inches(l.width_m), l.width_m);
    let inner_w = cols.saturating_sub(2);
    let dash = inner_w.saturating_sub(wtxt.chars().count());
    let wline = format!("├{}{}{}┤", "─".repeat(dash / 2), wtxt, "─".repeat(dash - dash / 2));
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(wline, Style::new().fg(LINE)))),
        Rect { x: rect.x + 1, y: oy, width: cols as u16, height: 1 },
    );
    // Depth dimension down the left wall.
    let dtxt = [feet_inches(l.depth_m), format!("{:.2} m", l.depth_m)];
    let mid = rows / 2;
    let dlines: Vec<Line> = (0..rows)
        .map(|r| {
            let t = if r == 0 {
                "┬".to_string()
            } else if r + 1 == rows {
                "┴".to_string()
            } else if r == mid.saturating_sub(1) {
                dtxt[0].clone()
            } else if r == mid {
                dtxt[1].clone()
            } else {
                "│".to_string()
            };
            Line::from(Span::styled(t, Style::new().fg(LINE))).alignment(Alignment::Center)
        })
        .collect();
    f.render_widget(Paragraph::new(dlines), Rect { x: ox, y: rect.y + 1, width: LEFT - 1, height: rows as u16 });

    let top = img.map(|i| i.max()).unwrap_or(0.0).max(1e-9);
    let mut grid: Vec<Vec<(String, Style)>> = (0..rows)
        .map(|r| {
            (0..cols)
                .map(|c| {
                    let x = (c as f32 + 0.5) * s / 2.0;
                    let y = l.depth_m - (r as f32 + 0.5) * s;
                    let bg = match img {
                        Some(i) => ramp(i.at(x, y) / top),
                        None => NAVY,
                    };
                    let on_metre = (x - x.round()).abs() < s / 4.0 && (y - y.round()).abs() < s / 2.0;
                    let ch = if img.is_none() && on_metre { "·" } else { " " };
                    (ch.to_string(), Style::new().bg(bg).fg(FAINT))
                })
                .collect()
        })
        .collect();
    // Guide lines from the selected device to the left wall and the top wall.
    if let Some(n) = sel.and_then(|k| l.nodes.get(k)) {
        let (r, c) = to_cell(n.x, n.y);
        let g = Style::new().fg(ACCENT).bg(NAVY);
        for cc in 0..c {
            grid[r][cc] = ("┄".into(), g);
        }
        for rr in 0..r {
            grid[rr][c] = ("┆".into(), g);
        }
    }
    for (k, n) in l.nodes.iter().enumerate() {
        let (r, c) = to_cell(n.x, n.y);
        let mut st = match n.kind {
            Kind::Speaker => Style::new().fg(HIT).bg(NAVY).add_modifier(Modifier::BOLD),
            Kind::Mic => Style::new().fg(Color::White).bg(NAVY).add_modifier(Modifier::BOLD),
        };
        if sel == Some(k) {
            st = Style::new().fg(NAVY).bg(ACCENT).add_modifier(Modifier::BOLD);
        }
        let text: Vec<char> = format!("{}{}", n.label, arrow(n.facing_deg)).chars().collect();
        let start = c.min(cols.saturating_sub(text.len()));
        for (j, ch) in text.iter().enumerate() {
            if let Some(cell) = grid[r].get_mut(start + j) {
                *cell = (ch.to_string(), st);
            }
        }
    }
    // Radar: each detection sends out a red ring that grows and fades; older
    // detections stay as a fading red trail; the newest is a solid red ◉.
    let mut ringed: Vec<&Ping> = pings.iter().filter(|p| p.age < RING_S).collect();
    ringed.sort_by(|a, b| a.age.total_cmp(&b.age));
    let mut ringed_ids: Vec<u32> = Vec::new();
    ringed.retain(|p| {
        let fresh = !ringed_ids.contains(&p.id);
        ringed_ids.push(p.id);
        fresh
    });
    for p in ringed.into_iter().take(4) {
        let radius = 0.12 + p.age / RING_S * 0.75;
        let t = 1.0 - p.age / RING_S;
        for (r, row) in grid.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                let x = (c as f32 + 0.5) * s / 2.0;
                let y = l.depth_m - (r as f32 + 0.5) * s;
                if (plan::dist((x, y), (p.x, p.y)) - radius).abs() < s * 0.45 {
                    cell.0 = "∘".into();
                    cell.1 = cell.1.fg(fade(person_rgb(p.id), t)).add_modifier(Modifier::BOLD);
                }
            }
        }
    }
    let mut trail: Vec<&Ping> = pings.iter().filter(|p| p.age < TRAIL_S).collect();
    trail.sort_by(|a, b| b.age.total_cmp(&a.age)); // oldest first, newest drawn on top
    for p in &trail {
        let (r, c) = to_cell(p.x, p.y);
        let (cr, cg, cb) = person_rgb(p.id);
        let soft = (cr.saturating_add(0).max(120), cg.saturating_add(43), cb.saturating_add(43));
        grid[r][c] = ("•".into(), Style::new().fg(fade(soft, 1.0 - p.age / TRAIL_S)).bg(NAVY));
    }
    // Each person's newest position: a solid ◉ in their colour, with their ID next to it.
    let mut seen: Vec<u32> = Vec::new();
    for p in trail.iter().rev() {
        if seen.contains(&p.id) || p.age >= 0.8 {
            continue;
        }
        seen.push(p.id);
        let (r, c) = to_cell(p.x, p.y);
        let st = Style::new().fg(person_color(p.id)).bg(NAVY).add_modifier(Modifier::BOLD);
        grid[r][c] = ("◉".into(), st);
        if p.id > 0 {
            for (j, ch) in p.id.to_string().chars().enumerate() {
                if let Some(cell) = grid[r].get_mut(c + 1 + j) {
                    *cell = (ch.to_string(), st);
                }
            }
        }
    }
    let lines: Vec<Line> = grid
        .into_iter()
        .map(|row| Line::from(row.into_iter().map(|(t, st)| Span::styled(t, st)).collect::<Vec<_>>()))
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(if alert { RED } else { Color::White }))
        .title(Span::styled(title, Style::new().fg(LINE)))
        .title(Line::from(Span::styled(" TOP WALL ", Style::new().fg(DIM))).alignment(Alignment::Right))
        .title_bottom(Line::from(Span::styled(" BOTTOM WALL ", Style::new().fg(DIM))).alignment(Alignment::Right));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn room_title(l: &plan::Layout) -> String {
    format!(" {} ", l.name)
}

// ------------------------------------------------------------------ room designer

pub(super) struct Designer {
    pub l: plan::Layout,
    pub sel: usize,
    outs: Vec<Choice>,
    ins: Vec<Choice>,
}

pub(super) enum Outcome {
    Stay,
    Done(Option<String>),
}

/// "default" plus one entry per sound card (the sysdefault:CARD=… names), so
/// cycling devices is a handful of presses, not fifty ALSA aliases.
/// What `n` cycles through: "default", then every PipeWire speaker/mic by its
/// real name (routed through PipeWire, so nothing is "busy"); plain ALSA
/// cards only when PipeWire isn't available.
type Choice = (String, String); // (stored device string, label shown)

fn device_choices() -> (Vec<Choice>, Vec<Choice>) {
    let def = || vec![("default".to_string(), "system default".to_string())];
    let sinks = audio::pw_endpoints("sinks");
    let sources = audio::pw_endpoints("sources");
    if !sinks.is_empty() || !sources.is_empty() {
        let wrap = |v: Vec<(String, String)>| {
            let mut out = def();
            out.extend(v.into_iter().map(|(n, d)| (format!("pw:{n}"), d)));
            out
        };
        return (wrap(sinks), wrap(sources));
    }
    audio::quiet(|| {
        let host = cpal::default_host();
        use cpal::traits::{DeviceTrait, HostTrait};
        let pick = |names: Vec<String>| {
            let mut v = def();
            v.extend(
                names
                    .into_iter()
                    .filter(|n| n.starts_with("sysdefault:CARD=") || n.starts_with("hdmi:CARD="))
                    .map(|n| (n.clone(), n)),
            );
            v
        };
        let outs = host.output_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        let ins = host.input_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        (pick(outs), pick(ins))
    })
}

/// "Starship/Matisse HD Audio Controller Digital Stereo (IEC958)" → "Starship/Matisse (optical)".
fn short(desc: &str) -> String {
    let mut d = desc.to_string();
    for junk in [" HD Audio Controller", " Audio Controller", " Digital Stereo", " Analog Stereo", " Analog Mono"] {
        d = d.replace(junk, "");
    }
    d = d.replace("(IEC958)", "(optical)");
    if d.chars().count() > 30 {
        d = d.chars().take(29).collect::<String>() + "…";
    }
    d
}

fn label_for(choices: &[Choice], device: &str) -> String {
    choices
        .iter()
        .find(|(id, _)| id == device)
        .map(|(_, l)| short(l))
        .unwrap_or_else(|| short(device.trim_start_matches("pw:")))
}

impl Designer {
    pub fn new() -> Self {
        let (outs, ins) = if sim_mode() {
            (vec![("default".into(), "simulated".into())], vec![("default".into(), "simulated".into())])
        } else {
            device_choices()
        };
        Designer { l: plan::load_active().unwrap_or_else(plan::Layout::starter), sel: 0, outs, ins }
    }

    fn add(&mut self, kind: Kind) {
        let label = self.l.next_label(kind);
        self.l.nodes.push(Node {
            kind,
            label,
            device: "default".into(),
            x: self.l.width_m / 2.0,
            y: self.l.depth_m / 2.0,
            facing_deg: 0.0,
            channel: None,
        });
        self.sel = self.l.nodes.len() - 1;
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome {
        let big = k.modifiers.contains(KeyModifiers::SHIFT);
        let step = if big { 0.30 } else { 0.05 };
        let n = self.l.nodes.len();
        match k.code {
            KeyCode::Esc => return Outcome::Done(Some("Room designer closed without saving.".into())),
            KeyCode::Enter => {
                self.l.clamp();
                if !self.l.is_usable() {
                    return Outcome::Done(Some("Not saved: a room needs at least one speaker (a) and one mic (m).".into()));
                }
                return Outcome::Done(Some(match plan::save(&self.l) {
                    Ok(_) => format!("Saved room \"{}\" — {} speaker(s), {} mic(s). Try 8 Array tracker.", self.l.name, self.l.speakers().len(), self.l.mics().len()),
                    Err(e) => format!("Couldn't save the room: {e:#}"),
                }));
            }
            KeyCode::Tab if n > 0 => self.sel = (self.sel + 1) % n,
            KeyCode::BackTab if n > 0 => self.sel = (self.sel + n - 1) % n,
            KeyCode::Char('a') => self.add(Kind::Speaker),
            KeyCode::Char('m') => self.add(Kind::Mic),
            KeyCode::Char('x') | KeyCode::Delete if n > 0 => {
                self.l.nodes.remove(self.sel);
                self.l.relabel();
                self.sel = self.sel.min(self.l.nodes.len().saturating_sub(1));
            }
            KeyCode::Char('c') if n > 0 && self.l.nodes[self.sel].kind == Kind::Speaker => {
                let node = &mut self.l.nodes[self.sel];
                node.channel = match node.channel {
                    None => Some(0),
                    Some(c) if c < 5 => Some(c + 1),
                    _ => None,
                };
            }
            KeyCode::Char('w') => self.l.width_m -= 0.10,
            KeyCode::Char('W') => self.l.width_m += 0.10,
            KeyCode::Char('d') => self.l.depth_m -= 0.10,
            KeyCode::Char('D') => self.l.depth_m += 0.10,
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Char('[') | KeyCode::Char(']') | KeyCode::Char('n')
                if n > 0 =>
            {
                let (outs, ins) = (self.outs.clone(), self.ins.clone());
                let node = &mut self.l.nodes[self.sel];
                match k.code {
                    KeyCode::Up => node.y += step,
                    KeyCode::Down => node.y -= step,
                    KeyCode::Left => node.x -= step,
                    KeyCode::Right => node.x += step,
                    KeyCode::Char('[') => node.facing_deg -= 45.0,
                    KeyCode::Char(']') => node.facing_deg += 45.0,
                    _ => {
                        let list = if node.kind == Kind::Speaker { &outs } else { &ins };
                        let i = list.iter().position(|(id, _)| *id == node.device).map_or(0, |i| (i + 1) % list.len());
                        node.device = list[i].0.clone();
                    }
                }
            }
            _ => {}
        }
        self.l.clamp();
        Outcome::Stay
    }
}

pub(super) fn draw_designer(f: &mut Frame, left: Rect, right: Rect, d: &Designer) {
    let mut lines = vec![
        Line::from(Span::styled(" ROOM", Style::new().fg(DIM))),
        Line::from(format!(" {} × {}  ({:.2} × {:.2} m)", feet_inches(d.l.width_m), feet_inches(d.l.depth_m), d.l.width_m, d.l.depth_m)),
        Line::raw(""),
        Line::from(Span::styled(" DEVICES", Style::new().fg(DIM))),
    ];
    lines.insert(3, Line::from(Span::styled(" Positions are measured from the", Style::new().fg(DIM))));
    lines.insert(4, Line::from(Span::styled(" LEFT wall and the TOP wall of the plan.", Style::new().fg(DIM))));
    lines.insert(5, Line::raw(""));
    let speakers: Vec<&Node> = d.l.speakers();
    for (k, n) in d.l.nodes.iter().enumerate() {
        let head = if k == d.sel { Style::new().fg(NAVY).bg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().add_modifier(Modifier::BOLD) };
        let kind = if n.kind == Kind::Speaker { "speaker" } else { "mic" };
        lines.push(Line::from(Span::styled(format!(" {} {} {:<29}", n.label, arrow(n.facing_deg), kind), head)));
        lines.push(Line::from(vec![
            Span::styled("    ← ", Style::new().fg(ACCENT)),
            Span::raw(feet_inches(n.x)),
            Span::styled(" from left · ", Style::new().fg(DIM)),
            Span::styled("↑ ", Style::new().fg(ACCENT)),
            Span::raw(feet_inches(d.l.depth_m - n.y)),
            Span::styled(" from top", Style::new().fg(DIM)),
        ]));
        if n.kind == Kind::Mic {
            for sp in &speakers {
                lines.push(Line::from(vec![
                    Span::styled("    ↔ ", Style::new().fg(ACCENT)),
                    Span::raw(format!("{:<7}", feet_inches(plan::dist(n.pos(), sp.pos())))),
                    Span::styled(format!(" from {}", sp.label), Style::new().fg(DIM)),
                ]));
            }
        }
        let choices = if n.kind == Kind::Speaker { &d.outs } else { &d.ins };
        lines.push(Line::from(Span::styled(format!("    device: {}", label_for(choices, &n.device)), Style::new().fg(DIM))));
        if n.kind == Kind::Speaker {
            lines.push(Line::from(Span::styled(format!("    plays on: {}", plan::channel_name(n.channel)), Style::new().fg(DIM))));
        }
    }
    lines.push(Line::raw(""));
    let sp = d.l.speakers().len();
    let mc = d.l.mics().len();
    lines.push(Line::from(Span::styled(format!(" {} speaker × {} mic = {} pairs", sp, mc, sp * mc), Style::new().fg(OK))));
    lines.push(Line::raw(""));
    for (key, what) in [
        ("Tab", "next device"),
        ("arrows", "move 5 cm (Shift: 30 cm)"),
        ("[ ]", "turn 45°"),
        ("n", "which physical device"),
        ("c", "speaker channel (L/R/all)"),
        ("a / m", "add speaker / mic"),
        ("x", "delete"),
        ("w W d D", "room width/depth ±10 cm"),
        ("Enter", "save · Esc cancel"),
    ] {
        lines.push(Line::from(vec![Span::styled(format!(" {key:<8}"), Style::new().fg(ACCENT)), Span::styled(what, Style::new().fg(DIM))]));
    }
    let block = Block::bordered().border_type(BorderType::Plain).border_style(Style::new().fg(LINE)).title(Span::styled(" ROOM DESIGNER ", Style::new().fg(LINE)));
    f.render_widget(Paragraph::new(lines).block(block), left);
    draw_plan(f, right, &d.l, None, &[], Some(d.sel), room_title(&d.l), false);
}

// ------------------------------------------------------------------ 2D array tracker

enum AMsg {
    Status(String),
    Desc(String),
    /// Speed of sound in use, and whether it was measured in this room.
    Speed { c: f32, fitted: bool },
    Mode { simultaneous: bool, silent: bool },
    Frame { img: Image, peak: (f32, f32, f32), detected: bool, tracks: Vec<Track> },
    Recording(Option<(String, usize)>),
    Error(String),
}

pub(super) struct ArrayView {
    pub diff: bool,
    stop: Arc<AtomicBool>,
    /// Shared with the worker: true while frames are being saved.
    pub rec: Arc<AtomicBool>,
    rx: Receiver<AMsg>,
    handle: Option<JoinHandle<()>>,
    l: plan::Layout,
    status: String,
    desc: String,
    img: Option<Image>,
    peak: Option<(f32, f32, f32)>,
    detected: bool,
    tracks: Vec<Track>,
    frames: u64,
    started: Instant,
    speed: Option<(f32, bool)>,
    mode: Option<(bool, bool)>,
    rec_info: Option<(String, usize)>,
    error: Option<String>,
    /// Recent detections for the radar view: (person ID, position, when).
    pings: VecDeque<(u32, (f32, f32), Instant)>,
}

impl Drop for ArrayView {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

enum Backend {
    Sim(SimArray),
    Real(CpalArray),
}

impl Backend {
    fn io(&mut self) -> &mut dyn ArrayIo {
        match self {
            Backend::Sim(s) => s,
            Backend::Real(r) => r,
        }
    }
}

const GUARD_M: f32 = 0.3;
const GUARD_DIFF_M: f32 = 0.6;
const CELL_M: f32 = 0.05;
const MAX_PEOPLE: usize = 4;

/// Colour per person: 1 red, 2 orange, 3 magenta, 4 green.
pub(super) fn person_rgb(id: u32) -> (u8, u8, u8) {
    match id {
        0 | 1 => (255, 77, 77),
        2 => (255, 168, 64),
        3 => (230, 102, 255),
        _ => (120, 230, 150),
    }
}

fn person_color(id: u32) -> Color {
    let (r, g, b) = person_rgb(id);
    Color::Rgb(r, g, b)
}

impl ArrayView {
    pub fn start(diff: bool) -> std::result::Result<Self, String> {
        let l = plan::load_active().ok_or("Draw your room first: 7 Room designer, then Enter to save.")?;
        if !l.is_usable() {
            return Err("The saved room needs at least one speaker and one mic (7 Room designer).".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let rec = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (l2, stop2, rec2) = (l.clone(), stop.clone(), rec.clone());
        let handle = std::thread::spawn(move || {
            if let Err(e) = run_array(l2, diff, stop2, rec2, &tx) {
                let _ = tx.send(AMsg::Error(format!("{e:#}")));
            }
        });
        Ok(ArrayView {
            diff,
            stop,
            rec,
            rx,
            handle: Some(handle),
            l,
            status: "Starting…".into(),
            desc: String::new(),
            img: None,
            peak: None,
            detected: false,
            tracks: Vec::new(),
            frames: 0,
            started: Instant::now(),
            speed: None,
            mode: None,
            rec_info: None,
            error: None,
            pings: VecDeque::new(),
        })
    }

    pub fn toggle_recording(&mut self) {
        let on = !self.rec.load(Ordering::SeqCst);
        self.rec.store(on, Ordering::SeqCst);
    }

    pub fn recording(&self) -> bool {
        self.rec.load(Ordering::SeqCst)
    }

    pub fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                AMsg::Status(s) => self.status = s,
                AMsg::Desc(s) => self.desc = s,
                AMsg::Speed { c, fitted } => self.speed = Some((c, fitted)),
                AMsg::Mode { simultaneous, silent } => self.mode = Some((simultaneous, silent)),
                AMsg::Recording(r) => self.rec_info = r,
                AMsg::Frame { img, peak, detected, tracks } => {
                    self.frames += 1;
                    self.img = Some(img);
                    self.peak = Some(peak);
                    self.detected = detected;
                    // One ping per person per ~⅓ s keeps the radar readable at any frame rate.
                    if self.diff {
                        for tr in &tracks {
                            let due = self
                                .pings
                                .iter()
                                .rev()
                                .find(|p| p.0 == tr.id)
                                .map_or(true, |p| p.2.elapsed().as_secs_f32() >= PING_GAP_S);
                            if due {
                                self.pings.push_back((tr.id, (tr.x, tr.y), Instant::now()));
                            }
                        }
                    }
                    self.tracks = tracks;
                }
                AMsg::Error(e) => self.error = Some(e),
            }
        }
        while self.pings.front().is_some_and(|p| p.2.elapsed().as_secs_f32() > TRAIL_S) {
            self.pings.pop_front();
        }
        while self.pings.len() > TRAIL_N * MAX_PEOPLE {
            self.pings.pop_front();
        }
    }
}

fn run_array(l: plan::Layout, diff: bool, stop: Arc<AtomicBool>, rec: Arc<AtomicBool>, tx: &Sender<AMsg>) -> Result<()> {
    let sim = sim_mode();
    let mut be = if sim {
        let mut s = SimArray::new(l.clone(), 0);
        s.c = 346.0; // a warm room (~25 °C), so the self-check has something to find
        Backend::Sim(s)
    } else {
        Backend::Real(CpalArray::open(&l)?)
    };
    let fs = be.io().fs();
    let cfg = crate::settings::load();
    let (f0, f1, vol) = if sim {
        let (a, b) = crate::settings::band(fs, 3000.0, 16_000.0);
        (a, b, 0.7)
    } else {
        array::band_and_volume(fs)
    };
    let codes = array::Codes::for_layout(&l, fs, f0, f1, cfg.simultaneous);
    let _ = tx.send(AMsg::Desc(be.io().describe()));
    let _ = tx.send(AMsg::Mode { simultaneous: codes.is_simultaneous(), silent: cfg.silent });

    // A few frames of the room as it is: the background (tracker mode) and the
    // speed-of-sound self-check (walls are where the layout says they are).
    let _ = tx.send(AMsg::Status(if diff {
        "Capturing the empty room — keep still and out of the way…".into()
    } else {
        "Listening to the room…".into()
    }));
    let mut frames = Vec::new();
    for _ in 0..5 {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        frames.push(array::measure(be.io(), &l, &codes, vol, 1)?);
    }
    let (c, fitted) = match array::fit_speed_of_sound(&array::average_frames(&frames), &l, fs) {
        Some((c, _)) => (c, true),
        None => (dsp::SPEED_OF_SOUND, false),
    };
    let _ = tx.send(AMsg::Speed { c, fitted });
    let bg = if diff {
        if let Backend::Sim(s) = &mut be {
            s.walkers = 2;
        }
        let _ = tx.send(AMsg::Status("Background captured — walk around the room.".into()));
        Some(ArrayBackground::capture(&frames, fs))
    } else {
        let _ = tx.send(AMsg::Status("Showing every reflector the array hears.".into()));
        None
    };

    let max_m = l.width_m.hypot(l.depth_m) * 2.0;
    let mut tracker = Tracker::new();
    let t0 = Instant::now();
    let mut recorder: Option<crate::session::Recorder> = None;
    while !stop.load(Ordering::SeqCst) {
        let pairs = array::measure(be.io(), &l, &codes, vol, 1)?;
        let t = t0.elapsed().as_secs_f64();
        let (img, peak, detected, tracks) = match &bg {
            Some(b) => {
                let (pairs, th) = b.subtract(&pairs, fs, max_m);
                // Pairs must agree: a spot only lights up if every pair hears something there.
                // Pairs must agree, and each person found is explained away before looking for the next.
                let (img, cands) = array::detect_people(&pairs, &th, l.width_m, l.depth_m, CELL_M, fs, GUARD_DIFF_M, c, codes.dilate_m(), MAX_PEOPLE);
                let tracks = tracker.update(t, &cands);
                let peak = img.peak();
                (img, peak, !tracks.is_empty(), tracks)
            }
            None => {
                let img = array::backproject(&pairs, l.width_m, l.depth_m, CELL_M, fs, GUARD_M, c, array::Fusion::Sum);
                let peak = img.peak();
                (img, peak, true, Vec::new())
            }
        };

        // Recording on/off follows the 'v' key.
        match (rec.load(Ordering::SeqCst), recorder.is_some()) {
            (true, false) => {
                let h = crate::session::Header {
                    version: 1,
                    started_unix: crate::session::now_unix(),
                    layout: l.clone(),
                    speed_of_sound: c,
                    silent: cfg.silent,
                    simultaneous: codes.is_simultaneous(),
                    diff,
                };
                match crate::session::Recorder::start(&h) {
                    Ok(r) => recorder = Some(r),
                    Err(e) => {
                        rec.store(false, Ordering::SeqCst);
                        let _ = tx.send(AMsg::Status(format!("Couldn't start recording: {e:#}")));
                    }
                }
            }
            (false, true) => {
                if let Some(r) = recorder.take() {
                    let _ = tx.send(AMsg::Status(format!("Saved {} frames to {}", r.frames, r.path.display())));
                }
                let _ = tx.send(AMsg::Recording(None));
            }
            _ => {}
        }
        if let Some(r) = &mut recorder {
            let _ = r.push(&crate::session::FrameRec::new(t as f32, &img, &tracks));
            let _ = tx.send(AMsg::Recording(Some((r.path.display().to_string(), r.frames))));
        }

        if tx.send(AMsg::Frame { img, peak, detected, tracks }).is_err() {
            break;
        }
    }
    Ok(())
}

fn speed_line(c: f32, fitted: bool) -> String {
    if fitted {
        format!("speed of sound {c:.1} m/s (≈ {:.0} °C, checked against your walls)", array::temperature_c(c))
    } else {
        format!("speed of sound {c:.0} m/s (default — walls not clear enough to check)")
    }
}

pub(super) fn draw_array(f: &mut Frame, area: Rect, a: &ArrayView) {
    let title = if a.diff { " ARRAY TRACKER · 2D · radar " } else { " ARRAY · 2D · every reflector " };
    let pings: Vec<Ping> = a.pings.iter().map(|(id, (x, y), t)| Ping { x: *x, y: *y, age: t.elapsed().as_secs_f32(), id: *id }).collect();
    let alert = pings.iter().any(|p| p.age < 0.35);
    let fps = a.frames as f32 / a.started.elapsed().as_secs_f32().max(0.5);
    let mut right = vec![Span::styled(format!(" {} frames · {:.1}/s ", a.frames, fps), Style::new().fg(DIM))];
    if a.recording() {
        let n = a.rec_info.as_ref().map_or(0, |r| r.1);
        right.insert(0, Span::styled(format!(" ● REC {n} "), Style::new().fg(RED).add_modifier(Modifier::BOLD)));
    }
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(if alert { RED } else { LINE }))
        .title(Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title(Line::from(right).alignment(Alignment::Right));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if let Some(e) = &a.error {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("Array error", Style::new().fg(WARN).add_modifier(Modifier::BOLD))),
                Line::raw(e.clone()),
                Line::raw(""),
                Line::raw("Check each device in 7 Room designer (n cycles devices). Esc to go back."),
            ])
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    let parts = Layout::vertical([Constraint::Length(4), Constraint::Min(4)]).split(inner);
    let pairs = a.l.speakers().len() * a.l.mics().len();
    let centre = {
        let n = a.l.nodes.len().max(1) as f32;
        (a.l.nodes.iter().map(|n| n.x).sum::<f32>() / n, a.l.nodes.iter().map(|n| n.y).sum::<f32>() / n)
    };
    let head: Line = if a.diff && pairs >= 2 {
        if a.tracks.is_empty() {
            Line::from(Span::styled(if a.peak.is_some() { "· nothing moving ·".to_string() } else { a.status.clone() }, Style::new().fg(DIM)))
        } else {
            let mut spans = vec![Span::styled(
                format!("● {} {} · ", a.tracks.len(), if a.tracks.len() == 1 { "PERSON" } else { "PEOPLE" }),
                Style::new().fg(RED).add_modifier(Modifier::BOLD),
            )];
            for (k, tr) in a.tracks.iter().enumerate() {
                if k > 0 {
                    spans.push(Span::raw("   "));
                }
                spans.push(Span::styled(
                    format!("#{} {} across, {} up", tr.id, fmt_len(tr.x), fmt_len(tr.y)),
                    Style::new().fg(person_color(tr.id)).add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        }
    } else {
        let (text, col) = match (a.peak, a.detected) {
            (Some((x, y, _)), true) if pairs < 2 => (
                format!("◯ ≈ {} from the desk — 1 pair gives distance only; add a 2nd mic (7) for a 2D fix", fmt_len(plan::dist((x, y), centre))),
                HIT,
            ),
            (Some((x, y, _)), true) => (format!("◉ strongest reflector · {} across · {} up the plan", fmt_len(x), fmt_len(y)), HIT),
            (Some(_), false) => ("· nothing moving ·".to_string(), DIM),
            (None, _) => (a.status.clone(), DIM),
        };
        Line::from(Span::styled(text, Style::new().fg(col).add_modifier(Modifier::BOLD)))
    };
    let mut info = Vec::new();
    if let Some((c, fitted)) = a.speed {
        info.push(speed_line(c, fitted));
    }
    if let Some((sim, silent)) = a.mode {
        info.push(format!("{} · {}", if sim { "all speakers at once" } else { "speakers take turns" }, if silent { "silent 18–21 kHz" } else { "audible band" }));
    }
    f.render_widget(
        Paragraph::new(vec![
            head.alignment(Alignment::Center),
            Line::from(Span::styled(a.status.clone(), Style::new().fg(DIM))).alignment(Alignment::Center),
            Line::from(Span::styled(info.join(" · "), Style::new().fg(DIM))).alignment(Alignment::Center),
            Line::from(Span::styled(a.desc.clone(), Style::new().fg(FAINT))).alignment(Alignment::Center),
        ]),
        parts[0],
    );
    // "Every reflector" mode has no pings; show its strongest reflector as one.
    let shown: Vec<Ping> = match (a.diff, a.peak, a.detected) {
        (false, Some((x, y, _)), true) if pairs >= 2 => vec![Ping { x, y, age: 0.0, id: 0 }],
        (false, _, _) => Vec::new(),
        _ => pings,
    };
    draw_plan(f, parts[1], &a.l, a.img.as_ref(), &shown, None, room_title(&a.l), alert);
}

// ------------------------------------------------------------------ replay

pub(super) struct Replay {
    files: Vec<std::path::PathBuf>,
    which: usize,
    s: Option<crate::session::Session>,
    pos: usize,
    playing: bool,
    speed: f32,
    /// Session time shown at the last tick, and when that was.
    clock: (f32, Instant),
    error: Option<String>,
}

impl Replay {
    pub fn new() -> Self {
        let files = crate::session::list();
        let mut r = Replay { files, which: 0, s: None, pos: 0, playing: true, speed: 1.0, clock: (0.0, Instant::now()), error: None };
        r.open();
        r
    }

    fn open(&mut self) {
        self.pos = 0;
        self.s = None;
        self.error = None;
        match self.files.get(self.which) {
            None => self.error = Some(format!("No recorded sessions yet. In 8 Array tracker press v to record; files go to {}.", crate::session::dir().display())),
            Some(p) => match crate::session::load(p) {
                Ok(s) if s.frames.is_empty() => self.error = Some(format!("{} has no frames.", p.display())),
                Ok(s) => self.s = Some(s),
                Err(e) => self.error = Some(format!("{e:#}")),
            },
        }
        self.clock = (self.now_t(), Instant::now());
    }

    fn now_t(&self) -> f32 {
        self.s.as_ref().and_then(|s| s.frames.get(self.pos)).map_or(0.0, |f| f.t)
    }

    fn seek(&mut self, pos: usize) {
        if let Some(s) = &self.s {
            self.pos = pos.min(s.frames.len().saturating_sub(1));
        }
        self.clock = (self.now_t(), Instant::now());
    }

    /// Advance in real time while playing.
    pub fn tick(&mut self) {
        let Some(s) = &self.s else { return };
        if !self.playing {
            return;
        }
        let target = self.clock.0 + self.clock.1.elapsed().as_secs_f32() * self.speed;
        while self.pos + 1 < s.frames.len() && s.frames[self.pos + 1].t <= target {
            self.pos += 1;
        }
        if self.pos + 1 >= s.frames.len() {
            self.playing = false;
        }
    }

    /// Returns true when the viewer should close.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        let n = self.s.as_ref().map_or(0, |s| s.frames.len());
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => return true,
            KeyCode::Char(' ') => {
                if self.pos + 1 >= n {
                    self.seek(0);
                }
                self.playing = !self.playing;
                self.clock = (self.now_t(), Instant::now());
            }
            KeyCode::Right => {
                self.playing = false;
                self.seek(self.pos + 1);
            }
            KeyCode::Left => {
                self.playing = false;
                self.seek(self.pos.saturating_sub(1));
            }
            KeyCode::PageUp => self.seek(self.pos.saturating_sub(30)),
            KeyCode::PageDown => self.seek(self.pos + 30),
            KeyCode::Home => self.seek(0),
            KeyCode::End => self.seek(n),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.speed = (self.speed * 2.0).min(8.0);
                self.clock = (self.now_t(), Instant::now());
            }
            KeyCode::Char('-') => {
                self.speed = (self.speed / 2.0).max(0.25);
                self.clock = (self.now_t(), Instant::now());
            }
            KeyCode::Char('[') | KeyCode::Up if self.which + 1 < self.files.len() => {
                self.which += 1; // older
                self.open();
            }
            KeyCode::Char(']') | KeyCode::Down if self.which > 0 => {
                self.which -= 1; // newer
                self.open();
            }
            _ => {}
        }
        false
    }
}

pub(super) fn draw_replay(f: &mut Frame, area: Rect, r: &Replay) {
    let name = r.files.get(r.which).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(LINE))
        .title(Span::styled(" REPLAY ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title(Line::from(Span::styled(format!(" {name} · {}/{} ", r.which + 1, r.files.len().max(1)), Style::new().fg(DIM))).alignment(Alignment::Right));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(s) = &r.s else {
        f.render_widget(Paragraph::new(r.error.clone().unwrap_or_default()).style(Style::new().fg(DIM)).wrap(Wrap { trim: true }), inner);
        return;
    };
    let parts = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(inner);
    let fr = &s.frames[r.pos];
    let total = s.frames.last().map_or(0.0, |f| f.t);
    let width = parts[0].width.saturating_sub(24).max(10) as usize;
    let at = if total > 0.0 { ((fr.t / total) * (width - 1) as f32).round() as usize } else { 0 };
    let bar: String = (0..width).map(|i| if i == at { '◆' } else if i < at { '━' } else { '─' }).collect();
    let mut people: Vec<Span> = vec![Span::styled(if r.playing { "▶ " } else { "❚❚ " }, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))];
    if fr.tracks.is_empty() {
        people.push(Span::styled("nobody moving", Style::new().fg(DIM)));
    }
    for (k, tr) in fr.tracks.iter().enumerate() {
        if k > 0 {
            people.push(Span::raw("   "));
        }
        people.push(Span::styled(format!("#{} {} across, {} up", tr.id, fmt_len(tr.x), fmt_len(tr.y)), Style::new().fg(person_color(tr.id)).add_modifier(Modifier::BOLD)));
    }
    let clock = |t: f32| format!("{}:{:04.1}", (t / 60.0) as u32, t % 60.0);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(people).alignment(Alignment::Center),
            Line::from(vec![
                Span::styled(format!("{} ", clock(fr.t)), Style::new().fg(LINE)),
                Span::styled(bar, Style::new().fg(ACCENT)),
                Span::styled(format!(" {} · {}×", clock(total), r.speed), Style::new().fg(DIM)),
            ])
            .alignment(Alignment::Center),
            Line::from(Span::styled(
                format!("frame {}/{} · {}", r.pos + 1, s.frames.len(), speed_line(s.header.speed_of_sound, true)),
                Style::new().fg(FAINT),
            ))
            .alignment(Alignment::Center),
        ]),
        parts[0],
    );
    // Trails: where each person was over the last few seconds of the recording.
    let mut pings = Vec::new();
    for back in s.frames[..=r.pos].iter().rev().take_while(|b| fr.t - b.t < TRAIL_S) {
        for tr in &back.tracks {
            pings.push(Ping { x: tr.x, y: tr.y, age: fr.t - back.t, id: tr.id });
        }
    }
    let img = fr.image();
    draw_plan(f, parts[1], &s.header.layout, Some(&img), &pings, None, room_title(&s.header.layout), false);
}
