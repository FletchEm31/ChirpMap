//! Turning echoes into a floor plan: occupancy grid with measured vs. assumed
//! cells, wall tracing between neighbouring hits, and coverage %.
//!
//! Coordinates are metres relative to the first scan position. Headings are
//! compass-style: 0° = "forward" (+y, top of the map), increasing clockwise.

use crate::dsp::Echo;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cell {
    Unknown,
    FreeAssumed,
    WallAssumed,
    FreeMeasured,
    WallMeasured,
}

impl Cell {
    pub fn measured(self) -> bool {
        matches!(self, Cell::FreeMeasured | Cell::WallMeasured)
    }
    pub fn assumed(self) -> bool {
        matches!(self, Cell::FreeAssumed | Cell::WallAssumed)
    }
    /// Order used when merging evidence (measured always beats assumed).
    fn rank(self) -> u8 {
        match self {
            Cell::Unknown => 0,
            Cell::FreeAssumed => 1,
            Cell::WallAssumed => 2,
            Cell::FreeMeasured => 3,
            Cell::WallMeasured => 4,
        }
    }
    /// Order used when several cells share one character on screen (walls win).
    pub fn display_rank(self) -> u8 {
        match self {
            Cell::Unknown => 0,
            Cell::FreeAssumed => 1,
            Cell::FreeMeasured => 2,
            Cell::WallAssumed => 3,
            Cell::WallMeasured => 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub pos: (f32, f32),
    pub heading_deg: f32,
    pub echoes: Vec<Echo>,
    pub max_range: f32,
}

impl Observation {
    pub fn dir(&self) -> (f32, f32) {
        dir(self.heading_deg)
    }
    pub fn wall_point(&self) -> Option<(f32, f32)> {
        let e = self.echoes.first()?;
        let (dx, dy) = self.dir();
        Some((self.pos.0 + dx * e.dist_m, self.pos.1 + dy * e.dist_m))
    }
}

pub fn dir(deg: f32) -> (f32, f32) {
    let a = deg.to_radians();
    (a.sin(), a.cos())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanMap {
    pub version: u32,
    pub device: String,
    pub created_unix: u64,
    pub beam_deg: f32,
    pub cell_m: f32,
    pub observations: Vec<Observation>,
}

pub struct Grid {
    pub cell_m: f32,
    pub n: usize,
    pub origin: usize,
    pub cells: Vec<Cell>,
}

impl Grid {
    fn new(cell_m: f32, extent_m: f32) -> Self {
        let half = (extent_m / cell_m).ceil() as usize + 1;
        let n = 2 * half + 1;
        Self { cell_m, n, origin: half, cells: vec![Cell::Unknown; n * n] }
    }

    /// Grid column/row of a world point (row increases with +y).
    pub fn ij(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let i = (x / self.cell_m).round() as i64 + self.origin as i64;
        let j = (y / self.cell_m).round() as i64 + self.origin as i64;
        (i >= 0 && j >= 0 && (i as usize) < self.n && (j as usize) < self.n).then(|| (i as usize, j as usize))
    }

    pub fn xy(&self, i: usize, j: usize) -> (f32, f32) {
        (
            (i as f32 - self.origin as f32) * self.cell_m,
            (j as f32 - self.origin as f32) * self.cell_m,
        )
    }

    pub fn get(&self, i: usize, j: usize) -> Cell {
        self.cells[j * self.n + i]
    }

    fn put(&mut self, x: f32, y: f32, c: Cell) {
        if let Some((i, j)) = self.ij(x, y) {
            let old = &mut self.cells[j * self.n + i];
            let ok = match (old.measured(), c.measured()) {
                (false, true) => true,
                (true, false) => false,
                _ => c.rank() > old.rank(),
            };
            if ok {
                *old = c;
            }
        }
    }

    fn march(&mut self, from: (f32, f32), deg: f32, len: f32, c: Cell) {
        let (dx, dy) = dir(deg);
        let step = self.cell_m * 0.5;
        let mut t = 0.0;
        while t <= len {
            self.put(from.0 + dx * t, from.1 + dy * t, c);
            t += step;
        }
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32), c: Cell) {
        let d = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        let steps = (d / (self.cell_m * 0.5)).ceil().max(1.0) as usize;
        for k in 0..=steps {
            let t = k as f32 / steps as f32;
            self.put(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, c);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Coverage {
    pub measured_pct: f32,
    pub assumed_pct: f32,
    pub unscanned_pct: f32,
    /// Footprint the coverage is computed over (bounding box of detected walls).
    pub bbox: ((f32, f32), (f32, f32)),
}

impl Coverage {
    pub fn width_m(&self) -> f32 {
        self.bbox.1 .0 - self.bbox.0 .0
    }
    pub fn height_m(&self) -> f32 {
        self.bbox.1 .1 - self.bbox.0 .1
    }
}

/// Wall hits further apart than this aren't joined into an assumed wall.
const MAX_WALL_GAP_M: f32 = 2.0;

impl ScanMap {
    pub fn new(device: String, beam_deg: f32, cell_m: f32, created_unix: u64) -> Self {
        Self { version: 1, device, created_unix, beam_deg, cell_m, observations: Vec::new() }
    }

    pub fn positions(&self) -> Vec<(f32, f32)> {
        let mut v: Vec<(f32, f32)> = Vec::new();
        for o in &self.observations {
            if !v.iter().any(|p| (p.0 - o.pos.0).abs() < 1e-3 && (p.1 - o.pos.1).abs() < 1e-3) {
                v.push(o.pos);
            }
        }
        v
    }

    pub fn wall_points(&self) -> Vec<(f32, f32)> {
        self.observations.iter().filter_map(|o| o.wall_point()).collect()
    }

    pub fn build_grid(&self) -> Grid {
        let reach = self
            .observations
            .iter()
            .map(|o| o.pos.0.abs().max(o.pos.1.abs()) + o.max_range)
            .fold(1.0f32, f32::max);
        let mut g = Grid::new(self.cell_m, reach + 0.5);
        let c = self.cell_m;

        for o in &self.observations {
            let (r, hit) = match o.echoes.first() {
                Some(e) => (e.dist_m, true),
                None => (o.max_range, false),
            };
            // The beam is wide: anything inside it *could* have produced the echo.
            let mut a = -self.beam_deg;
            while a <= self.beam_deg + 1e-3 {
                g.march(o.pos, o.heading_deg + a, (r - c).max(0.0), Cell::FreeAssumed);
                if hit {
                    let (dx, dy) = dir(o.heading_deg + a);
                    g.put(o.pos.0 + dx * r, o.pos.1 + dy * r, Cell::WallAssumed);
                }
                a += 1.5;
            }
            // Along the boresight we treat the range as measured.
            g.march(o.pos, o.heading_deg, (r - c).max(0.0), Cell::FreeMeasured);
            if hit {
                let (dx, dy) = o.dir();
                g.put(o.pos.0 + dx * r, o.pos.1 + dy * r, Cell::WallMeasured);
            }
        }

        // Join neighbouring wall hits from the same spot into assumed wall segments.
        for p in self.positions() {
            let mut hits: Vec<(f32, (f32, f32))> = self
                .observations
                .iter()
                .filter(|o| (o.pos.0 - p.0).abs() < 1e-3 && (o.pos.1 - p.1).abs() < 1e-3)
                .filter_map(|o| o.wall_point().map(|w| (o.heading_deg.rem_euclid(360.0), w)))
                .collect();
            hits.sort_by(|a, b| a.0.total_cmp(&b.0));
            let n = hits.len();
            if n < 2 {
                continue;
            }
            for k in 0..n {
                if n == 2 && k == 1 {
                    break;
                }
                let a = hits[k].1;
                let b = hits[(k + 1) % n].1;
                let d = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
                if d <= MAX_WALL_GAP_M {
                    g.line(a, b, Cell::WallAssumed);
                }
            }
        }
        g
    }

    pub fn coverage(&self, g: &Grid) -> Coverage {
        let mut pts = self.wall_points();
        pts.extend(self.positions());
        if pts.len() < 2 {
            for j in 0..g.n {
                for i in 0..g.n {
                    if g.get(i, j) != Cell::Unknown {
                        pts.push(g.xy(i, j));
                    }
                }
            }
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in &pts {
            x0 = x0.min(*x);
            y0 = y0.min(*y);
            x1 = x1.max(*x);
            y1 = y1.max(*y);
        }
        if pts.is_empty() {
            (x0, y0, x1, y1) = (0.0, 0.0, 0.0, 0.0);
        }
        let (mut m, mut a, mut u) = (0usize, 0usize, 0usize);
        if let (Some((i0, j0)), Some((i1, j1))) = (g.ij(x0, y0), g.ij(x1, y1)) {
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let c = g.get(i, j);
                    if c.measured() {
                        m += 1;
                    } else if c.assumed() {
                        a += 1;
                    } else {
                        u += 1;
                    }
                }
            }
        }
        let tot = (m + a + u).max(1) as f32;
        Coverage {
            measured_pct: 100.0 * m as f32 / tot,
            assumed_pct: 100.0 * a as f32 / tot,
            unscanned_pct: 100.0 * u as f32 / tot,
            bbox: ((x0, y0), (x1, y1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(heading: f32, d: f32) -> Observation {
        Observation {
            pos: (0.0, 0.0),
            heading_deg: heading,
            echoes: vec![Echo { dist_m: d, strength: 0.2 }],
            max_range: 5.0,
        }
    }

    #[test]
    fn coverage_sums_to_100_and_walls_land_where_expected() {
        let mut m = ScanMap::new("test".into(), 25.0, 0.1, 0);
        for k in 0..8 {
            m.observations.push(obs(k as f32 * 45.0, 2.0));
        }
        let g = m.build_grid();
        let (i, j) = g.ij(0.0, 2.0).unwrap();
        assert_eq!(g.get(i, j), Cell::WallMeasured);
        let (i, j) = g.ij(0.0, 1.0).unwrap();
        assert_eq!(g.get(i, j), Cell::FreeMeasured);
        let c = m.coverage(&g);
        let sum = c.measured_pct + c.assumed_pct + c.unscanned_pct;
        assert!((sum - 100.0).abs() < 0.01);
        assert!(c.measured_pct > 0.0 && c.assumed_pct > 0.0);
        assert!((c.width_m() - 4.0).abs() < 0.05);
    }
}
