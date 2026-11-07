//! Signal processing: chirp generation, matched filtering, range profiles,
//! echo picking and frequency-band selection.

use rustfft::{num_complex::Complex32, FftPlanner};
use serde::{Deserialize, Serialize};
use std::f32::consts::PI;

/// Speed of sound in air at ~20 °C, m/s.
pub const SPEED_OF_SOUND: f32 = 343.0;

/// Linear frequency sweep f0 -> f1 with a Hann window so the speaker doesn't click.
pub fn chirp(fs: u32, dur_s: f32, f0: f32, f1: f32) -> Vec<f32> {
    let n = ((fs as f32) * dur_s).round().max(2.0) as usize;
    let k = (f1 - f0) / dur_s;
    (0..n)
        .map(|i| {
            let t = i as f32 / fs as f32;
            let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (n - 1) as f32).cos();
            (2.0 * PI * (f0 * t + 0.5 * k * t * t)).sin() * w
        })
        .collect()
}

fn to_complex(x: &[f32], n: usize) -> Vec<Complex32> {
    let mut v = vec![Complex32::new(0.0, 0.0); n];
    for (d, &s) in v.iter_mut().zip(x) {
        d.re = s;
    }
    v
}

/// Matched filter + envelope in one pass.
///
/// Correlates the recording with the *analytic* version of the template, so the
/// magnitude of the result is the echo envelope directly. Output index `k` is the
/// lag (in samples) at which a copy of the template starts in `rec`.
pub fn matched_envelope(rec: &[f32], tpl: &[f32]) -> Vec<f32> {
    if rec.len() < tpl.len() || tpl.is_empty() {
        return Vec::new();
    }
    let n = (rec.len() + tpl.len()).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);

    let mut r = to_complex(rec, n);
    let mut s = to_complex(tpl, n);
    fwd.process(&mut r);
    fwd.process(&mut s);

    // Analytic template: keep DC/Nyquist, double positive bins, drop negative bins.
    for (k, v) in s.iter_mut().enumerate() {
        let g = if k == 0 || k == n / 2 {
            1.0
        } else if k < n / 2 {
            2.0
        } else {
            0.0
        };
        *v *= g;
    }
    let mut c: Vec<Complex32> = r.iter().zip(&s).map(|(a, b)| a * b.conj()).collect();
    inv.process(&mut c);
    let valid = rec.len() - tpl.len() + 1;
    c[..valid].iter().map(|z| z.norm() / n as f32).collect()
}

pub fn samples_to_m(i: usize, fs: u32) -> f32 {
    i as f32 / fs as f32 * SPEED_OF_SOUND / 2.0
}

pub fn m_to_samples(m: f32, fs: u32) -> usize {
    (m.max(0.0) * 2.0 / SPEED_OF_SOUND * fs as f32).round() as usize
}

pub fn argmax(x: &[f32]) -> usize {
    x.iter()
        .enumerate()
        .fold((0, f32::MIN), |(bi, bv), (i, &v)| if v > bv { (i, v) } else { (bi, bv) })
        .0
}

/// Echo envelope vs. round-trip distance, with index 0 = the direct
/// speaker-to-mic arrival. Using the direct path as t0 cancels unknown
/// audio driver latency.
#[derive(Clone, Debug)]
pub struct RangeProfile {
    pub fs: u32,
    pub env: Vec<f32>,
    pub direct_peak: f32,
    /// Where the direct path landed in each raw ping (latency jitter diagnostic).
    pub direct_idx: Vec<usize>,
}

impl RangeProfile {
    pub fn dist(&self, i: usize) -> f32 {
        samples_to_m(i, self.fs)
    }
}

