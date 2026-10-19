# Using simplooper

A quick tour, then the reference details. Everything lives in one window
(~1080×640, resizable down to 780×480); configuration is saved automatically
when you quit.

- [First launch](#first-launch)
- [The main window](#the-main-window)
- [Recording & overdubbing](#recording--overdubbing)
- [Loop modes](#loop-modes)
- [Metronome](#metronome)
- [Easy quantize](#easy-quantize)
- [Import & export](#import--export)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [MIDI control](#midi-control)
- [Per-loop effects](#per-loop-effects)
- [Latency compensation](#latency-compensation)
- [Troubleshooting](#troubleshooting)

## First launch

1. Start your JACK-compatible audio server (PipeWire with `pipewire-jack`,
   or jack2). simplooper never starts one for you.
2. Launch simplooper. The top bar shows `JACK ready — simplooper @ 48000 Hz`
   (client name + sample rate) once the audio engine is up.
3. Click the **hamburger ☰** (top-left) → settings panel:
   - **INPUTS** — tick the remote output ports you want to record (your mic /
     instrument chain). Multiple inputs are summed into every loop.
   - **OUTPUTS** — for each port tick **L**, **R** or both: `L` plays only
     the left side of the stereo mix, `R` only the right, and both play a
     mono downmix of the two (the classic behaviour — that's the default
     for previously saved selections too).
   - **Apply Selection**. Your choice is remembered and re-applied
     automatically on every launch, including when the JACK graph rebuilds.
4. While selected, inputs are also passed straight through to the outputs
   (× *master gain*), independent of the loop engine — you always hear
   yourself live. Live input **monitoring** rides the *selected* loop's
   chain: its gain × volume × effect chain × pan shape the pass-through, so
   you hear exactly what that loop's chain will do to your signal.
   Recording and overdubs still capture the raw, unprocessed input.

## The main window

The window is a scrollable stack of **16 track rows**. Each row is one loop
(~60 s max recording length per track) and shows:

| Control | What it does |
| --- | --- |
| waveform | live view of the loop's content; clicking the row selects that loop |
| **REC** | record a fresh take (old content is replaced); auto-stops at the ~60 s cap and starts playing |
| **OD** | overdub — plays the loop while writing live input on top of it (additive: your playing is summed onto the existing take at its original level, never replacing it) |
| **PLAY** | toggle playback of the loop |
| **CLEAR** | zero the buffer |
| **IMPORT** | load an audio file into the loop (see [Import & export](#import--export)) |
| **vol** | playback trim: 0 = −∞ … 127 = 0 dB. The text box shows the level in dB — type a value (e.g. `-6.5`) and press Enter; the number at the right of the slider is the raw MIDI value |
| **gain** | gain trim: 0 = −∞ … 80 = 0 dB … 127 = +10 dB. Same editable dB box + raw MIDI readout |
| **pan** | stereo pan: -100 = hard left, **0 = centre**, +100 = hard right (equal power across the L/R output buses). Editable -100..100 text box + raw MIDI readout; bindable to a knob like vol/gain |
| **FX** | open the effects window for this loop (see [Per-loop effects](#per-loop-effects)) |
| effect buttons | enabled effects appear right of FX, left-to-right in chain order, as name buttons — clicking one opens that effect's controls window (auto-generated sliders, or the effect's own scripted GUI) |

Above the rows: **UNDO**/**REDO** (last overdub), the **dependent** checkbox
(loop mode), the metronome bar, the quantizer, and **EXPORT**.

*Volume/gain only trim playback (or overdub input) — recorded audio is never
re-leveled, so you can fix levels after the fact.*

## Recording & overdubbing

- **Record**: select a loop (keys `1`–`4`, click a row, or a MIDI binding),
  press **REC** (or `R`). Press again to stop — playback starts immediately.
- **Overdub**: **OD** (or `O`) loops the existing take and sums your live
  input onto it. Every OD start is snapshot, so **UNDO** (`U`) restores the
  take exactly as it was before that overdub; **REDO** (`Y`) re-applies it
  (8 levels deep in each direction). Re-recording, clearing, importing, or
  quantizing a track discards history that could resurrect deleted audio.
- Punch in/out by feel — then fix the length with [quantize](#easy-quantize).

## Loop modes

- **Independent** (default): every track runs on its own clock, like 16
  separate loop pedals. Tracks started at different times stay offset.
- **Dependent**: one ensemble clock. The longest track ("root") defines the
  cycle; shorter tracks tile/wrap on top of it (modulo), and the whole
  ensemble repeats after a settable number of **root loops** (default 1).
  It loops forever — nothing stops mid-cycle.
  *Quantize your loops first* (the tooltip warns you): unquantized lengths
  don't divide into the grid and you'll hear a skip at every wrap.

Dependent mode is what you want for structured looping; the **whole-project
export** also requires it (it's switched on for you automatically).

## Metronome

The main-window bar: **▶ METRO / ■ METRO** (or key `M`), **BPM** (20–300,
default 120), **beats/measure** (1–16, default 4), and **subdivision**
(quarter, eighth, triplet, sixteenth, triplet-eighth, thirty-second).

- Three tones: high = downbeat (beat 1), mid = quarter notes, low = chosen
  subdivision clicks.
- Activation always starts the click on the next **downbeat**; subdivision
  changes take effect at the next quarter note.
- The click goes to your **outputs only** — it is never recorded into loops.
- *quantize to metronome* (next to the quantizer) makes
  [Easy quantize](#easy-quantize) use the metronome's tempo instead of
  manual time-signature maths.

## Easy quantize

Below the mode row. Set the time signature (numerator/denominator) of Loop 1
and how many **bars Loop 1 spans** — the grid for every other loop follows
from that. Choose snap **to beat** or **to bar**, then **Quantize all loops**:
each recorded loop is cut to the nearest grid multiple, refined to the
nearest zero crossing (±5 ms), and the seam gets a short crossfade, so the
edit is click-free.

## Import & export

**Import** (per row): WAV, MP3, OGG/OGA, FLAC, M4A, AAC, AIFF/AIF — decoded
and resampled to the engine rate as mono. Handy for dropping a backing track
into loop 1 before you build on top.

**Export** (top bar): renders through the *full* per-loop chain (vol × gain ×
effects), not just the raw buffer.

- **Single loop** — exports the selected loop.
- **Whole project** — exports the whole dependent-mode mixdown (the mode is
  enabled automatically). Written as `simplooper-project.<ext>`.
- Formats: **WAV** (16-bit PCM) and **FLAC** are built in; **MP3** and
  **OGG Vorbis** need `ffmpeg` installed.

## Keyboard shortcuts

Defaults (every one is rebindable in settings; keys are ignored while a text
field has focus, and key auto-repeat never re-fires an action):

| Key | Action |
| --- | --- |
| `1`–`4` | select loop 1–4 (5–16 are bindable via the action list) |
| `Space` | play / stop the selected loop |
| `R` | record toggle (fresh capture) |
| `O` | overdub toggle |
| `C` | clear the selected loop |
| `D` | toggle independent/dependent mode |
| `M` | metronome on/off |
| `U` | undo the last overdub |
| `Y` | redo the last undone overdub |

## MIDI control

simplooper exposes a JACK MIDI input port, **`simplooper:midi_in`**. Every
incoming event is parsed and shown in the bottom monitor panel (and on
stdout), so you can verify a controller before binding anything. Wire your
keyboard/pedalboard to it (PipeWire picks up MIDI hardware automatically).

**Binding**: settings → *MIDI bindings* → click an action's cell (it turns
blue, *listening…*) → send a **Note On/Off, CC, or Program Change**. The
first message of the matching kind becomes the binding. Velocity / CC value
is ignored for trigger actions (pedals and knobs fire anywhere) — the
per-loop **volume (knob)** / **gain (knob)** / **pan (knob)** bindings are
the exception and act on the value (0–127: 0 = silence / hard left,
127 = max / hard right).

**Knob calibration**: controllers with a limited or backwards travel can be
learned — start a knob slot's calibration and sweep the physical control: the
window shows its most recent raw value live, so you can hunt for the travel
limits. The axis is the message *type + channel*: every message of that kind
counts and its data byte is the number shown, so both min and max keep
following until you lock them. Press **Lock min** at the lowest position (the
minimum stops following), sweep to the maximum, press **Finish**.
The control's range is then stretched onto the full 0–127 program range
(a backwards-wired knob is inverted automatically), so the extremes of your
hardware mean the extremes in simplooper.

- Click the cell again or press `Esc` to cancel; press `Del` while a cell is
  listening to clear that binding.
- **Reset to defaults** restores the factory keyboard table.
- Bindings persist across sessions.

**All bindable actions** (keyboard or MIDI): record / overdub / clear /
play-stop / import on the *selected* loop, export, select loop 1–16, select
next/previous (clamped), per-loop record / overdub / clear / play-stop
(all 16), per-loop volume / gain / pan (knob), dependent toggle, metronome
toggle, overdub undo / redo.

## Per-loop effects

Each loop has its own effect chain; each effect instance is **per-loop**
(same script on two loops = two instances, independent parameter values).

The **FX** window on each row:

- **Effects location** — the folder scanned for scripts (default
  `~/.local/share/simplooper/effects`, created and seeded with `template.rs`
  and the ABI contract file on first run). Type a path or **browse**, then
  **Set**. **Rescan** re-reads the folder after you edit a script.
- **The list** — one row per script: toggle (default off), display name,
  hover text with author/description. Enabled effects snap to the top; ↑↓
  arrow buttons reorder the chain — top-to-bottom **is** the audio chain order. Scripts
  that fail to compile stay visible, dimmed, with the compiler error.
- The main window shows enabled effects as chips with live sliders; each
  effect chip also opens an **editor window** — automatic (one labelled
  slider per parameter) or, if the script provides one, a custom scripted GUI.

Signal order per loop: `recorded × volume × gain → effects in chain order`.
The same chain runs during export renders.

**Writing effects**: an effect is a single standalone Rust file dropped into
the effects folder — copy `template.rs` (also in [`../effects/`](../effects)
of the repo), rename, edit. It `include!`s the ABI contract
(`effect_api.rs`, written into the folder for you) and implements
`create` / `process` / `destroy` plus a description block (name required;
author, description, and up to **32 parameters** optional; an optional
custom GUI entry point). simplooper compiles scripts with
`rustc --edition 2021 --crate-type cdylib` — so **rustc must be installed**
to use custom effects. Examples ship in the repo: `parametric_eq.rs` (response
curve + 240-band spectrum GUI, draggable handles, B1 LP / B6 HP toggles) and
`space_echo.rs` (Echo + Reverb: animated echo window, independent echo/verb
levels, factory presets — plain echo + room reverbs). Keep `process()`
allocation-free and non-blocking — it runs in the realtime audio path.

## Latency compensation

Settings → **latency (ms)**: each *fresh recording* discards its first N ms
of input, cutting out the silence gap between pressing REC and the sound
reaching the mic — set it to your system's capture latency and your
button-press click lands exactly on the loop start. Monitoring, overdubs,
and the output mix are never delayed, so nothing drifts.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| "no JACK server" / connection error on launch | Start PipeWire or `jackd` first — simplooper never starts a server itself (`NO_START` by design). |
| No sound although `JACK ready` | ☰ settings → tick output (and input) ports → **Apply Selection**. Selections persist, but if your hardware changed, re-apply. |
| Loops click/skip at loop points | Use [Easy quantize](#easy-quantize); in dependent mode quantize before enabling, and make lengths divide the grid. |
| MIDI not seen | Check the bottom MIDI monitor panel; make sure a source is connected to `simplooper:midi_in`. |
| Effect scripts "compile failed" | Fix the script (the error text is in the FX list) → **Rescan**. Missing rustc entirely → install the Rust toolchain; other features keep working. |
| MP3/OGG export offered nothing | Install `ffmpeg`. |
| Config reset / moved | Config lives at `$XDG_CONFIG_HOME/simplooper/config.json`; effects default to `~/.local/share/simplooper/effects/`. |

---

Next: [INSTALL.md](INSTALL.md) to build/install, or
[AGENT-GUIDE.md](AGENT-GUIDE.md) if you're automating against this repo.
