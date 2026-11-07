//! Room designer (draw the room, place speakers and mics) and the 2D array view.

use super::*;
use crate::array::{self, ArrayBackground, ArrayIo, CpalArray, Image, SimArray};
use crate::layout::{self as plan, Kind, Node};
use ratatui::crossterm::event::KeyEvent;

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
fn draw_plan(f: &mut Frame, area: Rect, l: &plan::Layout, img: Option<&Image>, mark: Option<(f32, f32)>, sel: Option<usize>, title: String) {
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
    if let Some((x, y)) = mark {
        let (r, c) = to_cell(x, y);
        grid[r][c] = ("◉".into(), Style::new().fg(HIT).bg(NAVY).add_modifier(Modifier::BOLD));
    }
    let lines: Vec<Line> = grid
        .into_iter()
        .map(|row| Line::from(row.into_iter().map(|(t, st)| Span::styled(t, st)).collect::<Vec<_>>()))
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(Color::White))
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
    outs: Vec<String>,
    ins: Vec<String>,
}

pub(super) enum Outcome {
    Stay,
    Done(Option<String>),
}

/// "default" plus one entry per sound card (the sysdefault:CARD=… names), so
/// cycling devices is a handful of presses, not fifty ALSA aliases.
fn device_choices() -> (Vec<String>, Vec<String>) {
    audio::quiet(|| {
        let host = cpal::default_host();
        use cpal::traits::{DeviceTrait, HostTrait};
        let pick = |names: Vec<String>| {
            let mut v = vec!["default".to_string()];
            // One entry per card (+ HDMI outputs, where AV receivers usually live).
            v.extend(names.into_iter().filter(|n| n.starts_with("sysdefault:CARD=") || n.starts_with("hdmi:CARD=")));
            v
        };
        let outs = host.output_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        let ins = host.input_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        (pick(outs), pick(ins))
    })
}

impl Designer {
    pub fn new() -> Self {
        let (outs, ins) = if sim_mode() { (vec!["default".into()], vec!["default".into()]) } else { device_choices() };
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
                        let i = list.iter().position(|d| *d == node.device).map_or(0, |i| (i + 1) % list.len());
                        node.device = list[i].clone();
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
        lines.push(Line::from(Span::styled(format!("    device: {}", n.device), Style::new().fg(DIM))));
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
    draw_plan(f, right, &d.l, None, None, Some(d.sel), room_title(&d.l));
}

// ------------------------------------------------------------------ 2D array tracker

enum AMsg {
    Status(String),
    Desc(String),
    Frame { img: Image, peak: (f32, f32, f32), detected: bool },
    Error(String),
}

pub(super) struct ArrayView {
    pub diff: bool,
    stop: Arc<AtomicBool>,
    rx: Receiver<AMsg>,
    handle: Option<JoinHandle<()>>,
    l: plan::Layout,
    status: String,
    desc: String,
    img: Option<Image>,
    peak: Option<(f32, f32, f32)>,
    detected: bool,
    frames: u64,
    error: Option<String>,
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

impl ArrayView {
    pub fn start(diff: bool) -> std::result::Result<Self, String> {
        let l = plan::load_active().ok_or("Draw your room first: 7 Room designer, then Enter to save.")?;
        if !l.is_usable() {
            return Err("The saved room needs at least one speaker and one mic (7 Room designer).".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (l2, stop2) = (l.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            if let Err(e) = run_array(l2, diff, stop2, &tx) {
                let _ = tx.send(AMsg::Error(format!("{e:#}")));
            }
        });
        Ok(ArrayView {
            diff,
            stop,
            rx,
            handle: Some(handle),
            l,
            status: "Starting…".into(),
            desc: String::new(),
            img: None,
            peak: None,
            detected: false,
            frames: 0,
            error: None,
        })
    }

    pub fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                AMsg::Status(s) => self.status = s,
                AMsg::Desc(s) => self.desc = s,
                AMsg::Frame { img, peak, detected } => {
                    self.frames += 1;
                    self.img = Some(img);
                    self.peak = Some(peak);
                    self.detected = detected;
                }
                AMsg::Error(e) => self.error = Some(e),
            }
        }
    }
}

