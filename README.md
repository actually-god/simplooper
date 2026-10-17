# simplooper

A minimalist live audio looper built in Rust for Linux (JACK / PipeWire-JACK).
Sixteen simultaneous loops with record/overdub, waveform dashboard, MIDI
binding, independent/dependent loop modes, and an easy quantize tool.

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
  buttons. Every row also carries per-loop **volume** and **gain** sliders
  with live dB readouts (volume: 0 = −∞ … 127 = 0 dB; gain: 80 = 0 dB,
  127 = +10 dB) trimming that loop's playback — recorded audio itself is
  never re-leveled.
- **Settings menu** — the three-bar (hamburger) button in the top-left opens
  the settings panel: input/output port selection, master gain, latency
  compensation, and per-action MIDI + keyboard bindings. Port selections are
  persisted between sessions and automatically re-applied when JACK comes up,
  so audio flows on launch without re-selecting.
- **MIDI binding** — every action (loop select, record/overdub/clear/play-stop
  on the selected loop, direct per-loop controls, dependent toggle) can be
  bound to a live-captured Note On/Off, CC, or Program Change (channel +
  note/controller/program). Bindings are recorded by clicking the cell and
  sending a message; velocity/CC value is ignored so pedals and knobs fire
  anywhere — except the per-loop *volume (knob)* / *gain (knob)* bindings,
  which read the message value (0–127: 0 = off/−∞, 127 = max). All bindings
  persist between sessions.
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
- **Easy quantize** — lives in the main window (below the mode row): enter
  Loop 1's time signature (numerator/denominator), how many bars Loop 1 spans,
  and "quantize to beat/bar"; every recorded loop is snapped to that grid
  (nearest multiple → zero-crossing refinement within ±5 ms → short
  crossfade at the seam).
- **MIDI monitor** — a permanent `simplooper:midi_in` JACK MIDI port; every
  incoming event is parsed and shown in the bottom panel and on stdout.
- **Live mix passthrough** — selected inputs are summed × master gain into
  selected outputs, independent of the loop engine.
- **Latency compensation** — the settings panel's *latency (ms)* delays the
  INCOMING signal (recording + monitoring) so your playing lands on the
  already-recorded loop grid; the output mix (loops, metronome, monitors)
  stays exactly on grid. Use it to cancel the fixed input delay of an
  interface/DAW chain without shifting the loops themselves.

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
   **OUTPUTS** tick your speakers/monitor mix, then **Apply Selection**. Your
   selection is saved and re-applied automatically on the next launch.
3. On each track row: **REC** captures a fresh loop (auto-stops at ~60 s and
   starts playing), **OD** overdubs live input onto the existing loop,
   **PLAY** toggles playback, **CLEAR** zeros the buffer.
4. Tick **dependent** to lock all tracks to one ensemble clock (see
   *Loop modes* above).

### MIDI binding

Send events into `simplooper:midi_in` (hardware via PipeWire, or
`cargo run --example midi_tester`). In settings → **MIDI bindings**, click the
cell next to any action — it turns blue and shows *listening…* — then send a
Note On/Off, CC, or Program Change; the first matching message becomes the
binding (the second data byte — velocity / CC value — is ignored, so pedals and
knobs fire anywhere; the per-loop *volume (knob)* / *gain (knob)* bindings are
the exception — they act on the value, 0 = silence/−∞ up to 127 = max).
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
avoided.

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

Where to run `makepkg`: running it inside a copy of this project makes
makepkg's default source dir `$PWD/src`, which collides with Rust's own
`src/`. The PKGBUILD copes (it builds from the extracted
`simplooper-<ver>/` subdirectory by absolute path), but the downloaded
tarball and extracted tree are left inside `src/` afterwards. A dedicated
build dir is tidier:

```sh
mkdir -p ~/build/simplooper && cd ~/build/simplooper
cp /path/to/repo/PKGBUILD .
makepkg -si   # fetches the v${pkgver} tag tarball and builds it
```

Never use `makepkg -C` inside a project copy — `-C` wipes `$PWD/src`, i.e.
the Rust sources.

The `source=` URL points at the `v${pkgver}` tag on GitHub — cut a tag for
each release (`git tag v0.1.0 && git push origin v0.1.0`). The sha256 in
`sha256sums` must match that tarball; recompute it after every tagged release.

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

Packaging is covered by `tests/packaging.sh` (see the Packaging section):
run it before submitting to the AUR or after any PKGBUILD change.

## Roadmap (live looping)

- [x] Per-loop recording: 16 simultaneous loops with record/overdub/play/clear
- [x] Loop slots UI: waveform dashboard, per-slot controls
- [x] Action mapping: MIDI note/CC/PC → any action (live-captured bindings)
- [x] Quantize to grid (beat/bar) with zero-crossing refinement
- [ ] Per-slot send/return levels, mute/solo
- [x] Keyboard shortcuts for loop actions
- [x] Undo/redo of loop edits (overdub undo/redo)
