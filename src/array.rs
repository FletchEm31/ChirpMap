//! Multi-speaker / multi-mic arrays and 2D imaging.
//!
//! Every speaker→mic pair turns an echo delay into a total path length
//! speaker→object→mic, which on the floor plan is an ellipse with the speaker
//! and mic at its foci. Back-projection paints every pair's echo energy along
//! its ellipses; where several pairs agree, a real object lights up. More
//! speakers and mics = more pairs = a sharper, more confident picture.
//!
//! Each input device runs on its own clock, so pairs are anchored by
//! geometry: the direct speaker→mic arrival is found in the recording, and the
//! known speaker–mic distance from the room layout says when the chirp left.

use crate::audio::{build_input, build_output, dev_name, quiet, select};
use crate::dsp::{self, argmax, matched_envelope, resample, Background, RangeProfile, SPEED_OF_SOUND};
use crate::layout::{dist, Layout};
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Seconds → samples for a full path length (not a round trip).
fn path_samples(m: f32, fs: u32) -> f32 {
    m / SPEED_OF_SOUND * fs as f32
}

pub trait ArrayIo {
    fn fs(&self) -> u32;
    fn describe(&self) -> String;
    /// Play `sig` on speaker `spk` (layout order) and return one recording per
    /// mic (layout order), all at `fs()`.
    fn ping(&mut self, spk: usize, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>>;
}

// ------------------------------------------------------------------ real hardware

struct Port {
    dev: Device,
    cfg: StreamConfig,
    fmt: SampleFormat,
    rate: u32,
    /// Speakers: play on just this channel (None = all).
    only: Option<usize>,
    /// PipeWire node this stream must be routed to (devices picked by real name).
    pw: Option<String>,
}

pub struct CpalArray {
    outs: Vec<Port>,
    ins: Vec<Port>,
    fs: u32,
}

fn sel(device: &str) -> Option<&str> {
    crate::audio::resolve_device(device).0
}

fn pw_of(device: &str) -> Option<String> {
    crate::audio::resolve_device(device).1.map(str::to_string)
}

impl CpalArray {
    pub fn open(l: &Layout) -> Result<Self> {
        quiet(|| Self::open_inner(l))
    }

    fn open_inner(l: &Layout) -> Result<Self> {
        if !l.is_usable() {
            bail!("the room layout needs at least one speaker and one mic (Room designer)");
        }
        let host = cpal::default_host();
        let mut outs = Vec::new();
        for n in l.speakers() {
            let dev = select(host.output_devices()?.collect(), host.default_output_device(), sel(&n.device), "output")
                .with_context(|| format!("{}: speaker '{}'", n.label, n.device))?;
            let c = dev.default_output_config()?;
            let only = n.channel.map(|c| c as usize);
            if let Some(ch) = only {
                if ch >= c.channels() as usize {
                    bail!("{}: '{}' has {} channel(s), no channel {}", n.label, n.device, c.channels(), ch + 1);
                }
            }
            outs.push(Port { rate: c.sample_rate().0, fmt: c.sample_format(), cfg: c.into(), dev, only, pw: pw_of(&n.device) });
        }
        let fs = outs[0].rate;
        let mut ins = Vec::new();
        for n in l.mics() {
            let dev = select(host.input_devices()?.collect(), host.default_input_device(), sel(&n.device), "input")
                .with_context(|| format!("{}: mic '{}'", n.label, n.device))?;
            let c = dev.default_input_config()?;
            let mut p = Port { rate: c.sample_rate().0, fmt: c.sample_format(), cfg: c.into(), dev, only: None, pw: pw_of(&n.device) };
            if p.rate != fs {
                if let Ok(ranges) = p.dev.supported_input_configs() {
                    if let Some(r) = ranges
                        .filter(|r| r.min_sample_rate().0 <= fs && r.max_sample_rate().0 >= fs)
                        .max_by_key(|r| (r.sample_format() == SampleFormat::F32) as u8)
                    {
                        p.fmt = r.sample_format();
                        p.cfg = r.with_sample_rate(cpal::SampleRate(fs)).into();
                        p.rate = fs;
                    }
                }
            }
            ins.push(p);
        }
        Ok(Self { outs, ins, fs })
    }
}

impl ArrayIo for CpalArray {
    fn fs(&self) -> u32 {
        self.fs
    }

