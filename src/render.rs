//! Output: range-profile bar chart, terminal blueprint, and SVG blueprint export.

use crate::dsp::{feet_inches, fmt_len, Echo, RangeProfile};
use crate::map::{Cell, Coverage, Grid, ScanMap};
use std::fmt::Write as _;

/// UTC "YYYY-MM-DD HH:MM" from a unix timestamp (no date crate needed).
pub fn utc(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60)
}

pub fn file_stamp(ts: u64) -> String {
    utc(ts).replace(" UTC", "").replace([' ', ':'], "").replace('-', "")
}

// ------------------------------------------------------------------ ping bars

/// `floor` keeps the scale honest: bars are drawn relative to max(strongest, floor),
/// so pure noise doesn't get stretched to full width.
pub fn bars(rp: &RangeProfile, echoes: &[Echo], max_range: f32, bin: f32, blank: f32, floor: f32, header: &str) -> String {
    let nb = (max_range / bin).ceil() as usize;
    let mut vals = vec![0.0f32; nb];
    for (i, &v) in rp.env.iter().enumerate() {
        let d = rp.dist(i);
        if d < blank || d >= max_range {
            continue;
        }
        let b = (d / bin) as usize;
        if b < nb {
            vals[b] = vals[b].max(v);
        }
    }
    let top = vals.iter().cloned().fold(0.0, f32::max).max(floor).max(1e-12);
    let width = 50.0;
    let mut s = String::new();
    let _ = writeln!(s, "{header}\n");
    for (k, v) in vals.iter().enumerate() {
        let lo = k as f32 * bin;
        let hit = echoes.iter().any(|e| e.dist_m >= lo && e.dist_m < lo + bin);
        let _ = writeln!(
            s,
            "{lo:5.2} m {:>7} |{}{}",
            feet_inches(lo),
            "█".repeat((v / top * width) as usize),
            if hit { "  ◀" } else { "" }
        );
    }
    let _ = writeln!(s);
    if echoes.is_empty() {
        let _ = writeln!(s, "No echoes above threshold.");
    } else {
        let list: Vec<String> = echoes
            .iter()
            .take(5)
            .map(|e| format!("{} {:.0} dB", fmt_len(e.dist_m), e.db()))
            .collect();
        let _ = writeln!(s, "Echoes (nearest first): {}", list.join(", "));
    }
    s
}

// ------------------------------------------------------------------ terminal blueprint

struct Crop {
    i0: usize,
    i1: usize,
    j0: usize,
    j1: usize,
}

fn crop(g: &Grid, margin: usize) -> Crop {
    let (mut i0, mut j0, mut i1, mut j1) = (usize::MAX, usize::MAX, 0, 0);
    for j in 0..g.n {
        for i in 0..g.n {
            if g.get(i, j) != Cell::Unknown {
                i0 = i0.min(i);
                j0 = j0.min(j);
                i1 = i1.max(i);
                j1 = j1.max(j);
            }
        }
    }
    if i0 == usize::MAX {
        return Crop { i0: 0, i1: g.n - 1, j0: 0, j1: g.n - 1 };
    }
    Crop {
        i0: i0.saturating_sub(margin),
        j0: j0.saturating_sub(margin),
        i1: (i1 + margin).min(g.n - 1),
        j1: (j1 + margin).min(g.n - 1),
    }
}

const RESET: &str = "\x1b[0m";
const BG_ROOM: &str = "\x1b[48;2;12;52;97m";
const BG_OUT: &str = "\x1b[48;2;6;24;46m";

