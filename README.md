# slooper

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A minimalist clip-based audio DAW built in Rust for Linux (JACK /
PipeWire-JACK). Sixteen tracks on one global, draggable playhead: drop audio
from the **asset rack** onto a metronome-defined bar grid, record takes that
turn themselves into clips, stack child lanes additively, quantize with one
click, and save whole projects to disk — with MIDI binding, keyboard
shortcuts, per-track effects, and clean export.

**Docs:** [install guide](docs/INSTALL.md) · [user guide](docs/USAGE.md) ·
[agent/automation guide](docs/AGENT-GUIDE.md)

## Features

- **Resizable tracks** — drag the separation bar under any track; only that
  track changes, its lanes always share the height equally, and a new child
  lane grows the track instead of shrinking its siblings. **Ctrl + wheel**
  zooms the timeline, **Shift + wheel** pans horizontally.
- **16-track timeline DAW model** — no per-track clocks or loop modes: one
  global transport and one **always-on playhead** over a bar/beat grid derived
  from the metronome. Press play or stop **anywhere** — the position is kept;
  **drag the playhead** across the ruler or any lane to scrub to the exact
  spot you want to hear. A per-track **play gate** (`PLAYED` / `muted`) mutes
  a track without touching its clips, and the transport bar's **PLAY/STOP**
  button (or `Space`, or bound MIDI) starts/stops everything at once.
- **Clips from the asset rack** — every track is a lane space you drop audio
  into, not a single pre-allocated buffer. Clips are references to audio
  files: drag them from the asset rack onto any lane, drag them along the
  timeline to reposition (with snap), and stack as many as you like — clips
  that overlap simply add together.
- **Child lanes** — every track has a base lane plus any number of child
  lanes (`+ child` / `- child` in the row). Children stack additively on the
  same track mixer (vol, gain, pan, effects). **Ctrl+C / Ctrl+V / Ctrl+D**
  copy, paste and duplicate clips — a paste lands on the track nearest your
  last click, right at the playhead. When a recording or a drop
  would land **on top of existing clip material, a child lane opens
  automatically** — you can never punch a hole in another clip by accident.
- **Record → take → clip** — each track keeps its **REC** button (and its
  keyboard/MIDI binding). Press REC, play, press REC again: the take is
  written as a WAV into the rack's **primary folder** (so it is instantly an
  asset for every future project) and automatically placed as a clip **at
  the playhead** in the recorded-into track — opening a child lane if the
  base lane is busy there. Takes longer than the ~60 s take buffer auto-stop.
- **Asset rack** — a panel on the left of the main window listing audio
  folders (add any number with **+ folder**, reorder, remove with `x`,
  re-scan with `...`). The **first folder is the primary folder**: all
  recordings are stored there. Supported import formats (WAV, MP3, OGG,
  FLAC, M4A, AAC, AIFF) are decoded and resampled to the engine rate on
  demand; per-asset actions quantize, trim silence, or remove.
- **Metronome grid & easy quantize** — the metronome (BPM 20–300, beats per
  measure 1–16, six subdivisions) *is* the project clock: its tempo and
  time-signature define the visible bar grid, the ruler, and the quantize
  grid. Quantization is always metronome-based: pick a **quantize
  subdivision** (quarter … thirty-second, incl. triplets) and one click
  snaps the selected asset's content onto it (zero-crossing refinement +
  seam crossfade keep it click-free); **Trim** clips silence off the start
  and end of an asset; tick **auto-quantize** and every fresh recording is
  quantized the moment it lands. Placed clips snap to a separate **snap
  subdivision** grid while you drag.
- **Projects** — Ctrl+S saves everything (grid, mixer, FX, lanes, clips,
  asset table) into a single JSON `.slooper` file in your projects
  folder; the first save asks for a name. **Save as** re-names, **Load**
  shows every project in the folder newest-first, and the projects folder
  itself is a setting. Audio is never embedded — derived assets are written
  next to the project and missing files are re-found by name.
- **Mixer dock & effects** — a **Mixer** panel docked right of the timeline
  holds every track's full strip, each one pixel-aligned to its track block:
  name, REC, play gate, child-lane buttons, **vol** (−∞…+6 dB), **gain**,
  **pan** (−100…+100, equal power; editable dB / position text boxes, raw
  MIDI readout and a **0** reset button) and **FX…**, all scrolling in
  lockstep with the timeline. The
  **master volume** (top bar) spans the same −∞…+6 dB range. Effects are
  per-track runtime-compiled chains (Rust scripts hot-compiled with rustc,
  per-effect sliders, scripted custom GUIs, user presets) running after
  vol × gain, live and in export renders.
