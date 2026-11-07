//! ChirpMap — terminal-native acoustic echolocation that maps rooms like an
//! architect's blueprint, using any speaker and mic. Fully offline.

mod array;
mod audio;
mod calib;
mod dsp;
mod layout;
mod levels;
mod map;
mod render;
mod tui;

use anyhow::{bail, Context, Result};
use audio::{CpalSonar, SimSonar, Sonar};
use calib::Profile;
use clap::{Args, Parser, Subcommand};
use dsp::{Echo, RangeProfile};
use map::{Observation, ScanMap};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Parser)]
#[command(
    name = "chirpmap",
    version,
    about = "Terminal-native acoustic echolocation that maps rooms like an architect's blueprint. Fully offline."
)]
struct Cli {
    /// Leave out to open the launcher menu
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Ctrl+C inside a live view returns to the menu instead of killing the program.
static IN_LIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Args, Clone, Default)]
struct DevArgs {
    /// Speaker: index or part of the name from `chirpmap devices` (default: system default)
    #[arg(long)]
    output: Option<String>,
    /// Microphone: index or part of the name from `chirpmap devices` (default: system default)
    #[arg(long)]
    input: Option<String>,
    /// Use a simulated 5.2 × 4.1 m room instead of real hardware
    #[arg(long)]
    sim: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Hardware panel: list speakers and microphones and what they can do
    Devices,
    /// Auto-calibrate this speaker + mic pair and save a per-device profile
    Calibrate {
        #[command(flatten)]
        dev: DevArgs,
        /// Save the profile even if the quality checks fail
        #[arg(long)]
        force: bool,
    },
    /// Single-direction range reading as a bar chart
    Ping {
        #[command(flatten)]
        dev: DevArgs,
        /// Refresh continuously (Ctrl+C to quit)
        #[arg(long)]
        live: bool,
        /// Pings averaged per reading
        #[arg(long, default_value_t = 4)]
        avg: usize,
        /// Metres shown
        #[arg(long, default_value_t = 4.0)]
        max_range: f32,
        /// Metres per bar
        #[arg(long, default_value_t = 0.1)]
        bin: f32,
        /// Show only what CHANGED vs. the empty scene (captured at start). Makes a hand or book obvious
        #[arg(long)]
        diff: bool,
        /// Override the calibrated blanking distance, metres
        #[arg(long)]
        blank: Option<f32>,
        /// Save each raw mic recording to this .wav (overwritten every reading)
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Map a room: rotate the device through headings at one or more spots
    Scan {
        #[command(flatten)]
        dev: DevArgs,
        /// Headings per spot (8 = every 45°)
        #[arg(long, default_value_t = 8)]
        steps: usize,
        /// Assumed half-width of the speaker's beam, degrees
        #[arg(long, default_value_t = 30.0)]
        beam: f32,
        /// Pings averaged per heading
        #[arg(long, default_value_t = 4)]
        avg: usize,
        /// Maximum range, metres
        #[arg(long, default_value_t = 5.0)]
        max_range: f32,
        /// Map grid size, metres
        #[arg(long, default_value_t = 0.1)]
        cell: f32,
        /// Override the calibrated blanking distance, metres
        #[arg(long)]
        blank: Option<f32>,
        /// Pre-planned spots as "x,y;x,y" in metres from the first spot (skips the prompts between spots)
        #[arg(long)]
        positions: Option<String>,
        /// Don't wait for Enter before each heading
        #[arg(long)]
        auto: bool,
        /// Folder for the saved scan (.json) and blueprint (.svg)
        #[arg(long, default_value = "scans")]
        out: PathBuf,
        /// Plain ASCII output
        #[arg(long)]
        no_color: bool,
    },
    /// Re-draw a saved scan, optionally exporting a new SVG
    Render {
        file: PathBuf,
        #[arg(long)]
        svg: Option<PathBuf>,
        #[arg(long)]
        no_color: bool,
    },
}

fn open(dev: &DevArgs) -> Result<Box<dyn Sonar>> {
    if dev.sim {
        return Ok(Box::new(SimSonar::new()));
    }
    let s = CpalSonar::open(dev.output.as_deref(), dev.input.as_deref())
        .context("opening audio devices (try `chirpmap devices`, or --sim to test without hardware)")?;
    Ok(Box::new(s))
}

fn term_cols() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(110)
}

