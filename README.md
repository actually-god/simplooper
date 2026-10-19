# simplooper

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A minimalist live audio looper built in Rust for Linux (JACK / PipeWire-JACK).
Sixteen simultaneous stereo-aware loops with record/overdub, waveform
dashboard, MIDI binding, per-loop pan and L/R output routing,
independent/dependent loop modes, an easy quantize tool, a metronome,
import/export, and runtime-compiled per-loop effects.

**Docs:** [install guide](docs/INSTALL.md) · [user guide](docs/USAGE.md) ·
[agent/automation guide](docs/AGENT-GUIDE.md)

## Features

- **16-loop engine** — each track has its own pre-allocated buffer (~60 s at
  48 kHz) and runs fully in the realtime process callback: record (fresh
  capture), overdub (play while writing live input on top), play, clear.
- **Overdub undo/redo** — `UNDO` / `REDO` buttons in the main window (keys
  `U` / `Y`, both rebindable and MIDI-bindable) revert or re-apply the most
  recent overdub: the loop it was dubbed over goes back to the exact content
  it had before that overdub started. Each overdub start snapshots the track,
  8 levels deep per direction; re-recording, clearing, importing or
  quantizing invalidates the affected history so discarded audio is never
  resurrected.
- **Dashboard** — the main window is a scrollable stack of sixteen track rows,
  each showing the recorded waveform plus REC / OD / PLAY / CLEAR / IMPORT
  buttons. Every row also carries per-loop **vol**, **gain** and **pan**
  controls: the text boxes are editable (type a dB value or pan 0–100 and
  press Enter), the number beside each slider is the raw MIDI value. Volume
  trims playback (0 = −∞ … 127 = 0 dB), gain trims in/out (80 = 0 dB,
  127 = +10 dB), and pan positions the loop across the stereo L/R buses
  (0 = hard left, 50 = centre, 100 = hard right, equal power). They only
  shape playback/monitoring — recorded audio itself is never re-leveled.
- **Settings menu** — the three-bar (hamburger) button in the top-left opens
  the settings panel: input/output port selection (each output gets **L**,
  **R** or both — both plays the mono downmix), master gain, latency
  compensation, and per-action MIDI + keyboard bindings. Port selections are
  persisted between sessions and automatically re-applied when JACK comes up,
  so audio flows on launch without re-selecting.
- **MIDI binding** — every action (loop select, record/overdub/clear/play-stop
  on the selected loop, direct per-loop controls, dependent toggle) can be
  bound to a live-captured Note On/Off, CC, or Program Change (channel +
  note/controller/program). Bindings are recorded by clicking the cell and
  sending a message; velocity/CC value is ignored so pedals and knobs fire
  anywhere — except the per-loop *volume* / *gain* / *pan* **(knob)**
  bindings, which read the message value (0–127: 0 = off/−∞, 127 = max) and
  support **two-point axis calibration** (sweep the knob to its physical
  min/max so odd controllers use their whole travel, inverted axes
  included). All bindings persist between sessions.
- **Keyboard shortcuts** — keys `1`–`4` select a loop (the selected row is
  highlighted); `space` play/stop, `R` record, `O` overdub, `C` clear act on
  the selected loop; `D` toggles dependent mode; `M` toggles the metronome;
  `U`/`Y` undo/redo the last overdub. Every shortcut can be reassigned in
  Settings. Keys are ignored while a text field has focus, and key auto-repeat
  never re-fires an action.
- **Loop modes** — a checkbox in the dashboard switches between:
  - *independent*: each track plays on its own clock (sixteen independent
    players);
  - *dependent*: the longest track defines the master; shorter tracks wrap on
    top of it (modulo tiling), and the ensemble clock wraps after a settable
    number of "root loops" (set in the main window, default 1) — the ensemble
    loops forever, never stopping in the middle. A tooltip warns to quantize
    first or you'll hear skips at the loop points.
- **Metronome** — a ▶/■ METRO button with BPM (20–300), beats-per-measure
  (1–16, configurable time-signature numerator), and subdivision (quarter
  through thirty-second, incl. triplets). Three tones: high downbeat, mid
  quarter notes, low subdivisions. The click goes to your outputs only and
  is never recorded. The quantizer can lock to the metronome's tempo.
- **Easy quantize** — lives in the main window (below the mode row): enter
  Loop 1's time signature (numerator/denominator), how many bars Loop 1 spans,
  and "quantize to beat/bar"; every recorded loop is snapped to that grid
  (nearest multiple → zero-crossing refinement within ±5 ms → short
  crossfade at the seam).