- **Global loop region** — a **loop** toggle plus mode dropdown on the grid
  toolbar: **current longest** (default — wraps at the end of the longest
  playing clip), **whole song**, or **area**, whose start and end show up as
  bright yellow grab bars on the ruler (default bar 1 → bar 2, snap-aware,
  drag anywhere). The playhead
  draws as one unbroken line across all tracks, and with looping off it
  parks at the region end.
- **Bundled TR-808 pack** — fourteen one-shots sampled from a real Roland
  TR-808 (kick, sub, snares, clap, closed/open hats, crash, cowbell, three
  toms, rimshot, clave) seed themselves into the app data dir and open as
  the default rack folder on a fresh install (public-domain lineage; see
  `samples/808/CREDITS.md`).
- **Settings menu** — hamburger (top-left): input/output port picker (per
  output **L** / **R** / both), projects folder, latency
  compensation, MIDI + keyboard bindings for every action.
- **MIDI binding** — every action — per-track record / clear / play-gate,
  track select, **play all** / **stop all**, metronome, export, per-track
  volume / gain / pan knobs — binds to a live-captured Note, CC, or Program
  Change; knob bindings get two-point axis calibration (incl. inverted
  travel). Everything persists between sessions.
- **Keyboard shortcuts** — `Space` play/stop all, `R` record, `C` clear,
  `E` export prompt, `M` metronome, `1`–`4` select track (5–16 bindable),
  Ctrl+S save (Ctrl+Shift+S rename), Ctrl+C/Ctrl+V/Ctrl+D clip
  copy/paste/duplicate. All rebindable; ignored while typing
  in text boxes; auto-repeat never re-fires.
- **Import / export** — export one track (its mixer + chain baked in) or the
  whole project as WAV/FLAC (built in) or MP3/OGG (via ffmpeg).
- **MIDI monitor** — permanent `slooper:midi_in` JACK MIDI port; every
  event parsed and shown in the bottom panel.
- **Latency compensation** — *latency (ms)* discards that many leading
  samples of every fresh take, so the REC click itself lands on the clip
  start. Monitoring and the output mix are never delayed.

## Requirements

- Rust (stable) and `cargo`; **`rustc` at runtime** for building effect
  scripts on the fly (optional — the app runs fine without custom effects)
- Build-time system libraries: JACK headers + `.pc` file (provided by
  `pipewire-jack` or the official `libjack` package), plus `pkgconf`.
  No other system deps — the GUI stack is pure-Rust.
- `ffmpeg` (optional) for MP3/OGG export.
- A JACK-compatible audio server:
  - **PipeWire** (recommended): just install `pipewire` + `pipewire-jack`.
  - or classic **jack2** (`jackd`).

The app opens its JACK client with `NO_START`, so it will never spawn a
server for you — if none is running it shows an error and stays idle.

## Install

**Arch Linux** — once published, from the AUR:

```sh
yay -S slooper        # or paru -S slooper
```

**Any distro** — build from source (prereqs per distro in
[docs/INSTALL.md](docs/INSTALL.md)):

```sh
git clone https://github.com/actually-god/slooper
cd slooper
cargo build --release
./target/release/slooper
```

## Using the GUI

Full walkthrough in [docs/USAGE.md](docs/USAGE.md). The short version:

1. Wait for the JACK readout in the top bar (client name + sample rate).
2. Open the **hamburger** (top-left), tick your inputs and each output's
   **L** / **R** / both, **Apply Selection**. Remembered across launches.
3. Add an audio folder to the **asset rack** (+ folder) — the first one
   doubles as the recordings folder.
4. Set your tempo/meter in the transport bar (that *is* the grid), then
   either drag an asset onto a track lane, or arm a track and press **REC**,
   play, and press REC again — the take lands on the timeline by itself.
5. Press `Space` to run the whole project; drag the playhead to scan.
6. Ctrl+S to save the project once it sounds right.

### Effects

Each track owns an independent effect chain. Click a track's **FX** button:

- **Effects location** — folder scanned for effect scripts; default
  `~/.local/share/slooper/effects`, created and seeded with `template.rs`
  + the ABI contract on first run; **Rescan** compiles edited scripts.
- **The list** — one row per script: toggle (default off), display name,
  author/description hover text; enabled effects snap to the top and ↑↓
  reorder the chain (top-to-bottom *is* audio order). Failing scripts stay
  listed, dimmed, with the compiler error.
- Enabled effects show as **name buttons** on the track row (chain order),
  each opening an editor window — auto sliders or the script's own GUI.

Signal order per track: `clip × volume × gain → effects chain → pan →
output buses`. The same chain runs in export renders.

