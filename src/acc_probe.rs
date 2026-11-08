//! Accuracy probe (not shipped): cargo test --release acc_probe -- --ignored --nocapture
use crate::array::*;
use crate::dsp::{self, SPEED_OF_SOUND};
use crate::layout::{dist, Kind, Layout, Node};
use crate::tracker::Tracker;

fn node(kind: Kind, label: &str, x_ft_in: (f32, f32), top_ft_in: (f32, f32), depth: f32) -> Node {
    let ft = |(f, i): (f32, f32)| (f * 12.0 + i) * 0.0254;
    Node { kind, label: label.into(), device: "default".into(), x: ft(x_ft_in), y: depth - ft(top_ft_in), facing_deg: 180.0, channel: None }
}

fn fletch(extra_mic: bool) -> Layout {
    let d = 3.0;
    let mut nodes = vec![
        node(Kind::Speaker, "S1", (3.0, 1.0), (1.0, 6.0), d),
        node(Kind::Speaker, "S2", (0.0, 2.0), (3.0, 9.0), d),
        node(Kind::Speaker, "S3", (5.0, 1.0), (0.0, 8.0), d),
        node(Kind::Mic, "M1", (3.0, 3.0), (2.0, 7.0), d),
    ];
    if extra_mic {
        nodes.push(node(Kind::Mic, "M2", (4.0, 0.0), (0.0, 10.0), d));
    }
    Layout { name: "fletch".into(), width_m: 2.40, depth_m: d, nodes }
}

struct Stats {
    errs: Vec<f32>,
    ghosts: usize,
    misses: usize,
    frames: usize,
}

fn pct(v: &[f32], q: f32) -> f32 {
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        return f32::NAN;
    }
    v[((v.len() - 1) as f32 * q) as usize]
}

/// new = pairs-agree fusion + tracker; old = sum + strongest peak.
fn run(l: &Layout, new: bool, walkers: usize, simultaneous: bool, room_c: f32, self_check: bool) -> (Stats, f32) {
    run_at(l, new, walkers, simultaneous, room_c, self_check, 0.3, 3)
}

fn run_at(l: &Layout, new: bool, walkers: usize, simultaneous: bool, room_c: f32, self_check: bool, dt: f64, step: u64) -> (Stats, f32) {
    let fs = 48_000;
    let codes = Codes::for_layout(l, fs, 3000.0, 16_000.0, simultaneous);
    let mut still = SimArray::new(l.clone(), 0);
    still.c = room_c;
    let frames: Vec<Vec<PairEcho>> = (0..5).map(|_| measure(&mut still, l, &codes, 0.7, 1).unwrap()).collect();
    let c = if self_check { fit_speed_of_sound(&average_frames(&frames), l, fs).map_or(SPEED_OF_SOUND, |f| f.0) } else { SPEED_OF_SOUND };
    let bg = ArrayBackground::capture(&frames, fs);
    let mut sim = SimArray::new(l.clone(), walkers);
    sim.c = room_c;
    sim.rng = still.rng;
    sim.mic_lat = still.mic_lat.clone();
    let mut tk = Tracker::new();
    let mut st = Stats { errs: vec![], ghosts: 0, misses: 0, frames: 0 };
    for k in 0..150u64 {
        sim.pings = k * step;
        let truth: Vec<(f32, f32)> = (0..walkers).map(|w| sim.walker_pos(w)).collect();
        let pairs = measure(&mut sim, l, &codes, 0.7, 1).unwrap();
        let (diff, th) = bg.subtract(&pairs, fs, 10.0);
        let est: Vec<(f32, f32)> = if new {
            let (_, cands) = detect_people(&diff, &th, l.width_m, l.depth_m, 0.05, fs, 0.6, c, codes.dilate_m(), 4);
            tk.update(k as f64 * dt, &cands).iter().map(|t| (t.x, t.y)).collect()
        } else {
            let img = backproject(&diff, l.width_m, l.depth_m, 0.05, fs, 0.6, c, Fusion::Sum);
            let p = img.peak();
            vec![(p.0, p.1)]
        };
        if (k as f64) * dt < 1.0 {
            continue; // tracker warm-up
        }
        st.frames += 1;
        for e in &est {
            let d = truth.iter().map(|t| dist(*t, *e)).fold(f32::MAX, f32::min);
            if d > 0.5 {
                st.ghosts += 1;
            }
        }
        for t in &truth {
            let d = est.iter().map(|e| dist(*t, *e)).fold(f32::MAX, f32::min);
            if d > 0.5 {
                st.misses += 1;
            } else {
                st.errs.push(d);
            }
        }
    }
    (st, c)
}

fn show(label: &str, (s, c): (Stats, f32), walkers: usize) {
    let within = s.errs.iter().filter(|&&e| e <= 0.15).count() as f32 / (s.frames * walkers) as f32 * 100.0;
    println!(
        "{label:<48} median {:>3.0} cm · 90% {:>3.0} cm · within 15 cm {:>3.0}% · missed {:>3.0}% · ghosts/frame {:.2} · c {:.1}",
        pct(&s.errs, 0.5) * 100.0,
        pct(&s.errs, 0.9) * 100.0,
        within,
        s.misses as f32 / (s.frames * walkers) as f32 * 100.0,
        s.ghosts as f32 / s.frames as f32,
        c
    );
}

#[test]
#[ignore]
fn acc_probe() {
    let _ = dsp::SPEED_OF_SOUND;
    let mut room = fletch(false);
    room.width_m = 4.0;
    room.depth_m = 3.5;
    room.nodes = vec![
        Node { kind: Kind::Speaker, label: "S1".into(), device: "default".into(), x: 0.55, y: 2.6, facing_deg: 180.0, channel: None },
        Node { kind: Kind::Mic, label: "M1".into(), device: "default".into(), x: 2.05, y: 2.6, facing_deg: 180.0, channel: None },
        Node { kind: Kind::Speaker, label: "S2".into(), device: "default".into(), x: 3.5, y: 2.95, facing_deg: 0.0, channel: None },
        Node { kind: Kind::Mic, label: "M2".into(), device: "default".into(), x: 2.0, y: 0.25, facing_deg: 0.0, channel: None },
    ];
    println!("--- 2 spk + 2 mic demo room, real-time frame rate (15/s)");
    show("new: 1 person", run_at(&room, true, 1, true, 343.0, false, 1.0 / 15.0, 1), 1);
    show("new: 2 people", run_at(&room, true, 2, true, 343.0, false, 1.0 / 15.0, 1), 2);
    for (name, l) in [("3 spk + 1 mic", fletch(false)), ("3 spk + 2 mic", fletch(true))] {
        println!("--- {name}");
        show("old: sum + peak, 1 person, 343 room", run(&l, false, 1, false, 343.0, false), 1);
        show("new: agree + tracker, 1 person", run(&l, true, 1, false, 343.0, false), 1);
        show("new: 2 people", run(&l, true, 2, false, 343.0, false), 2);
        show("old: 1 person, warm room 347, no self-check", run(&l, false, 1, false, 347.0, false), 1);
        show("new: 1 person, warm room 347, self-check", run(&l, true, 1, false, 347.0, true), 1);
        show("new: 1 person, simultaneous chirps", run(&l, true, 1, true, 343.0, false), 1);
        show("new: 2 people, simultaneous chirps", run(&l, true, 2, true, 343.0, false), 2);
    }
}
