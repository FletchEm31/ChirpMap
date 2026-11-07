# ChirpMap

Terminal-native acoustic echolocation that maps rooms like an architect's blueprint, using any speaker and mic. Fully offline. Written in Rust.

ChirpMap plays a short frequency sweep (a *chirp*) through your speaker, records the mic at the same time, and matched-filters the recording to find echoes. Rotate the device through a few headings and it builds a floor plan in your terminal, plus an SVG blueprint with dimensions and a title block.

```
 ##++++. . . . . . . . . . . . . @ . . . ++. ++. . . ##
 ├────────┤ 1 m
 ## wall (measured)   ++ wall (assumed)   . open (measured)   :: unscanned   @ scan spot

 Footprint ≈ 5.26 × 4.14 m   Coverage: measured 11% · assumed 70% · unscanned 19%
```

## Install

```bash
# Linux needs ALSA headers once:
sudo apt install libasound2-dev pkg-config     # Debian/Ubuntu
sudo dnf install alsa-lib-devel                # Fedora

cargo build --release
./target/release/chirpmap --help
```

No network access is used at any point.

## Commands

| Command | What it does |
|---|---|
| `chirpmap devices` | **Hardware panel**: every speaker and mic, its sample rate, channels and format, what chirp band and range resolution it supports, and which ones are already calibrated |
| `chirpmap calibrate` | **Auto-calibration** for the current speaker+mic pair (below). Saves a profile that every later command uses automatically |
| `chirpmap ping [--live]` | One-direction range reading as a bar chart (the original `echo.py` view) |
| `chirpmap scan` | Maps a room: turn the device clockwise through headings (8 × 45° by default), optionally at several spots |
| `chirpmap render scans/scan-….json [--svg out.svg]` | Redraws a saved scan |

Every command takes `--output <# or name>` and `--input <# or name>` to choose devices (default: the system defaults), and `--sim` to run against a simulated 5.2 × 4.1 m room without any audio hardware.

## Auto-calibration

`chirpmap calibrate` adapts to whatever speaker and mic you have:

1. **Noise floor:** listens to the room for 1 s.
2. **Frequency band:** sweeps 1 kHz up to 20 kHz and keeps the widest band at least 10 dB above the noise (cheap laptop speakers often lose the top end).
3. **Volume:** the quietest level that reaches 35 dB SNR without clipping the mic.
4. **Ring-down and latency:** measures how long the speaker keeps ringing (that sets the blanking distance, since nothing closer can be seen) and how much the driver latency jitters. Every ping is aligned on its own direct speaker-to-mic arrival, so latency doesn't affect distances.

Profiles live in `~/.config/chirpmap/profiles/`.

## Scanning a room

```bash
chirpmap scan
```

1. Put the device somewhere open, ideally away from walls, and pick a "forward" direction (0°).
2. Press Enter at each heading, turning clockwise 45° each time.
3. When asked, enter another spot as an `x,y` offset in metres from the first (e.g. `1.5,0`) to fill in gaps, or press Enter to finish.

Each scan saves `scans/scan-<time>.json` (raw readings) and `scans/scan-<time>.svg` (blueprint).

### Measured vs. assumed vs. unscanned

- **Measured:** along the exact heading you pointed, the distance to the first echo is a real measurement. The path up to it is open, and the hit is a wall or object.
- **Assumed:** a speaker's sound spreads over a wide beam (`--beam`, default ±30°), so the echo could have come from anywhere in that cone. Those cells, and wall segments joining neighbouring hits, are drawn as assumed.
- **Unscanned:** cells inside the room's footprint that no reading reached.

Coverage % is computed over the footprint, the bounding box of every detected wall.

## Limits (honest version)

- One speaker and one mic give **distance, not direction**. Direction comes from which way you're pointing the device, which is why the beam cone is marked assumed. Built-in laptop speakers are close to omnidirectional, so expect a coarse map.
- Soft furnishings absorb high frequencies. Glass and bare walls give the strongest echoes.
- Range resolution is about 1–2 cm in theory and more like 5–10 cm in practice.

## Roadmap

- Two-mic arrays (stereo laptop mics, USB mic pairs): time-difference-of-arrival bearing, so the map stops depending on where you point the device
- Phone as a second, moving node
- Sensor fusion with Wi-Fi CSI, mmWave and thermal
- A live ratatui interface

## Origin

Grew out of `echo.py`, a Python proof of concept (chirp, matched filter, terminal bar chart). The same pipeline now lives in `src/dsp.rs`.