Writing an effect: copy `template.rs`, rename, edit — one standalone Rust
file that `include!`s `effect_api.rs` and provides `create` / `process` /
`destroy`, a description block (name required, up to 32 parameters), and
optionally a scripted custom GUI. Compiled with
`rustc --edition 2021 --crate-type cdylib`. Working examples in `effects/`:
`parametric_eq.rs` (6-band EQ, live spectrum, draggable log-axis handles)
and `space_echo.rs` (echo + reverb with tone voicing and factory presets).

### MIDI binding

Send events into `slooper:midi_in` (hardware via PipeWire, or
`cargo run --example midi_tester`). Settings → **MIDI bindings**: click an
action's cell (turns blue, *listening…*), send a Note On/Off, CC, or
Program Change — first matching message binds. Velocity/CC value is ignored
for trigger actions, except the **knob** bindings (track vol/gain/pan),
which read the value 0–127 and support the three-step **axis calibration**
wizard (lock min, lock max; inverted travel is handled). Click again / `Esc`
cancels; `Del` clears the binding while listening. **Reset to defaults**
restores the factory table. Old v0.1 shortcut/MIDI tables are migrated
forward automatically (removed actions — overdub, import, dependent-mode,
undo/redo — are dropped).

### Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Space` | play / stop the whole project (transport toggle) |
| `R` | record toggle on the selected track (take → rack + clip) |
| `C` | clear the selected track's clips |
| `E` | export prompt |
| `M` | metronome on / off |
| `1`–`4` | select track 1–4 (5–16 via the action list) |
| `Ctrl+S` | save project (asks for a name the first time) |
| `Ctrl+Shift+S` | save under a new name |

Every key is rebindable; keys are ignored while you type in any text box.

### Quantize

Quantization always follows the **metronome** — set BPM / beats-per-measure
/ subdivision in the transport bar, then:

- **quantize** dropdown — the grid an asset's content snaps to (quarter,
  eighth, triplet, sixteenth, triplet-eighths, thirty-second).
- **Quantize** (asset rack, on the selected asset) — snaps the take's
  start/end onto that grid with zero-crossing refinement (±5 ms) and a seam
  crossfade.
- **Trim** — clips silence off the start and end of the selected asset.
- **auto-quantize** checkbox — every fresh recording is quantized
  automatically when it lands in the rack.
- **snap** dropdown — the grid dragged clips and auto-placed takes snap to.

## Architecture

```
src/
├── main.rs    entry point, eframe window setup
├── lib.rs     crate root: pub mods
├── audio.rs   JACK backend + DAW engine: manager thread, Cmd/Evt channels,
│              RT process handler (clip mixing, takes, metronome, transport)
├── arrange.rs arrangement model: tracks → lanes → clips (asset id + start
│              sample), placement/snap/lane-overlap rules, serialization
├── assets.rs  asset rack: folder scan (audio extensions), recordings into
│              the primary folder, take save/resolve
├── project.rs .slooper save format (versioned JSON), projects dir
│              listing, asset re-resolution by name
├── audio_io.rs import decoding (symphonia) + export (WAV/FLAC native,
│              MP3/OGG via ffmpeg, resampled with rubato)
├── config.rs  JSON config: $XDG_CONFIG_HOME/slooper/config.json
├── samples808.rs  bundled TR-808 pack: embedded WAVs + startup seeding
│              into $XDG_DATA_HOME/slooper/samples/808 (default rack folder)
├── fx.rs      effects framework: scan/compile/cache/load effect scripts,
│              ABI validation, per-track chains, RT-safe parameter relay
├── fxgui.rs   per-effect editor windows: auto sliders or scripted GUIs
├── effect_api.rs  the effect ABI contract (source of truth; also written
│              into every effects folder for scripts to `include!`)
├── midi.rs    MIDI parsing/formatting, MidiBind matching, MidiAction table,
│              keyboard-shortcut layout migration
├── quantize.rs  metronome grids, content quantize, silence trim,
│                zero-crossing refinement + seam crossfade
└── ui.rs      egui app: top bar (transport/project buttons), asset rack,
               grid toolbar, timeline lanes + playhead drag, settings,
               MIDI monitor, export window
```

- `Backend::start()` spawns the **audio manager thread**, which owns the
  `jack::Client` (`NO_START`). GUI ↔ audio run over two bounded channels:
  commands (`Refresh`, `Apply{ins,outs}`, `Quit`, `ToggleRecord`,
  `SetTrackOn`/`ToggleTrackOn`, `PlayAll`, `StopAll`, `Seek`, `SetProject`,
  `ExportTrack`, `ExportProject`, `SetTrackEffects`, `SetLatency`,
  `SetMetro`, …) and events (`Ready`, `Ports`, `Applied`, `Midi`,
  `MidiSources`, `Xrun`, `Transport`, `TrackState`, `Take { track, at,
  data }`, `Notice`, errors).
