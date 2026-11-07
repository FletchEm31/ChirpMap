//! Temporarily adjusting the system speaker volume and mic sensitivity
//! (PipeWire/PulseAudio via `pactl`), with a hard guarantee that the user's own
//! levels come back:
//!   * normal finish or error  → the `Session` guard restores on drop
//!   * Ctrl+C                  → the signal handler calls `restore_active()`
//!   * crash / killed / power  → the originals are written to disk first and
//!                               restored the next time chirpmap starts.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Saved {
    sink: String,
    source: String,
    sink_vol: Vec<u32>,
    source_vol: Vec<u32>,
    sink_mute: bool,
    source_mute: bool,
}

static ACTIVE: Mutex<Option<Saved>> = Mutex::new(None);

fn out(args: &[&str]) -> Option<String> {
    let o = Command::new("pactl").args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn run(args: &[&str]) -> bool {
    Command::new("pactl")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Every "NN%" on the "Volume:" line (one per channel, keeps L/R balance).
fn percents(s: &str) -> Vec<u32> {
    let line = s.lines().find(|l| l.contains("Volume:")).unwrap_or("");
    line.split('/')
        .filter_map(|t| t.trim().strip_suffix('%'))
        .filter_map(|n| n.trim().parse().ok())
        .collect()
}

fn muted(s: &str) -> bool {
    s.to_lowercase().contains("yes")
}

pub fn available() -> bool {
    out(&["get-default-sink"]).is_some() && out(&["get-default-source"]).is_some()
}

pub fn current_speaker_pct() -> Option<u32> {
    percents(&out(&["get-sink-volume", "@DEFAULT_SINK@"])?).into_iter().max()
}

fn snapshot() -> Option<Saved> {
    let sink = out(&["get-default-sink"])?;
    let source = out(&["get-default-source"])?;
    Some(Saved {
        sink_vol: percents(&out(&["get-sink-volume", &sink])?),
        source_vol: percents(&out(&["get-source-volume", &source])?),
        sink_mute: muted(&out(&["get-sink-mute", &sink])?),
        source_mute: muted(&out(&["get-source-mute", &source])?),
        sink,
        source,
    })
}

fn restore_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("chirpmap")
        .join("restore-levels.json")
}

fn vols(v: &[u32]) -> Vec<String> {
    v.iter().map(|p| format!("{p}%")).collect()
}

fn restore(s: &Saved) -> bool {
    let mut ok = true;
    if !s.sink_vol.is_empty() {
        let mut a = vec!["set-sink-volume".to_string(), s.sink.clone()];
        a.extend(vols(&s.sink_vol));
        ok &= run(&a.iter().map(String::as_str).collect::<Vec<_>>());
    }
    if !s.source_vol.is_empty() {
        let mut a = vec!["set-source-volume".to_string(), s.source.clone()];
        a.extend(vols(&s.source_vol));
        ok &= run(&a.iter().map(String::as_str).collect::<Vec<_>>());
    }
    ok &= run(&["set-sink-mute", &s.sink, if s.sink_mute { "1" } else { "0" }]);
    ok &= run(&["set-source-mute", &s.source, if s.source_mute { "1" } else { "0" }]);
    ok
}

fn describe(s: &Saved) -> String {
    let fmt = |v: &[u32]| v.iter().map(|p| format!("{p}%")).collect::<Vec<_>>().join("/");
    format!(
        "speaker {}{}, mic {}{}",
        fmt(&s.sink_vol),
        if s.sink_mute { " (muted)" } else { "" },
        fmt(&s.source_vol),
        if s.source_mute { " (muted)" } else { "" }
    )
}

/// Put back whatever an active session changed. Safe to call any time, from any thread.
pub fn restore_active() {
    let saved = ACTIVE.lock().ok().and_then(|mut g| g.take());
    if let Some(s) = saved {
        if restore(&s) {
            println!("↺ Restored your audio levels ({}).", describe(&s));
            let _ = std::fs::remove_file(restore_file());
        } else {
            println!(
                "⚠ Couldn't fully restore audio levels. Originals: {}. Saved in {}",
                describe(&s),
                restore_file().display()
            );
        }
    }
}

/// If a previous run was killed before it could restore, fix that now.
pub fn recover_after_crash() {
    let path = restore_file();
    let Ok(txt) = std::fs::read_to_string(&path) else { return };
    if let Ok(s) = serde_json::from_str::<Saved>(&txt) {
        if restore(&s) {
            println!("↺ A previous ChirpMap session didn't finish — restored your audio levels ({}).", describe(&s));
            let _ = std::fs::remove_file(path);
        }
    }
}

/// While this exists ChirpMap may change levels; dropping it puts everything back.
pub struct Session {
    sink: String,
    source: String,
    pub original: (u32, u32),
}

impl Session {
    pub fn begin() -> Option<Session> {
        let s = snapshot()?;
        let path = restore_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&s).ok()?).ok()?;
        let original = (
            s.sink_vol.iter().copied().max().unwrap_or(0),
            s.source_vol.iter().copied().max().unwrap_or(0),
        );
        let sess = Session { sink: s.sink.clone(), source: s.source.clone(), original };
        *ACTIVE.lock().ok()? = Some(s);
        // A muted device would make every measurement meaningless.
        run(&["set-sink-mute", &sess.sink, "0"]);
        run(&["set-source-mute", &sess.source, "0"]);
        Some(sess)
    }

    pub fn set_speaker(&self, pct: u32) -> bool {
        run(&["set-sink-volume", &self.sink, &format!("{pct}%")])
    }

    pub fn set_mic(&self, pct: u32) -> bool {
        run(&["set-source-volume", &self.source, &format!("{pct}%")])
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore_active();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses the stateful fake `pactl` in $CHIRPMAP_FAKE_PACTL (skipped if unset).
    #[test]
    fn session_restores_on_drop_and_on_ctrl_c() {
        let Ok(dir) = std::env::var("CHIRPMAP_FAKE_PACTL") else { return };
        std::env::set_var("PATH", format!("{dir}:{}", std::env::var("PATH").unwrap()));
        let st = format!("{dir}/st");
        let read = |f: &str| std::fs::read_to_string(format!("{st}/{f}")).unwrap().trim().to_string();
        let set = |f: &str, v: &str| std::fs::write(format!("{st}/{f}"), v).unwrap();
        set("sink", "42% 38%");
        set("src", "65%");
        set("smute", "yes");
        set("mmute", "no");

        // Normal finish: guard dropped.
        {
            let s = Session::begin().unwrap();
            assert_eq!(read("smute"), "no", "speaker unmuted while measuring");
            s.set_speaker(70);
            s.set_mic(100);
            assert_eq!(read("sink"), "70%");
        }
        assert_eq!(read("sink"), "42% 38%");
        assert_eq!(read("src"), "65%");
        assert_eq!(read("smute"), "yes");

        // Ctrl+C: handler calls restore_active() and exits without dropping.
        let s = Session::begin().unwrap();
        s.set_speaker(55);
        std::mem::forget(s);
        restore_active();
        assert_eq!(read("sink"), "42% 38%");
        assert_eq!(read("smute"), "yes");
    }

    #[test]
    fn parses_pactl_volume_lines() {
        let s = "Volume: front-left: 39321 /  60% / -13.31 dB,   front-right: 36045 /  55% / -15.58 dB\n        balance -0.08";
        assert_eq!(percents(s), vec![60, 55]);
        assert!(muted("Mute: yes"));
        assert!(!muted("Mute: no"));
    }
}
