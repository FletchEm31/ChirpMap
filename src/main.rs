//! ChirpMap — terminal-native acoustic echolocation that maps rooms like an
//! architect's blueprint, using any speaker and mic. Fully offline.

mod audio;
mod calib;
mod dsp;
mod map;
mod render;

use anyhow::{bail, Context, Result};
use audio::{CpalSonar, SimSonar, Sonar};
use calib::Profile;
use clap::{Args, Parser, Subcommand};
use dsp::{Echo, RangeProfile};
use map::{Observation, ScanMap};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "chirpmap",
    version,
    about = "Terminal-native acoustic echolocation that maps rooms like an architect's blueprint. Fully offline."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
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

/// Average `avg` pings and pick echoes.
fn measure(sonar: &mut dyn Sonar, p: &Profile, tpl: &[f32], avg: usize, max_range: f32) -> Result<(RangeProfile, Vec<Echo>)> {
    let listen = listen_for(max_range);
    let mut envs = Vec::with_capacity(avg);
    for _ in 0..avg.max(1) {
        let rec = sonar.playrec(tpl, listen, p.volume)?;
        envs.push(dsp::matched_envelope(&rec, tpl));
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    let rp = dsp::aligned_average(&envs, p.fs);
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Devices => print!("{}", audio::hardware_panel()?),

        Cmd::Calibrate { dev } => {
            let mut sonar = open(&dev)?;
            let p = calib::run(sonar.as_mut())?;
            if sonar.is_sim() {
                println!("\n(simulated device — profile not saved)");
            } else {
                let path = calib::save(&p)?;
                println!("\nSaved profile → {}", path.display());
            }
        }

        Cmd::Ping { dev, live, avg, max_range, bin } => {
            let mut sonar = open(&dev)?;
            let p = calib::load_or_default(sonar.as_ref());
            let tpl = p.template();
            loop {
                let (rp, echoes) = measure(sonar.as_mut(), &p, &tpl, avg, max_range)?;
                let header = format!(
                    "Echo strength vs distance  ({:.1}–{:.1} kHz chirp, avg {avg}, blank {:.2} m)",
                    p.f0 / 1000.0,
                    p.f1 / 1000.0,
                    p.blank_m
                );
                let out = render::bars(&rp, &echoes, max_range, bin, p.blank_m, &header);
                if live {
                    print!("\x1b[H\x1b[J{out}\n(Ctrl+C to quit)\n");
                    std::io::stdout().flush()?;
                } else {
                    print!("{out}");
                    break;
                }
            }
        }

        Cmd::Scan { dev, steps, beam, avg, max_range, cell, positions, auto, out, no_color } => {
            let mut sonar = open(&dev)?;
            let p = calib::load_or_default(sonar.as_ref());
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
                        Some(e) => println!("  {heading:>5.1}°  nearest surface {:.2} m ({:.0} dB)", e.dist_m, e.db()),
                        None => println!("  {heading:>5.1}°  nothing within {max_range:.1} m"),
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
