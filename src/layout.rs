//! The room blueprint the user draws: room size plus every speaker and mic,
//! where it sits and which way it faces. Knowing the geometry turns each
//! speaker→mic pair from "some echo at some distance" into a curve on the
//! floor plan, and lets several pairs agree on one 2D position.
//!
//! Coordinates: metres, origin at the room's bottom-left corner on the
//! blueprint, x to the right (width), y up the page (depth).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Speaker,
    Mic,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub kind: Kind,
    /// Short label shown on the plan: S1, S2, M1…
    pub label: String,
    /// Which physical device: "default" or a name/number from the hardware panel.
    pub device: String,
    pub x: f32,
    pub y: f32,
    /// Compass-style on the plan: 0° = up the page, clockwise.
    pub facing_deg: f32,
    /// Speakers only: play on just this output channel (0 = left, 1 = right, …).
    /// None = all channels, e.g. a soundbar used as one speaker.
    #[serde(default)]
    pub channel: Option<u8>,
}

pub fn channel_name(c: Option<u8>) -> String {
    match c {
        None => "all channels".into(),
        Some(0) => "left (ch 1)".into(),
        Some(1) => "right (ch 2)".into(),
        Some(2) => "centre (ch 3)".into(),
        Some(3) => "sub (ch 4)".into(),
        Some(4) => "rear left (ch 5)".into(),
        Some(5) => "rear right (ch 6)".into(),
        Some(n) => format!("ch {}", n + 1),
    }
}

impl Node {
    pub fn pos(&self) -> (f32, f32) {
        (self.x, self.y)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout {
    pub name: String,
    pub width_m: f32,
    pub depth_m: f32,
    pub nodes: Vec<Node>,
}

pub fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Layout {
    /// A sensible starting room: ~11'6" × 10', speaker and mic on a desk at the top wall.
    pub fn starter() -> Self {
        Layout {
            name: "my-room".into(),
            width_m: 3.5,
            depth_m: 3.0,
            nodes: vec![
                Node { kind: Kind::Speaker, label: "S1".into(), device: "default".into(), x: 1.75, y: 2.6, facing_deg: 180.0, channel: None },
                Node { kind: Kind::Mic, label: "M1".into(), device: "default".into(), x: 2.05, y: 2.6, facing_deg: 180.0, channel: None },
            ],
        }
    }

    pub fn speakers(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.kind == Kind::Speaker).collect()
    }

    pub fn mics(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.kind == Kind::Mic).collect()
    }

    /// Next free label for a kind: S3, M2…
    pub fn next_label(&self, kind: Kind) -> String {
        let p = if kind == Kind::Speaker { 'S' } else { 'M' };
        let n = self.nodes.iter().filter(|n| n.kind == kind).count() + 1;
        format!("{p}{n}")
    }

    /// Keep every node inside the room.
    pub fn clamp(&mut self) {
        self.width_m = self.width_m.clamp(1.0, 30.0);
        self.depth_m = self.depth_m.clamp(1.0, 30.0);
        for n in &mut self.nodes {
            n.x = n.x.clamp(0.0, self.width_m);
            n.y = n.y.clamp(0.0, self.depth_m);
            n.facing_deg = n.facing_deg.rem_euclid(360.0);
        }
    }

    /// Re-number labels after a delete so they stay S1..Sn, M1..Mn.
    pub fn relabel(&mut self) {
        let (mut s, mut m) = (0, 0);
        for n in &mut self.nodes {
            match n.kind {
                Kind::Speaker => {
                    s += 1;
                    n.label = format!("S{s}");
                }
                Kind::Mic => {
                    m += 1;
                    n.label = format!("M{m}");
                }
            }
        }
    }

    pub fn is_usable(&self) -> bool {
        !self.speakers().is_empty() && !self.mics().is_empty()
    }
}

fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("chirpmap").join("rooms")
}

fn slug(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "room".into() } else { s }
}

pub fn save(l: &Layout) -> Result<PathBuf> {
    let d = dir();
    std::fs::create_dir_all(&d).context("creating rooms folder")?;
    let p = d.join(format!("{}.json", slug(&l.name)));
    std::fs::write(&p, serde_json::to_string_pretty(l)?)?;
    std::fs::write(d.join("active"), slug(&l.name))?;
    Ok(p)
}

/// The room the user last saved, if any.
pub fn load_active() -> Option<Layout> {
    let d = dir();
    let name = std::fs::read_to_string(d.join("active")).ok()?;
    let txt = std::fs::read_to_string(d.join(format!("{}.json", name.trim()))).ok()?;
    serde_json::from_str(&txt).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_clamping() {
        let mut l = Layout::starter();
        assert_eq!(l.next_label(Kind::Mic), "M2");
        l.nodes.push(Node { kind: Kind::Mic, label: "M2".into(), device: "default".into(), x: 9.0, y: -1.0, facing_deg: 370.0, channel: None });
        l.clamp();
        assert_eq!((l.nodes[2].x, l.nodes[2].y), (3.5, 0.0));
        assert!((l.nodes[2].facing_deg - 10.0).abs() < 1e-3);
        l.nodes.remove(1);
        l.relabel();
        assert_eq!(l.nodes[1].label, "M1");
    }
}