fn listen_for(max_range: f32) -> f32 {
    2.0 * max_range / dsp::SPEED_OF_SOUND + 0.05
}

/// Average `avg` pings into one range profile. Also returns the last raw recording.
fn measure_profile(sonar: &mut dyn Sonar, p: &Profile, tpl: &[f32], avg: usize, max_range: f32) -> Result<(RangeProfile, Vec<f32>)> {
    let listen = listen_for(max_range);
    let mut envs = Vec::with_capacity(avg);
    let mut raw = Vec::new();
    for _ in 0..avg.max(1) {
        raw = sonar.playrec(tpl, listen, p.volume)?;
        envs.push(dsp::matched_envelope(&raw, tpl));
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok((dsp::aligned_average(&envs, p.fs), raw))
}

/// Average `avg` pings and pick echoes.
fn measure(sonar: &mut dyn Sonar, p: &Profile, tpl: &[f32], avg: usize, max_range: f32) -> Result<(RangeProfile, Vec<Echo>)> {
    let (rp, _) = measure_profile(sonar, p, tpl, avg, max_range)?;
    let th = dsp::echo_threshold(p.noise_env, rp.direct_peak);
    let echoes = dsp::find_echoes(&rp, p.blank_m, max_range, th);
    Ok((rp, echoes))
}

fn parse_positions(s: &str) -> Result<Vec<(f32, f32)>> {
    s.split(';')
        .filter(|t| !t.trim().is_empty())
        .map(|t| {
            let v: Vec<&str> = t.split(',').collect();
            if v.len() != 2 {
                bail!("bad position '{t}', expected x,y");
            }
            Ok((v[0].trim().parse()?, v[1].trim().parse()?))
        })
        .collect()
}

fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn ask_yes(prompt: &str) -> Result<bool> {
    let a = read_line(prompt)?.to_lowercase();
    Ok(a.is_empty() || a.starts_with('y'))
}

/// Ask permission (and a speaker ceiling) before touching system levels.
fn ask_levels_for_calibration() -> Result<Option<(levels::Session, u32)>> {
    if !levels::available() {
        return Ok(None);
    }
    println!();
    if !ask_yes("Allow ChirpMap to adjust your speaker volume and mic sensitivity while it calibrates?\nYour current levels are saved and put back when it finishes. [Y/n] ")? {
        println!("OK — leaving your levels alone.\n");
        return Ok(None);
    }
    let cur = levels::current_speaker_pct().unwrap_or(0);
    let cap = loop {
        let a = read_line(&format!(
            "Highest speaker volume ChirpMap may use, 10–100% (yours is {cur}% now) [60]: "
        ))?;
        if a.is_empty() {
            break 60;
        }
        match a.trim_end_matches('%').trim().parse::<u32>() {
            Ok(v) if (10..=100).contains(&v) => break v,
            _ => println!("  enter a number from 10 to 100"),
        }
    };
    println!();
    Ok(levels::Session::begin().map(|s| (s, cap)))
}

/// If calibration saved levels, offer to use them for this run (restored afterwards).
fn ask_levels_for_run(p: &Profile, sim: bool) -> Result<Option<levels::Session>> {
    let (Some(spk), Some(mic)) = (p.speaker_pct, p.mic_pct) else { return Ok(None) };
    if sim || !levels::available() {
        return Ok(None);
    }
    if !ask_yes(&format!(
        "Use your calibrated levels (speaker {spk}%, mic {mic}%)? They're put back when you stop. [Y/n] "
    ))? {
        return Ok(None);
    }
    let sess = levels::Session::begin();
    if let Some(s) = &sess {
        s.set_speaker(spk);
        s.set_mic(mic);
    }
    Ok(sess)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _ = ctrlc::set_handler(|| {
        if IN_LIVE.load(Ordering::SeqCst) {
            STOP.store(true, Ordering::SeqCst);
        } else {
            levels::restore_active();
            println!();
            std::process::exit(130);
        }
    });
    levels::recover_after_crash();
    match cli.cmd {
        Some(cmd) => run(cmd),
        // Full-screen window when we're on a real terminal; plain menu otherwise (pipes, scripts).
        None if std::io::IsTerminal::is_terminal(&std::io::stdout()) && std::env::var_os("CHIRPMAP_PLAIN").is_none() => tui::run(),
        None => menu(),
    }
}

fn scans_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("chirpmap-scans")
}

