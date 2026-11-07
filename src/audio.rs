//! Audio I/O: the real speaker/mic backend (cpal), a simulated room for testing
//! without hardware, and the hardware panel.

use crate::dsp::{m_to_samples, resample, SPEED_OF_SOUND};
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Anything that can emit a sound and record what comes back.
pub trait Sonar {
    fn fs(&self) -> u32;
    fn name(&self) -> String;
    /// Play `sig` (scaled by `volume`) followed by `listen_s` of silence while
    /// recording the mic. Returns mono samples at `fs()`.
    fn playrec(&mut self, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<f32>>;
    /// Record the mic only (room-noise measurement).
    fn record(&mut self, secs: f32) -> Result<Vec<f32>>;
    /// Tell the backend where the device is and which way it faces.
    /// Real hardware ignores this (the user physically moves); the simulator uses it.
    fn aim(&mut self, _pos: (f32, f32), _heading_deg: f32) {}
    fn is_sim(&self) -> bool {
        false
    }
}

/// Run `f` with stderr pointed at /dev/null. ALSA prints dozens of harmless
/// "unable to open slave" lines while probing virtual devices; this hides them.
/// Set CHIRPMAP_ALSA_DEBUG=1 to see them.
#[cfg(unix)]
pub fn quiet<R>(f: impl FnOnce() -> R) -> R {
    if std::env::var_os("CHIRPMAP_ALSA_DEBUG").is_some() {
        return f();
    }
    // SAFETY: plain fd juggling on stderr; restored before returning.
    unsafe {
        let devnull = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_WRONLY);
        if devnull < 0 {
            return f();
        }
        let saved = libc::dup(2);
        libc::dup2(devnull, 2);
        libc::close(devnull);
        let r = f();
        if saved >= 0 {
            libc::dup2(saved, 2);
            libc::close(saved);
        }
        r
    }
}