pub fn terminal(map: &ScanMap, g: &Grid, cov: &Coverage, color: bool, cols: usize) -> String {
    let c = crop(g, 3);
    let w_cells = c.i1 - c.i0 + 1;
    let s = ((w_cells * 2) as f32 / cols.saturating_sub(4).max(20) as f32).ceil().max(1.0) as usize;
    let block_m = g.cell_m * s as f32;
    let positions = map.positions();
    let ((bx0, by0), (bx1, by1)) = cov.bbox;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "CHIRPMAP · {} · {}",
        map.device,
        utc(map.created_unix)
    );
    let _ = writeln!(out);

    let mut j = c.j1 as i64;
    while j >= c.j0 as i64 {
        let ju = j as usize;
        let mut line = String::new();
        let mut i = c.i0;
        while i <= c.i1 {
            // Aggregate an s×s block, walls win.
            let mut best = Cell::Unknown;
            let mut pos_here: Option<usize> = None;
            let mut grid_mark = false;
            for dj in 0..s {
                for di in 0..s {
                    let (ii, jj) = (i + di, ju.saturating_sub(dj));
                    if ii > c.i1 || jj < c.j0 {
                        continue;
                    }
                    let cell = g.get(ii, jj);
                    if cell.display_rank() > best.display_rank() {
                        best = cell;
                    }
                    let (x, y) = g.xy(ii, jj);
                    if let Some(k) = positions.iter().position(|p| {
                        (p.0 - x).abs() <= g.cell_m * 0.5 && (p.1 - y).abs() <= g.cell_m * 0.5
                    }) {
                        pos_here = Some(k);
                    }
                    if (x - x.round()).abs() < g.cell_m * 0.5 && (y - y.round()).abs() < g.cell_m * 0.5 {
                        grid_mark = true;
                    }
                }
            }
            let (x, y) = g.xy(i, ju);
            let inside = x >= bx0 - block_m && x <= bx1 + block_m && y >= by0 - block_m && y <= by1 + block_m;

            let (glyph, fg): (String, &str) = if let Some(k) = pos_here {
                let mark = if k == 0 { "◉ ".to_string() } else { format!("{} ", (k + 1) % 10) };
                (mark, "\x1b[1;38;2;255;214;102m")
            } else {
                match best {
                    Cell::WallMeasured => ("██".into(), "\x1b[38;2;255;255;255m"),
                    Cell::WallAssumed => ("▓▓".into(), "\x1b[38;2;130;190;245m"),
                    Cell::FreeMeasured => ("· ".into(), "\x1b[38;2;95;150;205m"),
                    Cell::FreeAssumed if grid_mark => ("+ ".into(), "\x1b[38;2;45;95;150m"),
                    Cell::FreeAssumed => ("  ".into(), ""),
                    Cell::Unknown if inside => ("░░".into(), "\x1b[38;2;35;75;120m"),
                    Cell::Unknown => ("  ".into(), ""),
                }
            };
            if color {
                let bg = if inside || best != Cell::Unknown { BG_ROOM } else { BG_OUT };
                line += bg;
                line += fg;
                line += &glyph;
                line += RESET;
            } else {
                let ascii = match (pos_here, best) {
                    (Some(_), _) => "@ ",
                    (_, Cell::WallMeasured) => "##",
                    (_, Cell::WallAssumed) => "++",
                    (_, Cell::FreeMeasured) => ". ",
                    (_, Cell::Unknown) if inside => "::",
                    _ => "  ",
                };
                line += ascii;
            }
            i += s;
        }
        let _ = writeln!(out, " {line}");
        j -= s as i64;
    }

    // Scale bar: 1 m.
    let chars_per_m = (2.0 / block_m).round() as usize;
    if chars_per_m >= 3 {
        let _ = writeln!(out, "\n ├{}┤ 1 m (3' 3\")", "─".repeat(chars_per_m - 2));
    }
    let legend = if color {
        "\x1b[38;2;255;255;255m██\x1b[0m wall (measured)   \x1b[38;2;130;190;245m▓▓\x1b[0m wall (assumed)   \x1b[38;2;95;150;205m·\x1b[0m open (measured)   \x1b[38;2;35;75;120m░░\x1b[0m unscanned   \x1b[1;38;2;255;214;102m◉\x1b[0m scan spot"
    } else {
        "## wall (measured)   ++ wall (assumed)   . open (measured)   :: unscanned   @ scan spot"
    };
    let _ = writeln!(out, " {legend}");
    let _ = writeln!(
        out,
        "\n Footprint ≈ {:.2} × {:.2} m ({} × {})   Coverage: measured {:.0}% · assumed {:.0}% · unscanned {:.0}%",
        cov.width_m(),
        cov.height_m(),
        feet_inches(cov.width_m()),
        feet_inches(cov.height_m()),
        cov.measured_pct,
        cov.assumed_pct,
        cov.unscanned_pct
    );
    let _ = writeln!(
        out,
        " {} spot(s), {} readings, beam ±{:.0}°, grid {:.0} cm",
        positions.len(),
        map.observations.len(),
        map.beam_deg,
        g.cell_m * 100.0
    );
    out
}

