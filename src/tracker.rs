//! Multi-target tracking over time.
//!
//! Each frame the imager proposes a few candidate positions. A real person
//! moves smoothly, so a candidate only becomes a confirmed track after it
//! shows up in consecutive frames near where a track was predicted to be.
//! One-frame "ghosts" (where measurement curves happen to cross somewhere
//! nobody is) never get confirmed. Each person keeps a stable ID.

use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub hits: u32,
    pub misses: u32,
    pub confirmed: bool,
    /// Seconds since this track last matched a detection.
    pub since_update: f32,
    last_t: f64,
    born_t: f64,
    seen: u32,
    pub trail: VecDeque<(f32, f32)>,
}

pub struct Tracker {
    tracks: Vec<Track>,
    next_id: u32,
    last_t: Option<f64>,
}

/// Base association radius (m) plus how far someone can walk between frames.
const GATE_M: f32 = 0.45;
const MAX_SPEED: f32 = 2.0; // m/s, brisk walk
/// A new track must be seen this many times, over at least CONFIRM_S, in at
/// least CONFIRM_RATIO of the frames since it appeared.
const CONFIRM_HITS: u32 = 3;
const CONFIRM_S: f32 = 0.5;
const CONFIRM_RATIO: f32 = 0.7;
/// A tentative track is given up after this many misses in a row.
const TENTATIVE_MISSES: u32 = 2;
const DROP_AFTER_S: f32 = 1.6;
const MAX_TRACKS: usize = 4;
const TRAIL: usize = 10;
const ALPHA: f32 = 0.6; // position correction
const BETA: f32 = 0.25; // velocity correction

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Tracker { tracks: Vec::new(), next_id: 1, last_t: None }
    }

    /// Feed one frame: `t` seconds, candidates `(x, y, strength)` strongest first.
    /// Returns the confirmed tracks.
    pub fn update(&mut self, t: f64, cands: &[(f32, f32, f32)]) -> Vec<Track> {
        let dt = self.last_t.map_or(0.3, |l| ((t - l) as f32).clamp(0.02, 2.0));
        self.last_t = Some(t);

        // Predict.
        for tr in &mut self.tracks {
            tr.x += tr.vx * dt;
            tr.y += tr.vy * dt;
            tr.seen += 1;
        }

        // Greedy nearest-neighbour association inside each track's gate.
        let gate = GATE_M + MAX_SPEED * dt;
        let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (ti, tr) in self.tracks.iter().enumerate() {
            for (ci, c) in cands.iter().enumerate() {
                let d = ((tr.x - c.0).powi(2) + (tr.y - c.1).powi(2)).sqrt();
                if d <= gate {
                    // Confirmed tracks win ties over tentative ones.
                    let bias = if tr.confirmed { 0.0 } else { 0.05 };
                    pairs.push((d + bias, ti, ci));
                }
            }
        }
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut t_used = vec![false; self.tracks.len()];
        let mut c_used = vec![false; cands.len()];
        for (_, ti, ci) in pairs {
            if t_used[ti] || c_used[ci] {
                continue;
            }
            t_used[ti] = true;
            c_used[ci] = true;
            let tr = &mut self.tracks[ti];
            let (zx, zy) = (cands[ci].0, cands[ci].1);
            let (rx, ry) = (zx - tr.x, zy - tr.y);
            tr.x += ALPHA * rx;
            tr.y += ALPHA * ry;
            tr.vx = (tr.vx + BETA * rx / dt).clamp(-MAX_SPEED, MAX_SPEED);
            tr.vy = (tr.vy + BETA * ry / dt).clamp(-MAX_SPEED, MAX_SPEED);
            tr.hits += 1;
            tr.misses = 0;
            tr.last_t = t;
            let age = (t - tr.born_t) as f32;
            if tr.hits >= CONFIRM_HITS && age >= CONFIRM_S && tr.hits as f32 >= CONFIRM_RATIO * tr.seen as f32 {
                tr.confirmed = true;
            }
            tr.trail.push_back((tr.x, tr.y));
            while tr.trail.len() > TRAIL {
                tr.trail.pop_front();
            }
        }

        // Unmatched tracks coast and slow down; stale or failed tentatives are dropped.
        for (ti, tr) in self.tracks.iter_mut().enumerate() {
            if !t_used[ti] {
                tr.misses += 1;
                tr.vx *= 0.7;
                tr.vy *= 0.7;
            }
            tr.since_update = (t - tr.last_t) as f32;
        }
        self.tracks.retain(|tr| {
            if tr.confirmed {
                tr.since_update <= DROP_AFTER_S
            } else {
                tr.misses < TENTATIVE_MISSES && tr.hits as f32 >= 0.5 * tr.seen as f32
            }
        });

        // Unmatched candidates start tentative tracks (not on top of an existing one).
        for (ci, c) in cands.iter().enumerate() {
            if c_used[ci] || self.tracks.len() >= MAX_TRACKS {
                continue;
            }
            let crowded = self.tracks.iter().any(|tr| ((tr.x - c.0).powi(2) + (tr.y - c.1).powi(2)).sqrt() < GATE_M);
            if crowded {
                continue;
            }
            let mut trail = VecDeque::new();
            trail.push_back((c.0, c.1));
            self.tracks.push(Track {
                id: 0,
                x: c.0,
                y: c.1,
                vx: 0.0,
                vy: 0.0,
                hits: 1,
                misses: 0,
                confirmed: false,
                since_update: 0.0,
                last_t: t,
                born_t: t,
                seen: 1,
                trail,
            });
        }

        // IDs are handed out only when a track is confirmed, so ghosts never burn one.
        for tr in &mut self.tracks {
            if tr.confirmed && tr.id == 0 {
                tr.id = self.next_id;
                self.next_id += 1;
            }
        }
        self.tracks.iter().filter(|t| t.confirmed).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghosts_are_ignored_and_two_people_keep_their_ids() {
        let mut tk = Tracker::new();
        let mut t = 0.0;
        let mut ids = Vec::new();
        for k in 0..20 {
            let s = k as f32 * 0.1;
            let a = (0.5 + s * 0.5, 1.0); // walking right
            let b = (2.0, 2.5 - s * 0.5); // walking down
            // A ghost somewhere random every third frame.
            let mut cands = vec![(a.0, a.1, 1.0), (b.0, b.1, 0.9)];
            if k % 3 == 0 {
                cands.push((0.2 + (k as f32 * 0.37) % 2.0, 0.3 + (k as f32 * 0.73) % 2.5, 0.8));
            }
            let out = tk.update(t, &cands);
            t += 0.3;
            if k >= 3 {
                assert_eq!(out.len(), 2, "frame {k}: {out:?}");
                let mut f: Vec<(u32, f32)> = out.iter().map(|tr| (tr.id, tr.x)).collect();
                f.sort_by(|p, q| p.0.cmp(&q.0));
                ids.push((f[0].0, f[1].0));
            }
        }
        // IDs never change while both people stay in view.
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    }
}
