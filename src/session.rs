//! Record an array session to disk and play it back later.
//!
//! One JSON-lines file per session in ~/chirpmap-sessions/: the first line is
//! a header (room layout, speed of sound, settings), every following line is
//! one frame (time, heat image quantised to bytes, confirmed tracks).

use crate::array::Image;
use crate::layout::Layout;
use crate::tracker::Track;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Header {
    pub version: u32,
    pub started_unix: u64,
    pub layout: Layout,
    pub speed_of_sound: f32,
    pub silent: bool,
    pub simultaneous: bool,
    pub diff: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrackRec {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FrameRec {
    /// Seconds since the session started.
    pub t: f32,
    pub nx: usize,
    pub ny: usize,
    pub cell: f32,
    /// Brightest cell's value; `img` is scaled 0–255 against it.
    pub max: f32,
    /// Hex-encoded bytes, row-major from the bottom-left.
    pub img: String,
    pub tracks: Vec<TrackRec>,
}

impl FrameRec {
    pub fn new(t: f32, img: &Image, tracks: &[Track]) -> Self {
        let max = img.max().max(1e-12);
        let mut hex = String::with_capacity(img.v.len() * 2);
        for v in &img.v {
            let b = ((v / max).clamp(0.0, 1.0) * 255.0).round() as u8;
            hex.push_str(&format!("{b:02x}"));
        }
        FrameRec {
            t,
            nx: img.nx,
            ny: img.ny,
            cell: img.cell,
            max,
            img: hex,
            tracks: tracks.iter().map(|tr| TrackRec { id: tr.id, x: tr.x, y: tr.y }).collect(),
        }
    }

    pub fn image(&self) -> Image {
        let b = self.img.as_bytes();
        let v = (0..self.nx * self.ny)
            .map(|k| {
                let h = b.get(2 * k..2 * k + 2).and_then(|s| std::str::from_utf8(s).ok()).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0);
                h as f32 / 255.0 * self.max
            })
            .collect();
        Image { nx: self.nx, ny: self.ny, cell: self.cell, v }
    }
}

pub fn dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("chirpmap-sessions")
}

pub struct Recorder {
    w: BufWriter<std::fs::File>,
    pub path: PathBuf,
    pub frames: usize,
}

impl Recorder {
    pub fn start(h: &Header) -> Result<Self> {
        let d = dir();
        std::fs::create_dir_all(&d)?;
        let stamp = chrono_like(h.started_unix);
        let path = d.join(format!("session-{stamp}.jsonl"));
        let f = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut w = BufWriter::new(f);
        writeln!(w, "{}", serde_json::to_string(h)?)?;
        Ok(Recorder { w, path, frames: 0 })
    }

    pub fn push(&mut self, fr: &FrameRec) -> Result<()> {
        writeln!(self.w, "{}", serde_json::to_string(fr)?)?;
        self.frames += 1;
        if self.frames % 10 == 0 {
            self.w.flush()?;
        }
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.w.flush();
    }
}

pub struct Session {
    pub header: Header,
    pub frames: Vec<FrameRec>,
}

pub fn load(path: &PathBuf) -> Result<Session> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut lines = std::io::BufReader::new(f).lines();
    let Some(first) = lines.next() else { bail!("empty session file") };
    let header: Header = serde_json::from_str(&first?).context("session header")?;
    let mut frames = Vec::new();
    for l in lines {
        let l = l?;
        if l.trim().is_empty() {
            continue;
        }
        // A session cut off mid-write loses only its last line.
        if let Ok(fr) = serde_json::from_str::<FrameRec>(&l) {
            frames.push(fr);
        }
    }
    Ok(Session { header, frames })
}

/// Saved sessions, newest first.
pub fn list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir())
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// YYYYMMDD-HHMMSS (UTC) without pulling in a date crate.
fn chrono_like(unix: u64) -> String {
    let (days, secs) = (unix / 86_400, unix % 86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_and_dates() {
        let img = Image { nx: 3, ny: 2, cell: 0.5, v: vec![0.0, 1.0, 2.0, 0.5, 0.0, 2.0] };
        let fr = FrameRec::new(1.5, &img, &[]);
        let back = fr.image();
        for (a, b) in img.v.iter().zip(&back.v) {
            assert!((a - b).abs() < 0.01);
        }
        assert_eq!(chrono_like(1_762_560_000), "20251108-000000");
    }
}
