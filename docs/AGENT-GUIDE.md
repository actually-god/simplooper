# AGENT-GUIDE — simplooper for AI agents & automation

Machine-readable orientation for agents asked to install, use, build, or
extend this repo. Humans: see [../README.md](../README.md),
[INSTALL.md](INSTALL.md), [USAGE.md](USAGE.md).
(This file follows the `AGENTS.md` convention — if your tooling looks for a
root `AGENTS.md`, this is it.)

## What this is

- **simplooper**: a small clip-based **DAW** for Linux — Rust, egui/eframe
  GUI, JACK audio (works against PipeWire's JACK shim or jack2).
- 16 tracks on one global, draggable playhead over a metronome-defined bar
  grid; asset-rack drag & drop; additive child lanes; record → take → asset
  + auto-placed clip; metronome-based quantize/trim; `.simplooper` project
  files; MIDI + keyboard binding; per-track runtime-compiled Rust effects;
  WAV/FLAC/MP3/OGG export. (v0.1's loop-pedal model — per-loop buffers,
  overdub, independent/dependent modes — was replaced in v0.2.)
- License: **MIT** ([../LICENSE](../LICENSE)). Upstream:
  `https://github.com/actually-god/simplooper` (branch `main`, releases are
  semver tags `v<X.Y.Z>`; version of record is `version` in `Cargo.toml`).

## Install (headless-safe summary)

```sh
# Arch: PKGBUILD lives in the AUR package repo, NOT here:
git clone https://aur.archlinux.org/simplooper.git && (cd simplooper && makepkg -si)

# Any distro: needs stable Rust, cargo, pkgconf, JACK headers (see INSTALL.md)
cargo build --release && ./target/release/simplooper
```

The app connects to JACK with `NO_START` — it never spawns a server; a running
PipeWire/jack2 is required, or it logs an error and idles. GUI needs a
display (X11 or Wayland).

## Repository layout

```
src/            application source (Rust 2021 edition)
  main.rs         entry point, window setup
  lib.rs          crate root (pub mods)
  audio.rs        JACK backend + realtime DAW engine (Cmd/Evt channels);
                  clip-lane mixing, take recording (Evt::Take handshake),
                  global transport/seek/loop-wrap, metronome, stereo buses
                  with per-track equal-power pan + per-output L/R routing
  arrange.rs      arrangement model: tracks → lanes → clips
                  (asset id + start sample); insert/move/remove, snap,
                  auto child-lane on overlap, lane removal rules
  assets.rs       asset rack store: folder scan (audio extensions), take
                  WAVs into the primary folder, name resolution
  project.rs      `.simplooper` save format (versioned JSON), projects-dir
                  listing, audio re-resolution by file name
  audio_io.rs     import (symphonia: wav/mp3/ogg/flac/m4a/aac/aiff…) +
                  export (WAV/FLAC native; MP3/OGG shell out to ffmpeg)
  config.rs       JSON persistence: $XDG_CONFIG_HOME/simplooper/config.json
                  (rack folders, projects dir, quantize/snap, mixer, FX,
                  shortcut + MIDI tables)
  effect_api.rs   the effect-script ABI contract (also copied into every
                  user effects folder; scripts include! this file)
  fx.rs           effects scan/compile/cache/load, ABI validation, chains
  fxgui.rs        child editor windows (auto sliders or scripted GUIs)
  midi.rs         MIDI message parsing, MidiBind matching, MidiAction
                  table, keyboard-shortcut layout migration (v0.1 → v0.2)
  ports.rs        JACK port enumeration helpers
  quantize.rs     metronome grid math, content quantize, silence trim,
                  zero-crossing refinement + seam crossfade
  samples808.rs   bundled TR-808 pack: WAVs embedded via include_bytes!,
                  seeded to $XDG_DATA_HOME/simplooper/samples/808 on startup
                  (size-compare, so deletions self-heal); default rack folder
  ui.rs           the whole egui app (top bar transport, asset rack, grid
                  toolbar with loop controls, seamless per-track lane blocks
                  + ruler + loop handles + playhead scrub, Mixer dock aligned
                  per track, copy/paste buffer, settings, project
                  prompt/browser, export, MIDI monitor)
effects/        effect-script authoring kit: template.rs (start here),
                effect_api.rs (ABI), parametric_eq.rs, space_echo.rs examples
examples/       small dev utilities run via `cargo run --example <name>`
tests/e2e.rs    end-to-end tests that drive a LIVE JACK server
tests/packaging.sh  packaging regression test (needs a PKGBUILD; see below)
assets/icon.png 256x256 app icon (placeholder artwork)
simplooper.desktop  freedesktop menu entry
docs/           INSTALL.md, USAGE.md (human docs)
```

## Build & test commands

| Command | Notes |
| --- | --- |
| `cargo build --release` | release profile: LTO + panic=abort; ~15 MB binary |
| `cargo test -- --test-threads=1` | full suite (85 lib + 7 e2e); e2e tests **self-skip** without a live JACK server |
| `cargo clippy --all-targets` | repo standard: zero warnings |
| `PKGBUILD_PATH=/path/to/aur/checkout/PKGBUILD ./tests/packaging.sh` | packaging checks (the PKGBUILD now lives in the AUR package repo) |

Unit tests cover MIDI parsing + shortcut migration, quantize/trim math, the
arrangement model, asset store, project format, and engine renders. The e2e
tests drive real JACK clients in real time (transport, seek, loop wrap,
metronome grid, record→take, export, MIDI monitor, mixing/routing).
**Never remove the `E2E_LOCK` mutex serialization in `tests/e2e.rs`** —
concurrent e2e runs starve each other's JACK clients ("ensemble stalled").

## Runtime state (outside the repo)

- Config: `$XDG_CONFIG_HOME/simplooper/config.json` (fallback `~/.config/…`)
- Projects: `~/.local/share/simplooper-projects/` by default (setting in
  the config: `project_dir`); one `<name>.simplooper` JSON per project,
  plus a `<name> audio/` sidecar folder for derived assets at save time
- Rack folders: `rack_folders: [...]` — **entry 0 is the primary folder**
  where take WAVs are recorded
- Effects folder: `~/.local/share/simplooper/effects/` (configurable)

## Invariants an agent must not break

1. **Realtime safety**: the JACK `ProcessHandler` must stay lock-free and
   allocation-free; GUI↔audio only via bounded Cmd/Evt channels + atomics.
   The arrangement crosses as one `Arc<Arrangement>` per edit
   (`Cmd::SetProject`); never read GUI state from RT.
2. **`MidiAction::ALL` is position-persistent** — persisted bindings index it
   by slot. New actions may only be APPENDED, never inserted or reordered.
   The keyboard table persists positionally too: layout changes require a
   width bump + a migration arm in `midi::migrate_shortcuts`.
3. **Behavior specs live in tests and docs** (`README.md`, `docs/USAGE.md`,
   `IDEA.md`, `tests/e2e.rs`). Current pins: the metronome defines the grid
   and can run the playhead with the transport stopped (metro on + transport
   off = click track, playhead rolls, clips silent); the click is
   output-only, never recorded; a take always becomes a rack asset **and**
   an auto-placed clip, and overlaps open child lanes instead of touching
   existing clips; quantize is always metronome-based; the primary rack
   folder is a hard prerequisite for recording (the seeded 808 pack supplies
   one by default); looping is a user toggle (default on, mode
   *current longest*) with area / longest / whole-song modes — area handles
   drag with snap; paste targets
   the track nearest the last click at the playhead. Code aligns to them;
   changing behavior means deliberately updating tests + docs.
   **Track geometry is shared state**: the timeline and the Mixer dock draw
   from `App::track_h(t)` (user height from `trk_height`, else auto =
   `lanes × LANE_H`, floor `lanes × LANE_MIN_H`/`TRACK_MIN_H`) plus the
   per-frame `grid_anchor` (top of the track stack) and shared `vscroll` —
   if you touch track layout, both consumers must agree. The separation bar
   under each track is also its resize handle (`TRACK_SEP_HIT`).
4. **Audio server**: keep `NO_START`; never auto-spawn jackd or bypass JACK.
5. **Packaging**: the AUR `PKGBUILD` (separate repo) fetches the GitHub
   **tag tarball**; `pkgver` must equal the `Cargo.toml` version, and
   `sha256sums` must match the tarball of the tag currently pushed. GitHub
   regenerates tag assets lazily — download and hash the live tarball to
   confirm before trusting a checksum. Never run `makepkg -C` inside a
   checkout (`-C` wipes `$PWD/src` = the Rust sources).
6. **Releases**: source commit on `main` → tag `vX.Y.Z` → push tag → update
   the AUR repo (`pkgver`/`pkgrel` + fresh `makepkg --printsrcinfo`). Do not
   force-move public tags after the AUR package references them.

## Where things are wired (quick grep targets)

- Commands/events: `Cmd::` / `Evt::` in `src/audio.rs` (take placement
  lands in the manager thread's `Evt::Take` handler)
- Clip/lane rules: `src/arrange.rs` (`insert`, `place_auto`, `remove_lane`)
- Loop region math: `loop_wrap`/`region_end` (`src/audio.rs`); handles +
  modes in the grid toolbar (`src/ui.rs`, `LoopMode`)
- Project format + migration: `src/project.rs`, `midi::migrate_shortcuts`
- All bindable actions + key defaults: `MidiAction::ALL`, `SHORTCUT_DEFAULTS` (`src/ui.rs`)
- Effect ABI (audio v1 + GUI v1): `src/effect_api.rs`
- Import formats: `IMPORT_EXTENSIONS` (`src/audio_io.rs`)
- Export formats: `ExportFormat` (`src/audio_io.rs`)

## Docs map

| Audience | Read |
| --- | --- |
| end user | [USAGE.md](USAGE.md) |
| sysadmin / packager | [INSTALL.md](INSTALL.md) |
| effect-script author | `effects/template.rs` (self-documenting) + *Track effects* in USAGE.md |
| CI/automation agents | this file |