#[cfg(not(unix))]
pub fn quiet<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Save mono samples as a 32-bit float WAV (for inspecting what the mic heard).
pub fn write_wav(path: &std::path::Path, fs: u32, x: &[f32]) -> Result<()> {
    let data_len = (x.len() * 4) as u32;
    let mut b: Vec<u8> = Vec::with_capacity(44 + x.len() * 4);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&fs.to_le_bytes());
    b.extend_from_slice(&(fs * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for v in x {
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b)?;
    Ok(())
}

pub fn dev_name(d: &Device) -> String {
    d.name().unwrap_or_else(|_| "<unnamed>".into())
}

fn select(devs: Vec<Device>, default: Option<Device>, sel: Option<&str>, kind: &str) -> Result<Device> {
    match sel {
        None => default.ok_or_else(|| anyhow!("no default {kind} device found")),
        Some(s) => {
            if let Ok(i) = s.parse::<usize>() {
                return devs
                    .into_iter()
                    .nth(i)
                    .ok_or_else(|| anyhow!("no {kind} device #{i} (see `chirpmap devices`)"));
            }
            let want = s.to_lowercase();
            devs.into_iter()
                .find(|d| dev_name(d).to_lowercase().contains(&want))
                .ok_or_else(|| anyhow!("no {kind} device matching '{s}' (see `chirpmap devices`)"))
        }
    }
}

// ------------------------------------------------------------------ real hardware

pub struct CpalSonar {
    out: Device,
    inp: Device,
    out_cfg: StreamConfig,
    out_fmt: SampleFormat,
    in_cfg: StreamConfig,
    in_fmt: SampleFormat,
    fs: u32,
    in_rate: u32,
}

impl CpalSonar {
    pub fn open(output: Option<&str>, input: Option<&str>) -> Result<Self> {
        quiet(|| Self::open_inner(output, input))
    }

    fn open_inner(output: Option<&str>, input: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let out = select(
            host.output_devices()?.collect(),
            host.default_output_device(),
            output,
            "output",
        )?;
        let inp = select(
            host.input_devices()?.collect(),
            host.default_input_device(),
            input,
            "input",
        )?;

        let oc = out
            .default_output_config()
            .context("speaker has no usable output config")?;
        let fs = oc.sample_rate().0;
        let out_fmt = oc.sample_format();
        let out_cfg: StreamConfig = oc.into();

        let ic = inp
            .default_input_config()
            .context("microphone has no usable input config")?;
        let mut in_fmt = ic.sample_format();
        let mut in_cfg: StreamConfig = ic.into();
        // Prefer recording at the playback rate; otherwise resample afterwards.
        if in_cfg.sample_rate.0 != fs {
            if let Ok(ranges) = inp.supported_input_configs() {
                if let Some(r) = ranges
                    .filter(|r| r.min_sample_rate().0 <= fs && r.max_sample_rate().0 >= fs)
                    .max_by_key(|r| (r.sample_format() == SampleFormat::F32) as u8)
                {
                    in_fmt = r.sample_format();
                    in_cfg = r.with_sample_rate(cpal::SampleRate(fs)).into();
                }
            }
        }
        let in_rate = in_cfg.sample_rate.0;
        Ok(Self { out, inp, out_cfg, out_fmt, in_cfg, in_fmt, fs, in_rate })
    }
}

fn err_fn(e: cpal::StreamError) {
    eprintln!("audio stream error: {e}");
}

fn push_frames<T>(buf: &Mutex<Vec<f32>>, data: &[T], ch: usize)
where
    T: Sample,
    f32: FromSample<T>,
{
    if let Ok(mut b) = buf.lock() {
        for f in data.chunks(ch) {
            let s: f32 = f.iter().map(|&x| x.to_sample::<f32>()).sum::<f32>() / ch as f32;
            b.push(s);
        }
    }
}

fn fill<T>(out: &mut [T], ch: usize, play: &[f32], pos: &AtomicUsize)
where
    T: Sample + FromSample<f32>,
{
    let mut p = pos.load(Ordering::Relaxed);
    for f in out.chunks_mut(ch) {
        let v = play.get(p).copied().unwrap_or(0.0);
        p += 1;
        for s in f.iter_mut() {
            *s = T::from_sample(v);
        }
    }
    pos.store(p, Ordering::Relaxed);
}

fn build_input(dev: &Device, cfg: &StreamConfig, fmt: SampleFormat, buf: Arc<Mutex<Vec<f32>>>) -> Result<Stream> {
    let ch = cfg.channels as usize;
    fn mk<T>(dev: &Device, cfg: &StreamConfig, buf: Arc<Mutex<Vec<f32>>>, ch: usize) -> Result<Stream>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        Ok(dev.build_input_stream(
            cfg,
            move |d: &[T], _: &cpal::InputCallbackInfo| push_frames(&buf, d, ch),
            err_fn,
            None,
        )?)
    }
    match fmt {
        SampleFormat::F32 => mk::<f32>(dev, cfg, buf, ch),
        SampleFormat::I16 => mk::<i16>(dev, cfg, buf, ch),
        SampleFormat::U16 => mk::<u16>(dev, cfg, buf, ch),
        SampleFormat::I32 => mk::<i32>(dev, cfg, buf, ch),
        SampleFormat::U8 => mk::<u8>(dev, cfg, buf, ch),
        f => bail!("unsupported microphone sample format {f:?}"),
    }
}

fn build_output(
    dev: &Device,
    cfg: &StreamConfig,
    fmt: SampleFormat,
    play: Arc<Vec<f32>>,
    pos: Arc<AtomicUsize>,
) -> Result<Stream> {
    let ch = cfg.channels as usize;
    fn mk<T>(dev: &Device, cfg: &StreamConfig, play: Arc<Vec<f32>>, pos: Arc<AtomicUsize>, ch: usize) -> Result<Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        Ok(dev.build_output_stream(
            cfg,
            move |d: &mut [T], _: &cpal::OutputCallbackInfo| fill(d, ch, &play, &pos),
            err_fn,
            None,
        )?)
    }
    match fmt {
        SampleFormat::F32 => mk::<f32>(dev, cfg, play, pos, ch),
        SampleFormat::I16 => mk::<i16>(dev, cfg, play, pos, ch),
        SampleFormat::U16 => mk::<u16>(dev, cfg, play, pos, ch),
        SampleFormat::I32 => mk::<i32>(dev, cfg, play, pos, ch),
        SampleFormat::U8 => mk::<u8>(dev, cfg, play, pos, ch),
        f => bail!("unsupported speaker sample format {f:?}"),
    }
}

impl Sonar for CpalSonar {
    fn fs(&self) -> u32 {
        self.fs
    }

    fn name(&self) -> String {
        format!("{} → {}", dev_name(&self.out), dev_name(&self.inp))
    }