/// Average several pings, each aligned on its own direct-path peak
/// (separate input/output streams don't share a fixed latency).
pub fn aligned_average(envs: &[Vec<f32>], fs: u32) -> RangeProfile {
    let idx: Vec<usize> = envs.iter().map(|e| argmax(e)).collect();
    let len = envs
        .iter()
        .zip(&idx)
        .map(|(e, &i)| e.len() - i)
        .min()
        .unwrap_or(0);
    let mut acc = vec![0.0f32; len];
    for (e, &i) in envs.iter().zip(&idx) {
        for (k, a) in acc.iter_mut().enumerate() {
            *a += e[i + k];
        }
    }
    let n = envs.len().max(1) as f32;
    acc.iter_mut().for_each(|v| *v /= n);
    RangeProfile {
        fs,
        direct_peak: acc.first().copied().unwrap_or(0.0),
        env: acc,
        direct_idx: idx,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Echo {
    /// One-way distance to the reflector, metres.
    pub dist_m: f32,
    /// Echo amplitude relative to the direct path (0..1).
    pub strength: f32,
}

impl Echo {
    pub fn db(&self) -> f32 {
        20.0 * self.strength.max(1e-9).log10()
    }
}

/// Local maxima above `threshold`, between the blanking distance and max range,
/// with ~12 cm non-maximum suppression. Sorted nearest first.
pub fn find_echoes(p: &RangeProfile, blank_m: f32, max_range_m: f32, threshold: f32) -> Vec<Echo> {
    let fs = p.fs;
    let e = &p.env;
    if e.len() < 3 {
        return Vec::new();
    }
    let lo = m_to_samples(blank_m, fs).max(1);
    let hi = m_to_samples(max_range_m, fs).min(e.len() - 2);
    if lo >= hi {
        return Vec::new();
    }
    let nms = m_to_samples(0.12, fs).max(1);
    let mut peaks: Vec<usize> = (lo..=hi)
        .filter(|&i| e[i] > threshold && e[i] >= e[i - 1] && e[i] > e[i + 1])
        .collect();
    peaks.sort_by(|a, b| e[*b].total_cmp(&e[*a]));
    let mut kept: Vec<usize> = Vec::new();
    for i in peaks {
        if kept.iter().all(|&k| k.abs_diff(i) > nms) {
            kept.push(i);
        }
    }
    kept.sort_unstable();
    kept.into_iter()
        .map(|i| Echo {
            dist_m: samples_to_m(i, fs),
            strength: e[i] / p.direct_peak.max(1e-12),
        })
        .collect()
}

/// Detection threshold: well above the measured noise floor, and never below
/// a small fraction of the direct path.
pub fn echo_threshold(noise_env: f32, direct_peak: f32) -> f32 {
    if noise_env > 0.0 {
        (noise_env * 6.0).max(direct_peak * 0.01)
    } else {
        direct_peak * 0.03
    }
}

/// Hann-windowed power spectrum of `x` zero-padded to `n` (n/2 bins).
fn power_spectrum(x: &[f32], n: usize) -> Vec<f32> {
    let len = x.len().min(n);
    let mut buf = vec![Complex32::new(0.0, 0.0); n];
    for i in 0..len {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (len.max(2) - 1) as f32).cos();
        buf[i].re = x[i] * w;
    }
    FftPlanner::<f32>::new().plan_fft_forward(n).process(&mut buf);
    buf[..n / 2].iter().map(|z| z.norm_sqr()).collect()
}

/// Per-1 kHz band SNR (dB) of a probe recording vs. room noise of equal length.
pub fn band_snr(sig: &[f32], noise: &[f32], fs: u32, lo_hz: f32, hi_hz: f32) -> Vec<(f32, f32)> {
    let len = sig.len().min(noise.len());
    let n = len.next_power_of_two();
    let ps = power_spectrum(&sig[..len], n);
    let pn = power_spectrum(&noise[..len], n);
    let hz_per_bin = fs as f32 / n as f32;
    let mut out = Vec::new();
    let mut f = lo_hz;
    while f + 1000.0 <= hi_hz {
        let a = (f / hz_per_bin) as usize;
        let b = (((f + 1000.0) / hz_per_bin) as usize).min(ps.len());
        let s: f32 = ps[a..b].iter().sum();
        let nn: f32 = pn[a..b].iter().sum();
        out.push((f, 10.0 * ((s + 1e-20) / (nn + 1e-20)).log10()));
        f += 1000.0;
    }
    out
}

/// Widest contiguous run of bands with SNR >= `min_db`, at least 4 kHz wide.
pub fn pick_band(bands: &[(f32, f32)], min_db: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    let mut start: Option<f32> = None;
    for (i, &(f, snr)) in bands.iter().enumerate() {
        if snr >= min_db {
            start.get_or_insert(f);
        }
        let run_ends = snr < min_db || i == bands.len() - 1;
        if run_ends {
            if let Some(s) = start.take() {
                let end = if snr >= min_db { f + 1000.0 } else { f };
                if best.map_or(true, |(a, b)| end - s > b - a) {
                    best = Some((s, end));
                }
            }
        }
    }
    best.filter(|(a, b)| b - a >= 4000.0)
}

/// Linear-interpolation resampler (used when mic and speaker rates differ).
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let r = from as f64 / to as f64;
    let n = (x.len() as f64 / r) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 * r;
            let j = t.floor() as usize;
            let f = (t - j as f64) as f32;
            let a = x[j.min(x.len() - 1)];
            let b = *x.get(j + 1).unwrap_or(&a);
            a + (b - a) * f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_echo_at_known_distance() {
        let fs = 48_000;
        let tpl = chirp(fs, 0.01, 2000.0, 16000.0);
        let mut rec = vec![0.0f32; 20_000];
        let t0 = 1234;
        for (i, &v) in tpl.iter().enumerate() {
            rec[t0 + i] += v;
        }
        let d = 1.75;
        let t1 = t0 + m_to_samples(d, fs);
        for (i, &v) in tpl.iter().enumerate() {
            rec[t1 + i] += 0.2 * v;
        }
        let env = matched_envelope(&rec, &tpl);
        let p = aligned_average(&[env], fs);
        let th = echo_threshold(0.0, p.direct_peak);
        let echoes = find_echoes(&p, 0.2, 4.0, th);
        assert_eq!(echoes.len(), 1, "{echoes:?}");
        assert!((echoes[0].dist_m - d).abs() < 0.02, "{echoes:?}");
        assert!((echoes[0].strength - 0.2).abs() < 0.03);
    }

    #[test]
    fn band_picker_takes_widest_run() {
        let b: Vec<(f32, f32)> = (1..20)
            .map(|k| {
                let f = k as f32 * 1000.0;
                (f, if (3..=14).contains(&k) { 20.0 } else { 2.0 })
            })
            .collect();
        assert_eq!(pick_band(&b, 10.0), Some((3000.0, 15000.0)));
    }

    #[test]
    fn resample_length() {
        let x = vec![0.0; 44_100];
        assert_eq!(resample(&x, 44_100, 48_000).len(), 48_000);
    }
}