fn run_array(l: plan::Layout, diff: bool, stop: Arc<AtomicBool>, tx: &Sender<AMsg>) -> Result<()> {
    let mut be = if sim_mode() { Backend::Sim(SimArray::new(l.clone(), false)) } else { Backend::Real(CpalArray::open(&l)?) };
    let fs = be.io().fs();
    let (tpl, vol) = if sim_mode() { (dsp::chirp(fs, 0.01, 3000.0, 16_000.0), 0.7) } else { array::template(fs) };
    let _ = tx.send(AMsg::Desc(be.io().describe()));
    let bg = if diff {
        let _ = tx.send(AMsg::Status("Capturing the empty room — keep still and out of the way…".into()));
        let mut frames = Vec::new();
        for _ in 0..5 {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            frames.push(array::measure(be.io(), &l, &tpl, vol, 1)?);
        }
        if let Backend::Sim(s) = &mut be {
            s.walker = true;
        }
        let _ = tx.send(AMsg::Status("Background captured — walk around the room.".into()));
        Some(ArrayBackground::capture(&frames, fs))
    } else {
        let _ = tx.send(AMsg::Status("Showing every reflector the array hears.".into()));
        None
    };
    let max_m = l.width_m.hypot(l.depth_m) * 2.0;
    while !stop.load(Ordering::SeqCst) {
        let pairs = array::measure(be.io(), &l, &tpl, vol, 1)?;
        let (pairs, th) = match &bg {
            Some(b) => b.subtract(&pairs, fs, max_m),
            None => (pairs, 0.0),
        };
        // Movers: a wider guard so leftover speaker ringing next to the devices isn't "someone".
        let guard = if diff { GUARD_DIFF_M } else { GUARD_M };
        let img = array::backproject(&pairs, l.width_m, l.depth_m, CELL_M, fs, guard);
        let peak = img.peak();
        let detected = !diff || peak.2 > th;
        if tx.send(AMsg::Frame { img, peak, detected }).is_err() {
            break;
        }
    }
    Ok(())
}

pub(super) fn draw_array(f: &mut Frame, area: Rect, a: &ArrayView) {
    let title = if a.diff { " ARRAY TRACKER · 2D · what moves " } else { " ARRAY · 2D · every reflector " };
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(LINE))
        .title(Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title(Line::from(Span::styled(format!(" {} frames ", a.frames), Style::new().fg(DIM))).alignment(Alignment::Right));
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
    let parts = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(inner);
    let pairs = a.l.speakers().len() * a.l.mics().len();
    let centre = {
        let n = a.l.nodes.len().max(1) as f32;
        (a.l.nodes.iter().map(|n| n.x).sum::<f32>() / n, a.l.nodes.iter().map(|n| n.y).sum::<f32>() / n)
    };
    let (head, col) = match (a.peak, a.detected) {
        (Some((x, y, _)), true) if pairs < 2 => (
            format!("◯ ≈ {} from the desk — 1 pair gives distance only; add a 2nd mic (7) for a 2D fix", fmt_len(plan::dist((x, y), centre))),
            HIT,
        ),
        (Some((x, y, _)), true) => (
            format!("◉ {} across · {} up the plan", fmt_len(x), fmt_len(y)),
            HIT,
        ),
        (Some(_), false) => ("· nothing moving ·".to_string(), DIM),
        (None, _) => (a.status.clone(), DIM),
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(head, Style::new().fg(col).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
            Line::from(Span::styled(a.status.clone(), Style::new().fg(DIM))).alignment(Alignment::Center),
            Line::from(Span::styled(a.desc.clone(), Style::new().fg(FAINT))).alignment(Alignment::Center),
        ]),
        parts[0],
    );
    let mark = match (a.peak, a.detected) {
        (Some((x, y, _)), true) if pairs >= 2 => Some((x, y)),
        _ => None,
    };
    draw_plan(f, parts[1], &a.l, a.img.as_ref(), mark, None, room_title(&a.l));
}