- The client is **never deactivated**: a fixed pool of 16 in / 16 out ports
  is created once; `Apply` only (dis)connects them. Connections retry
  (10 × 25 ms) because PipeWire-JACK rejects connections to
  freshly-activated ports for ~300 ms.
- The realtime `ProcessHandler` walks every lane's clip list and sums the
  active clips into the track bus — no locks, no allocation: the
  arrangement arrives as one `Arc<Arrangement>` swap per edit
  (`Cmd::SetProject`), track states/transport/seek ride atomics, and the
  playhead advances every block while the transport *or* the metronome is
  on (a bare click keeps the always-visible playhead rolling; the click
  itself is output-only, never recorded).
- **Takes**: recording writes into the shared take buffer; when a take
  finishes (REC press or capacity) the engine hands the audio to the
  manager thread (`Evt::Take`), which saves the WAV into the primary rack
  folder, appends the asset, and places the clip at the take's start
  position — opening a child lane when the base lane is occupied there.
- **Loop region**: the engine keeps `loop_start`/`loop_end` samples and a
  mode (area / longest / whole song). `region_end()` resolves the wrap point
  each block (longest = furthest playing clip end, min = the region's end);
  the playhead wraps there, or parks and stops when looping is off. The UI
  pushes the region whenever the handles move; area handles drag on the ruler
  with snap, mirrored live from the RT position stream so dragging keeps up.

## Packaging (Arch / AUR)

The Arch `PKGBUILD` **does not live in this repository** (AUR convention: the
packaging recipe is maintained in its own AUR package repo, `slooper`,
building from this project's GitHub **release tag tarball**). Once the package
is published, see [docs/INSTALL.md](docs/INSTALL.md) for install/build
instructions.

Repo-side obligations that keep the package healthy:

- Every release is a **tag** (`git tag v0.3.0 && git push origin v0.3.0`) —
  the PKGBUILD's `source=` URL points at that tarball, so don't force-move
  public tags once the AUR package references them; release new versions
  instead.
- `assets/icon.png` — **placeholder** blank icon; drop the final artwork over
  this file (the package installs whatever is in it).
- `slooper.desktop` — application-menu entry (`Icon=slooper`).
- `tests/packaging.sh` — packaging regression test (PKGBUILD syntax, required
  fields, version sync with Cargo.toml, referenced files, desktop entry,
  icon, and dependency resolvability). It reads the PKGBUILD from the AUR
  checkout: `PKGBUILD_PATH=~/aur/slooper/PKGBUILD ./tests/packaging.sh --full`.

⚠️ Packaging footguns that still apply when testing from an AUR checkout:
never run `makepkg -C` inside a *copy of this project* (`-C` wipes `$PWD/src`,
i.e. the Rust sources), and prefer a dedicated build dir over in-tree builds —
makepkg's default `$srcdir=$PWD/src` collides with Rust's own `src/`.

## Tests

```sh
cargo test -- --test-threads=1
```

Unit tests (85) cover MIDI parsing/binding and shortcut-table migration,
quantize/silence-trim math, the arrangement model (placement, snapping,
child-lane creation on overlap, removal rules), the asset store, the
`.slooper` project format, and engine render/mixing. The e2e suite (7
tests) drives a real JACK client pair in real time: MIDI monitor, audio
mixing/routing, the global transport (play/stop anywhere, seek, loop wrap),
the metronome grid, the record → take → clip pipeline, and export. Tests
self-skip when no JACK/PipeWire server is running; the e2e tests serialize
on a shared lock (`E2E_LOCK`) and need a running server.
Packaging is covered by `tests/packaging.sh` (see the Packaging section).

## License

slooper is distributed under the **MIT license**
([LICENSE](LICENSE)) — free to use, modify, and redistribute, commercial use
included, as long as the copyright notice travels with substantial copies.

## Roadmap

- [x] 16 tracks with per-track mixer (vol/gain/pan) + effects chains
- [x] Action mapping: MIDI note/CC/PC → any action (live-captured bindings)
- [x] Global transport with draggable playhead + loop wrap
- [x] Clip timeline: asset rack drag & drop, snapping, additive child lanes
- [x] Record → asset + auto-placed clip (child lane on overlap)
- [x] Metronome-based quantize, silence trim, auto-quantize on record
- [x] `.slooper` project files (named save, load browser, projects folder)
- [x] Keyboard shortcuts for track + transport actions
- [ ] Clip-level fades, split-at-playhead, per-clip gain strips
