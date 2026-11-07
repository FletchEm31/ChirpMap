//! Per-device auto-calibration: noise floor, usable frequency band, volume,
//! speaker ring-down (blanking distance) and latency jitter. Saved as JSON so
//! every later ping/scan on that speaker+mic pair uses the right settings.

use crate::audio::Sonar;
use crate::dsp::{self, chirp, matched_envelope, samples_to_m, m_to_samples};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub device: String,
    pub fs: u32,
    pub f0: f32,
    pub f1: f32,
    pub chirp_s: f32,
    pub volume: f32,
    /// Ignore echoes closer than this (direct blast + speaker ringing), metres.
    pub blank_m: f32,
    /// Matched-filter envelope level of room noise (absolute). 0 = unknown.
    pub noise_env: f32,
    pub direct_snr_db: f32,
    pub latency_jitter_ms: f32,
    /// (band start Hz, SNR dB) for each 1 kHz band probed.
    pub band_snr: Vec<(f32, f32)>,
    pub created_unix: u64,
}

impl Profile {
    pub fn defaults(sonar: &dyn Sonar) -> Self {
        let fs = sonar.fs();
        Self {
            device: sonar.name(),
            fs,
            f0: 2000.0,
            f1: (0.45 * fs as f32).min(16_000.0),
            chirp_s: 0.01,
            volume: 0.7,
            blank_m: 0.3,
            noise_env: 0.0,
            direct_snr_db: 0.0,
            latency_jitter_ms: 0.0,
            band_snr: Vec::new(),
            created_unix: 0,
        }
    }

