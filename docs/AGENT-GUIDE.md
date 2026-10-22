# AGENT-GUIDE — slooper for AI agents & automation

Machine-readable orientation for agents asked to install, use, build, or
extend this repo. Humans: see [../README.md](../README.md),
[INSTALL.md](INSTALL.md), [USAGE.md](USAGE.md), [PLUGINS.md](PLUGINS.md).
(This file follows the `AGENTS.md` convention — if your tooling looks for a
root `AGENTS.md`, this is it.)

## What this is

- **slooper**: a small clip-based **DAW** for Linux — Rust, egui/eframe
  GUI, JACK audio (works against PipeWire's JACK shim or jack2).
- 16 tracks (count is a setting) on one global, draggable playhead over a
  metronome-defined bar grid; asset-rack drag & drop; additive child lanes;
  audio takes **and MIDI takes (MREC)** → asset + auto-placed clip; clip
  edge trimming (cut / silent extension); a full piano-roll editor;
  per-track **synths** (bundled modal piano, sampler script, scanned VSTi);
  one **mixer chain** per track mixing runtime-compiled Rust effect scripts
  with binary **VST2/VST3 plugins**; group editing + clipboard;
  metronome-based quantize/trim; `.slooper` project files; MIDI + keyboard
  binding; WAV/FLAC/MP3/OGG export incl. current-selection export.
  (v0.1's loop-pedal model — per-loop buffers, overdub,
  independent/dependent modes — was replaced in v0.2.)
- License: **MIT** ([../LICENSE](../LICENSE)). Upstream:
  `https://github.com/actually-god/slooper` (branch `main`, releases are
  semver tags `v<X.Y.Z>`; version of record is `version` in `Cargo.toml`).

## Install (headless-safe summary)

```sh
# Arch: PKGBUILD lives in the AUR package repo, NOT here:
git clone https://aur.archlinux.org/slooper.git && (cd slooper && makepkg -si)

# Any distro: needs stable Rust, cargo, pkgconf, JACK headers (see INSTALL.md)
cargo build --release && ./target/release/slooper
```

The app connects to JACK with `NO_START` — it never spawns a server; a running
PipeWire/jack2 is required, or it logs an error and idles. GUI needs a
display (X11 or Wayland); native VST2 plugin editor windows additionally
require an X11 display (XWayland counts).

## Repository layout

```
src/            application source (Rust 2021 edition)
  main.rs         entry point, window setup
  lib.rs          crate root (pub mods)
  audio.rs        JACK backend + realtime DAW engine (Cmd/Evt channels);
                  clip-lane mixing, take recording (Evt::Take handshake),
                  global transport/seek/loop-wrap, metronome, per-track
                  synth + chain playback, stereo buses with per-track
                  equal-power pan + per-output L/R routing, offline renders
  arrange.rs      arrangement model: tracks → lanes → clips
                  (asset id + start sample + `cut` trim offset + `play_len`
                  — `play_len > avail` renders a silent extension);
                  insert/move/remove, snap, edge-trim math, auto child-lane
                  on overlap, lane removal rules
  assets.rs       asset rack store: folder scan (audio + .mid), take WAVs
                  into the primary folder, name resolution
  midipat.rs      SMF format-0 MIDI pattern model (PPQ 480; notes with
                  channel pressure, CC, channel pressure; read + write;
                  pattern length survives via a private text meta)
  synth.rs        scripted synth plugins (rust-script): own ABI v1 + host
                  vtable, global-Mutex compile cache, bundled piano (modal
                  physical model) + sampler seeded/updated-in-place in the
                  synths folder (SLOOPER_SYNTHS_DIR override)
  vsthost.rs      VST2/VST3 hosting: folder scan, block-based processing
                  (live RT 512-block queue + offline renders), parameter
                  relay via generation counter (VST2 via the patched
                  vendor/vst2 crate, VST3 via vst3-host; plugin libraries
                  are NEVER dlclosed)
  vstedit.rs      VST2 editor hosting (effEditOpen/Idle/GetRect/Close over
                  an X11 plug/replug container window, idle pump thread;
                  see PLUGINS.md)
  project.rs      `.slooper` save format (versioned JSON), projects-dir
                  listing, audio re-resolution by file name
  audio_io.rs     import (symphonia: wav/mp3/ogg/flac/m4a/aac/aiff…) +
                  export (WAV/FLAC native; MP3/OGG shell out to ffmpeg)
  config.rs       JSON persistence: $XDG_CONFIG_HOME/slooper/config.json
                  (rack folders, projects dir, quantize/snap, mixer,
                  `track_chain` + `track_synth`(+params), `vst_dirs`,
                  `n_tracks`, shortcut + MIDI tables + `midi_axis` cal)
  effect_api.rs   the effect-script ABI contract (also copied into every
                  user effects folder; scripts include! this file)
  fx.rs           effects scan/compile/cache/load, ABI validation, chains,
                  hand-rolled user-preset JSON
  fxgui.rs        child editor windows (auto sliders or scripted GUIs)
  midi.rs         MIDI message parsing, MidiBind matching, MidiAction
                  table, keyboard-shortcut layout migration (v0.1 → v0.2)
  ports.rs        JACK port enumeration helpers
  quantize.rs     metronome grid math, content quantize, silence trim,
                  zero-crossing refinement + seam crossfade
  samples808.rs   bundled TR-808 pack: WAVs embedded via include_bytes!,
                  seeded to $XDG_DATA_HOME/slooper/samples/808 on startup
                  (size-compare, so deletions self-heal); default rack folder
  ui.rs           the whole egui app (top bar transport, asset rack, grid
                  toolbar with loop/snap/♫ Piano roll controls, seamless
                  per-track lane blocks + ruler + loop handles + playhead
                  scrub + clip edge drag, Mixer dock aligned per track
                  (strips, MIXER CHAIN window, synth bindings, VST plugin
                  picker), group selection + clipboard, piano-roll window,
                  settings, project prompt/browser, export, MIDI monitor)
synths/         bundled synth scripts (piano.rs, sampler.rs) + synth_api.rs
                + THIRD-PARTY-NOTICES.md (piano model credits)
effects/        effect-script authoring kit: template.rs (start here),
                effect_api.rs (ABI), parametric_eq.rs, space_echo.rs examples
vendor/vst2/    in-tree patched VST2 crate (see VENDOR-NOTES.md; keep the
                entry-name + chunk patches when vendoring anything newer)
examples/       small dev utilities run via `cargo run --example <name>`
tests/e2e.rs    end-to-end tests that drive a LIVE JACK server
tests/packaging.sh  packaging regression test (needs a PKGBUILD; see below)
assets/icon.png 256x256 app icon (placeholder artwork)
slooper.desktop  freedesktop menu entry
docs/           INSTALL.md, USAGE.md, PLUGINS.md (human docs), this file
```

## Build & test commands

| Command | Notes |
| --- | --- |
| `cargo build --release` | release profile: LTO + panic=abort; ~13–15 MB binary |
| `cargo test -- --test-threads=1` | full suite (128 lib + 1 bin + 8 e2e); e2e tests **self-skip** without a live JACK server |
| `cargo test --release` | the release suite is the clean reference — the DEBUG lib suite has a known intermittent glibc `corrupted size vs. prev_size` abort inside dlopen (~15–30 % of runs) unrelated to correctness; verify with `--release` before chasing |
| `cargo clippy --all-targets` | repo standard: zero warnings (check with `... 2>&1 | grep -cE "^warning: \|^error"` = 0 — grepping just `^error` false-passes) |
| `PKGBUILD_PATH=/path/to/aur/checkout/PKGBUILD ./tests/packaging.sh` | packaging checks (the PKGBUILD now lives in the AUR package repo) |

Unit tests cover MIDI parsing + shortcut migration, the SMF pattern
round-trip, synth script load/render, quantize/trim math, the arrangement
model (incl. edge trimming), chain persistence, asset store, project format,
engine renders, and the egui layer headlessly (real clicks/drags through
`App::ui`). Feature work lands as **acceptance pins** named
`vNN_<area>_<behavior>` (e.g. `v07_roll_fills_scrolls_gates_and_closes`)
that drive the real UI, not the implementer directly — run `cargo test
vNN_` first. The e2e tests drive real JACK clients in real time (transport,
seek, loop wrap, metronome grid, record→take, MIDI capture + sinco fire, VST
processing in export, export, MIDI monitor, mixing/routing). **Never remove
the `E2E_LOCK` mutex serialization in `tests/e2e.rs`** — concurrent e2e runs
starve each other's JACK clients ("ensemble stalled"). Tests never open
JACK for app startup (`SLOOPER_NULL_AUDIO=1` in `test_app`).

## Runtime state (outside the repo)

- Config: `$XDG_CONFIG_HOME/slooper/config.json` (fallback `~/.config/…`)
- Projects: `~/.local/share/slooper-projects/` by default (setting in
  the config: `project_dir`); one `<name>.slooper` JSON per project,
  plus a `<name> audio/` sidecar folder for derived assets at save time
- Rack folders: `rack_folders: [...]` — **entry 0 is the primary folder**
  where take WAVs / MREC `.mid` / piano-roll `roll.mid` are recorded
- Effects folder: `~/.local/share/slooper/effects/` (configurable);
  app-shipped scripts **update in place** when stale (`.bak` backup)
- Synth scripts: `~/.local/share/slooper/synths/` (same in-place update;
  `SLOOPER_SYNTHS_DIR` override); compile cache `~/.cache/slooper-synth`
- VST scan dirs: config `vst_dirs` (defaults `~/.vst`, `~/.vst3`, system
  dirs; yabridge dirs scan transparently)

## Invariants an agent must not break

1. **Realtime safety**: the JACK `ProcessHandler` must stay lock-free and
   allocation-free; GUI↔audio only via bounded Cmd/Evt channels + atomics.
   The arrangement crosses as one `Arc<Arrangement>` per edit
   (`Cmd::SetProject`); never read GUI state from RT. Replaced chain/plugin
   entries are HELD on the RT thread and reaped on the UI thread
   (`drain_vst_held`).
2. **Plugin/effect libraries are NEVER dlclosed** (`ManuallyDrop` handle):
   dlclose frees a plugin's embedded Rust runtime bookkeeping while host
   teardown destructors still run → heap-corruption abort at exit.
3. **`MidiAction::ALL` is position-persistent** — persisted bindings index it
   by slot. New actions may only be APPENDED, never inserted or reordered
   (MIDI rec was appended: `N_ACTIONS = 137`). The keyboard table persists
   positionally too: layout changes require a width bump + a migration arm
   in `midi::migrate_shortcuts`.
4. **Behavior specs live in tests and docs** (`README.md`, `docs/USAGE.md`,
   `IDEA.md`, `tests/e2e.rs`, and the `vNN_*` pins). Current pins: the
   metronome defines the grid and runs on its own free-running clock while
   the transport is stopped (metro on + transport off = click track,
   playhead frozen, clips silent); the click is
   output-only, never recorded; a take always becomes a rack asset **and**
   an auto-placed clip, and overlaps open child lanes instead of touching
   existing clips; quantize is always metronome-based; the primary rack
   folder is a hard prerequisite for recording (the seeded 808 pack supplies
   one by default); looping is a user toggle (default on, mode
   *current longest*) with area / longest / whole-song modes — area handles
   drag with snap; paste targets the track nearest the last click at the
   playhead **and scrolls the view to the pasted clips**; group clipboard
   preserves trims + relative offsets; clip edges re-expose cut audio
   (SIGNED cut math) and extend into silent padding; the piano roll follows
   the main bpm, gates main shortcuts while focused, and its X closes only
   the roll; MREC records only UNBOUND messages. Code aligns to them;
   changing behavior means deliberately updating tests + docs.
   **Track geometry is shared state**: the timeline and the Mixer dock draw
   from `App::track_h(t)` (user height from `trk_height`, else auto =
   `lanes × LANE_H`, floor `lanes × LANE_MIN_H`/`TRACK_MIN_H`) plus the
   per-frame `grid_anchor` (top of the track stack) and shared `vscroll` —
   if you touch track layout, both consumers must agree. The separation bar
   under each track is also its resize handle (`TRACK_SEP_HIT`).
5. **Adding a `Config` field**: `#[serde(default)]` covers deserialization
   but NOT struct literals — update every `Config { .. }` literal in
   `config.rs` (Default impl + round-trip test) in the same edit.
6. **Audio server**: keep `NO_START`; never auto-spawn jackd or bypass JACK.
7. **Packaging**: the AUR `PKGBUILD` (separate repo) fetches the GitHub
   **tag tarball**; `pkgver` must equal the `Cargo.toml` version, and
   `sha256sums` must match the tarball of the tag currently pushed. Version
   bumps commit `Cargo.lock` in the same push (`--locked` builds). GitHub
   regenerates tag assets lazily — download and hash the live tarball to
   confirm before trusting a checksum. Never run `makepkg -C` inside a
   checkout (`-C` wipes `$PWD/src` = the Rust sources).
8. **Releases**: source commit on `main` → tag `vX.Y.Z` → push tag → update
   the AUR repo (`pkgver`/`pkgrel` + fresh `makepkg --printsrcinfo`). Do not
   force-move public tags after the AUR package references them.

## Where things are wired (quick grep targets)

- Commands/events: `Cmd::` / `Evt::` in `src/audio.rs` (take placement
  lands in the manager thread's `Evt::Take` handler)
- Clip/lane rules: `src/arrange.rs` (`insert`, `place_auto`, `remove_lane`)
  + edge drag in `src/ui.rs` (`ClipResize`, signed `new_cut`)
- MIDI pattern IO: `Pattern::save_mid` / `Pattern::load_mid` (`src/midipat.rs`)
- Synth bindings: `App::track_synth` + `synth::Registry` (`src/synth.rs`);
  VSTi bindings encode as `vst2:<abs path>` / `vst3:<abs path>`
  (`vst_synth_encode`)
- Mixer chain: `FxSlot` (`src/audio.rs`), `App::fx_rows` / `rebuild_fx` +
  the chain window (`src/ui.rs`), persistence `ChainSave` / `Config::chain`
  (migrates legacy `track_fx` + `track_vst`)
- VST scanning/editing: `src/vsthost.rs` (`scan`, `default_vst_dirs`),
  `src/vstedit.rs`
- Loop region math: `loop_wrap`/`region_end` (`src/audio.rs`); handles +
  modes in the grid toolbar (`src/ui.rs`, `LoopMode`)
- Project format + migration: `src/project.rs`, `midi::migrate_shortcuts`
- All bindable actions + key defaults: `MidiAction::ALL`, `SHORTCUT_DEFAULTS` (`src/ui.rs`)
- Effect ABI (audio v1 + GUI v1): `src/effect_api.rs`; synth ABI:
  `synths/synth_api.rs`
- Import formats: `IMPORT_EXTENSIONS` (`src/audio_io.rs`)
- Export formats: `ExportFormat` (`src/audio_io.rs`)

## Docs map

| Audience | Read |
| --- | --- |
| end user | [USAGE.md](USAGE.md) |
| sysadmin / packager | [INSTALL.md](INSTALL.md) |
| effect-script author | `effects/template.rs` (self-documenting) + *Track effects & plugins* in USAGE.md |
| synth-script / VST author | [PLUGINS.md](PLUGINS.md) + `synths/synth_api.rs` |
| CI/automation agents | this file |
