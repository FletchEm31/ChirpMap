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

use crate::audio::{build_input, dev_name, quiet, select};
use crate::dsp::{self, argmax, matched_envelope, resample, Background, RangeProfile, SPEED_OF_SOUND};
use crate::layout::{dist, Layout};
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub trait ArrayIo {
    fn fs(&self) -> u32;
    fn describe(&self) -> String;
    /// Play `sig` on speaker `spk` (layout order) and return one recording per
    /// mic (layout order), all at `fs()`.
    fn ping(&mut self, spk: usize, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>>;
    /// Every speaker plays its own code (`sigs`, layout order) at the same
    /// moment; one recording per mic.
    fn ping_all(&mut self, sigs: &[Vec<f32>], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>>;
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
        if spk >= self.outs.len() {
            bail!("no such speaker");
        }
        self.play_and_record(&[(spk, sig)], listen_s, volume)
    }

    fn ping_all(&mut self, sigs: &[Vec<f32>], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>> {
        let parts: Vec<(usize, &[f32])> = sigs.iter().enumerate().take(self.outs.len()).map(|(k, s)| (k, s.as_slice())).collect();
        self.play_and_record(&parts, listen_s, volume)
    }
}

impl CpalArray {
    /// Play each (speaker, signal) at the same moment and record every mic.
    /// Speakers on the same device (e.g. receiver L and R) share one stream.
    fn play_and_record(&mut self, parts: &[(usize, &[f32])], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>> {
        // Group speakers by device.
        let mut groups: Vec<(usize, Vec<(Option<usize>, Vec<f32>)>)> = Vec::new();
        for &(k, sig) in parts {
            let out = &self.outs[k];
            let scaled: Vec<f32> = sig.iter().map(|x| x * volume).collect();
            let key = self.outs.iter().position(|o| o.pw == out.pw && dev_name(&o.dev) == dev_name(&out.dev)).unwrap_or(k);
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => g.1.push((out.only, scaled)),
                None => groups.push((key, vec![(out.only, scaled)])),
            }
        }

        // Mics: devices picked by name record through pw-record; the default via cpal.
        let bufs: Vec<Arc<Mutex<Vec<f32>>>> = self.ins.iter().map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
        let mut pw_recs: Vec<Option<crate::pwio::Recording>> = Vec::new();
        for (k, p) in self.ins.iter().enumerate() {
            pw_recs.push(match &p.pw {
                Some(node) => Some(crate::pwio::record(node, self.fs, k)?),
                None => None,
            });
        }
        let ins: Vec<Stream> = quiet(|| -> Result<Vec<Stream>> {
            let mut v = Vec::new();
            for (p, b) in self.ins.iter().zip(&bufs) {
                if p.pw.is_none() {
                    let s = build_input(&p.dev, &p.cfg, p.fmt, b.clone())?;
                    s.play()?;
                    v.push(s);
                }
            }
            Ok(v)
        })?;
        let warm = if pw_recs.iter().any(Option::is_some) { crate::pwio::RECORD_WARMUP } else { Duration::from_millis(60) };
        std::thread::sleep(warm);

        // Start every device, then wait for all of them.
        let mut players = Vec::new();
        let mut streams = Vec::new();
        let mut longest = 0.0f32;
        for (gi, (key, sigs)) in groups.iter().enumerate() {
            let out = &self.outs[*key];
            let ch = out.cfg.channels as usize;
            match &out.pw {
                Some(node) => {
                    let refs: Vec<(Option<usize>, &[f32])> = sigs.iter().map(|(o, s)| (*o, s.as_slice())).collect();
                    let frames = crate::audio::interleave(ch, &refs);
                    players.push(crate::pwio::play_frames(node, self.fs, ch as u16, &frames, gi)?);
                }
                None => {
                    let r = out.rate;
                    let pad = vec![0.0f32; (0.05 * r as f32) as usize];
                    let res: Vec<Vec<f32>> = sigs
                        .iter()
                        .map(|(_, s)| {
                            let mut v = pad.clone();
                            v.extend(resample(s, self.fs, r));
                            v.extend(std::iter::repeat(0.0).take((listen_s * r as f32) as usize));
                            v
                        })
                        .collect();
                    let refs: Vec<(Option<usize>, &[f32])> = sigs.iter().zip(&res).map(|((o, _), v)| (*o, v.as_slice())).collect();
                    let frames = crate::audio::interleave(ch, &refs);
                    longest = longest.max(frames.len() as f32 / ch.max(1) as f32 / r as f32);
                    let st = quiet(|| -> Result<Stream> {
                        let s = crate::audio::build_output_frames(&out.dev, &out.cfg, out.fmt, Arc::new(frames), Arc::new(AtomicUsize::new(0)))?;
                        s.play()?;
                        Ok(s)
                    })?;
                    streams.push(st);
                }
            }
        }
        for p in players {
            crate::pwio::wait_player(p)?;
        }
        std::thread::sleep(Duration::from_secs_f32(longest.max(listen_s) + 0.3));
        drop(streams);
        std::thread::sleep(Duration::from_millis(20));
        drop(ins);

        let mut recs = Vec::new();
        for ((p, b), pr) in self.ins.iter().zip(&bufs).zip(pw_recs) {
            let rec = match pr {
                Some(r) => {
                    let (x, rate) = r.stop()?;
                    resample(&x, rate.max(1), self.fs)
                }
                None => {
                    let x = b.lock().map(|g| g.clone()).unwrap_or_default();
                    resample(&x, p.rate, self.fs)
                }
            };
            if rec.is_empty() {
                bail!("a microphone recorded nothing ({}) — is it muted or in use?", p.pw.clone().unwrap_or_else(|| dev_name(&p.dev)));
            }
            recs.push(rec);
        }
        Ok(recs)
    }
}

// ------------------------------------------------------------------ simulator

/// The user's layout as a perfect rectangle (first-order wall reflections via
/// image sources) plus people walking slow loops. Each mic gets its own random
/// driver latency, so the geometry anchoring is exercised for real.
pub struct SimArray {
    l: Layout,
    fs: u32,
    pub rng: u64,
    pub pings: u64,
    pub mic_lat: Vec<usize>,
    /// How many people are walking (0 = empty room).
    pub walkers: usize,
    /// The room's true speed of sound (to exercise the self-calibration).
    pub c: f32,
}

impl SimArray {
    pub fn new(l: Layout, walkers: usize) -> Self {
        let n = l.mics().len();
        let mut s = SimArray { l, fs: 48_000, rng: 0x51D3_A11C_E5EE_D001, pings: 0, mic_lat: vec![0; n], walkers, c: SPEED_OF_SOUND };
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

    /// Where simulated person `k` is right now (slow loops, the second one the other way round).
    pub fn walker_pos(&self, k: usize) -> (f32, f32) {
        let t = self.pings as f32 * 0.05;
        let (w, d) = (self.l.width_m, self.l.depth_m);
        if k == 0 {
            (w * (0.5 + 0.28 * t.cos()), d * (0.45 + 0.25 * t.sin()))
        } else {
            let u = -t * 0.8 + 2.2;
            (w * (0.5 + 0.3 * u.cos()), d * (0.55 + 0.28 * u.sin()))
        }
    }

    /// Mic recordings for speakers (position, chirp) all sounding at once.
    fn render(&mut self, sources: &[((f32, f32), &[f32])], listen_s: f32, volume: f32) -> Vec<Vec<f32>> {
        let (w, d) = (self.l.width_m, self.l.depth_m);
        let people: Vec<(f32, f32)> = (0..self.walkers).map(|k| self.walker_pos(k)).collect();
        let mics: Vec<(f32, f32)> = self.l.mics().iter().map(|n| n.pos()).collect();
        let (fs, c) = (self.fs, self.c);
        let longest = sources.iter().map(|s| s.1.len()).max().unwrap_or(0);
        let mut out = Vec::new();
        for (i, m) in mics.iter().enumerate() {
            let jitter = (self.rand().abs() * 120.0) as usize;
            let lat = self.mic_lat[i] + jitter;
            let len = lat + longest + (listen_s * fs as f32) as usize + 64;
            let mut rec = vec![0.0f32; len];
            for &(s, sig) in sources {
                let add = |path: f32, amp: f32, rec: &mut Vec<f32>| {
                    let at = lat + (path / c * fs as f32).round() as usize;
                    for (k, &v) in sig.iter().enumerate() {
                        if let Some(r) = rec.get_mut(at + k) {
                            *r += v * amp * volume;
                        }
                    }
                };
                let dd = dist(s, *m);
                add(dd, 1.0 / (0.3 + dd), &mut rec);
                for im in [(-s.0, s.1), (2.0 * w - s.0, s.1), (s.0, -s.1), (s.0, 2.0 * d - s.1)] {
                    let p = dist(im, *m);
                    add(p, 0.35 / (0.3 + p), &mut rec);
                }
                for t in &people {
                    let p = dist(s, *t) + dist(*t, *m);
                    add(p, 0.6 / (0.3 + p), &mut rec);
                }
            }
            for v in rec.iter_mut() {
                *v += 0.002 * self.rand();
            }
            out.push(rec);
        }
        self.pings += 1;
        if !cfg!(test) {
            std::thread::sleep(Duration::from_millis(60)); // real-time-ish pacing
        }
        out
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
            match self.walkers {
                0 => String::new(),
                1 => " · someone walking".into(),
                n => format!(" · {n} people walking"),
            }
        )
    }

    fn ping(&mut self, spk: usize, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>> {
        let s = self.l.speakers().get(spk).map(|n| n.pos()).context("no such speaker")?;
        Ok(self.render(&[(s, sig)], listen_s, volume))
    }

    fn ping_all(&mut self, sigs: &[Vec<f32>], listen_s: f32, volume: f32) -> Result<Vec<Vec<f32>>> {
        let pos: Vec<(f32, f32)> = self.l.speakers().iter().map(|n| n.pos()).collect();
        let sources: Vec<((f32, f32), &[f32])> = pos.into_iter().zip(sigs.iter().map(|v| v.as_slice())).collect();
        Ok(self.render(&sources, listen_s, volume))
    }
}

// ------------------------------------------------------------------ pairs & imaging

/// One speaker→mic pair's echo envelope, normalised to the direct arrival and
/// re-timed so index 0 IS the direct arrival. An echo at index k travelled
/// k/fs · c metres further than the direct path, so the speed of sound can be
/// refined later without re-timing anything.
#[derive(Clone, Debug)]
pub struct PairEcho {
    pub s: (f32, f32),
    pub m: (f32, f32),
    pub direct_m: f32,
    pub env: Vec<f32>,
}

pub fn pair_from_recording(rec: &[f32], tpl: &[f32], s: (f32, f32), m: (f32, f32)) -> Option<PairEcho> {
    let env = matched_envelope(rec, tpl);
    if env.is_empty() {
        return None;
    }
    let pk = argmax(&env);
    let peak = env[pk].max(1e-12);
    Some(PairEcho { s, m, direct_m: dist(s, m), env: env[pk..].iter().map(|v| v / peak).collect() })
}

/// How the speakers are driven for one frame.
#[derive(Clone)]
pub enum Codes {
    /// One shared chirp; speakers take turns.
    Sequential(Vec<f32>),
    /// One code per speaker, all played at once.
    Simultaneous(Vec<Vec<f32>>),
}

impl Codes {
    pub fn for_layout(l: &Layout, fs: u32, f0: f32, f1: f32, simultaneous: bool) -> Self {
        let n = l.speakers().len();
        if simultaneous && n > 1 {
            if let Some(v) = crate::settings::speaker_codes(fs, f0, f1, n, 0.01) {
                return Codes::Simultaneous(v);
            }
        }
        Codes::Sequential(dsp::chirp(fs, 0.01, f0, f1))
    }

    pub fn is_simultaneous(&self) -> bool {
        matches!(self, Codes::Simultaneous(_))
    }

    /// How much to widen echoes before pairs vote (narrow-band codes already
    /// have wider echoes).
    pub fn dilate_m(&self) -> f32 {
        if self.is_simultaneous() {
            0.03
        } else {
            0.06
        }
    }
}

/// One frame: every speaker → every mic pair; `avg` frames averaged.
pub fn measure(io: &mut dyn ArrayIo, l: &Layout, codes: &Codes, volume: f32, avg: usize) -> Result<Vec<PairEcho>> {
    let reach = l.width_m.hypot(l.depth_m) * 2.0 + 0.5;
    let listen = reach / SPEED_OF_SOUND + 0.05;
    let spk: Vec<(f32, f32)> = l.speakers().iter().map(|n| n.pos()).collect();
    let mic: Vec<(f32, f32)> = l.mics().iter().map(|n| n.pos()).collect();
    let mut acc: Vec<Option<PairEcho>> = vec![None; spk.len() * mic.len()];
    let add = |k: usize, i: usize, p: PairEcho, acc: &mut Vec<Option<PairEcho>>| {
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
    };
    for _ in 0..avg.max(1) {
        match codes {
            Codes::Sequential(tpl) => {
                for (k, s) in spk.iter().enumerate() {
                    let recs = io.ping(k, tpl, listen, volume)?;
                    for (i, (rec, m)) in recs.iter().zip(&mic).enumerate() {
                        if let Some(p) = pair_from_recording(rec, tpl, *s, *m) {
                            add(k, i, p, &mut acc);
                        }
                    }
                    if !cfg!(test) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
            Codes::Simultaneous(tpls) => {
                let recs = io.ping_all(tpls, listen, volume)?;
                for (k, (s, tpl)) in spk.iter().zip(tpls).enumerate() {
                    for (i, (rec, m)) in recs.iter().zip(&mic).enumerate() {
                        if let Some(p) = pair_from_recording(rec, tpl, *s, *m) {
                            add(k, i, p, &mut acc);
                        }
                    }
                }
            }
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

    /// Each pair's |now − background| and that pair's own detection threshold.
    pub fn subtract(&self, pairs: &[PairEcho], fs: u32, max_m: f32) -> (Vec<PairEcho>, Vec<f32>) {
        let mut th = Vec::new();
        let out: Vec<PairEcho> = pairs
            .iter()
            .zip(&self.per_pair)
            .map(|(p, b)| {
                th.push(b.threshold(fs, 0.3, max_m));
                PairEcho { env: b.subtract(&as_profile(p, fs)).env, ..p.clone() }
            })
            .collect();
        (out, th)
    }
}

/// Average several frames of the same pairs (e.g. the background capture).
pub fn average_frames(frames: &[Vec<PairEcho>]) -> Vec<PairEcho> {
    let Some(first) = frames.first() else { return Vec::new() };
    first
        .iter()
        .enumerate()
        .map(|(i, p0)| {
            let n = frames.iter().filter_map(|f| f.get(i)).map(|p| p.env.len()).min().unwrap_or(0);
            let mut env = vec![0.0f32; n];
            let mut cnt = 0.0f32;
            for f in frames {
                if let Some(p) = f.get(i) {
                    for j in 0..n {
                        env[j] += p.env[j];
                    }
                    cnt += 1.0;
                }
            }
            env.iter_mut().for_each(|v| *v /= cnt.max(1.0));
            PairEcho { env, ..p0.clone() }
        })
        .collect()
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

/// How pairs are combined into one picture.
pub enum Fusion<'a> {
    /// Average of all pairs (good for showing every reflector).
    Sum,
    /// Pairs must agree: geometric mean of each pair's evidence relative to its
    /// own threshold. One pair alone can't light a spot, so ghosts where only
    /// some curves cross fade away. 1.0 = every pair right at threshold.
    Agree(&'a [f32]),
}

const AGREE_EPS: f32 = 0.08;

/// Paint each pair's echo energy along its ellipses and combine the pairs.
/// `guard_m`: ignore paths barely longer than the direct one (speaker ring-down).
/// `c`: speed of sound in m/s.
pub fn backproject(pairs: &[PairEcho], width_m: f32, depth_m: f32, cell: f32, fs: u32, guard_m: f32, c: f32, fusion: Fusion) -> Image {
    let nx = (width_m / cell).ceil().max(1.0) as usize;
    let ny = (depth_m / cell).ceil().max(1.0) as usize;
    let mut v = vec![0.0f32; nx * ny];
    let per_m = fs as f32 / c;
    let need = pairs.len().clamp(1, 2);
    if !pairs.is_empty() {
        for j in 0..ny {
            for i in 0..nx {
                let p = ((i as f32 + 0.5) * cell, (j as f32 + 0.5) * cell);
                let (mut acc, mut logs, mut used) = (0.0f32, 0.0f32, 0usize);
                for (n, pr) in pairs.iter().enumerate() {
                    let excess = dist(pr.s, p) + dist(p, pr.m) - pr.direct_m;
                    if excess < guard_m {
                        continue;
                    }
                    let x = excess * per_m;
                    let k = x.floor() as usize;
                    if k + 1 >= pr.env.len() {
                        continue;
                    }
                    let f = x - k as f32;
                    let e = pr.env[k] * (1.0 - f) + pr.env[k + 1] * f;
                    used += 1;
                    match fusion {
                        Fusion::Sum => acc += e,
                        Fusion::Agree(th) => logs += (AGREE_EPS + e / th.get(n).copied().unwrap_or(1.0).max(1e-6)).ln(),
                    }
                }
                v[j * nx + i] = match fusion {
                    Fusion::Sum => acc / pairs.len() as f32,
                    Fusion::Agree(_) if used >= need => ((logs / used as f32).exp() - AGREE_EPS).max(0.0),
                    Fusion::Agree(_) => 0.0,
                };
            }
        }
    }
    Image { nx, ny, cell, v }
}

/// Find up to `max_n` people, one at a time ("explain away"): take the
/// strongest spot every pair agrees on, remove that person's echo from every
/// pair, and look again. Spots that only lit up because of the first person's
/// curves vanish with them, so they can't come back as ghosts.
/// Returns the first (full) picture and the people found as (x, y, score).
pub fn detect_people(pairs: &[PairEcho], th: &[f32], width_m: f32, depth_m: f32, cell: f32, fs: u32, guard_m: f32, c: f32, dilate_m: f32, max_n: usize) -> (Image, Vec<(f32, f32, f32)>) {
    const MIN_SCORE: f32 = 1.0;
    const REMOVE_M: f32 = 0.12; // ± path length cleared around a found person's echo
    // Widen each echo slightly (a body is ~30 cm deep and the layout is
    // measured by hand) so pairs that are a few cm off still agree.
    let r = (dilate_m * fs as f32 / c).round() as usize;
    let pairs: Vec<PairEcho> = pairs
        .iter()
        .map(|p| {
            let n = p.env.len();
            let env = (0..n).map(|i| p.env[i.saturating_sub(r)..(i + r + 1).min(n)].iter().cloned().fold(0.0, f32::max)).collect();
            PairEcho { env, ..p.clone() }
        })
        .collect();
    let pairs = &pairs[..];
    let first = backproject(pairs, width_m, depth_m, cell, fs, guard_m, c, Fusion::Agree(th));
    let mut found = Vec::new();
    let mut work: Vec<PairEcho> = pairs.to_vec();
    let mut img = first.clone();
    let mut first_score = 0.0;
    for _ in 0..max_n {
        let (x, y, v) = img.peak();
        if v < MIN_SCORE || (first_score > 0.0 && v < 0.3 * first_score) {
            break;
        }
        if found.is_empty() {
            first_score = v;
        }
        found.push((x, y, v));
        let per_m = fs as f32 / c;
        for p in &mut work {
            let e = dist(p.s, (x, y)) + dist((x, y), p.m) - p.direct_m;
            let lo = ((e - REMOVE_M) * per_m).floor().max(0.0) as usize;
            let hi = (((e + REMOVE_M) * per_m).ceil() as usize).min(p.env.len());
            for v in &mut p.env[lo.min(hi)..hi] {
                *v = 0.0;
            }
        }
        img = backproject(&work, width_m, depth_m, cell, fs, guard_m, c, Fusion::Agree(th));
    }
    (first, found)
}

/// Fit the room's speed of sound: try values and keep the one that lines up
/// every pair's echoes with the wall reflections the layout predicts.
/// Returns (c in m/s, confidence ≥ 1; higher = clearer).
pub fn fit_speed_of_sound(pairs: &[PairEcho], l: &Layout, fs: u32) -> Option<(f32, f32)> {
    let (w, d) = (l.width_m, l.depth_m);
    let mut excess: Vec<(usize, f32)> = Vec::new();
    for (n, p) in pairs.iter().enumerate() {
        let s = p.s;
        for im in [(-s.0, s.1), (2.0 * w - s.0, s.1), (s.0, -s.1), (s.0, 2.0 * d - s.1)] {
            let e = dist(im, p.m) - p.direct_m;
            if e > 0.4 {
                excess.push((n, e));
            }
        }
    }
    if excess.len() < 3 {
        return None;
    }
    let score = |c: f32| -> f32 {
        let per_m = fs as f32 / c;
        excess
            .iter()
            .map(|&(n, e)| {
                let env = &pairs[n].env;
                let x = e * per_m;
                let k = x.round() as usize;
                // best value within ±1 sample (envelope peak is a few samples wide)
                (k.saturating_sub(1)..=k + 1).filter_map(|i| env.get(i)).cloned().fold(0.0, f32::max)
            })
            .sum()
    };
    let cands: Vec<(f32, f32)> = (0..=140).map(|i| 325.0 + i as f32 * 0.25).map(|c| (c, score(c))).collect();
    let best = cands.iter().cloned().fold((0.0, f32::MIN), |a, b| if b.1 > a.1 { b } else { a });
    let mut sorted: Vec<f32> = cands.iter().map(|c| c.1).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2].max(1e-9);
    let conf = best.1 / median;
    (conf > 1.15).then_some((best.0, conf))
}

/// Air temperature implied by a speed of sound (dry air).
pub fn temperature_c(c: f32) -> f32 {
    273.15 * ((c / 331.3).powi(2) - 1.0)
}

/// The chirp band and volume an array run uses: the calibrated band (or the
/// silent band in silent mode).
pub fn band_and_volume(fs: u32) -> (f32, f32, f32) {
    let (f0, f1, vol) = crate::audio::CpalSonar::open(None, None)
        .ok()
        .and_then(|s| {
            use crate::audio::Sonar;
            crate::calib::load(&s.name()).filter(|p| p.fs == fs).map(|p| (p.f0, p.f1, p.volume))
        })
        .unwrap_or((3000.0, (0.45 * fs as f32).min(16_000.0), 0.7));
    let (f0, f1) = crate::settings::band(fs, f0, f1);
    (f0, f1, vol)
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
        let tpl = Codes::Sequential(dsp::chirp(fs, 0.01, 3000.0, 16_000.0));
        // Empty room first.
        let mut still = SimArray::new(l.clone(), 0);
        let frames: Vec<Vec<PairEcho>> = (0..4).map(|_| measure(&mut still, &l, &tpl, 0.7, 1).unwrap()).collect();
        assert_eq!(frames[0].len(), 4, "2 speakers × 2 mics = 4 pairs");
        let bg = ArrayBackground::capture(&frames, fs);
        // Then someone walks in.
        let mut sim = SimArray::new(l.clone(), 1);
        sim.rng = still.rng;
        sim.mic_lat = still.mic_lat.clone();
        sim.pings = 37;
        let truth = sim.walker_pos(0);
        let pairs = measure(&mut sim, &l, &tpl, 0.7, 1).unwrap();
        let (diff, _) = bg.subtract(&pairs, fs, 10.0);
        let img = backproject(&diff, l.width_m, l.depth_m, 0.05, fs, 0.3, SPEED_OF_SOUND, Fusion::Sum);
        let (x, y, _) = img.peak();
        let err = dist((x, y), truth);
        assert!(err < 0.2, "found ({x:.2},{y:.2}), truth ({:.2},{:.2}), err {err:.2} m", truth.0, truth.1);
    }
}