/// One-line calibration health, and whether it's good.
fn calib_summary() -> (String, bool) {
    match CpalSonar::open(None, None) {
        Err(e) => (format!("audio not available ({e:#})"), false),
        Ok(s) => match calib::load(&s.name()) {
            None => ("none yet — run Calibrate first".into(), false),
            Some(p) if calib::problems(&p).is_empty() => (
                format!(
                    "{:.1}–{:.1} kHz · {:.0} dB SNR · blank {}{}",
                    p.f0 / 1000.0,
                    p.f1 / 1000.0,
                    p.direct_snr_db,
                    dsp::fmt_len(p.blank_m),
                    match (p.speaker_pct, p.mic_pct) {
                        (Some(a), Some(b)) => format!(" · levels spk {a}% mic {b}%"),
                        _ => String::new(),
                    }
                ),
                true,
            ),
            Some(_) => ("looks weak — recalibrate".into(), false),
        },
    }
}

fn status_line() -> String {
    let mut out = audio::in_use_block(None, None);
    out += &format!("  📐 CALIBRATION  {}\n", calib_summary().0);
    out
}

/// What `chirpmap` with no arguments does: a launcher menu.
fn menu() -> Result<()> {
    let dev = DevArgs::default();
    let sim = DevArgs { sim: true, ..DevArgs::default() };
    loop {
        print!("\x1b[H\x1b[J");
        println!("╔══════════════════════════════════════════════╗");
        println!("║  CHIRPMAP v{:<8}  acoustic room mapping     ║", env!("CARGO_PKG_VERSION"));
        println!("╚══════════════════════════════════════════════╝");
        println!("{}", status_line());
        println!("  1  Live tracker      shows only what moves (Enter = this)");
        println!("  2  Live echoes       everything the mic hears");
        println!("  3  Calibrate         tune to this speaker + mic");
        println!("  4  Scan a room       rotate-and-ping floor plan");
        println!("  5  Hardware panel    speakers, mics, profiles");
        println!("  6  Simulator demo    scan a fake room, no hardware");
        println!("  u  Update            git pull + reinstall");
        println!("  q  Quit\n");
        println!("  (Ctrl+C in a live view comes back here)");
        let choice = read_line("\n> ")?;
        let cmd = match choice.to_lowercase().as_str() {
            "1" | "" => Cmd::Ping { dev: dev.clone(), live: true, avg: 4, max_range: 4.0, bin: 0.1, diff: true, blank: None, dump: None },
            "2" => Cmd::Ping { dev: dev.clone(), live: true, avg: 4, max_range: 4.0, bin: 0.1, diff: false, blank: None, dump: None },
            "3" => Cmd::Calibrate { dev: dev.clone(), force: false },
            "4" => Cmd::Scan { dev: dev.clone(), steps: 8, beam: 30.0, avg: 4, max_range: 5.0, cell: 0.1, blank: None, positions: None, auto: false, out: scans_dir(), no_color: false },
            "5" => Cmd::Devices,
            "6" => Cmd::Scan { dev: sim.clone(), steps: 8, beam: 30.0, avg: 4, max_range: 5.0, cell: 0.1, blank: None, positions: None, auto: true, out: scans_dir(), no_color: false },
            "u" => {
                update();
                read_line("\nPress Enter, then restart chirpmap to use the new version…")?;
                return Ok(());
            }
            "q" | "quit" | "exit" => return Ok(()),
            _ => continue,
        };
        if let Err(e) = run(cmd) {
            println!("\nerror: {e:#}");
        }
        read_line("\nPress Enter for the menu…")?;
    }
}