- **Import / export** — import WAV, MP3, OGG, FLAC, M4A, AAC or AIFF files
  into any loop (decoded + resampled to engine rate). Export any loop — or
  the whole dependent-mode project — through the full effect chain as
  WAV/FLAC (built in) or MP3/OGG (via ffmpeg).
- **MIDI monitor** — a permanent `simplooper:midi_in` JACK MIDI port; every
  incoming event is parsed and shown in the bottom panel and on stdout.
- **Live mix passthrough & monitoring** — selected inputs are summed × master
  gain into selected outputs (per-port L/R routing). Input monitoring rides
  the **selected** loop's chain: its gain × volume × effects × pan shape the
  live pass-through, so what you hear is what that loop will do to your
  signal. Recording and overdubs always capture the raw input.
- **Per-loop effects** — a runtime-compiled effects framework. Each loop row's
  bottom strip stacks **vol**, **gain** and **pan** (bottom-left), puts an
  **FX** button next to them, and shows every enabled effect *after* the
  button as name buttons —
  each with live sliders for the parameters its script declares, plus a
  click-through editor window (automatic sliders, or a **scripted custom
  GUI** when the effect provides one). The Effects window (the FX button)
  points at an effects folder (text box + **browse**; default
  `~/.local/share/simplooper/effects`, created and seeded with a documented
  template on first run), lists every valid script with a toggle (default
  off), and lets enabled effects be reordered with ▲▼ — they snap to the top
  and the top-to-bottom order *is* the audio chain order. Effects run
  after the vol × gain trim, in GUI chain order, both live and in export
  renders. Effects are plain **Rust scripts**: drop a `.rs` file in the
  folder, simplooper compiles it with `rustc` and hot-loads it
  (**Rescan**). See *Effects* below.
- **Latency compensation** — the settings panel's *latency (ms)* trims the
  start of every fresh recording: each new take discards its first this-many
  milliseconds of input, cutting out the silence gap between pressing REC
  and that button click reaching the mic. Set it to your system's capture
  latency and the click lands exactly on the loop start — hit record, and
  the recorded button press *is* the loop's downbeat. Monitoring, overdubs
  and the output mix are never delayed, so loops never drift.

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

The app opens its JACK client with `NO_START`, so it will never spawn a server
for you — if none is running it shows an error and stays idle.

## Install

**Arch Linux** — once published, from the AUR:

```sh
yay -S simplooper        # or paru -S simplooper
```

**Any distro** — build from source (prereqs per distro in
[docs/INSTALL.md](docs/INSTALL.md)):

```sh
git clone https://github.com/actually-god/simplooper
cd simplooper
cargo build --release
./target/release/simplooper
```

## Using the GUI

Full walkthrough in [docs/USAGE.md](docs/USAGE.md). The short version:

1. Wait for `JACK ready` in the top bar (client name + sample rate).
2. Click the **hamburger** (top-left) to open settings; under **INPUTS** tick
   the remote output ports you want to loop (they are summed), under
   **OUTPUTS** tick each speaker/monitor port's **L**, **R** or both (both =
   mono downmix), then **Apply Selection**. Your selection is saved and
   re-applied automatically on the next launch.
3. On each track row: **REC** captures a fresh loop (auto-stops at ~60 s and
   starts playing), **OD** overdubs live input onto the existing loop,
   **PLAY** toggles playback, **CLEAR** zeros the buffer.
4. Tick **dependent** to lock all tracks to one ensemble clock (see
   *Loop modes* above).

### Effects

Each loop owns an independent effect chain. Click the loop's **FX** button:

- **Effects location** — the folder simplooper scans for effect scripts;
  type a path or press **browse** (same chooser as export) and press
  **Set**. The default folder is created automatically and seeded with
  `template.rs` + the ABI contract file.
- **The list** — one row per script found: a toggle (default off), the
  effect's display name, and hover text with author/description. Switching
  a script on snaps it to the top block; ▲▼ inside that block reorder the
  chain. Scripts that fail to compile or load are listed dimmed with the
  compiler's error, so an effect author can fix and **Rescan** without
  leaving the app.
- **Chain display** — enabled effects appear to the right of the FX button
  (left-to-right = chain order), each with a labelled slider per parameter,
  per loop; the effect's name button opens its editor window (auto sliders,
  or a scripted custom GUI). Parameter values persist with the config,
  keyed per script.

Signal order per loop: recorded audio × volume × gain, then effects in
chain order. The same chain runs when exporting/rendering a loop.