// ------------------------------------------------------------------ SVG blueprint

pub fn svg(map: &ScanMap, g: &Grid, cov: &Coverage) -> String {
    let c = crop(g, 6);
    let px = 10.0f32; // pixels per cell
    let cols = (c.i1 - c.i0 + 1) as f32;
    let rows = (c.j1 - c.j0 + 1) as f32;
    let pad = 60.0f32;
    let title_h = 90.0f32;
    let w = cols * px + 2.0 * pad;
    let h = rows * px + 2.0 * pad + title_h;
    let to_px = |x: f32, y: f32| -> (f32, f32) {
        let (ox, oy) = g.xy(c.i0, c.j1);
        (pad + (x - ox) / g.cell_m * px, pad + (oy - y) / g.cell_m * px)
    };

    let mut s = String::new();
    let _ = writeln!(s, r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="ui-monospace, Menlo, Consolas, monospace">"#);
    let _ = writeln!(s, r##"<defs><pattern id="hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><line x1="0" y1="0" x2="0" y2="6" stroke="#2f6aa3" stroke-width="1"/></pattern></defs>"##);
    let _ = writeln!(s, r##"<rect width="100%" height="100%" fill="#0b3560"/>"##);

    // Drafting grid: 10 cm fine, 1 m bold.
    for i in c.i0..=c.i1 + 1 {
        let (x, _) = g.xy(i, 0);
        let (sx, _) = to_px(x - g.cell_m * 0.5, 0.0);
        let major = ((x - g.cell_m * 0.5) / 1.0).fract().abs() < 1e-3 || ((x - g.cell_m * 0.5) / 1.0).fract().abs() > 0.999;
        let _ = writeln!(s, r##"<line x1="{sx:.1}" y1="{pad}" x2="{sx:.1}" y2="{:.1}" stroke="#ffffff" stroke-opacity="{}" stroke-width="1"/>"##, pad + rows * px, if major { 0.18 } else { 0.05 });
    }
    for j in c.j0..=c.j1 + 1 {
        let (_, y) = g.xy(0, j);
        let (_, sy) = to_px(0.0, y - g.cell_m * 0.5);
        let f = ((y - g.cell_m * 0.5) / 1.0).fract().abs();
        let major = f < 1e-3 || f > 0.999;
        let _ = writeln!(s, r##"<line x1="{pad}" y1="{sy:.1}" x2="{:.1}" y2="{sy:.1}" stroke="#ffffff" stroke-opacity="{}" stroke-width="1"/>"##, pad + cols * px, if major { 0.18 } else { 0.05 });
    }

    // Cells.
    let ((bx0, by0), (bx1, by1)) = cov.bbox;
    for j in c.j0..=c.j1 {
        for i in c.i0..=c.i1 {
            let (x, y) = g.xy(i, j);
            let (sx, sy) = to_px(x - g.cell_m * 0.5, y + g.cell_m * 0.5);
            let inside = x >= bx0 && x <= bx1 && y >= by0 && y <= by1;
            let fill = match g.get(i, j) {
                Cell::WallMeasured => Some(("#ffffff", 1.0)),
                Cell::WallAssumed => Some(("#8cc8ff", 0.75)),
                Cell::FreeMeasured => Some(("#2a6faf", 0.9)),
                Cell::FreeAssumed => Some(("#1a548c", 0.6)),
                Cell::Unknown if inside => Some(("url(#hatch)", 1.0)),
                Cell::Unknown => None,
            };
            if let Some((f, o)) = fill {
                let _ = writeln!(s, r#"<rect x="{sx:.1}" y="{sy:.1}" width="{px}" height="{px}" fill="{f}" fill-opacity="{o}"/>"#);
            }
        }
    }

    // Scan positions.
    for (k, p) in map.positions().iter().enumerate() {
        let (x, y) = to_px(p.0, p.1);
        let _ = writeln!(s, r##"<circle cx="{x:.1}" cy="{y:.1}" r="7" fill="none" stroke="#ffd666" stroke-width="2"/><line x1="{:.1}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="#ffd666"/><line x1="{x:.1}" y1="{:.1}" x2="{x:.1}" y2="{:.1}" stroke="#ffd666"/><text x="{:.1}" y="{:.1}" fill="#ffd666" font-size="11">P{}</text>"##, x - 11.0, x + 11.0, y - 11.0, y + 11.0, x + 10.0, y - 10.0, k + 1);
    }

    // Dimension lines on the footprint.
    let (ax, ay) = to_px(bx0, by1);
    let (bx, _) = to_px(bx1, by1);
    let (_, cy) = to_px(bx0, by0);
    let dim = "#cfe6ff";
    let _ = writeln!(s, r#"<g stroke="{dim}" stroke-width="1" fill="{dim}" font-size="12"><line x1="{ax:.1}" y1="{:.1}" x2="{bx:.1}" y2="{:.1}"/><line x1="{ax:.1}" y1="{:.1}" x2="{ax:.1}" y2="{:.1}"/><line x1="{bx:.1}" y1="{:.1}" x2="{bx:.1}" y2="{:.1}"/><text x="{:.1}" y="{:.1}" text-anchor="middle" stroke="none">{}</text><line x1="{:.1}" y1="{ay:.1}" x2="{:.1}" y2="{cy:.1}"/><line x1="{:.1}" y1="{ay:.1}" x2="{:.1}" y2="{ay:.1}"/><line x1="{:.1}" y1="{cy:.1}" x2="{:.1}" y2="{cy:.1}"/><text x="{:.1}" y="{:.1}" text-anchor="middle" stroke="none" transform="rotate(-90 {:.1} {:.1})">{}</text></g>"#,
        ay - 24.0, ay - 24.0, ay - 30.0, ay - 18.0, ay - 30.0, ay - 18.0,
        (ax + bx) / 2.0, ay - 30.0, xml_escape(&fmt_len(cov.width_m())),
        ax - 24.0, ax - 24.0, ax - 30.0, ax - 18.0, ax - 30.0, ax - 18.0,
        ax - 30.0, (ay + cy) / 2.0, ax - 30.0, (ay + cy) / 2.0, xml_escape(&fmt_len(cov.height_m())));

    // Title block.
    let tb_w = 340.0f32.min(w - 20.0);
    let tx = w - tb_w - 10.0;
    let ty = h - title_h + 5.0;
    let _ = writeln!(s, r##"<g fill="#cfe6ff" font-size="11"><rect x="{tx:.1}" y="{ty:.1}" width="{tb_w:.1}" height="{:.1}" fill="none" stroke="#cfe6ff"/><text x="{:.1}" y="{:.1}" font-size="16" font-weight="bold">CHIRPMAP</text><text x="{:.1}" y="{:.1}">ACOUSTIC SURVEY · {}</text><text x="{:.1}" y="{:.1}">MEASURED {:.0}% · ASSUMED {:.0}% · UNSCANNED {:.0}%</text><text x="{:.1}" y="{:.1}">GRID {:.0} CM · BEAM ±{:.0}° · {} READINGS</text></g>"##,
        title_h - 15.0,
        tx + 10.0, ty + 20.0,
        tx + 10.0, ty + 38.0, utc(map.created_unix),
        tx + 10.0, ty + 54.0, cov.measured_pct, cov.assumed_pct, cov.unscanned_pct,
        tx + 10.0, ty + 70.0, g.cell_m * 100.0, map.beam_deg, map.observations.len());
    let _ = writeln!(s, r##"<text x="10" y="{:.1}" fill="#8fb8e0" font-size="10">{}</text>"##, h - 10.0, xml_escape(&map.device));
    s.push_str("</svg>\n");
    s
}

fn xml_escape(t: &str) -> String {
    t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
