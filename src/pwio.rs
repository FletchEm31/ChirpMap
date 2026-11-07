//! Playing and recording on a *specific* PipeWire device via `pw-play` /
//! `pw-record`, which target a node reliably. Used for every device picked by
//! name in the Room designer ("pw:<node>"); the system default still goes
//! through cpal directly.
//!
//! Timing between these processes and ChirpMap is loose (process start-up,
//! buffering), which is fine: every speaker→mic pair is anchored on the direct
//! sound in the recording, never on when we asked for playback.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("chirpmap-{}-{name}", std::process::id()))
}


/// Write `sig` as 32-bit float WAV with `channels` channels; only channel
/// `only` carries sound when given (e.g. just the left tower).
fn write_wav(path: &PathBuf, fs: u32, channels: u16, only: Option<usize>, sig: &[f32]) -> Result<()> {
    let ch = channels.max(1) as usize;
    let data_len = (sig.len() * ch * 4) as u32;
    let mut b: Vec<u8> = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes());
    b.extend_from_slice(&(ch as u16).to_le_bytes());
    b.extend_from_slice(&fs.to_le_bytes());
    b.extend_from_slice(&(fs * 4 * ch as u32).to_le_bytes());
    b.extend_from_slice(&((4 * ch) as u16).to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for &v in sig {
        for c in 0..ch {
            let x = if only.map_or(true, |o| o == c) { v } else { 0.0 };
            b.extend_from_slice(&x.to_le_bytes());
        }
    }
    std::fs::write(path, b)?;
    Ok(())
}

/// Mono samples from a WAV file (float32 or int16; channels averaged).
pub fn read_wav(path: &PathBuf) -> Result<(Vec<f32>, u32)> {
    let b = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("not a WAV recording");
    }
    let (mut fmt, mut ch, mut fs, mut bits) = (0u16, 1usize, 0u32, 0u16);
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let mut len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        let body = i + 8;
        if id == b"fmt " && body + 16 <= b.len() {
            fmt = u16::from_le_bytes([b[body], b[body + 1]]);
            ch = u16::from_le_bytes([b[body + 2], b[body + 3]]).max(1) as usize;
            fs = u32::from_le_bytes([b[body + 4], b[body + 5], b[body + 6], b[body + 7]]);
            bits = u16::from_le_bytes([b[body + 14], b[body + 15]]);
            if fmt == 0xFFFE && body + 26 <= b.len() {
                fmt = u16::from_le_bytes([b[body + 24], b[body + 25]]); // WAVE_FORMAT_EXTENSIBLE sub-format
            }
        } else if id == b"data" {
            // A recorder stopped early may leave the length unset; take what's there.
            if len == 0 || len == 0xFFFF_FFFF || body + len > b.len() {
                len = b.len() - body;
            }
            let d = &b[body..body + len];
            let frames: Vec<f32> = match (fmt, bits) {
                (3, 32) => d.chunks_exact(4 * ch).map(|f| (0..ch).map(|c| f32::from_le_bytes([f[4 * c], f[4 * c + 1], f[4 * c + 2], f[4 * c + 3]])).sum::<f32>() / ch as f32).collect(),
                (1, 16) => d.chunks_exact(2 * ch).map(|f| (0..ch).map(|c| i16::from_le_bytes([f[2 * c], f[2 * c + 1]]) as f32 / 32768.0).sum::<f32>() / ch as f32).collect(),
                _ => bail!("unsupported WAV format {fmt}/{bits}-bit"),
            };
            return Ok((frames, fs));
        }
        i = body + len + (len & 1);
    }
    bail!("WAV recording had no audio data")
}

/// Start playing `sig` on node `node`; returns the player (wait on it).
pub fn play(node: &str, fs: u32, channels: u16, only: Option<usize>, sig: &[f32]) -> Result<Child> {
    let path = tmp(&format!("play-{}.wav", node.len()));
    write_wav(&path, fs, channels, only, sig)?;
    Command::new("pw-play")
        .arg(format!("--target={node}"))
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting pw-play (install pipewire-bin / pipewire-utils?)")
}

pub fn wait_player(mut c: Child) -> Result<()> {
    let st = c.wait()?;
    if !st.success() {
        let mut msg = String::new();
        if let Some(mut e) = c.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut msg);
        }
        bail!("pw-play failed: {}", msg.trim());
    }
    Ok(())
}

pub struct Recording {
    child: Child,
    path: PathBuf,
}

/// Start recording mono float32 at `fs` from node `node`.
pub fn record(node: &str, fs: u32, tag: usize) -> Result<Recording> {
    let path = tmp(&format!("rec-{tag}.wav"));
    let _ = std::fs::remove_file(&path);
    let child = Command::new("pw-record")
        .arg(format!("--target={node}"))
        .arg(format!("--rate={fs}"))
        .arg("--channels=1")
        .arg("--format=f32")
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting pw-record")?;
    Ok(Recording { child, path })
}

impl Recording {
    /// Stop cleanly (SIGINT lets pw-record finish the file) and read it back.
    pub fn stop(mut self) -> Result<(Vec<f32>, u32)> {
        // SAFETY: sending a signal to our own child process.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        for _ in 0..50 {
            if self.child.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let r = read_wav(&self.path);
        let _ = std::fs::remove_file(&self.path);
        r
    }
}

/// How long pw-record needs before it's actually capturing.
pub const RECORD_WARMUP: Duration = Duration::from_millis(350);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_roundtrip_keeps_only_the_chosen_channel() {
        let p = tmp("test.wav");
        write_wav(&p, 48_000, 2, Some(1), &[0.5, -0.25]).unwrap();
        // Averaging L (silent) and R gives half the value.
        let (x, fs) = read_wav(&p).unwrap();
        assert_eq!(fs, 48_000);
        assert_eq!(x, vec![0.25, -0.125]);
        let _ = std::fs::remove_file(p);
    }
}