Writing an effect: copy `template.rs` (it ships in the effects folder and in
`effects/` of the repo), rename the file, edit. An effect is one standalone
Rust file that `include!`s `effect_api.rs` (written into the folder for you)
and provides three `extern "C"` functions (`create` / `process` /
`destroy`), a `static DESC: EffectInfoV1` description block, and the
one-line `simplooper_effect_info` entry point; an optional
`simplooper_effect_gui` entry point adds a scripted custom editor. Only
`name` is required text; `author`, `description`, and up to
**32 parameters** are optional. The template's comments document every
field, how to add/remove parameters, and the validity rules (allocation-free,
non-blocking, `depth 0`-style pass-through not required but polite). Effects
are compiled with `rustc --edition 2021 --crate-type cdylib` — so **rustc
must be installed** to use custom effects; a missing compiler just lists
scripts with a hint. Working examples in the repo: `effects/parametric_eq.rs`
and `effects/space_echo.rs` (custom scripted window).

### MIDI binding

Send events into `simplooper:midi_in` (hardware via PipeWire, or
`cargo run --example midi_tester`). In settings → **MIDI bindings**, click the
cell next to any action — it turns blue and shows *listening…* — then send a
Note On/Off, CC, or Program Change; the first matching message becomes the
binding (the second data byte — velocity / CC value — is ignored, so pedals and
knobs fire anywhere; the per-loop *volume* / *gain* / *pan* **(knob)** bindings
are the exception — they act on the value, 0 = silence/−∞ up to 127 = max).
Knob bindings additionally support **calibration**: after binding, start the
calibration flow, sweep the physical control to its minimum and press **Lock
min**, sweep to the maximum, **Finish** — the control's travel is then scaled
onto the full 0–127 range (backwards-wired knobs are inverted automatically).
Click the cell again or press `Esc` to cancel without
changing anything; while a cell is listening, `Del` clears that binding.
**Reset to defaults** restores the factory key bindings.

### Keyboard shortcuts

`1`–`4` select a loop (its row is highlighted); the transport keys act on the
selected one:

| Key | Action |
| --- | --- |
| `1`–`4` | select loop 1–4 (loops 5–16 are bindable in settings) |
| `space` | play / stop the selected loop |
| `R` | record toggle (fresh capture) |
| `O` | overdub toggle |
| `C` | clear the selected loop |
| `D` | toggle independent/dependent mode |
| `M` | metronome on / off |
| `U` | undo the last overdub |
| `Y` | redo the last undone overdub |

Every key in the table can be reassigned in settings → **MIDI bindings**
(click the key cell, press the new key; `Del` while a cell listens clears that
binding, so `Del` itself is never bindable). The keys are ignored while a text
field has focus, so you can type numbers without switching loops.

### Easy quantize

In the main window (below the mode row) → **Easy quantize**: set the time
signature numerator/denominator, the number of bars Loop 1 spans, and whether
to snap each loop to a *beat* or a *bar*. **Quantize all loops** snaps every
recorded track to that grid. The cut is refined to the nearest zero crossing
(±5 ms) and the seam gets a short crossfade so clicks are
avoided. Alternatively tick *quantize to metronome* to derive the grid from
the metronome's tempo.

## Architecture

```
src/
├── main.rs    entry point, eframe window setup
├── audio.rs   JACK backend + LoopEngine: manager thread, Cmd/Evt channels,
│              RT process handler (record/overdub/playback inline)
├── audio_io.rs  import decoding (symphonia) + export (WAV/FLAC native,
│              MP3/OGG via ffmpeg, resampled with rubato)
├── config.rs  JSON config: $XDG_CONFIG_HOME/simplooper/config.json
├── fx.rs      effects framework: scan/compile/cache/load effect scripts,
│              ABI validation, per-loop chains, RT-safe parameter relay
├── fxgui.rs   per-effect editor windows: auto sliders or scripted GUIs
├── effect_api.rs  the effect ABI contract (source of truth; also written
│              into every effects folder for scripts to `include!`)
├── midi.rs    MIDI parsing, formatting, MidiBind matching (unit-tested)
├── quantize.rs  grid snap + zero-crossing refinement + seam crossfade
└── ui.rs      egui app: top bar (hamburger), settings panel, loop dashboard,
               waveform renderer, MIDI monitor
```

