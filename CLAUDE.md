# ChirpMap: notes for Claude Code

Terminal-native acoustic echolocation in Rust: plays a chirp through any speaker, records the mic, matched-filters
the echoes and draws a blueprint-style room map in the terminal (plus an SVG). Single crate, `src/`. Public repo.
Owner: Fletch (GitHub `FletchEm31`).

## Rules
- Commit and push straight to `main`: no branches, no pull requests.
- Public repo: no secrets, no personal paths, device serials or recordings of real rooms with people in them.
- The README is the public face of the project. When behaviour changes, update the README in the same commit.

## Build and test
- Linux needs the ALSA headers once (see README "Install").
- `cargo build --release`, `cargo test`
- Simulator accuracy probe: `cargo test --release acc_probe -- --ignored --nocapture`
- Real speaker/mic testing happens on the owner's desktop; a cloud session can't run audio.

## Tasks and handoffs live in the Obsidian vault
Tasks, session handoffs and planning notes live in the owner's Obsidian vault on the desktop,
`~/Obsidian/BlackholeNetwork-Notes`, not in this repo:
- Before starting: read the vault's `CLAUDE.md`, then `Projects/Repos/ChirpMap/Tasks.md` and the newest note in
  `Projects/Repos/ChirpMap/Handoffs/`.
- At the end: update that `Tasks.md` and write a handoff there (format in the vault's `CLAUDE.md`). Don't add
  session records or to-do lists to this repo.
- From a cloud session, reach the vault through the desktop bridge. No vault access: end the reply with the Tasks
  changes and the handoff in markdown so the owner can paste them in.
- The rules in this file win for work in this repo.
