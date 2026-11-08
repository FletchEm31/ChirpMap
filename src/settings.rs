//! User settings that persist between runs (~/.config/chirpmap/settings.json).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Chirp above most adults' hearing (18–21 kHz).
    pub silent: bool,
    /// All speakers chirp at once with separate codes (faster frames).
    pub simultaneous: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { silent: false, simultaneous: true }
    }
}

fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("chirpmap").join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save(s: &Settings) {
    let p = path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, serde_json::to_string_pretty(s).unwrap_or_default());
}

/// Lowest frequency of the silent band.
pub const SILENT_F0: f32 = 18_000.0;

/// The silent band for this sample rate (needs at least 44.1 kHz).
pub fn silent_band(fs: u32) -> (f32, f32) {
    (SILENT_F0, (0.47 * fs as f32).min(21_000.0))
}

/// The chirp band to use: silent band when silent mode is on, else the calibrated band.
pub fn band(fs: u32, f0: f32, f1: f32) -> (f32, f32) {
    if load().silent {
        silent_band(fs)
    } else {
        (f0, f1)
    }
}

/// Each speaker gets its own slice of the band (alternating up / down sweeps)
/// so they can all chirp at the same moment and still be told apart.
/// Returns None when slices would be too narrow to range well (< 2.5 kHz each).
pub fn speaker_codes(fs: u32, f0: f32, f1: f32, n: usize, dur_s: f32) -> Option<Vec<Vec<f32>>> {
    if n <= 1 {
        return Some(vec![crate::dsp::chirp(fs, dur_s, f0, f1)]);
    }
    let width = (f1 - f0) / n as f32;
    if width < 2_500.0 {
        return None;
    }
    let guard = width * 0.06;
    Some(
        (0..n)
            .map(|k| {
                let lo = f0 + k as f32 * width + guard;
                let hi = f0 + (k + 1) as f32 * width - guard;
                if k % 2 == 0 {
                    crate::dsp::chirp(fs, dur_s, lo, hi)
                } else {
                    crate::dsp::chirp(fs, dur_s, hi, lo)
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{argmax, matched_envelope};

    #[test]
    fn simultaneous_codes_stay_separable() {
        let fs = 48_000;
        let codes = speaker_codes(fs, 3_000.0, 16_000.0, 3, 0.01).unwrap();
        // A mic hears all three at different delays; each code must find its own.
        let delays = [1_000usize, 1_700, 2_300];
        let mut rec = vec![0.0f32; 6_000];
        for (c, d) in codes.iter().zip(delays) {
            for (i, v) in c.iter().enumerate() {
                rec[d + i] += v;
            }
        }
        for (c, d) in codes.iter().zip(delays) {
            let env = matched_envelope(&rec, c);
            assert!((argmax(&env) as i64 - d as i64).abs() <= 2);
        }
        // Silent band is too narrow to split three ways.
        let (a, b) = silent_band(fs);
        assert!(speaker_codes(fs, a, b, 3, 0.01).is_none());
    }
}
