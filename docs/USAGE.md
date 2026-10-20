# Using simplooper

A quick tour, then the reference details. Everything lives in one window
(~1080×640, resizable down to 780×480); configuration is saved automatically
when you quit, and your session itself saves into a **project** file (Ctrl+S).

simplooper is a small **clip-based DAW**: one global playhead over a
metronome-defined bar grid, sixteen tracks with additive child lanes, an
asset rack you drag audio from, and takes that place themselves onto the
timeline while you play.

- [First launch](#first-launch)
- [The window](#the-window)
- [The asset rack](#the-asset-rack)
- [Clips & lanes](#clips--lanes)
- [Recording takes](#recording-takes)
- [Transport & playhead](#transport--playhead)
- [Metronome & grid](#metronome--grid)
- [Quantize & trim](#quantize--trim)
- [Projects: save & load](#projects-save--load)
- [Export](#export)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [MIDI control](#midi-control)
- [Track effects](#track-effects)
- [Latency compensation](#latency-compensation)
- [Troubleshooting](#troubleshooting)

## First launch

1. Start your JACK-compatible audio server (PipeWire with `pipewire-jack`,
   or jack2). simplooper never starts one for you.
2. Launch simplooper. The top bar shows `simplooper @ 48000 Hz` once the
   audio engine is up.
3. Click the **hamburger ☰** (top-left) → settings panel:
   - **INPUTS** — tick the remote output ports you want to record (your mic /
     instrument chain). Multiple inputs are summed.
   - **OUTPUTS** — for each port tick **L**, **R** or both: `L` plays only
     the left side of the stereo mix, `R` only the right, and both play a
     mono downmix (the default behaviour).
   - **Apply Selection**. Your choice is remembered and re-applied
     automatically on every launch, including when the JACK graph rebuilds.
4. Add audio: **+ folder** in the asset rack (left panel) points simplooper
   at a directory of samples/backing tracks. The **first folder is the
   primary folder** — everything you record is written there as WAV, so one
   folder holds both your starting material and your growing take library.
5. Live input **monitoring** rides the *selected* track's chain: its gain ×
   volume × effects × pan shape the pass-through, so you hear exactly what
   that track will do to your signal. Takes always capture the raw,
   unprocessed input.

## The window

| Region | Contents |
| --- | --- |
| **Top bar** | ☰ settings · engine status · **PLAY/STOP** (transport) · **loop** checkbox · bar.beat position · **metro** + bpm + beats/measure + subdivision · **Load / Save as / Save / Export** · peak meter · master gain |
| **Left panel** | the [asset rack](#the-asset-rack); ☰ docks the **settings panel to its left**, ☰ again closes it |
| **Grid toolbar** | auto-quantize checkbox, **quantize** subdivision, **snap** subdivision, zoom slider |
| **Timeline** | **ruler pinned above the tracks** (bar numbers; click/drag scrubs, playhead visible while stopped) + one block per track: header row, always-visible mixer, and one lane strip per lane — all sharing one horizontal scroll, so ruler and lane grids stay aligned |
| **Bottom** | MIDI monitor (every event on `simplooper:midi_in`) |

Each **track header** holds: the track name (click the row to select the
track), **REC** (turns **STOP REC** while rolling), the play gate
(**PLAYED** ⇄ **muted** — a muted track contributes nothing while its clips
stay put), **+ child** / **- child** (add/remove child lanes), and a
`N clips · M lanes` status. **Right-click the header** removes the track's
clips (assets stay in the rack; same as `C`).

Every track shows its **vol / gain / pan** sliders (editable dB /
−100…+100 text boxes + raw MIDI readout) and an **fx** button directly to
their right — no selection needed; the button opens that track's FX chain.
Tracks with effects show their chain inline below the sliders.

## The asset rack

Folders at the top, audio under each. Drag any file row onto any lane to
place it; click a row to **select the asset** — three buttons then act on
it:

| Button | Effect |
| --- | --- |
| **Quantize** | snaps the asset's content start/end onto the current quantize subdivision grid (see [Quantize & trim](#quantize--trim)) |
| **Trim** | clips silence off the start and end (short edge fades prevent clicks) |
| **Remove** | drops the asset **and every clip that references it** (the file on disk is untouched) |

- **+ folder** adds a folder (`[...]` browse dialog); **x** removes a folder
  from the rack (files stay on disk); **scan** re-reads it after external
  changes. The first entry is tagged *primary* — that's where recordings are
  saved.
- Supported formats: WAV, MP3, OGG/OGA, FLAC, M4A, AAC, AIFF/AIF — decoded
  and resampled to the engine rate on first use, listed alphabetically.
- Below the folders, **project assets** lists audio that belongs to the
  current project but was edited/derived since loading; both lists drag.

## Clips & lanes

- **Place**: drag an asset from the rack; a ghost outline shows the landing
  spot. It snaps to the **snap subdivision** grid (grid toolbar).
- **Move**: drag a placed clip along its lane (same snap). Clips on the
  same track **add together** — across lanes and/or overlapping in time —
  so stacking is always additive, never replacing.
- **Select & delete**: click a clip (it highlights, red × hint at its
  corner), press **Delete** to remove that placement only; the asset stays
  in the rack.
- **Child lanes**: **+ child** adds a lane to a track; **- child** removes
  the last one (its clips go with it — base lane only ever survives).
  Recording or dropping onto an occupied base lane **opens a child lane
  automatically** instead of touching existing clips.
- **Clear**: **right-click the track header** (or press `C` on the selected
  track) removes the track's clips (all lanes except the base, which empties
  too); your rack assets stay untouched.

## Recording takes

Press **REC** on a track (button, `R` on the selected track, or a MIDI
binding), play over the timeline, press **STOP REC** again — or simply let
it run to the ~60 s take cap, which auto-stops. The take then, on one
step:

1. is written as a WAV into the **primary rack folder** (a timestamped
   `take-…` name, instantly listed in the rack like any other asset),
2. is placed as a clip **at the playhead position where recording started**,
   snapped to the snap grid, on the base lane — or into a **new child lane**
   when something already occupies that span,
3. is quantized automatically if **auto-quantize** is ticked.

Recording works with the transport stopped (you set the position by dragging
the playhead first, then punch in), or while the project plays. The
metronome can keep rolling while you record — its click is output-only and
is **never** captured into a take.

## Transport & playhead

- **PLAY/STOP** (top bar, or `Space`, or bound MIDI) runs **everything**
  from wherever the playhead is, and stopping **holds the position** —
  start and stop at any point of the project, like a DAW.
- **Drag the playhead**: click or drag anywhere on the **ruler** (always
  pinned above the tracks) or on any lane's empty grid to scrub — the
  movement area spans every track's grid in aligned columns; the whole view
  follows. The red line + position readout
  (`bar.beat` in project time signature) always show where you are, running
  or not.
- **loop** (default on) wraps the playhead at the project end forever;
  off, it parks at the end and the transport stops.
- **Play all / Stop all** are also separate MIDI/keyboard actions
  (`MidiAction::PlayAll` / `StopAll`) — `Space` toggles, `StopAll` always
  parks. Per-track **play gates** (PLAYED/muted) and per-track
  record/clear bindings keep working the same while the global transport
  runs.
- A bare **metronome** (metro on, transport stopped) keeps the playhead
  rolling on the grid without sounding any clips — a click track for
  punching in. Transport stopped *and* metro off freezes the playhead.

## Metronome & grid

The metronome **defines the project grid**: **bpm** (20–300, default 120),
**beats/measure** (1–16), and **subdivision** all come from the transport
bar, and the ruler/bar numbers/quantize/snap grids all derive from them.

- Three tones: high = downbeat (beat 1), mid = quarter notes, low = chosen
  subdivision.
- Activation always starts the click on the current grid line; subdivision
  changes are picked up at the next quarter note.
- The click goes to your **outputs only** — never recorded.

## Quantize & trim

Quantization is always metronome-based — no time-signature maths, no mode
checkbox. On the grid toolbar:

- **quantize** dropdown — the subdivision the *content* snaps to (quarter,
  eighth, triplet, sixteenth, triplet-eighths, thirty-second).
- **snap** dropdown — the grid *clip placement* follows while dragging and
  when takes auto-place.
- **auto-quantize** — every fresh recording is quantized on the way into
  the rack.

On the selected asset in the rack, **Quantize** trims/extends the take to
the surrounding grid: the cut snaps to the nearest subdivision multiple,
refined to the nearest zero crossing (±5 ms) with a short seam crossfade so
it stays click-free. **Trim** finds the first/last sample above a small
threshold and cuts the silence off both ends (with tiny fades). Both edit
the asset in place, so every clip referencing it follows.

## Projects: save & load

- **Ctrl+S** saves. The first save of a session opens a **name prompt**;
  after that the same key saves straight into
  `<projects folder>/<name>.simplooper`. **Ctrl+Shift+S** always re-asks
  (save-as). The **Save** button mirrors the key; **Save as** opens the
  prompt.
- **Load** lists every project in the projects folder, newest first —
  clicking one replaces the session (unsaved changes are lost, so save
  first).
- The **projects folder** is a setting (☰ settings → Projects): default
  `~/.local/share/simplooper-projects`, changeable/browsable.
- A `.simplooper` file is a small versioned JSON document: grid settings,
  quantize/snap, per-track mixer + FX, the whole track/lane/clip
  arrangement, and the asset table. **Audio is never embedded** — assets
  reference WAV files. At save time, derived/edited assets are written into
  `<projects folder>/<name> audio/`, and a moved or renamed file is
  re-found by file name inside the project's audio folder or the rack
  folders when loading.

## Export

**Export** (top bar, `E`): render **one track** (its clips mixed, then
vol × gain × effects × pan) or the **whole project** (every track summed
through its chain), as:

| Format | Notes |
| --- | --- |
| WAV | 16-bit PCM, built in |
| FLAC | built in |
| MP3 / OGG | requires `ffmpeg` |

The destination dialog suggests the project name; changing the format
changes the file extension at save automatically.

## Keyboard shortcuts

Defaults (every one rebindable in settings; keys are ignored while you type
in **any** text box, and auto-repeat never re-fires):

| Key | Action |
| --- | --- |
| `Space` | play / stop all (transport toggle, holds position) |
| `R` | record toggle on the selected track |
| `C` | clear the selected track's clips |
| `E` | export prompt |
| `M` | metronome on / off |
| `1`–`4` | select track 1–4 (5–16 via the action list) |
| `Ctrl+S` | save project (name prompt on first save) |
| `Ctrl+Shift+S` | save under a new name |
| `Delete` | remove the selected clip (when a clip is selected) |

## MIDI control

simplooper exposes a JACK MIDI input port, **`simplooper:midi_in`**. Every
incoming event is parsed and shown in the bottom monitor panel (and on
stdout), so you can verify a controller before binding anything.

**Binding**: settings → *MIDI bindings* → click an action's cell (it turns
blue, *listening…*) → send a **Note On/Off, CC, or Program Change**. The
first message of the matching kind becomes the binding. Velocity / CC value
is ignored for trigger actions — the per-track **volume / gain / pan
(knob)** bindings act on the value (0–127) and support the **three-step
axis calibration** wizard (lock min, lock max; backwards travel is inverted
automatically).

**All bindable actions**: per-track **record**, **clear**, **play gate**
(×16 each), track select 1–16, select next/prev (clamped), **play all**,
**stop all**, **export**, **metronome**, per-track volume / gain / pan
(knob). Overdub, per-track import, dependent-mode, and undo/redo actions
are gone with the DAW model — v0.1 bindings are migrated forward
automatically (removed slots dropped).

- Click the cell again or press `Esc` to cancel; `Del` while a cell listens
  clears that binding.
- **Reset to defaults** restores the factory keyboard table.
- Bindings persist across sessions.

## Track effects

Each track has its own effect chain; each effect instance is **per-track**
(same script on two tracks = two instances, independent parameters). The
**FX** window on each row:

- **Effects location** — folder scanned for scripts (default
  `~/.local/share/simplooper/effects`, seeded with `template.rs` + the ABI
  contract on first run). **Rescan** re-reads it after you edit a script.
- **The list** — toggle per script (default off), display name, author /
  description hover; enabled effects snap to the top, ↑↓ reorder —
  top-to-bottom **is** the audio chain order. Failing scripts stay visible,
  dimmed, with the compiler error.
- Enabled effects show as **name buttons** on the track row (chain order);
  each opens an editor window — automatic sliders or the script's own
  scripted GUI. User presets: every editor has a preset strip saving
  `{effect}-{name}.json` into `<effects folder>/user presets/`.

Signal order per track: `clip × volume × gain → effects in chain order →
pan → output buses`. The same chain runs during export renders.

**Writing effects**: one standalone Rust file dropped into the effects
folder — copy `template.rs` (also in [`../effects/`](../effects)), rename,
edit. It `include!`s the ABI contract (`effect_api.rs`, written into the
folder for you) and implements `create` / `process` / `destroy` plus a
description block (name required; up to **32 parameters**; optional custom
GUI entry point). Scripts compile with
`rustc --edition 2021 --crate-type cdylib` — so **rustc must be installed**
to use custom effects. Keep `process()` allocation-free and non-blocking —
it runs in the realtime audio path.

## Latency compensation

Settings → **latency (ms)**: each fresh take discards its first N ms of
input, cutting out the silence gap between pressing REC and the sound
reaching the mic — set it to your system's capture latency and your
button-press click lands exactly on the clip start. Monitoring and the
output mix are never delayed, so nothing drifts.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| "no JACK server" / connection error on launch | Start PipeWire or `jackd` first — simplooper never starts a server itself (`NO_START` by design). |
| No sound although the engine shows ready | ☰ settings → tick output (and input) ports → **Apply Selection**. Selections persist, but if your hardware changed, re-apply. |
| Record button does nothing | The rack needs a **primary folder** — takes are saved there before they can become clips. Press **+ folder** in the asset rack; the status line names the reason. |
| Recorded clip starts early/late | Settings → latency (ms); and check **auto-quantize**/**snap** for grid drift you can *hear*. |
| Clips skip at the loop wrap | Lengths that don't divide the bar grid: select the asset → **Quantize**, or enable **auto-quantize**. |
| MIDI not seen | Check the bottom MIDI monitor panel; make sure a source is connected to `simplooper:midi_in`. |
| Effect scripts "compile failed" | Fix the script (error text is in the FX list) → **Rescan**. Missing rustc entirely → install the Rust toolchain; other features keep working. |
| MP3/OGG export does nothing | Install `ffmpeg`. |
| Where do my files live? | Config: `$XDG_CONFIG_HOME/simplooper/config.json` · projects: `~/.local/share/simplooper-projects/` (a setting) · takes: your **primary rack folder** · effects: `~/.local/share/simplooper/effects/`. |

---
Next: [INSTALL.md](INSTALL.md) to build/install, or
[AGENT-GUIDE.md](AGENT-GUIDE.md) if you're automating against this repo.