    fn playrec(&mut self, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<f32>> {
        let fs = self.fs as f32;
        let mut play = vec![0.0f32; (0.05 * fs) as usize];
        play.extend(sig.iter().map(|x| x * volume));
        play.extend(std::iter::repeat(0.0).take((listen_s * fs) as usize));
        let dur = play.len() as f32 / fs + 0.3;

        let buf = Arc::new(Mutex::new(Vec::with_capacity(
            (dur * self.in_rate as f32) as usize + 4096,
        )));
        let ins = quiet(|| -> Result<Stream> {
            let s = build_input(&self.inp, &self.in_cfg, self.in_fmt, buf.clone())?;
            s.play()?;
            Ok(s)
        })?;
        std::thread::sleep(Duration::from_millis(40));
        let outs = quiet(|| -> Result<Stream> {
            let s = build_output(
                &self.out,
                &self.out_cfg,
                self.out_fmt,
                Arc::new(play),
                Arc::new(AtomicUsize::new(0)),
            )?;
            s.play()?;
            Ok(s)
        })?;
        std::thread::sleep(Duration::from_secs_f32(dur));
        drop(outs);
        std::thread::sleep(Duration::from_millis(20));
        drop(ins);

        let rec = buf
            .lock()
            .map_err(|_| anyhow!("recording buffer poisoned"))?
            .clone();
        if rec.is_empty() {
            bail!("recorded nothing — is the microphone muted, blocked, or in use?");
        }
        Ok(resample(&rec, self.in_rate, self.fs))
    }

    fn record(&mut self, secs: f32) -> Result<Vec<f32>> {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let ins = quiet(|| -> Result<Stream> {
            let s = build_input(&self.inp, &self.in_cfg, self.in_fmt, buf.clone())?;
            s.play()?;
            Ok(s)
        })?;
        std::thread::sleep(Duration::from_secs_f32(secs));
        drop(ins);
        let rec = buf
            .lock()
            .map_err(|_| anyhow!("recording buffer poisoned"))?
            .clone();
        if rec.is_empty() {
            bail!("recorded nothing — is the microphone muted, blocked, or in use?");
        }
        Ok(resample(&rec, self.in_rate, self.fs))
    }
}

// ------------------------------------------------------------------ simulator

/// A rectangular room with one round table, for exercising the whole pipeline
/// without a speaker or mic. Device beam is modelled as a Gaussian of `beam_deg`.
pub struct SimSonar {
    fs: u32,
    room: (f32, f32),
    start: (f32, f32),
    pos: (f32, f32),
    heading: f32,
    beam_deg: f32,
    rng: u64,
    obstacles: Vec<((f32, f32), f32)>,
}

impl SimSonar {
    pub fn new() -> Self {
        Self {
            fs: 48_000,
            room: (5.2, 4.1),
            start: (2.0, 1.6),
            pos: (2.0, 1.6),
            heading: 0.0,
            beam_deg: 25.0,
            rng: 0x9E37_79B9_7F4A_7C15,
            obstacles: vec![((4.0, 0.9), 0.4)],
        }
    }

    fn rand(&mut self) -> f32 {
        // xorshift64*, mapped to [-1, 1)
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let v = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }

    /// Distance from `o` along compass heading `deg` (0 = +y, clockwise) to the first surface.
    fn ray(&self, o: (f32, f32), deg: f32) -> f32 {
        let a = deg.to_radians();
        let (dx, dy) = (a.sin(), a.cos());
        let mut t = f32::MAX;
        let (w, h) = self.room;
        if dx > 1e-6 { t = t.min((w - o.0) / dx); }
        if dx < -1e-6 { t = t.min(-o.0 / dx); }
        if dy > 1e-6 { t = t.min((h - o.1) / dy); }
        if dy < -1e-6 { t = t.min(-o.1 / dy); }
        for &((cx, cy), r) in &self.obstacles {
            let (fx, fy) = (o.0 - cx, o.1 - cy);
            let b = fx * dx + fy * dy;
            let c = fx * fx + fy * fy - r * r;
            let disc = b * b - c;
            if disc >= 0.0 {
                let hit = -b - disc.sqrt();
                if hit > 0.0 {
                    t = t.min(hit);
                }
            }
        }
        t
    }
}

impl Sonar for SimSonar {
    fn fs(&self) -> u32 {
        self.fs
    }

    fn name(&self) -> String {
        format!("simulated {:.1} × {:.1} m room", self.room.0, self.room.1)
    }

    fn aim(&mut self, pos: (f32, f32), heading_deg: f32) {
        self.pos = (self.start.0 + pos.0, self.start.1 + pos.1);
        self.heading = heading_deg;
    }

    fn is_sim(&self) -> bool {
        true
    }