    pub fn template(&self) -> Vec<f32> {
        chirp(self.fs, self.chirp_s, self.f0, self.f1)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn profiles_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("chirpmap")
        .join("profiles")
}

fn slug(device: &str) -> String {
    let s: String = device
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let mut out = String::new();
    for c in s.chars() {
        if !(c == '_' && out.ends_with('_')) {
            out.push(c);
        }
    }
    out.trim_matches('_').chars().take(120).collect()
}

pub fn profile_path(device: &str) -> PathBuf {
    profiles_dir().join(format!("{}.json", slug(device)))
}

pub fn load(device: &str) -> Option<Profile> {
    let txt = std::fs::read_to_string(profile_path(device)).ok()?;
    serde_json::from_str(&txt).ok()
}

pub fn save(p: &Profile) -> Result<PathBuf> {
    let path = profile_path(&p.device);
    std::fs::create_dir_all(path.parent().unwrap()).context("creating profile directory")?;
    std::fs::write(&path, serde_json::to_string_pretty(p)?)?;
    Ok(path)
}

pub fn list_profiles() -> Vec<Profile> {
    let Ok(rd) = std::fs::read_dir(profiles_dir()) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect()
}

/// Load this device's profile, or fall back to defaults with a note.
pub fn load_or_default(sonar: &dyn Sonar) -> Profile {
    match load(&sonar.name()) {
        Some(p) if p.fs == sonar.fs() => p,
        Some(_) => {
            eprintln!("note: sample rate changed since calibration — using defaults; rerun `chirpmap calibrate`");
            Profile::defaults(sonar)
        }
        None => {
            if !sonar.is_sim() {
                eprintln!("note: no calibration for this speaker/mic yet — using defaults (run `chirpmap calibrate`)");
            }
            Profile::defaults(sonar)
        }
    }
}

fn percentile(v: &[f32], q: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[((s.len() - 1) as f32 * q) as usize]
}

fn peak_abs(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// Run the full calibration and return the profile (caller saves it).
pub fn run(sonar: &mut dyn Sonar) -> Result<Profile> {
    let mut p = Profile::defaults(sonar);
    let fs = p.fs;
    let listen = 0.06;

    println!("Calibrating: {}", p.device);
    println!("Keep the room quiet and the device still.\n");

    // 1. Room noise
    println!("[1/4] Listening to room noise (1 s)…");
    let noise = sonar.record(1.0)?;
    let noise_rms = (noise.iter().map(|x| x * x).sum::<f32>() / noise.len() as f32).sqrt();
    println!("      noise RMS {:.5}", noise_rms);

    // 2. Frequency response probe: wide chirp, compare to noise per 1 kHz band
    println!("[2/4] Probing speaker/mic frequency response…");
    let top = (0.45 * fs as f32).min(20_000.0);
    let probe = chirp(fs, 0.02, 1000.0, top);
    let rec = sonar.playrec(&probe, listen, 0.6)?;
    let env = matched_envelope(&rec, &probe);
    let start = dsp::argmax(&env);
    let seg_len = probe.len() + m_to_samples(0.5, fs);
    let seg = &rec[start..(start + seg_len).min(rec.len())];
    let nseg = &noise[..seg.len().min(noise.len())];
    p.band_snr = dsp::band_snr(seg, nseg, fs, 1000.0, top);
    match dsp::pick_band(&p.band_snr, 10.0) {
        Some((a, b)) => {
            p.f0 = a.max(1000.0);
            p.f1 = b.min(top);
            println!("      usable band {:.1}–{:.1} kHz", p.f0 / 1000.0, p.f1 / 1000.0);
        }
        None => println!(
            "      no clean ≥4 kHz band found — keeping default {:.1}–{:.1} kHz",
            p.f0 / 1000.0,
            p.f1 / 1000.0
        ),
    }

    // 3. Volume: quietest level that clears 35 dB SNR without clipping
    println!("[3/4] Setting volume…");
    let tpl = p.template();
    let nenv = matched_envelope(&noise, &tpl);
    p.noise_env = percentile(&nenv, 0.95);
    let mut chosen: Option<(f32, f32)> = None;
    let mut best_ok: Option<(f32, f32)> = None;
    for vol in [0.3f32, 0.5, 0.7, 0.9] {
        let rec = sonar.playrec(&tpl, listen, vol)?;
        let clip = peak_abs(&rec) > 0.95;
        let direct = matched_envelope(&rec, &tpl).iter().cloned().fold(0.0, f32::max);
        let snr = 20.0 * (direct / p.noise_env.max(1e-9)).log10();
        println!("      vol {vol:.1}: SNR {snr:5.1} dB{}", if clip { "  (clipping)" } else { "" });
        if clip {
            break;
        }
        best_ok = Some((vol, snr));
        if snr >= 35.0 {
            chosen = Some((vol, snr));
            break;
        }
    }
    let (vol, snr) = chosen.or(best_ok).unwrap_or((0.3, 0.0));
    p.volume = vol;
    p.direct_snr_db = snr;
    println!("      → volume {vol:.1} ({snr:.0} dB)");

    // 4. Ring-down and latency jitter
    println!("[4/4] Measuring speaker ring-down and latency jitter…");
    let envs: Result<Vec<Vec<f32>>> = (0..6)
        .map(|_| sonar.playrec(&tpl, listen, p.volume).map(|r| matched_envelope(&r, &tpl)))
        .collect();
    let rp = dsp::aligned_average(&envs?, fs);
    let w = m_to_samples(0.05, fs).max(1);
    let lim = rp.direct_peak * 0.05;
    let mut k = 1;
    while k + w < rp.env.len() && rp.env[k..k + w].iter().cloned().fold(0.0, f32::max) >= lim {
        k += 1;
    }
    p.blank_m = (samples_to_m(k, fs) + 0.05).clamp(0.12, 1.2);
    let idx: Vec<f32> = rp.direct_idx.iter().map(|&i| i as f32).collect();
    let mean = idx.iter().sum::<f32>() / idx.len() as f32;
    let sd = (idx.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / idx.len() as f32).sqrt();
    p.latency_jitter_ms = sd / fs as f32 * 1000.0;
    println!(
        "      blanking distance {:.2} m · latency jitter {:.2} ms (cancelled by direct-path alignment)",
        p.blank_m, p.latency_jitter_ms
    );

    p.created_unix = now();
    Ok(p)
}
