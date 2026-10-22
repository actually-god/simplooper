# Using slooper

A quick tour, then the reference details. Everything lives in one window
(~1080×640, resizable down to 780×480); configuration is saved automatically
when you quit, and your session itself saves into a **project** file (Ctrl+S).

slooper is a small **clip-based DAW**: one global playhead over a
metronome-defined bar grid, sixteen tracks with additive child lanes, an
asset rack you drag audio from, audio *and* MIDI takes that place themselves
onto the timeline while you play, per-track synths (bundled piano, sampler
scripts, or hosted VSTi) with a piano roll, and effect/plugin chains that
run live and in export.

- [First launch](#first-launch)
- [The window](#the-window)
- [The asset rack](#the-asset-rack)
- [Clips & lanes](#clips--lanes)
- [Clip trimming](#clip-trimming)
- [Recording takes](#recording-takes)
- [MIDI recording (MREC)](#midi-recording-mrec)
- [Instruments & synths](#instruments--synths)
- [Piano roll](#piano-roll)
- [Transport & playhead](#transport--playhead)
- [Metronome & grid](#metronome--grid)
- [Quantize & trim](#quantize--trim)
- [Projects: save & load](#projects-save--load)
- [Export](#export)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [MIDI control](#midi-control)
- [Track effects & plugins](#track-effects--plugins)
- [Latency compensation](#latency-compensation)
- [Troubleshooting](#troubleshooting)

## First launch

1. Start your JACK-compatible audio server (PipeWire with `pipewire-jack`,
   or jack2). slooper never starts one for you.
2. Launch slooper. The top bar shows `slooper @ 48000 Hz` once the
   audio engine is up.
3. Click the **hamburger ☰** (top-left) → settings panel:
   - **INPUTS** — tick the remote output ports you want to record (your mic /
     instrument chain). Multiple inputs are summed.
   - **OUTPUTS** — for each port tick **L**, **R** or both: `L` plays only
     the left side of the stereo mix, `R` only the right, and both play a
     mono downmix (the default behaviour).
   - **Apply Selection**. Your choice is remembered and re-applied
     automatically on every launch, including when the JACK graph rebuilds.
   - **tracks:** also lives here — show 1–16 tracks; bindings for hidden
     tracks are kept, not dropped.
4. The **asset rack** (left panel) opens on the bundled **TR-808 pack** —
   drag any file onto the grid to hear it. **+ folder** points slooper at
   more samples/backing tracks; the **first folder is the primary folder** —
   everything you record is written there as WAV, so one folder holds both
   your starting material and your growing take library.
5. Live input **monitoring** rides the *selected* track's chain: its gain ×
   volume × effects × pan shape the pass-through, so you hear exactly what
   that track will do to your signal. Takes always capture the raw,
   unprocessed input. Switch the pass-through off entirely in the settings
   panel (**MONITORING → live input monitoring**, default on) — levels,
   recording and playback keep working.

## The window

| Region | Contents |
| --- | --- |
| **Top bar** | ☰ settings · engine status · **PLAY/STOP** (transport) · bar.beat position · **METRO/STOP** button + bpm + beats/measure + subdivision · **Load / Save as / Save / Export** |
| **Left panel** | the [asset rack](#the-asset-rack); ☰ docks the **settings panel to its left**, ☰ again closes it |
| **Grid toolbar** | **loop** toggle + mode dropdown (see [Looping](#looping)), auto-quantize checkbox, **quantize** subdivision, **snap** on/off checkbox + subdivision, **♫ Piano roll** (opens the [note editor](#piano-roll)), zoom slider |
| **Timeline** | **ruler pinned above the tracks** (bar numbers; click/drag scrubs) + one **seamless block per track** — one tall lane strip per lane, hairline separators between tracks (no gaps), one continuous red playhead line drawn across every track, and the **loop region** tinted between its draggable handles; MIDI clips draw their **notes** right on the grid |
| **Mixer** (right dock) | top band (level with the grid header, above Track 1): **master** peak meter + master volume row (−∞…+6 dB; editable dB box, slider and a **0** zeroize button); then one strip per track, aligned to the exact pixel its timeline block occupies (shared heights + scroll): **name · REC/STOP · on/mute · MREC · +/− child** header row, then **vol / gain / pan** sliders (−∞…+6 dB / raw MIDI readout / **0** reset button), an **FX** chip + enabled chain-slot buttons, and a **synth picker** + **sliders** — strips scroll in lockstep with the timeline |
| **Bottom** | MIDI monitor (every event on `slooper:midi_in`) |

Each **mixer strip** holds the track's whole control surface: the name row
(click it to select the track — the strip tints), **REC** (turns **STOP**
while rolling), the play gate (**on** ⇄ **mute** — a muted track contributes
nothing while its clips stay put), **MREC** / **M■** ([MIDI
recording](#midi-recording-mrec)), `+` / `−` (add/remove child lanes), an
`Nc/NL` clips·lanes status, the **vol / gain / pan** sliders with editable
dB / −100…+100 text boxes (+ raw MIDI readout), **FX…** opening that track's
[chain](#track-effects--plugins), and the **♪ synth** combo opening the
track's [instrument](#instruments--synths) (**sliders** edits its
parameters). **vol** and **master volume** both run −∞…+6 dB (0 dB = unity
at three-quarters of the slider).

Tracks sit flush against each other in the timeline — just a hairline
separator between blocks (drag it to resize that track), one unbroken
playhead line, and tall lanes meant for stacking lots of clips. **Right-click a track's lane area** removes that
track's clips (assets stay in the rack; same as `C`), whether you press on
empty grid or on a clip.

## The asset rack

A fresh install ships a **bundled TR-808 pack** — fourteen one-shots sampled
from a real Roland TR-808 (kick, sub, snares, clap, hats, crash, cowbell,
three toms, rimshot, clave; provenance in `samples/808/CREDITS.md`). It's
seeded into `~/.local/share/slooper/samples/808` on startup and is the
default rack folder until you add your own.

Folders at the top, audio under each. Drag any file row onto any lane to
place it; click a row to **select the asset** — and to **audition it**:
clicking a row plays it at the master output (an entry no longer than the
preview length — Settings → **preview length (s)**, default 5 — plays in
full, a longer one plays its first *n* seconds; clicking the row again
stops). Toggle auditioning off in the settings panel (**ASSET PREVIEW →
enable**). Auditions never feed recording or the export bus. Every row
carries a small badge: amber **AUDIO** or blue **MIDI** (`.mid` files and
recorded piano-roll/MREC takes appear as MIDI assets and render their notes
on the clip). Three buttons then act on the selected asset:

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
  and resampled to the engine rate on first use — plus **SMF format-0
  `.mid`** files; listed alphabetically.
- Below the folders, **project assets** lists audio that belongs to the
  current project but was edited/derived since loading; both lists drag.

## Clips & lanes

- **Place**: drag an asset from the rack; a ghost outline shows the landing
  spot. It snaps to the **snap subdivision** grid (grid toolbar).
- **Move**: drag a placed clip along its lane (same snap); drag it **over
  the lane lines** to move it to another lane. Clips on the same track
  **add together** — across lanes and/or overlapping in time — so stacking
  is always additive, never replacing.
- **Select**: click a clip; **Ctrl + click** adds or removes clips from the
  selection; **Shift + click** extends to a same-lane run; **Ctrl + A**
  selects every clip. A group drags sideways as one unit. **Delete**
  removes all selected placements; the assets stay in the rack.
- **Child lanes**: **+** adds a lane to a track; **−** removes the last one
  (its clips go with it — base lane only ever survives). Recording or
  dropping onto an occupied base lane **opens a child lane
  automatically** instead of touching existing clips.
- **Copy & paste**: select (or Ctrl-select) clips, `Ctrl+C` (or right-click
  → **Copy clip** — with nothing selected it copies the selected rack
  asset), then `Ctrl+V` — the whole group lands on the **nearest track to
  your last click** at the playhead, snapped, trims and relative offsets
  intact, opening child lanes where needed, and the view **scrolls to the
  pasted clips**. `Ctrl+D` duplicates the selection in place.
- **Clear**: **right-click the track's lane area** (or press `C` on the selected
  track) removes that track's clips (all lanes except the base, which empties
  too); your rack assets stay untouched. Right-clicking directly **on a clip**
  copies that clip instead.

## Clip trimming

Select a clip and its **edges become handles** (the outer ~6 px of each
end):

- **Drag inward** from either end to trim the clip's window on the asset.
- **Drag the head back outward past its start** to **re-expose audio you
  previously cut** — trimming only moves a window, the asset data is never
  destroyed.
- **Drag the tail past the asset's end** for a **silent extension** (render
  padding — drawn as a dark tail with an amber *original* marker where the
  real audio stops).
- Edges snap to the **asset's own boundaries** regardless of the grid-snap
  setting; the minimum span is one snap unit. With the grid-toolbar **snap**
  checkbox off, edges land on any single sample.

## Recording takes

Press **REC** on a track (button, `R` on the selected track, or a MIDI
binding), play over the timeline, press **STOP** again — or simply let
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

## MIDI recording (MREC)

Every strip carries **MREC** next to REC. Arm it (button, bound MIDI
action) and slooper captures every incoming MIDI message on
`slooper:midi_in` **that is not bound to an action** — your controller's
*unused* traffic becomes note data, and bound buttons never leak into the
take. Press again (or stop the transport) and the take is written as a
**`.mid` asset** into the primary folder and placed as a clip at the
playhead, exactly like an audio take — playable by the track's synth,
editable in the [piano roll](#piano-roll), notes visible on the clip.

## Instruments & synths

Each track plays its MIDI clips through its own **synth binding** (the
`♪ …` combo on the strip):

- **default (piano)** — a bundled physical-modelled (modal string + hammer)
  piano; any track without an explicit binding falls back to it.
- **sampler** — a bundled script that plays any rack asset pitched around a
  **root note** (the slider shows `C3`-style note names).
- **other script synths** — plain Rust files in
  `~/.local/share/slooper/synths`, compiled on first load (⟳ re-scans);
  they share the preset/param machinery of effect scripts.
- **VSTi** — any scanned VST2/VST3 *instrument* appears as
  *name (VSTi)* and is played by the track's MIDI clips.

**sliders** opens the synth's parameter window. For a bound VST2 with a
GUI, the sliders window also offers **open plugin window** — the native
editor runs in its own OS window (requires an X11 display; XWayland
counts). VST3 instruments load and play but currently expose sliders only.
Details in [PLUGINS.md](PLUGINS.md).

The preview path is the same: piano-roll and rack auditions of MIDI go
through the *picked track's* synth + chain, so what you audition is what
mixes.

## Piano roll

The grid toolbar's **♫ Piano roll** opens a full-window MIDI note editor:

- **Draw** notes on the snap grid (click), drag bodies to move, **drag the
  note edges** to resize (arm-on-press, like grid clips), marquee-box to
  select, Ctrl/Shift-click for groups, **Ctrl + A / C / V**, Delete removes.
  Note placement follows the grid-toolbar **snap** setting (off = any
  sample).
- **Preview** routes through a picked **track** — its synth *and* its
  effects chain — and fires as soon as a new note lands.
- The toolbar carries **−bar / +bar** (pattern length), **quantize** (snap
  every note onto the quantize grid), **▶ preview**, **clear**, and
  **save**. The status shows `notes · bars · bpm` — the tempo **is** the
  metronome's; change the bpm up top and the roll follows.
- **save** writes a `roll.mid` (unique name) into the **primary folder**,
  adds it as an asset, and places a clip on the selected track at the
  playhead.
- The roll **fills the window** with its own H/V scrollbars and takes
  keyboard focus when clicked — while it holds focus, main-window shortcuts
  (Space included) don't fire; the **X** in its title bar closes just the
  roll.

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
- **Looping** (grid toolbar): the **loop** toggle plus a mode dropdown,
  greyed while looping is off:

  | Mode | Behaviour |
  | --- | --- |
  | **current longest** (default) | wraps at the end of the longest playing clip — the classic looper feel |
  | **whole song** | wraps at the project end |
  | **area** | wraps between a **start and end handle** shown on the ruler as **bright yellow bars** (default bar 1 → bar 2) — grab either one anywhere on the ruler, snap subdivision respected; **Reset** snaps them back |

  With looping off the playhead parks at the region end and the transport
  stops.
- **Zoom & scroll**: **Ctrl + mouse wheel** zooms the timeline (~12 % per
  notch, same state as the grid-toolbar zoom slider), **Shift + mouse
  wheel** pans horizontally, plain wheel scrolls the track stack vertically.
  (These gestures are intercepted at the top of the frame — egui's own
  Ctrl+wheel interface zoom is pinned back to the GUI-scale setting every
  frame.) The zoom slider is **logarithmic**: at the far-left floor
  (3 px/s) a maximised window shows ~100 measures, the default sits near
  the slider centre, and the right end edits at the sample level.
- **Resize a track**: drag the **separation bar** under it. Only that track
  changes; its lanes (parent + children) always share the height equally.
  A new child lane never shrinks the others — the track grows by one lane's
  worth of height instead. Heights persist in the config.
- **Play all / Stop all** are also separate MIDI/keyboard actions
  (`MidiAction::PlayAll` / `StopAll`) — `Space` toggles, `StopAll` always
  parks. Per-track **play gates** (on/mute) and per-track
  record/clear bindings keep working the same while the global transport
  runs.
- A bare **metronome** (METRO clicked on, transport stopped) ticks on its
  own free-running grid **without moving the playhead or sounding any
  clips** — a click track to play along to. Only the transport advances the
  playhead; transport stopped freezes it, metro running or not. Starting
  the transport while the metro runs simply aligns both clocks.

## Metronome & grid

The metronome **defines the project grid**: **bpm** (20–300, default 120),
**beats/measure** (1–16), and **subdivision** all come from the transport
bar (both numbers are editable text boxes — type and press Enter), and the
ruler/bar numbers/quantize/snap grids all derive from them. Click **METRO**
to start the click (the button reads **STOP** while running; click again to
stop).

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
- **snap** checkbox + dropdown — the grid *clip placement*, drags, and
  piano-roll note editing follow. Untick the checkbox and snapping turns off
  completely: everything lands on **any single sample** (the dropdown
  greys out, with an *off: 1 sample* hint). The timeline also draws the
  snap grid: at sub-beat settings a third, clearly **dimmer** hairline tier
  marks every snap step (ruler and lanes) — line brightness runs
  **measure → beat → subdivision**; the tier vanishes when snap is the
  plain quarter note (the beat line already is the grid), when snap is off,
  or when zoomed out so far the steps would crowd.
- **auto-quantize** — every fresh recording is quantized on the way into
  the rack.

On the selected asset in the rack, **Quantize** trims/extends the take to
the surrounding grid: the cut snaps to the nearest subdivision multiple,
refined to the nearest zero crossing (±5 ms) with a short seam crossfade so
it stays click-free. **Trim** finds the first/last sample above a small
threshold and cuts the silence off both ends (with tiny fades). Both edit
the asset in place, so every clip referencing it follows. (Per-*placement*
windowing lives on the clip — see [Clip trimming](#clip-trimming).)

## Projects: save & load

- **Ctrl+S** saves. The first save of a session opens a **name prompt**;
  after that the same key saves straight into
  `<projects folder>/<name>.slooper`. **Ctrl+Shift+S** always re-asks
  (save-as). The **Save** button mirrors the key; **Save as** opens the
  prompt.
- **Load** lists every project in the projects folder, newest first —
  clicking one replaces the session (unsaved changes are lost, so save
  first).
- The **projects folder** is a setting (☰ settings → Projects): default
  `~/.local/share/slooper-projects`, changeable/browsable.
- A `.slooper` file is a small versioned JSON document: grid settings,
  quantize/snap, per-track mixer + **chain** + **synth binding**, clip
  trims, the whole track/lane/clip arrangement, and the asset table.
  **Audio is never embedded** — assets reference WAV/MIDI files. At save
  time, derived/edited assets are written into
  `<projects folder>/<name> audio/`, and a moved or renamed file is
  re-found by file name inside the project's audio folder or the rack
  folders when loading.

## Export

**Export** (top bar, `E`): render **one track** (its clips mixed, then
vol × gain × chain × pan), the **whole project** (every track summed
through its chain), or the **current selection** — just the selected
clips, bounced **time-aligned** with the first clip at sample 0 — as:

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
| `Delete` | remove all selected clips |
| `Ctrl+A` | select all clips |
| `Ctrl+C` | copy the selection (or the selected rack asset) |
| `Ctrl+V` | paste at the playhead, on the track nearest your last click |
| `Ctrl+D` | duplicate the selection |

While the **piano roll has focus**, `Ctrl + A/C/V` and `Delete` edit notes
instead, and transport keys stay quiet until focus leaves the roll.

## MIDI control

slooper exposes a JACK MIDI input port, **`slooper:midi_in`**. Every
incoming event is parsed and shown in the bottom monitor panel (and on
stdout), so you can verify a controller before binding anything.

**Binding**: settings → *MIDI bindings* → click an action's cell (it turns
blue, *listening…*) → send a **Note On/Off, CC, or Program Change**. The
first message of the matching kind becomes the binding. Velocity / CC value
is ignored for trigger actions — the per-track **volume / gain / pan
(knob)** bindings act on the value (0–127) and support the **three-step
axis calibration** wizard (lock min, lock max; backwards travel is inverted
automatically).

**All bindable actions**: per-track **record**, **MIDI record (MREC)**,
**clear**, **play gate** (×16 each), track select 1–16, select next/prev
(clamped), **play all**, **stop all**, **export**, **metronome**, per-track
volume / gain / pan (knob). Overdub, per-track import, dependent-mode, and
undo/redo actions are gone with the DAW model — v0.1 bindings are migrated
forward automatically (removed slots dropped).

Messages that are **not bound to anything** are the ones
[MREC](#midi-recording-mrec) records.

- Click the cell again or press `Esc` to cancel; `Del` while a cell listens
  clears that binding.
- **Reset to defaults** restores the factory keyboard table.
- Bindings persist across sessions.

## Track effects & plugins

Each track has **one ordered mixer chain** — runtime-compiled Rust effect
scripts and binary VST2/VST3 plugins interleaved in the same list (top of
the list = first in the signal path). Open it with the strip's **FX** chip:

- **Chain (top = first)** — one row per slot: **↑ / ↓** reorder, **×**
  remove. A slot whose file vanished or failed to load stays visible as
  *× not loaded* until the next rescan. Clicking a loaded slot opens its
  editor.
- **+ effect** adds a scripted effect; **+ VST plugin** adds a scanned
  binary plugin. **Effects folder** / **Rescan effects** manage script
  location (default `~/.local/share/slooper/effects`, seeded with
  `template.rs` + the ABI contract on first run); **Plugin folders**
  (Add / ×) + **Rescan Plugins** manage VST scanning (`~/.vst`, `~/.vst3`,
  system dirs, plus your own).
- **Editors**: effect scripts show their **scripted custom GUI** or
  automatic parameter sliders (with the shared user-preset strip saving
  `{effect}-{name}.json` under `<effects folder>/user presets/`); plugins
  show automatic parameter sliders, kept live in both directions.
  Track-bound VST2 instruments can additionally open their **native editor
  window** from the [synth window](#instruments--synths)
  ([details](PLUGINS.md)).
- Instances are **per-track** (the same plugin/effect on two tracks = two
  independent instances).

Signal order per track: `clip × volume × gain → chain in order →
pan → output buses`. The same chain runs during export renders.

**Writing effects**: one standalone Rust file dropped into the effects
folder — copy `template.rs` (also in [`../effects/`](../effects)), rename,
edit. It `include!`s the ABI contract (`effect_api.rs`, written into the
folder for you) and implements `create` / `process` / `destroy` plus a
description block (name required; up to **32 parameters**; optional custom
GUI entry point). Scripts compile with
`rustc --edition 2021 --crate-type cdylib` — so **rustc must be installed**
to use custom effects. Keep `process()` allocation-free and non-blocking —
it runs in the realtime audio path. Script **synths** follow the same
pattern (`synths/synth_api.rs`); the bundled `piano.rs` and `sampler.rs`
are the reference implementations.

## Latency compensation

Settings → **latency (ms)**: each fresh take discards its first N ms of
input, cutting out the silence gap between pressing REC and the sound
reaching the mic — set it to your system's capture latency and your
button-press click lands exactly on the clip start. Monitoring and the
output mix are never delayed, so nothing drifts.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| "no JACK server" / connection error on launch | Start PipeWire or `jackd` first — slooper never starts a server itself (`NO_START` by design). |
| No sound although the engine shows ready | ☰ settings → tick output (and input) ports → **Apply Selection**. Selections persist, but if your hardware changed, re-apply. |
| Record button does nothing | Takes write into the **primary (first) rack folder** before they can become clips — the bundled 808 pack covers you by default, but if you removed every folder, press **+ folder**; the status line names the reason. |
| Recorded clip starts early/late | Settings → latency (ms); and check **auto-quantize**/**snap** for grid drift you can *hear*. |
| Clips skip at the loop wrap | Lengths that don't divide the bar grid: select the asset → **Quantize**, or enable **auto-quantize**. |
| MIDI not seen | Check the bottom MIDI monitor panel; make sure a source is connected to `slooper:midi_in`. |
| MIDI clip plays nothing | Check the track's **♪ synth** combo (default piano plays anything), and whether the bound script/VSTi loaded — the status line reports synth load errors; **⟳** / **Rescan Plugins** re-scan after installs. |
| Effect scripts "compile failed" | Fix the script (error text is in the chain window) → **Rescan**. Missing rustc entirely → install the Rust toolchain; other features keep working. |
| MP3/OGG export does nothing | Install `ffmpeg`. |
| Where do my files live? | Config: `$XDG_CONFIG_HOME/slooper/config.json` · projects: `~/.local/share/slooper-projects/` (a setting) · takes: your **primary rack folder** · effects: `~/.local/share/slooper/effects/` · synth scripts: `~/.local/share/slooper/synths/`. |

---
Next: [INSTALL.md](INSTALL.md) to build/install, [PLUGINS.md](PLUGINS.md)
for synth scripts & VST hosting, or [AGENT-GUIDE.md](AGENT-GUIDE.md) if
you're automating against this repo.