    fn describe(&self) -> String {
        let o: Vec<String> = self.outs.iter().map(|p| dev_name(&p.dev)).collect();
        let i: Vec<String> = self.ins.iter().map(|p| dev_name(&p.dev)).collect();
        format!("{} speaker(s): {} · {} mic(s): {}", o.len(), o.join(", "), i.len(), i.join(", "))
    }

    fn ping(&mut self, spk: usize, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>> {
        let out = self.outs.get(spk).context("no such speaker")?;
        let bufs: Vec<Arc<Mutex<Vec<f32>>>> = self.ins.iter().map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
        let ins: Vec<Stream> = quiet(|| -> Result<Vec<Stream>> {
            let mut v = Vec::new();
            for (p, b) in self.ins.iter().zip(&bufs) {
                crate::audio::route_next_stream(p.pw.as_deref());
                let s = build_input(&p.dev, &p.cfg, p.fmt, b.clone())?;
                s.play()?;
                v.push(s);
            }
            Ok(v)
        })?;
        std::thread::sleep(Duration::from_millis(60));

        let r = out.rate as f32;
        let scaled: Vec<f32> = sig.iter().map(|x| x * volume).collect();
        let mut play = vec![0.0f32; (0.05 * r) as usize];
        play.extend(resample(&scaled, self.fs, out.rate));
        play.extend(std::iter::repeat(0.0).take((listen_s * r) as usize));
        let dur = play.len() as f32 / r + 0.3;
        let outs = quiet(|| -> Result<Stream> {
            crate::audio::route_next_stream(out.pw.as_deref());
            let s = build_output(&out.dev, &out.cfg, out.fmt, Arc::new(play), Arc::new(AtomicUsize::new(0)), out.only)?;
            s.play()?;
            Ok(s)
        })?;
        std::thread::sleep(Duration::from_secs_f32(dur));
        drop(outs);
        std::thread::sleep(Duration::from_millis(20));
        drop(ins);

        let mut recs = Vec::new();
        for (p, b) in self.ins.iter().zip(&bufs) {
            let rec = b.lock().map(|g| g.clone()).unwrap_or_default();
            if rec.is_empty() {
                bail!("a microphone recorded nothing ({}) — is it muted or in use?", dev_name(&p.dev));
            }
            recs.push(resample(&rec, p.rate, self.fs));
        }
        Ok(recs)
    }
}

// ------------------------------------------------------------------ simulator

/// The user's layout as a perfect rectangle (first-order wall reflections via
/// image sources) plus one person walking a slow loop. Each mic gets its own
/// random driver latency, so the geometry anchoring is exercised for real.
pub struct SimArray {
    l: Layout,
    fs: u32,
    rng: u64,
    pings: u64,
    mic_lat: Vec<usize>,
    pub walker: bool,
}

impl SimArray {
    pub fn new(l: Layout, walker: bool) -> Self {
        let n = l.mics().len();
        let mut s = SimArray { l, fs: 48_000, rng: 0x51D3_A11C_E5EE_D001, pings: 0, mic_lat: vec![0; n], walker };
        for i in 0..n {
            s.mic_lat[i] = 400 + (s.rand().abs() * 2400.0) as usize;
        }
        s
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let v = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }

    /// Where the simulated person is right now (a slow ellipse round the room).
    pub fn walker_pos(&self) -> (f32, f32) {
        let t = self.pings as f32 * 0.05;
        (self.l.width_m * (0.5 + 0.28 * t.cos()), self.l.depth_m * (0.45 + 0.25 * t.sin()))
    }
}

impl ArrayIo for SimArray {
    fn fs(&self) -> u32 {
        self.fs
    }

    fn describe(&self) -> String {
        format!(
            "simulated {:.1} × {:.1} m room · {} speaker(s), {} mic(s){}",
            self.l.width_m,
            self.l.depth_m,
            self.l.speakers().len(),
            self.l.mics().len(),
            if self.walker { " · someone walking" } else { "" }
        )
    }

    fn ping(&mut self, spk: usize, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>> {
        let s = self.l.speakers().get(spk).map(|n| n.pos()).context("no such speaker")?;
        let (w, d) = (self.l.width_m, self.l.depth_m);
        let images = [(-s.0, s.1), (2.0 * w - s.0, s.1), (s.0, -s.1), (s.0, 2.0 * d - s.1)];
        let target = self.walker.then(|| self.walker_pos());
        let mics: Vec<(f32, f32)> = self.l.mics().iter().map(|n| n.pos()).collect();
        let fs = self.fs;
        let mut out = Vec::new();
        for (i, m) in mics.iter().enumerate() {
            let jitter = (self.rand().abs() * 120.0) as usize;
            let lat = self.mic_lat[i] + jitter;
            let len = lat + sig.len() + (listen_s * fs as f32) as usize + 64;
            let mut rec = vec![0.0f32; len];
            let add = |path: f32, amp: f32, rec: &mut Vec<f32>| {
                let at = lat + path_samples(path, fs).round() as usize;
                for (k, &v) in sig.iter().enumerate() {
                    if let Some(r) = rec.get_mut(at + k) {
                        *r += v * amp * volume;
                    }
                }
            };
            let dd = dist(s, *m);
            add(dd, 1.0 / (0.3 + dd), &mut rec);
            for im in images {
                let p = dist(im, *m);
                add(p, 0.35 / (0.3 + p), &mut rec);
            }
            if let Some(t) = target {
                let p = dist(s, t) + dist(t, *m);
                add(p, 0.6 / (0.3 + p), &mut rec);
            }
            for v in rec.iter_mut() {
                *v += 0.002 * self.rand();
            }
            out.push(rec);
        }
        self.pings += 1;
        std::thread::sleep(Duration::from_millis(60)); // real-time-ish pacing
        Ok(out)
    }
}

// ------------------------------------------------------------------ pairs & imaging

/// One speaker→mic pair's echo envelope, re-timed so index 0 is the instant the
/// chirp left the speaker and normalised to the direct arrival.
#[derive(Clone, Debug)]
pub struct PairEcho {
    pub s: (f32, f32),
    pub m: (f32, f32),
    pub direct_m: f32,
    pub env: Vec<f32>,
}

pub fn pair_from_recording(rec: &[f32], tpl: &[f32], s: (f32, f32), m: (f32, f32), fs: u32) -> Option<PairEcho> {
    let env = matched_envelope(rec, tpl);
    if env.is_empty() {
        return None;
    }
    let pk = argmax(&env);
    let peak = env[pk].max(1e-12);
    let direct_m = dist(s, m);
    let lag = path_samples(direct_m, fs).round() as usize;
    let t0 = pk.checked_sub(lag)?;
    Some(PairEcho { s, m, direct_m, env: env[t0..].iter().map(|v| v / peak).collect() })
}

/// One full sweep: every speaker pings in turn, every mic listens; `avg` sweeps averaged.
pub fn measure(io: &mut dyn ArrayIo, l: &Layout, tpl: &[f32], volume: f32, avg: usize) -> Result<Vec<PairEcho>> {
    let fs = io.fs();
    let reach = l.width_m.hypot(l.depth_m) * 2.0 + 0.5;
    let listen = reach / SPEED_OF_SOUND + 0.05;
    let spk: Vec<(f32, f32)> = l.speakers().iter().map(|n| n.pos()).collect();
    let mic: Vec<(f32, f32)> = l.mics().iter().map(|n| n.pos()).collect();
    let mut acc: Vec<Option<PairEcho>> = vec![None; spk.len() * mic.len()];
    for _ in 0..avg.max(1) {
        for (k, s) in spk.iter().enumerate() {
            let recs = io.ping(k, tpl, listen, volume)?;
            for (i, (rec, m)) in recs.iter().zip(&mic).enumerate() {
                let Some(p) = pair_from_recording(rec, tpl, *s, *m, fs) else { continue };
                let slot = &mut acc[k * mic.len() + i];
                match slot {
                    None => *slot = Some(p),
                    Some(a) => {
                        let n = a.env.len().min(p.env.len());
                        a.env.truncate(n);
                        for j in 0..n {
                            a.env[j] += p.env[j];
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let n = avg.max(1) as f32;
    Ok(acc
        .into_iter()
        .flatten()
        .map(|mut p| {
            p.env.iter_mut().for_each(|v| *v /= n);
            p
        })
        .collect())
}

fn as_profile(p: &PairEcho, fs: u32) -> RangeProfile {
    RangeProfile { fs, env: p.env.clone(), direct_peak: 1.0, direct_idx: Vec::new() }
}

/// Per-pair static scene, for "show only what moves".
pub struct ArrayBackground {
    per_pair: Vec<Background>,
}

impl ArrayBackground {
    pub fn capture(frames: &[Vec<PairEcho>], fs: u32) -> Self {
        let n = frames.first().map_or(0, |f| f.len());
        let per_pair = (0..n)
            .map(|i| {
                let shots: Vec<RangeProfile> = frames.iter().filter_map(|f| f.get(i)).map(|p| as_profile(p, fs)).collect();
                Background::capture(&shots)
            })
            .collect();
        Self { per_pair }
    }

    /// Each pair's |now − background|, plus the detection threshold (average of pairs).
    pub fn subtract(&self, pairs: &[PairEcho], fs: u32, max_m: f32) -> (Vec<PairEcho>, f32) {
        let mut th = 0.0;
        let out: Vec<PairEcho> = pairs
            .iter()
            .zip(&self.per_pair)
            .map(|(p, b)| {
                th += b.threshold(fs, 0.3, max_m);
                PairEcho { env: b.subtract(&as_profile(p, fs)).env, ..p.clone() }
            })
            .collect();
        let n = out.len().max(1) as f32;
        (out, th / n)
    }
}

#[derive(Clone, Debug)]
pub struct Image {
    pub nx: usize,
    pub ny: usize,
    pub cell: f32,
    /// Row-major from the bottom-left: index j*nx + i, cell centre ((i+.5)·cell, (j+.5)·cell).
    pub v: Vec<f32>,
}

impl Image {
    pub fn at(&self, x: f32, y: f32) -> f32 {
        let i = ((x / self.cell) as isize).clamp(0, self.nx as isize - 1) as usize;
        let j = ((y / self.cell) as isize).clamp(0, self.ny as isize - 1) as usize;
        self.v[j * self.nx + i]
    }

    pub fn max(&self) -> f32 {
        self.v.iter().cloned().fold(0.0, f32::max)
    }

    /// Strongest cell: (x, y, value).
    pub fn peak(&self) -> (f32, f32, f32) {
        let k = argmax(&self.v);
        let (i, j) = (k % self.nx, k / self.nx);
        ((i as f32 + 0.5) * self.cell, (j as f32 + 0.5) * self.cell, self.v[k])
    }
}

/// Paint each pair's echo energy along its ellipses and average over pairs.
/// `guard_m`: ignore paths barely longer than the direct one (speaker ring-down).
pub fn backproject(pairs: &[PairEcho], width_m: f32, depth_m: f32, cell: f32, fs: u32, guard_m: f32) -> Image {
    let nx = (width_m / cell).ceil().max(1.0) as usize;
    let ny = (depth_m / cell).ceil().max(1.0) as usize;
    let mut v = vec![0.0f32; nx * ny];
    if !pairs.is_empty() {
        for j in 0..ny {
            for i in 0..nx {
                let p = ((i as f32 + 0.5) * cell, (j as f32 + 0.5) * cell);
                let mut acc = 0.0;
                for pr in pairs {
                    let path = dist(pr.s, p) + dist(p, pr.m);
                    if path - pr.direct_m < guard_m {
                        continue;
                    }
                    let x = path_samples(path, fs);
                    let k = x.floor() as usize;
                    if k + 1 < pr.env.len() {
                        let f = x - k as f32;
                        acc += pr.env[k] * (1.0 - f) + pr.env[k + 1] * f;
                    }
                }
                v[j * nx + i] = acc / pairs.len() as f32;
            }
        }
    }
    Image { nx, ny, cell, v }
}

/// The chirp an array run uses: the calibrated band when there is one.
pub fn template(fs: u32) -> (Vec<f32>, f32) {
    let (f0, f1, vol) = crate::audio::CpalSonar::open(None, None)
        .ok()
        .and_then(|s| {
            use crate::audio::Sonar;
            crate::calib::load(&s.name()).filter(|p| p.fs == fs).map(|p| (p.f0, p.f1, p.volume))
        })
        .unwrap_or((3000.0, (0.45 * fs as f32).min(16_000.0), 0.7));
    (dsp::chirp(fs, 0.01, f0, f1), vol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Kind, Node};

    fn two_by_two() -> Layout {
        let mut l = Layout::starter();
        l.width_m = 4.0;
        l.depth_m = 3.5;
        l.nodes = vec![
            Node { kind: Kind::Speaker, label: "S1".into(), device: "default".into(), x: 0.6, y: 3.2, facing_deg: 180.0, channel: None },
            Node { kind: Kind::Speaker, label: "S2".into(), device: "default".into(), x: 3.4, y: 3.2, facing_deg: 180.0, channel: None },
            Node { kind: Kind::Mic, label: "M1".into(), device: "default".into(), x: 1.4, y: 3.3, facing_deg: 180.0, channel: None },
            Node { kind: Kind::Mic, label: "M2".into(), device: "default".into(), x: 2.6, y: 0.3, facing_deg: 0.0, channel: None },
        ];
        l
    }

    #[test]
    fn array_locates_a_walking_person_in_2d() {
        let l = two_by_two();
        let fs = 48_000;
        let tpl = dsp::chirp(fs, 0.01, 3000.0, 16_000.0);
        // Empty room first.
        let mut still = SimArray::new(l.clone(), false);
        let frames: Vec<Vec<PairEcho>> = (0..4).map(|_| measure(&mut still, &l, &tpl, 0.7, 1).unwrap()).collect();
        assert_eq!(frames[0].len(), 4, "2 speakers × 2 mics = 4 pairs");
        let bg = ArrayBackground::capture(&frames, fs);
        // Then someone walks in.
        let mut sim = SimArray::new(l.clone(), true);
        sim.rng = still.rng;
        sim.mic_lat = still.mic_lat.clone();
        sim.pings = 37;
        let truth = sim.walker_pos();
        let pairs = measure(&mut sim, &l, &tpl, 0.7, 1).unwrap();
        let (diff, _) = bg.subtract(&pairs, fs, 10.0);
        let img = backproject(&diff, l.width_m, l.depth_m, 0.05, fs, 0.3);
        let (x, y, _) = img.peak();
        let err = dist((x, y), truth);
        assert!(err < 0.2, "found ({x:.2},{y:.2}), truth ({:.2},{:.2}), err {err:.2} m", truth.0, truth.1);
    }
}