    fn playrec(&mut self, sig: &[f32], listen_s: f32, volume: f32) -> Result<Vec<f32>> {
        let fs = self.fs;
        let lat = 300 + (self.rand().abs() * 200.0) as usize;
        let len = lat + sig.len() + (listen_s * fs as f32) as usize + 64;
        let mut rec = vec![0.0f32; len];
        let add = |at: usize, amp: f32, rec: &mut Vec<f32>| {
            for (i, &v) in sig.iter().enumerate() {
                if let Some(r) = rec.get_mut(at + i) {
                    *r += v * amp;
                }
            }
        };
        add(lat, volume, &mut rec); // direct speaker -> mic

        let offsets: Vec<f32> = (-20..=20).map(|k| k as f32 * 3.0).collect();
        let weights: Vec<f32> = offsets
            .iter()
            .map(|o| (-(o / self.beam_deg).powi(2)).exp())
            .collect();
        let wsum: f32 = weights.iter().sum();
        for (o, w) in offsets.iter().zip(&weights) {
            let d = self.ray(self.pos, self.heading + o);
            let at = lat + m_to_samples(d, fs);
            add(at, volume * 0.9 * w / wsum * 4.0 / (0.5 + d), &mut rec);
        }
        for i in 0..len {
            rec[i] += 0.004 * self.rand();
        }
        Ok(rec)
    }

    fn record(&mut self, secs: f32) -> Result<Vec<f32>> {
        let n = (secs * self.fs as f32) as usize;
        Ok((0..n).map(|_| 0.004 * self.rand()).collect())
    }
}

// ------------------------------------------------------------------ hardware panel

fn describe(cfg: Result<cpal::SupportedStreamConfig, cpal::DefaultStreamConfigError>) -> (String, Option<u32>) {
    match cfg {
        Ok(c) => {
            let fs = c.sample_rate().0;
            (
                format!("{:>6} Hz  {} ch  {:?}", fs, c.channels(), c.sample_format()),
                Some(fs),
            )
        }
        Err(e) => (format!("unusable ({e})"), None),
    }
}

/// Text panel listing speakers and mics, their formats, and what ChirpMap can do with them.
pub fn hardware_panel() -> Result<String> {
    quiet(hardware_panel_inner)
}

fn hardware_panel_inner() -> Result<String> {
    let host = cpal::default_host();
    let dout = host.default_output_device().map(|d| dev_name(&d));
    let din = host.default_input_device().map(|d| dev_name(&d));
    let mut s = String::new();
    s += &format!("CHIRPMAP · HARDWARE PANEL   (audio host: {})\n\n", host.id().name());

    s += "SPEAKERS (use with --output <# or name>)\n";
    let mut best_fs = 0;
    for (i, d) in host.output_devices()?.enumerate() {
        let n = dev_name(&d);
        let (desc, fs) = describe(d.default_output_config());
        best_fs = best_fs.max(fs.unwrap_or(0));
        let mark = if Some(&n) == dout.as_ref() { "*" } else { " " };
        s += &format!("  {mark}{i:>2}  {n:<40} {desc}\n");
    }
    s += "\nMICROPHONES (use with --input <# or name>)\n";
    for (i, d) in host.input_devices()?.enumerate() {
        let n = dev_name(&d);
        let (desc, _) = describe(d.default_input_config());
        let mark = if Some(&n) == din.as_ref() { "*" } else { " " };
        s += &format!("  {mark}{i:>2}  {n:<40} {desc}\n");
    }
    s += "\n  * = system default\n";

    if best_fs > 0 {
        let top = (0.45 * best_fs as f32).min(20_000.0);
        let bw = top - 2000.0;
        s += &format!(
            "\nCAPABILITY at {best_fs} Hz: chirp up to {:.1} kHz, theoretical range resolution ≈ {:.1} cm\n",
            top / 1000.0,
            SPEED_OF_SOUND / (2.0 * bw) * 100.0
        );
    }

    let profiles = crate::calib::list_profiles();
    s += "\nCALIBRATION PROFILES\n";
    if profiles.is_empty() {
        s += "  none yet — run `chirpmap calibrate`\n";
    }
    for p in profiles {
        s += &format!(
            "  {}  ·  {:.1}–{:.1} kHz  ·  vol {:.2}  ·  SNR {:.0} dB  ·  blank {:.2} m\n",
            p.device,
            p.f0 / 1000.0,
            p.f1 / 1000.0,
            p.volume,
            p.direct_snr_db,
            p.blank_m
        );
    }
    Ok(s)
}
