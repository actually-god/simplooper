# simplooper

A minimalist live audio looper built in Rust for Linux (JACK / PipeWire-JACK).
Four simultaneous loops with record/overdub, waveform dashboard, MIDI binding,
independent/dependent loop modes, and an easy quantize tool.

## Features

- **4-loop engine** — each track has its own pre-allocated buffer (~60 s at
  48 kHz) and runs fully in the realtime process callback: record (fresh
  capture), overdub (play while writing live input on top), play, clear.
- **Dashboard** — the main window is a grid of four track rows, each showing
  the recorded waveform plus REC / OD / PLAY / CLEAR buttons.
- **Settings menu** — the three-bar (hamburger) button in the top-left opens
  the settings panel: input/output port selection, master gain, per-track MIDI
  bindings, and the easy quantize tool.
- **MIDI binding** — any track's REC or OD button can be bound to a Note On,
  CC, or Program Change (channel + note/controller/program). Bindings are
  picked from live-detected messages in the MIDI monitor; velocity/CC value is
  ignored so pedals and knobs fire anywhere.
- **Keyboard shortcuts** — keys `1`–`4` select a loop (the selected row is
  highlighted); `space` play/stop, `R` record, `O` overdub, `C` clear act on
  the selected loop; `D` toggles dependent mode. Keys are ignored while a text
  field has focus, and key auto-repeat never re-fires an action.
- **Loop modes** — a checkbox in the dashboard switches between:
  - *independent*: each track plays on its own clock (four independent players);
  - *dependent*: the longest track defines the loop; shorter tracks wrap on top
    of it until it finishes, then the whole ensemble repeats. A tooltip warns
    to quantize first or you'll hear skips at the loop points.
- **Easy quantize** — enter Loop 1's time signature (numerator/denominator),
  how many bars Loop 1 spans, and "quantize to beat/bar"; every recorded loop
  is snapped to that grid (nearest multiple → zero-crossing refinement within
  ±5 ms → short equal-power crossfade at the seam).
- **MIDI monitor** — a permanent `simplooper:midi_in` JACK MIDI port; every
  incoming event is parsed and shown in the bottom panel and on stdout.
- **Live mix passthrough** — selected inputs are summed × master gain into
  selected outputs, independent of the loop engine.

## Requirements

- Rust (stable) and `cargo`
- Build-time system libraries: JACK headers + `.pc` file (provided by
  `pipewire-jack` or the official `libjack` package), plus `pkgconf`.
  No other system deps — the GUI stack is pure-Rust.
- A JACK-compatible audio server:
  - **PipeWire** (recommended): just install `pipewire` + `pipewire-jack`.
  - or classic **jack2** (`jackd`).

The app opens its JACK client with `NO_START`, so it will never spawn a server
for you — if none is running it shows an error and stays idle.

## Build & run

```sh
cargo build --release
./target/release/simplooper
# or: cargo run
```

### Using the GUI

1. Wait for `JACK ready` in the top bar (client name + sample rate).
2. Click the **hamburger** (top-left) to open settings; under **INPUTS** tick
   the remote output ports you want to loop (they are summed), under
   **OUTPUTS** tick your speakers/monitor mix, then **Apply Selection**.
3. On each track row: **REC** captures a fresh loop (auto-stops at ~60 s and
   starts playing), **OD** overdubs live input onto the existing loop,
   **PLAY** toggles playback, **CLEAR** zeros the buffer.
4. Tick **dependent** to lock all tracks to one ensemble clock (see
   *Loop modes* above).

### MIDI binding

Send events into `simplooper:midi_in` (hardware via PipeWire, or
`cargo run --example midi_tester`). In settings → **MIDI bindings**, open the
combo next to a track's REC or OD and pick one of the recently detected
messages — that message now toggles the button. Note On/CC/PC are bindable;
the second data byte (velocity / CC value) is ignored.

### Keyboard shortcuts

`1`–`4` select a loop (its row is highlighted); the transport keys act on the
selected one:

| Key | Action |
| --- | --- |
| `1`–`4` | select loop 1–4 |
| `space` | play / stop the selected loop |
| `R` | record toggle (fresh capture) |
| `O` | overdub toggle |
| `C` | clear the selected loop |
| `D` | toggle independent/dependent mode |

The keys are ignored while a quantize text field has focus, so you can type
numbers without switching loops.

### Easy quantize

In settings → **Easy quantize**: set the time signature numerator/denominator,
the number of bars Loop 1 spans, and whether to snap each loop to a *beat* or
a *bar*. **Quantize all loops** snaps every recorded track to that grid. The
cut is refined to the nearest zero crossing (±5 ms) and the seam gets a short
equal-power crossfade so clicks are avoided.

## Architecture

```
src/
├── main.rs    entry point, eframe window setup
├── audio.rs   JACK backend + LoopEngine: manager thread, Cmd/Evt channels,
│              RT process handler (record/overdub/playback inline)
├── midi.rs    MIDI parsing, formatting, MidiBind matching (unit-tested)
├── quantize.rs  grid snap + zero-crossing refinement + seam crossfade
└── ui.rs      egui app: top bar (hamburger), settings panel, loop dashboard,
               waveform renderer, MIDI monitor
```

- `Backend::start()` spawns the **audio manager thread**, which owns the
  `jack::Client` (`NO_START`). GUI ↔ audio communication is two bounded
  channels: commands (`Refresh`, `Apply{ins,outs}`, `Quit`, `ToggleRecord`,
  `ToggleOverdub`, `TogglePlay`, `ClearTrack`, `SetDependent`, `Quantize`) and
  events (`Ready`, `Ports`, `Applied`, `Midi`, `Xrun`, `LoopState`, errors).
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
  `master_pos % len[t]`, so shorter tracks wrap while the longest (the "loop")
  runs to completion before everything repeats together.

## Packaging (Arch / AUR)

The repository is laid out to be packaged directly with `makepkg`:

- `PKGBUILD` — builds with `cargo build --release --locked` and installs the
  binary, the desktop entry, and the icon into the hicolor theme. Depends on
  `libjack.so` (the shared library, not a specific package) so it resolves
  with either JACK implementation: pipewire-jack or official libjack.
- `simplooper.desktop` — application-menu entry (`Icon=simplooper`).
- `assets/icon.png` — **placeholder** blank icon; drop the final artwork over
  this file before release (the PKGBUILD installs whatever is in it).
- `tests/packaging.sh` — packaging regression test: checks PKGBUILD syntax,
  required fields, version sync with Cargo.toml, referenced files, desktop
  entry, icon, and that every dependency is installed or installable from the
  enabled repos (the failure mode of `makepkg -si`). Add `--full` for a
  sandboxed makepkg build + package-content verification.

To build a local `.pkg.tar.zst`:

```sh
makepkg -si   # in this directory (installs any missing deps, then the package)
./tests/packaging.sh --full   # optional: verify packaging end-to-end
```

The AUR `source=` URL and checksum in `PKGBUILD` are placeholders until the
project has a real upstream git repository; update them before submitting.

## Tests

```sh
cargo test -- --test-threads=1
```

Unit tests cover MIDI parsing/binding, quantize math (grid snap, zero-crossing
refinement, crossfade continuity), and the LoopEngine (record/clear roundtrip).
The e2e suite drives a real JACK client pair: it feeds a controllable source
(constant or PRNG) into simplooper, records → quantizes → overdubs → plays,
and discriminates independent vs dependent mode by autocorrelation of the
captured output. Tests self-skip when no JACK/PipeWire server is running.

Packaging is covered by `tests/packaging.sh` (see the Packaging section):
run it before submitting to the AUR or after any PKGBUILD change.

## Roadmap (live looping)

- [x] Per-loop recording: 4 simultaneous loops with record/overdub/play/clear
- [x] Loop slots UI: waveform dashboard, per-slot controls
- [x] Action mapping: MIDI note/CC/PC → per-track REC/OD buttons
- [x] Quantize to grid (beat/bar) with zero-crossing refinement
- [ ] Per-slot send/return levels, mute/solo
- [x] Keyboard shortcuts for loop actions
- [ ] Undo/redo of loop edits