/// Pull the latest code into the source folder this binary was built from and reinstall.
fn update() {
    let src = env!("CARGO_MANIFEST_DIR");
    println!("Updating from {src} …");
    let ok = std::process::Command::new("git").args(["-C", src, "pull", "--ff-only"]).status().map(|s| s.success()).unwrap_or(false)
        && std::process::Command::new("cargo").args(["install", "--path", src, "--force"]).status().map(|s| s.success()).unwrap_or(false);
    println!("{}", if ok { "Updated." } else { "Update failed — see messages above." });
}

fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Devices => print!("{}", audio::hardware_panel()?),

        Cmd::Calibrate { dev, force } => {
            if !dev.sim {
                print!("{}", audio::in_use_block(dev.output.as_deref(), dev.input.as_deref()));
            }
            let mut sonar = open(&dev)?;
            let session = if dev.sim { None } else { ask_levels_for_calibration()? };
            let result = calib::run(sonar.as_mut(), session.as_ref().map(|(s, cap)| (s, *cap)));
            drop(session); // puts your levels back, success or failure
            let p = result?;
            let issues = calib::problems(&p);
            if !issues.is_empty() {
                println!("\n⚠  Calibration looks bad:");
                for i in &issues {
                    println!("   • {i}");
                }
            }
            if !issues.is_empty() && !force {
                println!("\nNot saved. Fix the above and rerun, or add --force to save anyway.");
            } else if sonar.is_sim() {
                println!("\n(simulated device — profile not saved)");
            } else {
                let path = calib::save(&p)?;
                println!("\nSaved profile → {}", path.display());
            }
        }

        Cmd::Ping { dev, live, avg, max_range, bin, diff, blank, dump } => {
            let mut sonar = open(&dev)?;
            let mut p = calib::load_or_default(sonar.as_ref());
            if let Some(b) = blank {
                p.blank_m = b;
            }
            let _levels = ask_levels_for_run(&p, dev.sim)?;
            let tpl = p.template();
            let bg = if diff {
                println!("Capturing the empty scene — keep still and out of the way for a few seconds…");
                let mut shots = Vec::new();
                for _ in 0..8 {
                    shots.push(measure_profile(sonar.as_mut(), &p, &tpl, avg, max_range)?.0);
                }
                println!("Background captured. Now move something in front of the mic.");
                std::thread::sleep(std::time::Duration::from_millis(600));
                Some(dsp::Background::capture(&shots))
            } else {
                None
            };
            IN_LIVE.store(live, Ordering::SeqCst);
            STOP.store(false, Ordering::SeqCst);
            let result = (|| -> Result<()> {
            loop {
                if STOP.load(Ordering::SeqCst) {
                    break;
                }
                let (rp, raw) = measure_profile(sonar.as_mut(), &p, &tpl, avg, max_range)?;
                if let Some(path) = &dump {
                    audio::write_wav(path, p.fs, &raw)?;
                }
                let (shown, th, floor, mode) = match &bg {
                    Some(b) => {
                        let d = b.subtract(&rp);
                        let th = b.threshold(p.fs, p.blank_m, max_range);
                        (d, th, th * 2.0, "CHANGE vs background")
                    }
                    None => {
                        let th = dsp::echo_threshold(p.noise_env, rp.direct_peak);
                        (rp, th, 0.0, "Echo strength")
                    }
                };
                let echoes = dsp::find_echoes(&shown, p.blank_m, max_range, th);
                let header = format!(
                    "{mode} vs distance  ({:.1}–{:.1} kHz chirp, avg {avg}, blank {})",
                    p.f0 / 1000.0,
                    p.f1 / 1000.0,
                    dsp::fmt_len(p.blank_m)
                );
                let out = render::bars(&shown, &echoes, max_range, bin, p.blank_m, floor, &header);
                if live {
                    print!("\x1b[H\x1b[J{out}\n(Ctrl+C to stop)\n");
                    std::io::stdout().flush()?;
                } else {
                    print!("{out}");
                    break;
                }
            }
            Ok(())
            })();
            IN_LIVE.store(false, Ordering::SeqCst);
            result?;
        }

        Cmd::Scan { dev, steps, beam, avg, max_range, cell, blank, positions, auto, out, no_color } => {
            let mut sonar = open(&dev)?;
            let mut p = calib::load_or_default(sonar.as_ref());
            if let Some(b) = blank {
                p.blank_m = b;
            }
            let _levels = ask_levels_for_run(&p, dev.sim)?;
            let tpl = p.template();
            let auto = auto || sonar.is_sim();
            let mut planned = match positions {
                Some(s) => parse_positions(&s)?,
                None if sonar.is_sim() => vec![(0.0, 0.0), (1.2, 1.2)],
                None => vec![(0.0, 0.0)],
            };
            let interactive_spots = !sonar.is_sim() && planned.len() == 1;
            let mut map = ScanMap::new(sonar.name(), beam, cell, now());
            let step_deg = 360.0 / steps.max(1) as f32;

            println!("Scanning with {}", map.device);
            if !auto {
                println!(
                    "Hold the device level. Face your chosen \"forward\" (0°) first, then turn clockwise {step_deg:.0}° per step.\n\
                     Enter = ping · s = skip heading · q = finish this spot"
                );
            }
            let mut k = 0;
            while k < planned.len() {
                let pos = planned[k];
                println!("\n── Spot {} at ({:.2}, {:.2}) m ──", k + 1, pos.0, pos.1);
                for st in 0..steps {
                    let heading = st as f32 * step_deg;
                    if !auto {
                        let a = read_line(&format!("  face {heading:>5.1}° then Enter: "))?;
                        if a.eq_ignore_ascii_case("q") {
                            break;
                        }
                        if a.eq_ignore_ascii_case("s") {
                            continue;
                        }
                    }
                    sonar.aim(pos, heading);
                    let (_, echoes) = measure(sonar.as_mut(), &p, &tpl, avg, max_range)?;
                    match echoes.first() {
                        Some(e) => println!("  {heading:>5.1}°  nearest surface {} ({:.0} dB)", dsp::fmt_len(e.dist_m), e.db()),
                        None => println!("  {heading:>5.1}°  nothing within {}", dsp::fmt_len(max_range)),
                    }
                    map.observations.push(Observation { pos, heading_deg: heading, echoes, max_range });
                }
                if interactive_spots {
                    let a = read_line("\nScan another spot? Enter its offset from spot 1 as x,y metres (e.g. 1.5,0), or Enter to finish: ")?;
                    if !a.is_empty() {
                        match parse_positions(&a) {
                            Ok(mut v) if !v.is_empty() => planned.push(v.remove(0)),
                            _ => println!("  couldn't read that — finishing"),
                        }
                    }
                }
                k += 1;
            }

            let g = map.build_grid();
            let cov = map.coverage(&g);
            println!("\n{}", render::terminal(&map, &g, &cov, !no_color, term_cols()));

            std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            let stem = format!("scan-{}", render::file_stamp(map.created_unix));
            let json = out.join(format!("{stem}.json"));
            let svg = out.join(format!("{stem}.svg"));
            std::fs::write(&json, serde_json::to_string_pretty(&map)?)?;
            std::fs::write(&svg, render::svg(&map, &g, &cov))?;
            println!(" Saved {} and {}", json.display(), svg.display());
        }

        Cmd::Render { file, svg, no_color } => {
            let txt = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let map: ScanMap = serde_json::from_str(&txt).context("not a ChirpMap scan file")?;
            let g = map.build_grid();
            let cov = map.coverage(&g);
            println!("{}", render::terminal(&map, &g, &cov, !no_color, term_cols()));
            if let Some(path) = svg {
                std::fs::write(&path, render::svg(&map, &g, &cov))?;
                println!(" Wrote {}", path.display());
            }
        }
    }
    Ok(())
}