- `Backend::start()` spawns the **audio manager thread**, which owns the
  `jack::Client` (`NO_START`). GUI ↔ audio communication is two bounded
  channels: commands (`Refresh`, `Apply{ins,outs}`, `Quit`, `ToggleRecord`,
  `ToggleOverdub`, `TogglePlay`, `UndoOverdub`, `RedoOverdub`, `ClearTrack`,
  `ImportTrack`, `ExportTrack`, `ExportProject`, `SetDependent`, `Quantize`,
  `SetRootLoops`, `SetLatency`, `SetMetro`) and events (`Ready`, `Ports`,
  `Applied`, `Midi`, `MidiSources`, `Xrun`, `LoopState`, `UndoState`,
  `Quantized`, `Notice`, errors).
- The client is **never deactivated**: a fixed pool of 16 in / 16 out ports
  is created once; `Apply` only (dis)connects them. This avoids a jack-crate
  bug where notification callbacks survive `clear_callbacks` after deactivation.
  Connections retry (10 × 25 ms) because PipeWire-JACK rejects connections to
  freshly-activated ports for ~300 ms.
- The realtime `ProcessHandler` mixes inputs into outputs, records into the
  loop buffers, and plays them back — no locks, no allocation. Track state
  (`TR_IDLE/TR_REC/TR_PLAY/TR_OD`), lengths, mode, and a buffer-generation
  counter are shared with the manager thread via atomics; the GUI invalidates
  waveform caches on generation bumps.
- **Dependent mode** uses a single ensemble clock: track *t* reads at position
  `master_pos % len[t]`, so shorter tracks wrap (modulo tiling) while the
  longest ("root") loop runs to completion; at the end of `root_loops` copies
  of the root loop (settable in the main window, default 1) the clock wraps —
  the ensemble loops forever and never stops the tracks.

## Packaging (Arch / AUR)

The Arch `PKGBUILD` **does not live in this repository** (AUR convention: the
packaging recipe is maintained in its own AUR package repo, `simplooper`,
building from this project's GitHub **release tag tarball**). Once the package
is published, see [docs/INSTALL.md](docs/INSTALL.md) for install/build
instructions.

Repo-side obligations that keep the package healthy:

- Every release is a **tag** (`git tag v0.1.0 && git push origin v0.1.0`) —
  the PKGBUILD's `source=` URL points at that tarball, so don't force-move
  public tags once the AUR package references them; release new versions
  instead (or re-cut + bump `pkgrel` before anyone can pull).
- `assets/icon.png` — **placeholder** blank icon; drop the final artwork over
  this file (the package installs whatever is in it).
- `simplooper.desktop` — application-menu entry (`Icon=simplooper`).
- `tests/packaging.sh` — packaging regression test (PKGBUILD syntax, required
  fields, version sync with Cargo.toml, referenced files, desktop entry,
  icon, and dependency resolvability — the `makepkg -si` failure class).
  It reads the PKGBUILD from the AUR checkout:
  `PKGBUILD_PATH=~/aur/simplooper/PKGBUILD ./tests/packaging.sh --full`.

⚠️ Packaging footguns that still apply when testing from an AUR checkout:
never run `makepkg -C` inside a *copy of this project* (`-C` wipes `$PWD/src`,
i.e. the Rust sources), and prefer a dedicated build dir over in-tree builds —
makepkg's default `$srcdir=$PWD/src` collides with Rust's own `src/`.

## Tests

```sh
cargo test -- --test-threads=1
```

Unit tests cover MIDI parsing/binding, quantize math (grid snap, zero-crossing
refinement, crossfade continuity), and the LoopEngine (record/clear roundtrip,
capacity auto-stop, overdub undo/redo).
The e2e suite drives a real JACK client pair: it feeds a controllable source
(constant or PRNG) into simplooper, records → quantizes → overdubs → plays,
and discriminates independent vs dependent mode by autocorrelation of the
captured output. Tests self-skip when no JACK/PipeWire server is running.
Packaging is covered by `tests/packaging.sh` (see the Packaging section).

## License

simplooper is distributed under the **MIT license**
([LICENSE](LICENSE)) — free to use, modify, and redistribute, commercial use
included, as long as the copyright notice travels with substantial copies.

## Roadmap (live looping)

- [x] Per-loop recording: 16 simultaneous loops with record/overdub/play/clear
- [x] Loop slots UI: waveform dashboard, per-slot controls
- [x] Action mapping: MIDI note/CC/PC → any action (live-captured bindings)
- [x] Quantize to grid (beat/bar) with zero-crossing refinement
- [x] Keyboard shortcuts for loop actions
- [x] Undo/redo of loop edits (overdub undo/redo)
- [ ] Per-slot send/return levels, mute/solo
