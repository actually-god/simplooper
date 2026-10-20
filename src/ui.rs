//! egui front-end for slooper: a small DAW — sixteen tracks with child
//! lanes, a global draggable playhead, a global loop region, an asset rack,
//! clip copy/paste, clip-based arrangement, metronome-anchored quantize and
//! `.slooper` projects.
//!
//! Layout:
//! - top bar: hamburger (settings), status, transport, position readout,
//!   tempo + metronome, Save / Save as / Load / Export, peak meter, master
//!   volume (dB scale, −∞…+6)
//! - left panel: ASSET RACK — folder list (primary = recordings land there;
//!   defaults to the seeded [`crate::samples808`] pack), one row per audio
//!   file, project assets below
//! - central: grid toolbar (loop toggle + mode, auto-quantize,
//!   quantize/snap subdivisions, zoom — also Ctrl+wheel; Shift+wheel pans
//!   horizontally) above the timeline: a ruler pinned above the track
//!   scroll (scrub + draggable loop handles) and one seamless block per
//!   track (flush lane strips, hairline separators, one continuous playhead
//!   line). Track geometry is shared with the mixer via [`App::track_rows`].
//! - right panel: MIXER dock — one strip per track, vertically aligned with
//!   its timeline block: name/REC/play-gate/child buttons, vol/gain/pan
//!   sliders with dB text boxes, FX button; scrolls in lockstep
//! - the settings panel docks to the LEFT of the asset rack when open
//! - bottom: MIDI monitor
//! - windows: settings, effects, effect editor, knob learner, export,
//!   save-project prompt, load-project browser
//!
//! Clipboard: one clip lives in `clip_clipboard`; paste targets the track
//! nearest the last grid click (`paste_track`), at the playhead.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::Frame;
use egui::{Color32, RichText, Sense, Slider};

use crate::arrange::{Arrangement, Clip};
use crate::assets::{Asset, RackFolder};
use crate::audio::{
    Backend, Cmd, Evt, FxSlot, OutSel, PortInfo, TrackFx, GAIN_UNITY_MIDI, MAX_LOOPS, TR_REC,
};
use crate::config::{Config, MidiAxisCal, MidiBinding, TrackFxSave};
use crate::fx;
use crate::midi::{hex, scale_axis, MidiAction, MidiBind, MidiMsg};
use crate::project::{self, ProjAsset, ProjClip, ProjMetro, ProjQuantize, ProjTrack, Project};
use crate::quantize::{self, snap_start};

/// Keys a user may plausibly bind to a shortcut.
const KEY_CANDIDATES: [egui::Key; 51] = [
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
    egui::Key::Space,
    egui::Key::Enter,
    egui::Key::Tab,
    // Delete is deliberately NOT a candidate: it is reserved for clearing
    // bindings (it unbinds the cell being edited in both columns) and can
    // never be bound to an action. Configs persisted by builds where it was
    // assignable drop the binding to unbound on load.
    egui::Key::A,
    egui::Key::B,
    egui::Key::C,
    egui::Key::D,
    egui::Key::E,
    egui::Key::F,
    egui::Key::G,
    egui::Key::H,
    egui::Key::I,
    egui::Key::J,
    egui::Key::K,
    egui::Key::L,
    egui::Key::M,
    egui::Key::N,
    egui::Key::O,
    egui::Key::P,
    egui::Key::Q,
    egui::Key::R,
    egui::Key::S,
    egui::Key::T,
    egui::Key::U,
    egui::Key::V,
    egui::Key::W,
    egui::Key::X,
    egui::Key::Y,
    egui::Key::Z,
    egui::Key::F1,
    egui::Key::F2,
    egui::Key::F3,
    egui::Key::F4,
    egui::Key::F5,
    egui::Key::F6,
    egui::Key::F7,
    egui::Key::F8,
    egui::Key::F9,
    egui::Key::F10,
    egui::Key::F11,
    egui::Key::F12,
];

/// One row in the MIDI monitor log.
#[derive(Clone)]
struct MidiLine {
    ts: String,
    type_name: &'static str,
    describe: String,
    raw_hex: String,
}

fn key_debug(k: egui::Key) -> String {
    format!("{k:?}")
}

/// Short display form of a key for the shortcut rows.
fn key_display(k: egui::Key) -> String {
    match k {
        egui::Key::Num0 => "0".into(),
        egui::Key::Num1 => "1".into(),
        egui::Key::Num2 => "2".into(),
        egui::Key::Num3 => "3".into(),
        egui::Key::Num4 => "4".into(),
        egui::Key::Num5 => "5".into(),
        egui::Key::Num6 => "6".into(),
        egui::Key::Num7 => "7".into(),
        egui::Key::Num8 => "8".into(),
        egui::Key::Num9 => "9".into(),
        egui::Key::Space => "space".into(),
        other => format!("{other:?}"),
    }
}

/// Parse a persisted key name back into an `egui::Key` (None if unknown).
fn key_from_name(s: &str) -> Option<egui::Key> {
    KEY_CANDIDATES
        .iter()
        .find(|k| format!("{k:?}") == s)
        .copied()
}

// --- per-loop level controls ---------------------------------------------
// MIDI knobs/sliders send 0..=127. Volume runs -inf dB (silence) at 0 up to
// **+6 dB** at 127; the sliders show the resulting dB.

/// MIDI value of 0 dB on the volume sliders (-inf..=+6 dB range).
pub const VOL_UNITY_MIDI: u8 = 115;

/// Volume mapping: 0 = silence (-inf), 1 = -60 dB, [`VOL_UNITY_MIDI`] =
/// exactly 0 dB, 127 = +6 dB — linear in dB on each leg so the slider feels
/// even, with the 0 dB point sitting where the track/master defaults are.
pub fn vol_amp(v: u8) -> f32 {
    let db = if v <= VOL_UNITY_MIDI {
        (v as f32 - 1.0) * (60.0 / (VOL_UNITY_MIDI as f32 - 1.0)) - 60.0
    } else {
        (v as f32 - VOL_UNITY_MIDI as f32) * (6.0 / (127.0 - VOL_UNITY_MIDI as f32))
    };
    if v == 0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// dB -> volume MIDI value for the -inf..=+6 dB range (inverse of
/// [`vol_amp`]; finite values floor at 1, "-inf" is 0).
pub fn vol_midi(db: f32) -> u8 {
    if !db.is_finite() || db <= -60.0 {
        return if db == f32::NEG_INFINITY { 0 } else { 1 };
    }
    if db >= 6.0 {
        return 127;
    }
    if db <= 0.0 {
        (1.0 + (db + 60.0) * ((VOL_UNITY_MIDI - 1) as f32 / 60.0))
            .round()
            .clamp(1.0, VOL_UNITY_MIDI as f32) as u8
    } else {
        (VOL_UNITY_MIDI as f32 + db * ((127 - VOL_UNITY_MIDI) as f32 / 6.0))
            .round()
            .clamp(VOL_UNITY_MIDI as f32, 127.0) as u8
    }
}

/// Gain-trim mapping: 0 = silence, 80 = 0 dB, 127 = +10 dB
/// (~0.215 dB per step; below 80 it attenuates toward -inf at 1).
pub fn gain_amp(v: u8) -> f32 {
    if v == 0 {
        0.0
    } else {
        let db = (v as i32 - GAIN_UNITY_MIDI as i32) as f32 * 0.215;
        10f32.powf(db / 20.0)
    }
}

/// dB display string ("  +0.0 dB", "-12.3 dB", "-inf dB").
pub fn db_str(amp: f32) -> String {
    if amp <= 0.0 {
        "-inf dB".into()
    } else {
        format!("{:+.1} dB", 20.0 * amp.log10())
    }
}

/// MIDI value that centres the per-loop pan slider (the -100..=100 text
/// shows 0 there; pan is an equal-power split between the L / R buses).
pub const PAN_CENTRE_MIDI: u8 = 64;

/// Pan MIDI value (0..=127) -> the -100..=100 position shown in the text
/// box: -100 = hard left, 0 = centre, +100 = hard right. (The old scale was
/// 0..=100 with 50 centre; this is that position doubled and shifted, so
/// every old position maps 1:1 — 0 -> -100, 50 -> 0, 100 -> 100.)
pub fn pan_text(m: u8) -> i32 {
    (pan_text_old(m) as i32) * 2 - 100
}

/// The legacy 0..=100 position (50 = centre), kept as the intermediate so
/// centre lands exactly on 0 for the MIDI centre value.
fn pan_text_old(m: u8) -> u32 {
    (m as f32 * 100.0 / 127.0).round() as u32
}

/// -100..=100 text position -> pan MIDI value (0..=127).
pub fn pan_midi(t: f32) -> u8 {
    (((t.clamp(-100.0, 100.0) + 100.0) * 0.5) * 127.0 / 100.0).round() as u8
}

/// Engine pan position [0, 1] for a MIDI value (hard right at 127).
pub fn pan01(m: u8) -> f32 {
    m as f32 / 127.0
}

/// L/R routing byte of output `i`, defaulting to the mono downmix.
fn cfg_lr(v: &[u8], i: usize) -> u8 {
    *v.get(i).unwrap_or(&3)
}

/// In-flight three-step calibration for one continuous (knob) MIDI binding.
///
/// Bind-wizard state for one continuous (knob) binding slot.
/// Step 1 captures the AXIS — message TYPE + CHANNEL, never the data byte.
/// While the axis is unlocked every message re-picks it, so moving a
/// different control changes the knob being bound; **Lock axis** freezes it.
/// Step 2 then listens on that axis for ANY 0..=127 data value until
/// **Lock min**, and step 3 listens again until **Finish**. `lo`/`hi` track
/// the latest value seen on each end so a noisy first touch is not fatal.
/// `lo > hi` (a control wired backwards) is stored as captured —
/// `scale_axis` flips it so the translation stays continuous and monotonic.
struct KnobLearn {
    slot: usize,
    /// Axis (type + channel); `d1` is the last full id seen, shown for
    /// orientation ("which physical knob am I looking at?") only.
    bind: MidiBind,
    has_axis: bool,
    axis_locked: bool,
    /// Data byte of the most recent message (value/velocity/program), used
    /// to seed `lo` the moment the axis locks.
    last_val: Option<u8>,
    lo: u8,
    hi: u8,
    /// min locked (step 2 done) — collecting the maximum.
    locked: bool,
}

/// Number of bindable actions — one keyboard slot per entry of
/// [`MidiAction::ALL`].
pub const N_ACTIONS: usize = MidiAction::ALL.len();

/// Default keyboard bindings, one per entry of `MidiAction::ALL` (same
/// order); `None` = no default. Layout matches `MidiAction::ALL`: selected
/// actions (0–3), select 1–16 (4–19), per-track rec/clear/play × 16
/// (20–67, MIDI-bound), metronome + selection + transport (68–72), then the
/// value-driven knob ranges (never key-bound).
const SHORTCUT_DEFAULTS: [Option<egui::Key>; N_ACTIONS] = {
    let mut a = [None; N_ACTIONS];
    a[0] = Some(egui::Key::R); // record (selected)
    a[1] = Some(egui::Key::C); // clear (selected)
    a[3] = Some(egui::Key::E); // export prompt
    a[4] = Some(egui::Key::Num1);
    a[5] = Some(egui::Key::Num2);
    a[6] = Some(egui::Key::Num3);
    a[7] = Some(egui::Key::Num4);
    a[68] = Some(egui::Key::M); // metronome on / off
    a[71] = Some(egui::Key::Space); // play / stop all (transport toggle)
    a
};

/// Grid subdivisions, shared by the metronome click, the quantize grid and
/// the clip snap grid: (steps per beat, display name).
const SUBDIVISIONS: [(u32, &str); 6] = [
    (1, "quarter note"),
    (2, "eighth note"),
    (3, "triplet"),
    (4, "sixteenth note"),
    (6, "triplet eighths"),
    (8, "thirty-second note"),
];

/// Min/max columns for one asset's waveform thumbnail (cached per asset
/// index + column count; `peak` normalizes quiet takes to full height).
struct AssetWave {
    cols: usize,
    data: Vec<[f32; 2]>,
}

/// A clip being dragged on the timeline.
#[derive(Clone, Copy)]
struct ClipDrag {
    t: usize,
    lane: usize,
    c: usize,
    /// Samples between the clip's start and the grab point, so the clip
    /// does not jump when the drag begins.
    grab: u64,
}

const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 95, 86);
const COL_CLIP: Color32 = Color32::from_rgb(72, 118, 196);
const COL_CLIP_SEL: Color32 = Color32::from_rgb(128, 200, 255);
const COL_GHOST: Color32 = Color32::from_rgb(120, 160, 120);
const COL_BAR_LINE: Color32 = Color32::from_gray(120);
const COL_BEAT_LINE: Color32 = Color32::from_gray(68);
/// Sub-beat snap lines: a third, even fainter tier drawn at the snap
/// subdivision (grid-toolbar snap combo), so clip placement has a visible
/// target without cluttering the bar/beat grid.
const COL_SNAP_LINE: Color32 = Color32::from_rgba_premultiplied(120, 120, 128, 22);
/// Nominal lane-strip height in the grid. A track's block is
/// `lanes × LANE_H`, never shorter than `TRACK_MIN_H` (the height its
/// mixer strip needs), so clips get tall waveforms and the mixer stays
/// aligned strip-to-track.
const LANE_H: f32 = 48.0;
const TRACK_MIN_H: f32 = 96.0;
/// Smallest height a lane may be squeezed to when the user resizes a track.
const LANE_MIN_H: f32 = 36.0;
/// Separation bar painted between adjacent tracks (the only gap left).
const TRACK_SEP: f32 = 3.0;
/// Hit height around the separation bar — it doubles as the resize handle.
const TRACK_SEP_HIT: f32 = 7.0;
/// Loop region modes (see [`App::loop_mode`]); index = config value.
const LOOP_MODES: [&str; 3] = ["area", "current longest", "whole song"];

/// Timeline zoom range in px/second. The floor is tuned so max zoom-out
/// shows ~100 measures in the ~940 pt viewport of the default 1440×853
/// window at 80 BPM 4/4 (3 s per bar → 300 s → 940/300 ≈ 3.1 px/s), and
/// the ceiling gives deep zoom-in for sample-level editing. The default
/// stays at 160 px/s; the toolbar slider maps this range logarithmically,
/// which puts the default near the slider's centre (≈60 %).
const ZOOM_MIN: f32 = 3.0;
const ZOOM_MAX: f32 = 2400.0;
const ZOOM_DEFAULT: f32 = 160.0;
/// Ctrl+wheel zoom sensitivity: 12 % per wheel line (trackpad pixel deltas
/// use 1 % per px so a fast flick doesn't warp the timeline).
const ZOOM_WHEEL_LINE: f32 = 0.12;
const ZOOM_WHEEL_PX: f32 = 0.01;

/// The user's home folder (projects default to `~/slooper-projects`).
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The whole application: engine backend handle, persistent UI state, and
/// the per-frame layout cache (`row_h` → [`App::track_rows`]) that keeps the
/// timeline blocks and the Mixer dock strip-per-track aligned.
pub struct App {
    backend: Backend,

    // --- audio / device state ---
    status: String,
    client_name: Option<String>,
    sample_rate: u32,
    avail_in: Vec<PortInfo>,
    avail_out: Vec<PortInfo>,
    sel_in: HashSet<String>,
    /// Selected outputs with their stereo routing (bit 0 = L, bit 1 = R);
    /// absent / 0 = not selected.
    sel_out: HashMap<String, u8>,
    applied: Option<(usize, usize)>,
    applied_ports: Option<(Vec<String>, Vec<OutSel>)>,
    /// Master output volume, MIDI-range (-inf..=+6 dB via [`vol_amp`]).
    master_midi: u8,
    /// GUI zoom factor (Settings → GUI); 1.0 = default UI size.
    gui_scale: f32,
    show_settings: bool,

    // --- mixer (per track) ---
    trk_vol: [u8; MAX_LOOPS],
    trk_gain: [u8; MAX_LOOPS],
    trk_pan: [u8; MAX_LOOPS],
    lv_edit: HashMap<(u8, usize), String>,
    /// Last track selection propagated to the audio engine (input
    /// monitoring follows the selected track's chain/gain/pan).
    sel_sent: usize,
    levels_dirty: bool,

    // --- DAW state ---
    /// Track targeted by keyboard/MIDI "selected" actions.
    selected: usize,
    /// Timeline loop: wrap the playhead at the loop region's end (mode
    /// below) vs. park at the project end.
    loop_tl: bool,
    /// Loop region mode: 0 = area (dragged handles), 1 = current longest
    /// (0 → last clip end), 2 = whole song (0 → project end).
    loop_mode: usize,
    /// Area loop bounds in samples (mode 0; end 0 = "not set yet" → 2 bars
    /// once the sample rate is known).
    loop_area: (u64, u64),
    /// Which area-handle is being dragged on the ruler (0 = start, 1 = end).
    drag_loop_handle: Option<u8>,
    metro_on: bool,
    metro_bpm: String,
    metro_num: String,
    /// Index into [`SUBDIVISIONS`] (click subdivision).
    metro_sub: usize,
    /// Index into [`SUBDIVISIONS`] for content quantize / clip snap.
    q_sub: usize,
    snap_sub: usize,
    /// Quantize every fresh recording's content onto the grid.
    auto_quantize: bool,
    latency_ms: String,

    // --- project content (GUI master copies; engine holds Arc snapshots) ---
    arr: Arrangement,
    assets: Vec<Asset>,
    /// Waveform thumbnails by asset index (cleared on structural edits).
    wave: HashMap<u32, AssetWave>,
    /// Timeline zoom.
    px_per_sec: f32,
    drag_clip: Option<ClipDrag>,
    /// A press on empty grid owns the pointer until release: dragging it
    /// scrubs the global playhead (aligned with the lane grid exactly).
    scrub_drag: bool,
    /// Horizontal view offset of the timeline in px (shared by the pinned
    /// ruler and every lane strip — the playhead stays one straight line).
    hscroll: f32,
    /// Vertical scroll offset of the track stack, shared with the mixer
    /// panel so each mixer strip stays aligned with its track.
    vscroll: f32,
    /// Per-track grid-block height in points (0 = automatic:
    /// `lanes × LANE_H`). The user drags the separation bar under a track
    /// to resize it; the mixer strip mirrors the same height.
    trk_height: Vec<f32>,
    /// Active separation-bar drag: (track, height at drag start, pointer y
    /// at drag start). Growing a track never resizes its neighbours.
    sep_drag: Option<(usize, f32, f32)>,
    /// Absolute y of the timeline track stack's top edge, measured each
    /// frame by the central panel; the mixer aligns its strips to it (one
    /// frame late at most) so strip ↔ track can never drift apart.
    grid_anchor: f32,
    /// Selected clip (track, lane, clip index).
    sel_clip: Option<(usize, usize, usize)>,
    /// Track owning the last primary press inside the grid — clipboard
    /// paste targets this track (falls back to `selected`).
    last_click_track: Option<usize>,
    /// Clip copy buffer: just the asset (paste re-places it at the playhead
    /// on the last-clicked track, opening a child lane when busy).
    copied_asset: Option<u32>,

    // --- asset rack ---
    rack: Vec<RackFolder>,
    sel_asset: Option<usize>,
    /// Audition rack entries on click (Settings → ASSET PREVIEW).
    preview_enabled: bool,
    /// Text box value for the preview length in seconds.
    preview_secs: String,
    /// Asset name currently auditioning (clicking it again toggles stop).
    preview_playing: Option<String>,

    // --- project file ---
    proj_name: String,
    /// Text override for the projects folder ("" = `~/slooper-projects`).
    project_dir: String,
    show_save_prompt: bool,
    save_draft: String,
    show_load: bool,

    // --- per-track effects framework ---
    fx_registry: fx::Registry,
    fx_entries: Vec<Vec<fx::UiEntry>>,
    fx_live: Vec<Vec<std::sync::Arc<fx::Instance>>>,
    fx_window: Option<usize>,
    fx_editor: Option<Box<crate::fxgui::FxEditor>>,
    fx_dir_edit: String,
    fx_note: String,
    fx_save_pending: bool,

    // --- MIDI bindings: one slot per entry of MidiAction::ALL ---
    midi_binds: [Option<MidiBind>; N_ACTIONS],
    short_keys: [Option<egui::Key>; N_ACTIONS],
    learning_slot: Option<usize>,
    midi_learning_slot: Option<usize>,
    knob_learn: Option<KnobLearn>,
    knob_cal: Vec<Option<MidiAxisCal>>,
    midi_sources: Vec<String>,
    bind_loop_sel: usize,

    // --- export dialog ---
    show_export: bool,
    export_whole: bool,
    export_path: String,
    export_fmt: usize,

    // --- MIDI monitor ---
    last_midi: Option<MidiLine>,
    midi_log: VecDeque<MidiLine>,
    peak_disp: f32,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        let cfg = Config::load();
        // Effects framework: open (creating/seeding when needed) the folder
        // and compile+load every script; chains rebuild once the engine
        // sample rate is known (Evt::Ready).
        let fx_dir = fx::prepare_effects_dir(if cfg.fx_dir.trim().is_empty() {
            None
        } else {
            Some(cfg.fx_dir.trim())
        });
        let mut fx_registry = fx::Registry::open(fx_dir.clone());
        fx_registry.rescan();
        let fx_stems = fx_registry.stems();
        let fx_entries: Vec<Vec<fx::UiEntry>> = (0..MAX_LOOPS)
            .map(|i| {
                let saved: Vec<(String, Vec<f32>)> = cfg
                    .track_fx
                    .get(i)
                    .map(|v| {
                        v.iter()
                            .map(|s| (s.stem.clone(), s.params.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                fx::merge_entries(&[], &fx_stems, &saved)
            })
            .collect();
        // Keyboard table: this layout is trusted verbatim; older persisted
        // layouts are remapped, then defaults fill the gaps.
        let migrated = crate::midi::migrate_shortcuts(
            &cfg.shortcuts
                .iter()
                .map(|s| s.as_deref().and_then(key_from_name))
                .collect::<Vec<_>>(),
        );
        let trusted = cfg.shortcuts.len() == N_ACTIONS;
        let short_keys = core::array::from_fn(|i| match migrated.get(i).copied().flatten() {
            Some(k) => Some(k),
            None if trusted => None,
            None => SHORTCUT_DEFAULTS[i],
        });
        let mut midi_binds = [None; N_ACTIONS];
        for b in &cfg.midi {
            let i = MidiAction::idx(b.action);
            if i < N_ACTIONS {
                if let Some(bd) = b.bind {
                    midi_binds[i] = Some(bd);
                }
            }
        }
        let mut knob_cal: Vec<Option<MidiAxisCal>> = vec![None; N_ACTIONS];
        for c in &cfg.midi_axis {
            if let Some(s) = knob_cal.get_mut(usize::from(c.slot)) {
                *s = Some(*c);
            }
        }
        // Per-track levels: pad persisted vectors and push into the engine
        // atomics — those are the live source of truth for the RT thread.
        let trk_vol =
            core::array::from_fn(|i| cfg.trk_vol.get(i).copied().unwrap_or(VOL_UNITY_MIDI));
        let trk_gain =
            core::array::from_fn(|i| cfg.trk_gain.get(i).copied().unwrap_or(GAIN_UNITY_MIDI));
        let trk_pan = core::array::from_fn(|i| cfg.trk_pan.get(i).copied().unwrap_or(64));
        for t in 0..MAX_LOOPS {
            backend.engine.set_loop_vol(t, vol_amp(trk_vol[t]));
            backend.engine.set_loop_gain(t, gain_amp(trk_gain[t]));
            backend.engine.set_loop_pan(t, pan01(trk_pan[t]));
        }
        let sel_in: HashSet<String> = cfg.ports_in.iter().cloned().collect();
        let sel_out: HashMap<String, u8> = cfg
            .ports_out
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), cfg_lr(&cfg.ports_out_lr, i)))
            .collect();
        let applied_ports = if cfg.ports_in.is_empty() && cfg.ports_out.is_empty() {
            None
        } else {
            Some((
                cfg.ports_in.clone(),
                cfg.ports_out
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), cfg_lr(&cfg.ports_out_lr, i)))
                    .collect(),
            ))
        };
        // Bundled 808 pack: seed it into the app data dir; it becomes the
        // default rack folder when the user has configured none — takes then
        // record into it and every 808 voice is one drag away.
        let seeded = crate::samples808::seed_pack();
        let mut rack: Vec<RackFolder> = cfg
            .rack_folders
            .iter()
            .map(|p| RackFolder::new(p.clone()))
            .collect();
        if rack.is_empty() {
            rack.push(RackFolder::new(seeded.to_string_lossy().into_owned()));
        }
        for f in rack.iter_mut() {
            f.rescan();
        }
        let sub_idx = |v: u32| SUBDIVISIONS.iter().position(|(c, _)| *c == v).unwrap_or(3);
        Self {
            backend,
            status: "starting…".into(),
            client_name: None,
            sample_rate: 0,
            avail_in: Vec::new(),
            avail_out: Vec::new(),
            sel_in,
            sel_out,
            applied: None,
            applied_ports,
            master_midi: cfg.master_vol.min(127),
            gui_scale: cfg.gui_scale.clamp(0.7, 1.6),
            show_settings: false,
            trk_vol,
            trk_gain,
            trk_pan,
            lv_edit: HashMap::new(),
            sel_sent: usize::MAX,
            levels_dirty: false,
            selected: 0,
            loop_tl: cfg.loop_tl,
            loop_mode: cfg.loop_mode.min(2),
            loop_area: (cfg.loop_area_start, cfg.loop_area_end),
            drag_loop_handle: None,
            metro_on: false,
            metro_bpm: cfg.metro_bpm.to_string(),
            metro_num: cfg.metro_num.clamp(1, 16).to_string(),
            metro_sub: SUBDIVISIONS
                .iter()
                .position(|(c, _)| *c == cfg.metro_sub)
                .unwrap_or(0),
            q_sub: sub_idx(cfg.q_sub),
            snap_sub: sub_idx(cfg.snap_sub),
            auto_quantize: cfg.auto_quantize,
            latency_ms: if cfg.latency_ms == 0.0 {
                "0".into()
            } else {
                format!("{}", cfg.latency_ms)
            },
            arr: Arrangement::new(),
            assets: Vec::new(),
            wave: HashMap::new(),
            px_per_sec: ZOOM_DEFAULT,
            drag_clip: None,
            scrub_drag: false,
            hscroll: 0.0,
            vscroll: 0.0,
            trk_height: (0..MAX_LOOPS)
                .map(|i| cfg.trk_h.get(i).copied().unwrap_or(0.0))
                .collect(),
            sep_drag: None,
            grid_anchor: 0.0,
            sel_clip: None,
            last_click_track: None,
            copied_asset: None,
            rack,
            sel_asset: None,
            preview_enabled: cfg.preview_on,
            preview_secs: cfg.preview_secs.max(0.0).to_string(),
            preview_playing: None,
            proj_name: String::new(),
            project_dir: cfg.project_dir.clone(),
            show_save_prompt: false,
            save_draft: String::new(),
            show_load: false,
            fx_registry,
            fx_entries,
            fx_live: vec![Vec::new(); MAX_LOOPS],
            fx_window: None,
            fx_editor: None,
            fx_dir_edit: fx_dir.display().to_string(),
            fx_note: String::new(),
            fx_save_pending: false,
            midi_binds,
            short_keys,
            learning_slot: None,
            midi_learning_slot: None,
            knob_learn: None,
            knob_cal,
            midi_sources: Vec::new(),
            bind_loop_sel: 0,
            show_export: false,
            export_whole: false,
            export_path: String::new(),
            export_fmt: 0,
            last_midi: None,
            midi_log: VecDeque::with_capacity(300),
            peak_disp: 0.0,
        }
    }

    fn now_stamp() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() % 86_400)
            .unwrap_or(0);
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        )
    }

    /// File-name suffix that keeps fresh recordings unique.
    fn fstamp() -> String {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format!("{}{:03}", d.as_secs() % 100_000, d.subsec_millis())
    }

    /// Process all pending backend events right now (UI loop + tests).
    pub fn drain_events(&mut self) {
        while let Ok(evt) = self.backend.evt_rx.try_recv() {
            self.handle_evt(evt);
        }
    }

    /// Arm the MIDI "listening" slot for `idx` — what clicking a binding cell
    /// does; the next bindable MIDI message becomes that slot's binding.
    /// Arming cancels any armed keyboard cell: only one cell may listen at a
    /// time, otherwise the key-learn branch would swallow Delete/keys first.
    pub fn arm_midi_slot(&mut self, idx: usize) {
        self.midi_learning_slot = Some(idx);
        self.learning_slot = None;
    }

    /// Start the three-step knob learn for continuous slot `idx`: opens a
    /// sub-window that walks the user through locking the axis (type +
    /// channel), then capturing the raw minimum and maximum (see
    /// [`knob_learn_window`]).
    pub fn arm_knob_learn(&mut self, idx: usize) {
        self.midi_learning_slot = None;
        self.learning_slot = None;
        self.knob_learn = Some(KnobLearn {
            slot: idx,
            bind: MidiBind {
                ty: 0xff,
                ch: 0,
                d1: 0,
            },
            has_axis: false,
            axis_locked: false,
            last_val: None,
            lo: 0,
            hi: 127,
            locked: false,
        });
        self.status = format!(
            "knob learn: {} — move the knob to define its axis",
            MidiAction::ALL[idx].label()
        );
    }

    /// Step 1 done: freeze the axis (type + channel) found so far and start
    /// collecting the minimum. While the axis is unlocked, every message
    /// re-picks it (moving a different knob changes the binding); after
    /// Lock axis, messages on other axes are rejected. Restart unlocks it
    /// again.
    pub fn knob_lock_axis(&mut self) {
        if let Some(kl) = self.knob_learn.as_mut() {
            if !kl.has_axis {
                return;
            }
            kl.axis_locked = true;
            // Seed the min readout with the value seen at lock time, then
            // let step 2 follow every message from here on.
            kl.lo = kl.last_val.unwrap_or(kl.lo);
            self.status = format!(
                "axis {} locked — sweep the control to its MINIMUM, then Lock min",
                kl.bind.axis_label()
            );
        }
    }

    /// Step 2 done: freeze the minimum and start collecting the maximum.
    pub fn knob_lock_min(&mut self) {
        if let Some(kl) = self.knob_learn.as_mut() {
            kl.locked = true;
            kl.hi = kl.lo;
            self.status = "sweep the control to its MAXIMUM, then Finish".into();
        }
    }

    /// Step 3 done: commit the axis and the raw lo/hi range. lo > hi (an
    /// inverted control) is stored exactly as captured — `scale_axis` flips
    /// it so the knob still reads low→high continuously.
    pub fn knob_finish(&mut self) {
        let Some(kl) = self.knob_learn.take() else {
            return;
        };
        if kl.hi == kl.lo {
            // Same raw value at both ends: there is nothing to scale against.
            self.knob_learn = Some(kl);
            self.status = "finish needs a maximum different from the minimum — sweep the control and Finish again".into();
            return;
        }
        let (slot, lo, hi, bind) = (kl.slot, kl.lo, kl.hi, kl.bind);
        self.midi_binds[slot] = Some(bind);
        // 0..=127 is the program's native scale: no factor needed.
        self.knob_cal[slot] = if lo == 0 && hi == 127 {
            None
        } else {
            Some(MidiAxisCal {
                slot: slot as u16,
                lo,
                hi,
            })
        };
        self.save_config();
        self.status = format!(
            "bound {} → axis {} (raw {}–{}{})",
            MidiAction::ALL[slot].label(),
            bind.axis_label(),
            lo,
            hi,
            if lo > hi {
                " — INVERTED control: flipped so the knob still reads low→high"
            } else if lo == 0 && hi == 127 {
                ""
            } else {
                ", rescaled to full range"
            }
        );
    }

    /// Arm the keyboard "press a key" slot for `idx` (cancels any MIDI cell
    /// that was listening — see [`Self::arm_midi_slot`]).
    pub fn arm_key_slot(&mut self, idx: usize) {
        self.learning_slot = Some(idx);
        self.midi_learning_slot = None;
    }

    /// The currently captured MIDI binding for slot `idx`, if any.
    pub fn midi_bind_at(&self, idx: usize) -> Option<MidiBind> {
        self.midi_binds[idx]
    }

    /// Read-only backend access (tests / diagnostics).
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Currently selected loop index (test access).
    pub fn selected_loop(&self) -> usize {
        self.selected
    }

    /// Global keyboard shortcuts (IDEA.md: "simple action mapping system with
    /// midi input and keyboard support"). Every action in MidiAction::ALL has
    /// a rebindable key slot (Settings → MIDI + Keyboard Bindings); keys are
    /// persisted. Press edges are read from the per-frame event list so key
    /// auto-repeat doesn't re-fire an action, and the keys are ignored while
    /// ANY text box has focus.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Ignore keys while a text box anywhere in the program has focus —
        // the egui focus registry, so it covers every TextEdit (quantize
        // fields, FX preset name, export path, level nudge, …) including
        // ones added later, instead of a hand-maintained per-widget flag.
        // Ctrl/Cmd+S saves the project (with Shift: always re-ask the
        // name). Checked before the text-focus guard: saving while a field
        // has focus is always meant as save.
        let (save, shift) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::S),
                i.modifiers.shift,
            )
        });
        if save {
            self.request_save(shift);
        }

        if ctx.text_edit_focused() {
            return;
        }

        // Clip clipboard: Ctrl/Cmd+C copies the selected clip's asset,
        // Ctrl/Cmd+V places it at the playhead on the last grid-clicked
        // track (busy lanes open a child lane automatically). Handled
        // before action firing so a bare C/V binding never double-fires.
        let (ccopy, cpaste) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::C),
                i.modifiers.command && i.key_pressed(egui::Key::V),
            )
        });
        if ccopy || cpaste {
            if ccopy {
                if let Some((t, k, ci)) = self.sel_clip {
                    if let Some(c) = self.arr.tracks[t].lanes[k].clips.get(ci) {
                        self.copied_asset = Some(c.asset);
                        let nm = self
                            .assets
                            .get(c.asset as usize)
                            .map(|a| a.name.clone())
                            .unwrap_or_default();
                        self.status = format!(
                            "copied \u{201c}{nm}\u{201d} \u{b7} Ctrl+V pastes at the playhead"
                        );
                    }
                }
            }
            if cpaste {
                if let Some(a) = self.copied_asset {
                    let t = self.last_click_track.unwrap_or(self.selected);
                    let len = self.asset_len(a);
                    let pos = self.backend.engine.pos();
                    let nl0 = self.arr.tracks[t].lanes.len();
                    let lane = self.arr.place_auto(t, a, len, pos);
                    self.grow_for_lanes(t, nl0);
                    let li = self.arr.tracks[t].lanes[lane].clips.len() - 1;
                    self.sel_clip = Some((t, lane, li));
                    self.push_project();
                    self.status = format!(
                        "pasted at the playhead \u{b7} track {} {}",
                        t + 1,
                        if lane == 0 { "track" } else { "child lane" }
                    );
                }
            }
            return; // swallow the key
        }

        // The knob wizard is its own learn mode: it does NOT set
        // `midi_learning_slot` (arm_knob_learn clears it), so it needs its
        // own key branch. Escape cancels; Delete clears the slot's binding
        // and calibration — same vocabulary as the key/MIDI cells. All keys
        // are swallowed so nothing fires behind the wizard.
        if self.knob_learn.is_some() {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                let kl = match k {
                    egui::Key::Escape => Some(("cancelled knob learn", false)),
                    egui::Key::Delete => Some(("cleared knob binding", true)),
                    _ => None,
                };
                if let Some((verb, clear)) = kl {
                    if let Some(kl) = self.knob_learn.take() {
                        if clear {
                            self.midi_binds[kl.slot] = None;
                            if let Some(c) = self.knob_cal.get_mut(kl.slot) {
                                *c = None;
                            }
                            self.save_config();
                        }
                        self.status = format!("{verb}: {}", MidiAction::ALL[kl.slot].label());
                    }
                }
            }
            return; // swallow all keys while the knob wizard is open
        }

        // Learn mode: the next key press becomes the slot's new binding.
        if let Some(slot) = self.learning_slot {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                if k == egui::Key::Escape {
                    self.learning_slot = None; // cancel
                    self.midi_learning_slot = None;
                } else if k == egui::Key::Delete {
                    // Delete is reserved: it can never be bound. Pressing it
                    // while a key cell listens unbinds that slot (blank),
                    // mirroring the MIDI column's Delete-to-clear.
                    self.short_keys[slot] = None;
                    self.learning_slot = None;
                    self.save_config();
                    self.status = format!("cleared binding: {}", MidiAction::ALL[slot].label());
                } else {
                    match self.short_keys.iter().position(|s| *s == Some(k)) {
                        Some(s) if s != slot => {
                            self.status =
                                format!("{} is already bound — try another key", key_display(k));
                        }
                        _ => {
                            self.short_keys[slot] = Some(k);
                            self.learning_slot = None;
                            self.save_config();
                        }
                    }
                }
            }
            return; // swallow all keys while learning
        }

        // While a MIDI binding cell is listening, only Delete (clear the
        // binding) and Escape (cancel) are valid — every other key is
        // swallowed so it can neither fire an action nor rebind anything.
        if let Some(slot) = self.midi_learning_slot {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                if k == egui::Key::Escape {
                    self.midi_learning_slot = None;
                } else if k == egui::Key::Delete {
                    // Delete clears this MIDI cell's binding (the knob
                    // wizard is handled in its own branch above).
                    self.midi_binds[slot] = None;
                    self.midi_learning_slot = None;
                    self.save_config();
                    self.status =
                        format!("cleared MIDI binding: {}", MidiAction::ALL[slot].label());
                }
            }
            return; // swallow all keys while a MIDI cell is listening
        }

        // Collect this frame's key presses in ONE input pass (the old
        // per-slot closure locked the input state up to N_ACTIONS times per
        // frame). Press edges come from the event list so auto-repeat never
        // re-fires an action.
        let pressed_keys: Vec<egui::Key> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
                .collect()
        });

        // A selected clip deletes with Delete/Backspace — same vocabulary
        // as clearing bindings (learn modes above already swallowed the key
        // if they were open, and typing focus returned earlier). The asset
        // itself stays in the rack; only this placement disappears.
        if self.sel_clip.is_some()
            && pressed_keys
                .iter()
                .any(|k| matches!(k, egui::Key::Delete | egui::Key::Backspace))
        {
            if let Some((t, k, ci)) = self.sel_clip.take() {
                self.arr.remove_clip(t, k, ci);
                self.push_project();
                self.status = "clip removed (asset stays in the rack)".into();
            }
            return; // swallow the key
        }

        // Slot order matters: selection slots fire first so transport actions
        // in the same frame see the updated selection. Keys are unique across
        // slots (learn mode rejects duplicates), so at most one slot matches a
        // physical key.
        for i in 0..N_ACTIONS {
            if let Some(k) = self.short_keys[i] {
                if pressed_keys.contains(&k) {
                    self.fire_midi_action(MidiAction::ALL[i]);
                }
            }
        }
    }

    /// Set loop `i`'s playback volume from a MIDI-range value (0..=127).
    fn set_trk_vol(&mut self, i: usize, v: u8) {
        self.trk_vol[i] = v;
        self.backend.engine.set_loop_vol(i, vol_amp(v));
    }

    /// Set loop `i`'s gain trim from a MIDI-range value (0..=127).
    fn set_trk_gain(&mut self, i: usize, v: u8) {
        self.trk_gain[i] = v;
        self.backend.engine.set_loop_gain(i, gain_amp(v));
    }

    /// Set loop `i`'s stereo pan from a MIDI-range value (0 = hard left,
    /// 64 = centre, 127 = hard right).
    fn set_trk_pan(&mut self, i: usize, v: u8) {
        self.trk_pan[i] = v;
        self.backend.engine.set_loop_pan(i, pan01(v));
    }

    /// Rebuild loop `i`'s live effect chain from its window rows and hand it
    /// to the audio engine. Also seeds each row's slider values from the
    /// descriptors (stored values win, clamped to range).
    fn rebuild_fx(&mut self, i: usize) {
        // The editor window edits one specific live instance; a chain
        // rebuild replaces that instance, so the window must not outlive it.
        if self.fx_editor.as_ref().is_some_and(|ed| ed.loop_i == i) {
            self.fx_editor = None;
        }
        let mut live: Vec<std::sync::Arc<fx::Instance>> = Vec::new();
        let mut slots: Vec<FxSlot> = Vec::new();
        for e in self.fx_entries[i].iter_mut() {
            if !e.enabled {
                continue;
            }
            if slots.len() >= fx::MAX_FX_PER_TRACK {
                break;
            }
            let Some(desc) = self.fx_registry.desc(&e.stem).cloned() else {
                continue;
            };
            let vals = fx::seed_values(&e.params, &desc);
            e.params = vals.clone();
            if let Some(inst) =
                self.fx_registry
                    .create_instance(&e.stem, self.sample_rate as f64, &vals)
            {
                slots.push(FxSlot {
                    inst: inst.clone(),
                    desc,
                });
                live.push(inst);
            }
        }
        self.fx_live[i] = live;
        self.backend.cmd(Cmd::SetTrackEffects {
            i,
            chain: TrackFx {
                slots: std::sync::Arc::new(slots),
            },
        });
    }

    /// Open (or reopen) the child editor window for loop `i`'s chain slot
    /// `slot`. Effects that export a custom GUI get their own canvas;
    /// everything else gets the auto slider window. Params ride the same
    /// live instance the sliders used to sit next to on the loop row.
    fn open_fx_editor(&mut self, i: usize, slot: usize) {
        self.fx_editor = None; // closing an old window frees its GUI state
        let Some(stem) = self.fx_entries[i]
            .iter()
            .filter(|e| e.enabled)
            .nth(slot)
            .map(|e| e.stem.clone())
        else {
            return;
        };
        let inst = self.fx_live[i].get(slot).cloned();
        let desc = self.fx_registry.desc(&stem).cloned();
        let (Some(inst), Some(desc)) = (inst, desc) else {
            return;
        };
        let gui = self.fx_registry.gui(&stem);
        self.fx_editor = Some(Box::new(crate::fxgui::FxEditor::new(
            i,
            slot,
            stem,
            desc,
            inst,
            gui,
            self.sample_rate as f64,
            // The ACTIVE effects folder (not the possibly-unapplied edit
            // box): that's where `user presets/` lives for these scripts.
            self.fx_registry.dir().to_path_buf(),
        )));
    }

    /// Draw the editor window and flush its param rides into the persisted
    /// chain rows (config save goes through `fx_save_pending`).
    fn fx_editor_window(&mut self, ui: &mut egui::Ui) {
        let mut closed = false;
        if let Some(ed) = self.fx_editor.as_mut() {
            closed = !crate::fxgui::window(ui, ed);
            let pend = std::mem::take(&mut ed.pending);
            if !pend.is_empty() {
                if let Some(e) = self.fx_entries[ed.loop_i]
                    .iter_mut()
                    .filter(|e| e.enabled)
                    .nth(ed.slot)
                {
                    for (k, v) in pend {
                        if let Some(p) = e.params.get_mut(k) {
                            *p = v;
                        }
                    }
                    self.fx_save_pending = true;
                }
            }
        }
        if closed {
            self.fx_editor = None;
        }
    }

    /// Resolve the "effects location" text box, rescan, and re-point every
    /// loop's chain at the new folder (chains whose scripts vanished simply
    /// drop those slots; slider values of survivors are kept).
    fn apply_fx_dir(&mut self) {
        let dir = fx::prepare_effects_dir(Some(self.fx_dir_edit.trim()));
        self.fx_registry.set_dir(dir.clone());
        self.fx_dir_edit = dir.display().to_string();
        self.fx_registry.rescan();
        let stems = self.fx_registry.stems();
        for i in 0..MAX_LOOPS {
            self.fx_entries[i] = fx::merge_entries(&self.fx_entries[i], &stems, &[]);
            self.rebuild_fx(i);
        }
        self.fx_note = format!("folder: {}", dir.display());
        self.save_config();
    }

    /// Compile/load every script in the folder again and refresh all rows.
    fn rescan_fx(&mut self) {
        self.fx_registry.rescan();
        let stems = self.fx_registry.stems();
        for i in 0..MAX_LOOPS {
            self.fx_entries[i] = fx::merge_entries(&self.fx_entries[i], &stems, &[]);
            self.rebuild_fx(i);
        }
        let ok = self.fx_registry.stems().len();
        let bad = self
            .fx_registry
            .items()
            .iter()
            .filter(|e| e.error.is_some())
            .count();
        self.fx_note = format!("{ok} valid effect(s), {bad} rejected");
    }

    // ---- grid maths ------------------------------------------------------
    fn bpm(&self) -> u32 {
        self.metro_bpm
            .trim()
            .parse::<u32>()
            .unwrap_or(120)
            .clamp(20, 300)
    }

    fn beats(&self) -> u32 {
        self.metro_num
            .trim()
            .parse::<u32>()
            .unwrap_or(4)
            .clamp(1, 16)
    }

    /// One measure of the metronome grid, in engine samples: the timeline's
    /// bar length and the project's quantization unit.
    fn bar_samples(&self) -> u64 {
        (quantize::beat_samples(self.sample_rate, self.bpm()) * self.beats() as f64)
            .round()
            .max(1.0) as u64
    }

    fn q_unit(&self) -> f64 {
        quantize::unit_samples(self.sample_rate, self.bpm(), SUBDIVISIONS[self.q_sub].0)
    }

    fn snap_unit(&self) -> f64 {
        quantize::unit_samples(self.sample_rate, self.bpm(), SUBDIVISIONS[self.snap_sub].0)
    }

    fn asset_len(&self, id: u32) -> u64 {
        self.assets
            .get(id as usize)
            .map(|a| a.data.len() as u64)
            .unwrap_or(0)
    }

    /// Preview length in seconds (Settings text box, clamped 0.25…600 s;
    /// unparsable input falls back to the 5 s default).
    fn preview_secs_f(&self) -> f32 {
        self.preview_secs
            .trim()
            .parse::<f32>()
            .unwrap_or(5.0)
            .clamp(0.25, 600.0)
    }

    /// Audition asset `k` at the master output (asset-rack preview): an
    /// asset no longer than the preview length plays in full, a longer one
    /// plays its first `preview_secs` seconds. Clicking the asset that is
    /// already auditioning stops it. Engine-side the buffer auto-stops at
    /// its end, so `preview_playing` is only ever a hair stale.
    fn preview_asset(&mut self, k: usize) {
        if !self.preview_enabled {
            return;
        }
        let Some(a) = self.assets.get(k) else { return };
        let key = a.path.to_string_lossy().into_owned();
        // Toggle: clicking the entry that is auditioning stops the click.
        if self.preview_playing.as_deref() == Some(key.as_str()) {
            self.preview_playing = None;
            self.backend.cmd(Cmd::PreviewStop);
            self.status = "preview stopped".into();
            return;
        }
        if self.sample_rate == 0 {
            self.status = "preview: wait for the audio engine".into();
            return;
        }
        let cap = (self.preview_secs_f() as f64 * self.sample_rate as f64) as usize;
        let full = a.data.len();
        let end = full.min(cap.max(1));
        let samples: Arc<[f32]> = if end >= full {
            a.data.clone()
        } else {
            Arc::from(&a.data[..end])
        };
        let secs = end as f64 / self.sample_rate as f64;
        let nm = a.name.clone();
        self.preview_playing = Some(key);
        self.backend.cmd(Cmd::PreviewPlay { samples });
        self.status = format!("previewing {nm} ({secs:.1} s)");
    }

    /// Total timeline length: last clip end, rounded up to whole bars.
    fn total_len(&self) -> u64 {
        let bar = self.bar_samples();
        self.arr.total_len(&|id| self.asset_len(id), bar)
    }

    /// Hand the current project (asset table + arrangement + length) to the
    /// engine as one immutable snapshot.
    /// End of the last clip on any track/lane (0 when the grid is empty) —
    /// the "current longest" loop region end.
    fn last_clip_end(&self) -> u64 {
        let mut e = 0u64;
        for tr in self.arr.tracks.iter() {
            for l in tr.lanes.iter() {
                for c in l.clips.iter() {
                    e = e.max(c.pos.saturating_add(self.asset_len(c.asset)));
                }
            }
        }
        e
    }

    /// The loop region the engine should use right now (samples):
    /// *area* = the dragged ruler handles (unset end = 2 bars from 0),
    /// *current longest* = 0 → last clip end, *whole song* = 0 → project
    /// end. Degenerate areas collapse back to the 2-bar default.
    fn loop_region_eff(&self) -> (u64, u64) {
        if self.sample_rate == 0 {
            return (0, 0);
        }
        match self.loop_mode {
            0 => {
                let (mut s, mut e) = self.loop_area;
                if e == 0 {
                    e = 2 * self.bar_samples();
                }
                if e <= s {
                    s = 0;
                    e = 2 * self.bar_samples();
                }
                (s, e)
            }
            1 => (0, self.last_clip_end()),
            _ => (0, self.total_len()),
        }
    }

    /// Push the effective loop region to the audio engine.
    fn send_loop_region(&mut self) {
        let (s, e) = self.loop_region_eff();
        self.backend.cmd(Cmd::SetLoopRegion { start: s, end: e });
    }

    /// Height of one track's grid block — and of the mixer strip aligned
    /// with it (both read this, so they can never drift apart). User-resized
    /// via the separation bar under each track; lanes inside share it equally.
    fn track_h(&self, t: usize) -> f32 {
        let lanes = self.arr.tracks[t].lanes.len().max(1) as f32;
        let auto = (lanes * LANE_H).max(TRACK_MIN_H);
        let floor = (lanes * LANE_MIN_H).max(TRACK_MIN_H);
        match self.trk_height.get(t).copied().unwrap_or(0.0) {
            0.0 => auto,
            h => h.max(floor),
        }
    }

    /// User height for track `t` (clamped to the lane floor and a sane max).
    fn set_track_h(&mut self, t: usize, h: f32) {
        let floor = (self.arr.tracks[t].lanes.len().max(1) as f32 * LANE_MIN_H).max(TRACK_MIN_H);
        if let Some(s) = self.trk_height.get_mut(t) {
            *s = h.clamp(floor, 1600.0);
        }
    }

    /// A new child lane must NOT shrink the lanes above it: grow the track
    /// by one old-lane height so every lane keeps the height it had.
    fn grow_for_lanes(&mut self, t: usize, old_lanes: usize) {
        let old_lanes = old_lanes.max(1);
        let new = self.arr.tracks[t].lanes.len().max(1);
        if new > old_lanes {
            let per = self.track_h(t) / old_lanes as f32;
            let want = new as f32 * per;
            let cur = self.trk_height.get(t).copied().unwrap_or(0.0);
            self.set_track_h(t, cur.max(want));
        }
    }

    fn push_project(&mut self) {
        let table: Arc<Vec<Arc<[f32]>>> =
            Arc::new(self.assets.iter().map(|a| a.data.clone()).collect());
        let arr = Arc::new(self.arr.clone());
        let len = self.total_len();
        self.backend.cmd(Cmd::SetProject {
            assets: table,
            arr,
            len,
        });
        // "current longest" / "whole song" regions follow the arrangement.
        self.send_loop_region();
    }

    fn send_metro(&mut self) {
        let bpm = self
            .metro_bpm
            .trim()
            .parse::<u32>()
            .unwrap_or(120)
            .clamp(20, 300);
        self.backend.cmd(Cmd::SetMetro {
            on: self.metro_on,
            bpm,
            sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            num: self
                .metro_num
                .trim()
                .parse::<u32>()
                .unwrap_or(4)
                .clamp(1, 16),
        });
        self.save_config();
    }

    /// Persist every user-facing setting (bindings, mixer, grid, rack,
    /// ports, effects chains) to disk.
    fn save_config(&self) {
        let cfg = Config {
            shortcuts: (0..N_ACTIONS)
                .map(|i| self.short_keys[i].map(key_debug))
                .collect(),
            midi: MidiAction::ALL
                .iter()
                .zip(self.midi_binds.iter())
                .filter_map(|(a, b)| {
                    b.map(|bind| MidiBinding {
                        action: *a,
                        bind: Some(bind),
                    })
                })
                .collect(),
            midi_axis: self
                .knob_cal
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    c.map(|mut m| {
                        m.slot = i as u16;
                        m
                    })
                })
                .collect(),
            ports_in: self
                .applied_ports
                .as_ref()
                .map(|p| p.0.clone())
                .unwrap_or_default(),
            ports_out: self
                .applied_ports
                .as_ref()
                .map(|p| p.1.iter().map(|(n, _)| n.clone()).collect())
                .unwrap_or_default(),
            ports_out_lr: self
                .applied_ports
                .as_ref()
                .map(|p| p.1.iter().map(|(_, lr)| *lr).collect())
                .unwrap_or_default(),
            gui_scale: self.gui_scale,
            latency_ms: self
                .latency_ms
                .trim()
                .parse::<f32>()
                .unwrap_or(0.0)
                .clamp(0.0, 1000.0),
            trk_vol: self.trk_vol.to_vec(),
            trk_gain: self.trk_gain.to_vec(),
            trk_pan: self.trk_pan.to_vec(),
            trk_h: self.trk_height.clone(),
            metro_bpm: self.bpm(),
            metro_sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            metro_num: self.beats(),
            q_sub: SUBDIVISIONS[self.q_sub].0,
            snap_sub: SUBDIVISIONS[self.snap_sub].0,
            auto_quantize: self.auto_quantize,
            loop_tl: self.loop_tl,
            loop_mode: self.loop_mode,
            loop_area_start: self.loop_area.0,
            loop_area_end: self.loop_area.1,
            master_vol: self.master_midi,
            preview_on: self.preview_enabled,
            preview_secs: self.preview_secs_f(),
            vol_scale_v2: true,
            project_dir: self.project_dir.clone(),
            rack_folders: self.rack.iter().map(|f| f.path.clone()).collect(),
            fx_dir: self.fx_dir_edit.trim().to_string(),
            track_fx: (0..MAX_LOOPS)
                .map(|i| {
                    self.fx_entries[i]
                        .iter()
                        .filter(|e| e.enabled)
                        .map(|e| TrackFxSave {
                            stem: e.stem.clone(),
                            params: e.params.clone(),
                        })
                        .collect()
                })
                .collect(),
        };
        if let Err(e) = cfg.save() {
            eprintln!("[slooper] could not save config: {e}");
        }
    }

    // ---- DAW actions -----------------------------------------------------
    fn toggle_record(&mut self, i: usize) {
        if self.sample_rate == 0 {
            self.status = "record: wait for the audio engine".into();
            return;
        }
        // The engine owns the record state (and auto-stops at buffer
        // capacity); the GUI just toggles and mirrors events.
        self.backend.cmd(Cmd::ToggleRecord { i });
    }

    fn clear_track(&mut self, i: usize) {
        self.arr.clear_track(i);
        self.sel_clip = None;
        self.push_project();
        self.status = format!("track {} cleared", i + 1);
    }

    /// A finished take becomes: optional content quantize → WAV into the
    /// primary rack folder (when set) → asset → clip placed at the take's
    /// start position, snapped to the snap grid.
    fn take_pipeline(&mut self, track: usize, at: u64, data: Arc<[f32]>) {
        if track >= MAX_LOOPS {
            return;
        }
        let data = if self.auto_quantize {
            match quantize::quantize_asset(&data, self.sample_rate, self.q_unit()) {
                Some(q) => Arc::from(q),
                None => data,
            }
        } else {
            data
        };
        let mut path = PathBuf::new();
        let mut dirty = true;
        let name = format!("rec-t{}-{}", track + 1, Self::fstamp());
        if let Some(f) = self.rack.first_mut() {
            let dir = PathBuf::from(f.path.trim());
            if !dir.as_os_str().is_empty() {
                let p = dir.join(format!("{name}.wav"));
                if std::fs::create_dir_all(&dir).is_ok()
                    && crate::audio_io::export(
                        &p,
                        self.sample_rate,
                        &data,
                        crate::audio_io::ExportFormat::Wav,
                    )
                    .is_ok()
                {
                    path = p;
                    dirty = false;
                }
            }
        }
        let id = self.assets.len() as u32;
        let len = data.len() as u64;
        self.assets.push(Asset {
            id,
            name,
            path,
            data,
            dirty,
        });
        self.sel_asset = Some(id as usize);
        let pos = snap_start(at, self.snap_unit());
        let nl0 = self.arr.tracks[track].lanes.len();
        let lane = self.arr.place_auto(track, id, len, pos);
        self.grow_for_lanes(track, nl0);
        self.push_project();
        self.status = format!("track {}: recorded → clip in lane {}", track + 1, lane + 1);
    }

    /// Load (or reuse) an asset for a rack file path; returns its index.
    fn ensure_asset(&mut self, path: &Path) -> Option<usize> {
        if let Some(k) = self.assets.iter().position(|a| !a.dirty && a.path == path) {
            self.sel_asset = Some(k);
            return Some(k);
        }
        if self.sample_rate == 0 {
            self.status = "rack: wait for the audio engine".into();
            return None;
        }
        match crate::audio_io::decode_to_mono(path, self.sample_rate) {
            Ok(v) => {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let id = self.assets.len() as u32;
                self.assets.push(Asset {
                    id,
                    name,
                    path: path.to_path_buf(),
                    data: Arc::from(v),
                    dirty: false,
                });
                self.sel_asset = Some(id as usize);
                Some(id as usize)
            }
            Err(e) => {
                self.status = e;
                None
            }
        }
    }

    /// Transform the selected asset: mode 0 = quantize to grid,
    /// mode 1 = clip the silence off the ends. Content edits mark the asset
    /// dirty so the next save writes the new audio out.
    fn transform_asset(&mut self, k: usize, mode: u8) {
        if k >= self.assets.len() || self.sample_rate == 0 {
            return;
        }
        let res = if mode == 0 {
            let u = self.q_unit();
            quantize::quantize_asset(&self.assets[k].data, self.sample_rate, u)
        } else {
            quantize::trim_silence(&self.assets[k].data, self.sample_rate)
        };
        match res {
            Some(v) => {
                let a = &mut self.assets[k];
                a.data = Arc::from(v);
                a.dirty = true;
                let nm = a.name.clone();
                self.wave.remove(&(k as u32));
                self.status = if mode == 0 {
                    format!("asset “{nm}”: quantized to the grid")
                } else {
                    format!("asset “{nm}”: silence trimmed")
                };
                self.push_project();
            }
            None => self.status = "nothing to do (pure silence under the floor)".into(),
        }
    }

    /// Remove an asset from the project: its clips go with it, and the
    /// index-keyed clip ids shift down to follow the table.
    fn remove_asset_at(&mut self, k: usize) {
        if k >= self.assets.len() {
            return;
        }
        self.arr.remove_asset(k as u32);
        for tr in self.arr.tracks.iter_mut() {
            for lane in tr.lanes.iter_mut() {
                for c in lane.clips.iter_mut() {
                    if c.asset > k as u32 {
                        c.asset -= 1;
                    }
                }
            }
        }
        self.assets.remove(k);
        self.wave.clear();
        self.sel_asset = None;
        self.sel_clip = None;
        self.push_project();
        self.status = "asset removed (its clips too)".into();
    }

    // ---- projects --------------------------------------------------------
    fn projects_dir(&self) -> PathBuf {
        let t = self.project_dir.trim();
        if t.is_empty() {
            home_dir().join("slooper-projects")
        } else {
            PathBuf::from(t)
        }
    }

    /// Ctrl+S: save under the current name, or open the name prompt for an
    /// untitled project (Shift always re-asks).
    fn request_save(&mut self, force_name: bool) {
        if self.proj_name.is_empty() || force_name {
            self.save_draft = self.proj_name.clone();
            self.show_save_prompt = true;
        } else {
            let name = self.proj_name.clone();
            self.do_save(&name);
        }
    }

    /// Snapshot the GUI master copies into a `.slooper` document.
    fn build_project(&self, name: String) -> Project {
        let tracks: Vec<ProjTrack> = (0..MAX_LOOPS)
            .map(|t| ProjTrack {
                vol: self.trk_vol[t],
                gain: self.trk_gain[t],
                pan: self.trk_pan[t],
                on: self.backend.engine.track_on(t),
                fx: self.fx_entries[t]
                    .iter()
                    .filter(|e| e.enabled)
                    .map(|e| TrackFxSave {
                        stem: e.stem.clone(),
                        params: e.params.clone(),
                    })
                    .collect(),
                lanes: self.arr.tracks[t]
                    .lanes
                    .iter()
                    .map(|l| {
                        l.clips
                            .iter()
                            .map(|c| ProjClip {
                                asset: c.asset,
                                pos: c.pos,
                            })
                            .collect()
                    })
                    .collect(),
            })
            .collect();
        Project {
            format: project::FORMAT.into(),
            version: project::VERSION,
            name,
            metro: ProjMetro {
                bpm: self.bpm(),
                num: self.beats(),
                sub: SUBDIVISIONS[self.metro_sub].0,
            },
            quantize: ProjQuantize {
                q_sub: SUBDIVISIONS[self.q_sub].0,
                snap_sub: SUBDIVISIONS[self.snap_sub].0,
                auto: self.auto_quantize,
                loop_tl: self.loop_tl,
            },
            playhead: self.backend.engine.pos(),
            tracks,
            assets: self
                .assets
                .iter()
                .map(|a| ProjAsset {
                    name: a.name.clone(),
                    file: a.path.to_string_lossy().into_owned(),
                })
                .collect(),
        }
    }

    /// Write audio for new/edited assets, then the project file itself.
    fn do_save(&mut self, raw: &str) {
        let name = project::sanitize_name(raw);
        if name.is_empty() {
            self.status = "save: the project needs a name".into();
            return;
        }
        let dir = self.projects_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.status = format!("save: {e}");
            return;
        }
        let audio_dir = dir.join(format!("{name} audio"));
        for a in self.assets.iter_mut() {
            if !a.dirty && !a.path.as_os_str().is_empty() {
                continue;
            }
            if std::fs::create_dir_all(&audio_dir).is_err() {
                self.status = format!("save: cannot write {}", audio_dir.display());
                return;
            }
            let stem: String = a
                .name
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let p = audio_dir.join(format!(
                "{:03}-{}.wav",
                a.id,
                if stem.is_empty() {
                    "asset".to_string()
                } else {
                    stem
                }
            ));
            match crate::audio_io::export(
                &p,
                self.sample_rate,
                &a.data,
                crate::audio_io::ExportFormat::Wav,
            ) {
                Ok(()) => {
                    a.path = p;
                    a.dirty = false;
                }
                Err(e) => {
                    self.status = format!("save: {e}");
                    return;
                }
            }
        }
        let pr = self.build_project(name.clone());
        match project::save(&project::project_path(&dir, &name), &pr) {
            Ok(()) => {
                self.proj_name = name.clone();
                self.show_save_prompt = false;
                self.status = format!("saved project “{name}”");
            }
            Err(e) => self.status = format!("save: {e}"),
        }
        self.save_config();
    }

    /// Replace the whole working state with a saved project. Assets resolve
    /// through the project's audio folder first, then the rack folders.
    fn do_load(&mut self, path: &Path) {
        let pr = match project::load(path) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("load: {e}");
                return;
            }
        };
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut dirs = vec![dir.clone(), dir.join(format!("{} audio", pr.name))];
        for f in &self.rack {
            let p = PathBuf::from(f.path.trim());
            if !p.as_os_str().is_empty() {
                dirs.push(p);
            }
        }
        let sr = self.sample_rate.max(1);
        let mut assets = Vec::new();
        let mut missing = 0usize;
        for (i, pa) in pr.assets.iter().enumerate() {
            let found = project::resolve_asset(&pa.file, &dirs);
            match found
                .as_ref()
                .and_then(|p| crate::audio_io::decode_to_mono(p, sr).ok())
            {
                Some(v) => assets.push(Asset {
                    id: i as u32,
                    name: pa.name.clone(),
                    path: found.unwrap_or_default(),
                    data: Arc::from(v),
                    dirty: false,
                }),
                None => {
                    missing += 1;
                    // Keep the id space intact: missing audio becomes a
                    // silent placeholder (its clips simply play nothing).
                    assets.push(Asset {
                        id: i as u32,
                        name: pa.name.clone(),
                        path: PathBuf::new(),
                        data: Arc::from(vec![]),
                        dirty: false,
                    });
                }
            }
        }
        let mut arr = Arrangement::new();
        for (t, pt) in pr.tracks.iter().enumerate().take(MAX_LOOPS) {
            self.trk_vol[t] = pt.vol;
            self.trk_gain[t] = pt.gain;
            self.trk_pan[t] = pt.pan;
            self.backend.engine.set_loop_vol(t, vol_amp(pt.vol));
            self.backend.engine.set_loop_gain(t, gain_amp(pt.gain));
            self.backend.engine.set_loop_pan(t, pan01(pt.pan));
            self.backend.cmd(Cmd::SetTrackOn { i: t, on: pt.on });
            while arr.tracks[t].lanes.len() < pt.lanes.len() {
                arr.tracks[t].add_lane();
            }
            for (k, lane) in pt.lanes.iter().enumerate() {
                while arr.tracks[t].lanes.len() <= k {
                    arr.tracks[t].add_lane();
                }
                for c in lane {
                    arr.insert(
                        t,
                        k,
                        Clip {
                            asset: c.asset,
                            pos: c.pos,
                        },
                    );
                }
            }
            let saved: Vec<(String, Vec<f32>)> = pt
                .fx
                .iter()
                .map(|s| (s.stem.clone(), s.params.clone()))
                .collect();
            let stems = self.fx_registry.stems();
            self.fx_entries[t] = fx::merge_entries(&[], &stems, &saved);
            self.rebuild_fx(t);
        }
        self.assets = assets;
        self.arr = arr;
        // Heights are a per-session view; the new arrangement starts fresh.
        for h in self.trk_height.iter_mut() {
            *h = 0.0;
        }
        self.wave.clear();
        self.sel_asset = None;
        self.sel_clip = None;
        self.metro_bpm = pr.metro.bpm.to_string();
        self.metro_num = pr.metro.num.to_string();
        self.metro_sub = SUBDIVISIONS
            .iter()
            .position(|(c, _)| *c == pr.metro.sub)
            .unwrap_or(0);
        self.q_sub = SUBDIVISIONS
            .iter()
            .position(|(c, _)| *c == pr.quantize.q_sub)
            .unwrap_or(3);
        self.snap_sub = SUBDIVISIONS
            .iter()
            .position(|(c, _)| *c == pr.quantize.snap_sub)
            .unwrap_or(3);
        self.auto_quantize = pr.quantize.auto;
        self.loop_tl = pr.quantize.loop_tl;
        self.backend.cmd(Cmd::SetLoop(self.loop_tl));
        self.send_loop_region();
        self.proj_name = pr.name.clone();
        self.show_load = false;
        self.send_metro();
        self.backend.cmd(Cmd::Seek { pos: pr.playhead });
        self.push_project();
        self.save_config();
        self.status = if missing == 0 {
            format!("loaded project “{}” ({} assets)", pr.name, pr.assets.len())
        } else {
            format!(
                "loaded “{}”: {} asset(s) missing (silent)",
                pr.name, missing
            )
        };
    }

    fn do_export(&mut self) {
        let mut path = PathBuf::from(self.export_path.trim());
        if path.as_os_str().is_empty() {
            self.status = "export: choose a destination first".into();
            return;
        }
        let fmt = crate::audio_io::ExportFormat::ALL[self.export_fmt];
        // The selected format drives the extension: the destination is
        // picked without one, so switching formats never means re-picking.
        let ext = fmt.extension();
        let has_want = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext));
        if !has_want {
            path.set_extension(ext);
        }
        if self.export_whole {
            self.backend.cmd(Cmd::ExportProject { path, fmt });
        } else {
            self.backend.cmd(Cmd::ExportTrack {
                i: self.selected,
                path,
                fmt,
            });
        }
        self.show_export = false;
    }

    fn fire_midi_action(&mut self, a: MidiAction) {
        match a {
            MidiAction::Select(i) => self.selected = i,
            MidiAction::RecSelected => self.toggle_record(self.selected),
            MidiAction::ClearSelected => self.clear_track(self.selected),
            MidiAction::PlaySelected => self.backend.cmd(Cmd::ToggleTrackOn { i: self.selected }),
            MidiAction::Export => self.show_export = true,
            MidiAction::RecLoop(i) => self.toggle_record(i),
            MidiAction::ClearLoop(i) => self.clear_track(i),
            MidiAction::PlayLoop(i) => self.backend.cmd(Cmd::ToggleTrackOn { i }),
            MidiAction::Metronome => {
                self.metro_on = !self.metro_on;
                self.send_metro();
            }
            // Selection NEVER wraps: it stops at track 1 and track 16.
            MidiAction::SelectNext => self.selected = (self.selected + 1).min(MAX_LOOPS - 1),
            MidiAction::SelectPrev => self.selected = self.selected.saturating_sub(1),
            // The global transport: PlayAll toggles play/stop (space-bar
            // style), StopAll always parks (the playhead holds its place).
            MidiAction::PlayAll => {
                if self.backend.engine.transport_on() {
                    self.backend.cmd(Cmd::StopAll);
                } else {
                    self.backend.cmd(Cmd::PlayAll);
                }
            }
            MidiAction::StopAll => self.backend.cmd(Cmd::StopAll),
            // Value-driven knob actions fire only through the MIDI path
            // (handle_evt passes the CC value); a key press has no value.
            MidiAction::VolLoop(_) | MidiAction::GainLoop(_) | MidiAction::PanLoop(_) => {}
            MidiAction::Reserved => {}
        }
    }

    fn handle_evt(&mut self, evt: Evt) {
        match evt {
            Evt::Unavailable(msg) => self.status = msg,
            Evt::Ready {
                client_name,
                sample_rate,
            } => {
                self.client_name = Some(client_name.clone());
                self.sample_rate = sample_rate;
                self.status = "JACK ready — select ports, then Apply".into();
                self.backend.cmd(Cmd::SetLatency {
                    ms: self.latency_ms.trim().parse().unwrap_or(0.0),
                });
                self.backend.cmd(Cmd::SetLoop(self.loop_tl));
                self.send_metro();
                // Effect instances made before the backend was up were
                // created with rate 0; rebuild every chain at the real rate.
                for t in 0..MAX_LOOPS {
                    if self.fx_entries[t].iter().any(|e| e.enabled) {
                        self.rebuild_fx(t);
                    }
                }
                // Re-apply the port bindings persisted from the previous
                // session so audio flows on launch without re-clicking.
                if let Some((ins, outs)) = &self.applied_ports {
                    if !ins.is_empty() || !outs.is_empty() {
                        self.backend.cmd(Cmd::Apply {
                            ins: ins.clone(),
                            outs: outs.clone(),
                        });
                    }
                }
                self.push_project();
            }
            Evt::Ports { ins, outs } => {
                self.avail_in = ins;
                self.avail_out = outs;
            }
            Evt::Applied { ins, outs } => {
                self.applied = Some((ins, outs));
                self.status = format!("applied: {} in / {} out", ins, outs);
            }
            Evt::Transport { on } => {
                self.status = if on {
                    "transport running".into()
                } else {
                    "transport stopped".into()
                };
            }
            Evt::Take { track, at, data } => self.take_pipeline(track, at, data),

            Evt::Midi(raw) => {
                if let Some(msg) = MidiMsg::parse(&raw.bytes) {
                    // Bindable messages drive every bound action (Settings →
                    // MIDI Bindings); value/velocity are ignored.
                    if let Some(bind) = MidiBind::from_msg(&msg) {
                        // Live capture: a row in "listening" state takes the
                        // first bindable message it sees, like key learning.
                        if let Some(slot) = self.midi_learning_slot {
                            if self.midi_binds[slot] != Some(bind) {
                                self.midi_binds[slot] = Some(bind);
                                // A plain rebind replaces the axis: drop any
                                // stale min/max calibration for the slot.
                                self.knob_cal[slot] = None;
                                self.save_config();
                                self.status = format!(
                                    "bound {} → {}",
                                    MidiAction::ALL[slot].label(),
                                    bind.label()
                                );
                            }
                            self.midi_learning_slot = None;
                        } else if let Some(kl) = self.knob_learn.as_mut() {
                            // Three-step knob learn in flight (axis → min → max).
                            // The AXIS is the message TYPE + CHANNEL only — the
                            // data byte (CC value / note velocity / program
                            // number) is the VALUE steps 2 and 3 read, and must
                            // NEVER be part of the axis identity. Binding the
                            // data byte in step 1 is what locked min and max to
                            // the same value; matching d1 later rejected every
                            // sweep message from controllers that re-use
                            // numbers mid-sweep.
                            let cval = match &msg {
                                MidiMsg::ControlChange { value, .. } => Some(*value),
                                MidiMsg::NoteOn { velocity, .. } if *velocity > 0 => {
                                    Some(*velocity)
                                }
                                MidiMsg::ProgramChange { program, .. } => Some(*program),
                                _ => None,
                            };
                            if !kl.axis_locked {
                                // Step 1 — pick the axis. Until it is locked,
                                // the most recent message wins, so moving a
                                // different knob (or landing on another
                                // channel) simply re-selects the axis.
                                kl.bind = bind;
                                kl.has_axis = true;
                                if let Some(v) = cval {
                                    kl.last_val = Some(v);
                                }
                                self.status = format!(
                                    "axis {} — press Lock axis, or move a different control to change it",
                                    kl.bind.axis_label()
                                );
                            } else if !kl.bind.same_axis(&bind) {
                                self.status = format!(
                                    "knob learn: got {} — min and max must use the \
                                     axis {} (same message type and channel)",
                                    bind.label(),
                                    kl.bind.axis_label()
                                );
                            } else if let Some(v) = cval {
                                // Constant listeners: the number tracks the
                                // MOST RECENT message on this axis until
                                // Lock min / Finish freezes it.
                                if !kl.locked {
                                    kl.lo = v;
                                } else {
                                    kl.hi = v;
                                }
                            }
                        } else {
                            // Value for continuous (knob) bindings: the CC
                            // byte, note velocity, or program number. The
                            // knob wizard may have captured a raw min/max
                            // other than 0..=127 — rescale through the slot's
                            // calibration so the control's full physical
                            // travel maps onto the full program range (and
                            // inverted wiring is corrected).
                            let cval = match &msg {
                                MidiMsg::ControlChange { value, .. } => Some(*value),
                                MidiMsg::NoteOn { velocity, .. } if *velocity > 0 => {
                                    Some(*velocity)
                                }
                                MidiMsg::ProgramChange { program, .. } => Some(*program),
                                _ => None,
                            };
                            for i in 0..N_ACTIONS {
                                let Some(b) = self.midi_binds[i] else {
                                    continue;
                                };
                                // A continuous (knob) binding owns its whole
                                // axis (type + channel) — the wizard never
                                // binds the data byte, so CC-number drift
                                // mid-sweep keeps driving the parameter.
                                let hit = if MidiAction::ALL[i].is_continuous() {
                                    b.same_axis(&bind)
                                } else {
                                    b == bind
                                };
                                if hit {
                                    let a = MidiAction::ALL[i];
                                    let v = cval.map(|r| match self.knob_cal[i] {
                                        Some(c) => scale_axis(r, c.lo, c.hi),
                                        None => r,
                                    });
                                    match a {
                                        MidiAction::VolLoop(t) => {
                                            if let Some(v) = v {
                                                self.set_trk_vol(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        MidiAction::GainLoop(t) => {
                                            if let Some(v) = v {
                                                self.set_trk_gain(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        MidiAction::PanLoop(t) => {
                                            if let Some(v) = v {
                                                self.set_trk_pan(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        _ => self.fire_midi_action(a),
                                    }
                                }
                            }
                        }
                    }
                    let line = MidiLine {
                        ts: Self::now_stamp(),
                        type_name: msg.type_name(),
                        describe: msg.describe(),
                        raw_hex: hex(&raw.bytes),
                    };
                    // "print the last detected MIDI input" — also to stdout.
                    println!(
                        "[{}] {:<12} {}  | {}",
                        line.ts, line.type_name, line.describe, line.raw_hex
                    );
                    self.last_midi = Some(line.clone());
                    self.midi_log.push_front(line);
                    while self.midi_log.len() > 300 {
                        self.midi_log.pop_back();
                    }
                }
            }

            Evt::MidiSources(names) => {
                self.midi_sources = names;
            }
            Evt::Notice(msg) => self.status = msg,
            Evt::Xrun => {}
            Evt::Error(e) => self.status = e,
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.set_zoom_factor(self.gui_scale);
        // Keep the update loop ticking so backend events (MIDI, port
        // changes, transport) are drained promptly; while the transport
        // runs or a take rolls, tick at ~50 fps so the playhead is smooth.
        let active = self.backend.engine.transport_on()
            || (0..MAX_LOOPS).any(|t| self.backend.engine.rec_state_of(t) == TR_REC);
        ctx.request_repaint_after(std::time::Duration::from_millis(if active {
            20
        } else {
            100
        }));

        self.handle_shortcuts(ctx);

        let mut changed = false;
        while let Ok(evt) = self.backend.evt_rx.try_recv() {
            self.handle_evt(evt);
            changed = true;
        }

        let p = f32::from_bits(self.backend.peak.load(Ordering::Relaxed));
        self.peak_disp = p.max(self.peak_disp * 0.9);

        if changed {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        self.ui_root(ui);
    }
}

impl App {
    /// The whole window's content. Split out from [`eframe::App::ui`] so
    /// headless tests can drive the real widget tree (panels, DnD, mixer)
    /// through `egui::Context::run` without an eframe frame.
    pub fn ui_root(&mut self, ui: &mut egui::Ui) {
        // ---- global wheel routing (must run first thing in the frame) ----
        // Ctrl/Cmd+wheel zooms the timeline (same state as the zoom slider);
        // Shift+wheel pans it horizontally. egui turns MouseWheel into
        // smooth_scroll_delta (and its own interface zoom) while processing
        // input — BEFORE any panel code runs — so the only reliable place to
        // claim those gestures is here: remove the raw events and zero the
        // deltas, then apply them ourselves. Plain wheel is left alone so
        // ScrollAreas (rack, mixer, track stack) keep scrolling normally.
        // Zoom accumulates in log space (sum of delta·scale, then exp) so
        // scrolling in then out by the same amount returns exactly, several
        // events in one frame compose geometrically, and the per-event
        // scale differs by wheel unit: one notch is a LINE, a trackpad
        // flick reports PIXELS.
        let mut zoom_ln = 0.0f32;
        let mut pan = 0.0f32;
        ui.ctx().input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } => {
                    if modifiers.command || modifiers.ctrl {
                        let s = match unit {
                            egui::MouseWheelUnit::Line => ZOOM_WHEEL_LINE,
                            _ => ZOOM_WHEEL_PX,
                        };
                        zoom_ln += (delta.y * s).clamp(-1.5, 1.5);
                        false
                    } else if modifiers.shift {
                        pan += delta.x + delta.y;
                        false
                    } else {
                        true
                    }
                }
                _ => true,
            });
            if zoom_ln != 0.0 || pan != 0.0 {
                i.smooth_scroll_delta = egui::Vec2::ZERO;
            }
        });
        if zoom_ln != 0.0 {
            self.px_per_sec = (self.px_per_sec * zoom_ln.exp()).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if pan != 0.0 {
            self.hscroll = (self.hscroll - pan * 2.0).max(0.0);
        }

        // Master volume: same -inf..=+6 dB MIDI mapping as every track.
        let mamp = vol_amp(self.master_midi);
        self.backend.gain.store(mamp.to_bits(), Ordering::Relaxed);

        if self.sel_sent != self.selected {
            self.sel_sent = self.selected;
            self.backend.cmd(Cmd::SetSelected(self.selected));
        }

        // Flush level / chain edits to the config once the pointer is up:
        // sliders fire per pixel and knobs per message.
        if self.levels_dirty && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.levels_dirty = false;
            self.save_config();
        }
        if self.fx_save_pending && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.fx_save_pending = false;
            self.save_config();
        }

        top_bar(ui, self);
        // Left panels stack outward in add order, so rendering SETTINGS
        // before the rack pops it up to the rack's LEFT; the rack and the
        // timeline simply shift right while it is open.
        if self.show_settings {
            settings_panel(ui, self);
        }
        rack_panel(ui, self);
        mixer_panel(ui, self);
        midi_monitor(ui, self);
        central_panels(ui, self);

        // Watchdog: a scrub/clip drag that somehow loses its release event
        // must not stick (rack drops now run on egui's native DnD).
        if !ui.ctx().input(|i| i.pointer.any_down()) {
            self.scrub_drag = false;
            self.drag_loop_handle = None;
            if self.sep_drag.take().is_some() {
                self.save_config();
            }
        }

        if self.show_export {
            export_window(ui, self);
        }
        save_prompt_window(ui, self);
        load_window(ui, self);
        knob_learn_window(ui, self);
        if self.fx_window.is_some() {
            effects_window(ui, self);
        }
        if self.fx_editor.is_some() {
            self.fx_editor_window(ui);
        }
    }
}

fn top_bar(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::top("topbar").exact_size(44.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            // Hamburger (settings) button: three horizontal bars.
            let size = egui::vec2(26.0, 18.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            if rect.width() > 4.0 && rect.height() > 4.0 {
                let p = ui.painter();
                for k in 0..3u32 {
                    let y = rect.min.y + 4.0 + k as f32 * 5.0;
                    p.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(rect.min.x + 3.0, y),
                            egui::vec2(20.0, 2.0),
                        ),
                        1.0,
                        if app.show_settings {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(200, 200, 205)
                        },
                    );
                }
            }
            if resp.on_hover_text("Settings").clicked() {
                app.show_settings = !app.show_settings;
            }

            ui.separator();
            let title = match &app.proj_name {
                n if n.is_empty() => "slooper · untitled".to_string(),
                n => format!("slooper · {n}"),
            };
            ui.heading(RichText::new(title).strong());
            let status = match &app.client_name {
                Some(name) => format!("{} @ {} Hz", name, app.sample_rate),
                None => app.status.clone(),
            };
            ui.label(RichText::new(status).weak());
            if let Some((i, o)) = app.applied {
                ui.label(RichText::new(format!("applied: {} in / {} out", i, o)).weak());
            }
            let xruns = app.backend.xruns.load(Ordering::Relaxed);
            if xruns > 0 {
                ui.label(RichText::new(format!("xruns: {}", xruns)).color(Color32::YELLOW));
            }

            // ---- transport ----
            ui.separator();
            let on = app.backend.engine.transport_on();
            let play = if on { "STOP" } else { "PLAY" };
            let play_btn = ui.button(RichText::new(play).strong()).on_hover_text(
                "Global transport (Space): all sounding tracks play together \
                 from the playhead. Stop holds the position.",
            );
            if play_btn.clicked() {
                app.backend
                    .cmd(if on { Cmd::StopAll } else { Cmd::PlayAll });
            }
            ui.label(RichText::new(pos_label(app)).monospace());

            // ---- metronome / grid tempo ----
            ui.separator();
            // Start/stop button (was a checkbox): lights up while the
            // click runs, like the transport button next to it.
            let metro_btn = ui
                .add_sized(
                    [58.0, 20.0],
                    egui::Button::selectable(
                        app.metro_on,
                        RichText::new(if app.metro_on { "STOP" } else { "METRO" })
                            .strong()
                            .small(),
                    ),
                )
                .on_hover_text(
                    "Metronome start/stop: ticks the grid at the playhead \
                     without moving it (never recorded). M also toggles it.",
                );
            if metro_btn.clicked() {
                app.metro_on = !app.metro_on;
                app.send_metro();
            }
            ui.label("bpm");
            // Bound straight to the app state: the old pattern of cloning
            // into a local EVERY FRAME wiped each keystroke before it could
            // land, so the box could never be edited (it snapped back to
            // the saved value). Commit to the engine on Enter / focus-out.
            let bpm_rt = ui
                .add(
                    egui::TextEdit::singleline(&mut app.metro_bpm)
                        .desired_width(38.0)
                        .horizontal_align(egui::Align::Center)
                        .font(egui::TextStyle::Monospace),
                )
                .on_hover_text("Grid tempo 20..300 BPM. Press Enter to apply.");
            if bpm_rt.lost_focus() {
                app.send_metro();
            }
            ui.label("beat");
            let num_rt = ui
                .add(
                    egui::TextEdit::singleline(&mut app.metro_num)
                        .desired_width(24.0)
                        .horizontal_align(egui::Align::Center)
                        .font(egui::TextStyle::Monospace),
                )
                .on_hover_text("Beats per measure 1..16. Press Enter to apply.");
            if num_rt.lost_focus() {
                app.send_metro();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Peak meter.
                let (rect, _) = ui.allocate_exact_size(egui::vec2(80.0, 8.0), Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(rect, 1.0, Color32::from_rgb(40, 40, 45));
                let frac = (app.peak_disp * 0.9).clamp(0.0, 1.0);
                let color = if app.peak_disp > 0.99 {
                    Color32::RED
                } else if app.peak_disp > 0.7 {
                    Color32::YELLOW
                } else {
                    Color32::LIGHT_GREEN
                };
                if frac > 0.01 {
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            rect.min,
                            egui::vec2(rect.width() * frac, rect.height()),
                        ),
                        1.0,
                        color,
                    );
                }
                ui.label(RichText::new("vol").strong().small());
                // Master volume now mirrors the per-track rows: editable dB
                // text box, slider, raw MIDI readout and a "0" zeroize
                // button. The scratch edit lives in `lv_edit` under the
                // sentinel track index so it can't collide with a track.
                const MASTER_IDX: usize = usize::MAX;
                let mut mtxt = app
                    .lv_edit
                    .get(&(KIND_VOL, MASTER_IDX))
                    .cloned()
                    .unwrap_or_else(|| db_num(vol_amp(app.master_midi)));
                let mrt = ui
                    .add(
                        egui::TextEdit::singleline(&mut mtxt)
                            .desired_width(46.0)
                            .horizontal_align(egui::Align::Center)
                            .font(egui::TextStyle::Monospace),
                    )
                    .on_hover_text("Master level in dB (e.g. -6.5). Press Enter to apply.");
                ui.add(
                    egui::Label::new(RichText::new("dB").monospace().weak().small())
                        .selectable(false),
                );
                if mrt.has_focus() {
                    app.lv_edit.insert((KIND_VOL, MASTER_IDX), mtxt.clone());
                } else {
                    app.lv_edit.remove(&(KIND_VOL, MASTER_IDX));
                }
                if mrt.lost_focus() && ui.input(|u| u.key_pressed(egui::Key::Enter)) {
                    if let Some(v) = level_parse(KIND_VOL, &mtxt) {
                        app.master_midi = v;
                        app.levels_dirty = true;
                    }
                    app.lv_edit.remove(&(KIND_VOL, MASTER_IDX));
                }
                let mut mv = app.master_midi as i16;
                if ui
                    .add_sized(
                        egui::vec2(84.0, 18.0),
                        Slider::new(&mut mv, 0..=127).show_value(false),
                    )
                    .on_hover_text("Master volume: -inf .. +6 dB")
                    .changed()
                {
                    app.master_midi = mv.clamp(0, 127) as u8;
                    app.levels_dirty = true;
                }
                ui.add(
                    egui::Label::new(RichText::new(format!("{mv:>3}")).monospace().weak().small())
                        .selectable(false),
                )
                .on_hover_text("Raw MIDI value (0..=127) of the master fader.");
                if ui
                    .small_button(RichText::new("0").monospace())
                    .on_hover_text("Reset master volume to 0 dB")
                    .clicked()
                {
                    app.master_midi = VOL_UNITY_MIDI;
                    app.levels_dirty = true;
                }
                if ui.button("Export").clicked() {
                    app.show_export = true;
                }
                if ui
                    .button("Load")
                    .on_hover_text("Browse saved projects")
                    .clicked()
                {
                    app.show_load = true;
                }
                if ui
                    .button("Save as")
                    .on_hover_text("Save under a new name")
                    .clicked()
                {
                    app.request_save(true);
                }
                let save_tip = match &app.proj_name {
                    n if n.is_empty() => "Save — asks for a project name (Ctrl+S)".to_string(),
                    n => format!("Save “{n}” (Ctrl+S)"),
                };
                if ui.button("Save").on_hover_text(save_tip).clicked() {
                    app.request_save(false);
                }
            });
        });
    });
}

/// Playhead position as `bar.beat` + `m:ss.t`.
fn pos_label(app: &App) -> String {
    let sr = app.sample_rate.max(1) as f64;
    let pos = app.backend.engine.pos();
    let beats = quantize::beat_samples(app.sample_rate, app.bpm());
    let total_beats = pos as f64 / beats;
    let num = app.beats() as f64;
    let bar = (total_beats / num).floor() as u64 + 1;
    let beat = (total_beats % num).floor() as u64 + 1;
    let t = pos as f64 / sr;
    format!("{}.{}  {}:{:04.1}", bar, beat, (t / 60.0) as u64, t % 60.0)
}

fn base_name(p: &str) -> String {
    Path::new(p.trim())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.trim().to_string())
}

/// Rack row snapshot: (folder path, [(file name, path)], scan error).
type RackRow = (String, Vec<(String, PathBuf)>, Option<String>);

/// The asset rack: folders (primary first = recordings land there), one
/// draggable row per audio file, project-only assets below, and the
/// selected-asset toolbar (quantize / trim / remove).
fn rack_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::left("asset_rack")
        .resizable(true)
        .default_size(260.0)
        .min_size(190.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("ASSET RACK").strong());
                if ui.button("+ folder").on_hover_text("Add an audio folder to the rack").clicked()
                {
                    if let Some(p) = rfd::FileDialog::new().set_title("Add rack folder").pick_folder()
                    {
                        let mut f = RackFolder::new(p.to_string_lossy().into_owned());
                        f.rescan();
                        app.rack.push(f);
                        app.save_config();
                    }
                }
            });
            if app.rack.is_empty() {
                ui.label(
                    RichText::new(
                        "Add a folder: its audio files become clips you can \
                         drag onto tracks. The FIRST folder also receives \
                         every recording as a WAV.",
                    )
                    .weak()
                    .small(),
                );
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // Snapshot so rows can call &mut App methods (ensure_asset).
                let snap: Vec<RackRow> = app
                    .rack
                    .iter()
                    .map(|f| {
                        (f.path.clone(), f.files.iter().map(|r| (r.name.clone(), r.path.clone())).collect(), f.err.clone())
                    })
                    .collect();
                let mut rescan = None;
                let mut browse = None;
                let mut rm = None;
                for (fi, (path, files, err)) in snap.into_iter().enumerate() {
                    ui.add_space(3.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        let tag = if fi == 0 { " [primary]" } else { "" };
                        ui.label(
                            RichText::new(format!("{}{}", base_name(&path), tag))
                                .strong()
                                .small(),
                        )
                        .on_hover_text(format!("{}{}", path, if fi == 0 { " (primary — recordings are stored here)" } else { "" }));
                        if ui.small_button("scan").clicked() {
                            rescan = Some(fi);
                        }
                        if ui.small_button("...").on_hover_text("Change folder").clicked() {
                            browse = Some(fi);
                        }
                        if ui.small_button("x").on_hover_text("Remove from rack").clicked() {
                            rm = Some(fi);
                        }
                    });
                    if let Some(e) = err {
                        ui.label(RichText::new(format!("× {}", e)).color(Color32::from_rgb(220, 120, 120)).small());
                    }
                    for (name, fp) in files {
                        let sel = app.sel_asset.is_some_and(|k| app.assets.get(k).is_some_and(|a| a.path == fp));
                        // Full-width row: a plain selectable_label shrink-wraps
                        // to the text, leaving most of the row dead to clicks.
                        // Fixed slim rows: sizing to available_size() made
                        // every row fill the panel (the "huge hat" bug).
                        let w = ui.available_width();
                        let row = ui.add_sized(
                            egui::vec2(w, 20.0),
                            egui::Button::selectable(sel, RichText::new(&name).small()),
                        );
                        // ONE click_and_drag response for the row: a separate
                        // drag-sense overlay on top would CAPTURE the press
                        // and silently kill the row's own clicked().
                        let dr = ui.interact(row.rect, ui.id().with(("rackdrag", fi, &name)), Sense::click_and_drag());
                        if dr.clicked() {
                            // Click = select + audition (asset preview).
                            if let Some(k) = app.ensure_asset(&fp) {
                                app.preview_asset(k);
                            }
                        }
                        if dr.drag_started() {
                            if let Some(k) = app.ensure_asset(&fp) {
                                app.sel_asset = Some(k);
                                // egui-native drag: the payload travels with
                                // the gesture and lane strips receive the
                                // release themselves (a manual cross-panel
                                // drag never landed — the rack owned it).
                                // If the decode failed there is no payload
                                // and the drop does nothing.
                                dr.dnd_set_drag_payload(k);
                            }
                        }
                        row.on_hover_text(format!(
                            "{} \u{b7} click to audition (preview), drag onto a lane to place a clip",
                            fp.display()
                        ));
                    }
                }
                if let Some(fi) = rescan {
                    if let Some(f) = app.rack.get_mut(fi) {
                        f.rescan();
                    }
                }
                if let Some(fi) = browse {
                    if let Some(p) = rfd::FileDialog::new().set_title("Rack folder").pick_folder() {
                        app.rack[fi].path = p.to_string_lossy().into_owned();
                        app.rack[fi].rescan();
                        app.save_config();
                    }
                }
                if let Some(fi) = rm {
                    if app.rack.len() > 1 || fi != 0 {
                        app.rack.remove(fi);
                    } else {
                        app.rack.clear();
                    }
                    app.save_config();
                }

                // Project-only assets: recordings with no rack folder,
                // dirty edits, and files whose folder was removed.
                let mut extra: Vec<(usize, String, bool)> = Vec::new();
                for (k, a) in app.assets.iter().enumerate() {
                    let in_rack = app
                        .rack
                        .iter()
                        .any(|f| f.files.iter().any(|r| r.path == a.path && !a.dirty));
                    if !in_rack {
                        extra.push((k, a.name.clone(), a.dirty));
                    }
                }
                if !extra.is_empty() {
                    ui.add_space(3.0);
                    ui.separator();
                    ui.label(RichText::new("project assets").weak().small());
                    for (k, name, dirty) in extra {
                        let sel = app.sel_asset == Some(k);
                        let w = ui.available_width();
                        let row = ui.add_sized(
                            egui::vec2(w, 20.0),
                            egui::Button::selectable(
                                sel,
                                RichText::new(format!("{}{}", name, if dirty { " *" } else { "" })).small(),
                            ),
                        );
                        // Merged click+drag response (see rack rows above).
                        let dr = ui.interact(row.rect, ui.id().with(("assetdrag", k)), Sense::click_and_drag());
                        if dr.clicked() {
                            app.sel_asset = Some(k);
                            app.preview_asset(k);
                        }
                        if dr.drag_started() {
                            dr.dnd_set_drag_payload(k);
                        }
                        row.on_hover_text("part of this project (not a rack file) — draggable");
                    }
                }

                // ---- selected-asset toolbar ----
                ui.add_space(6.0);
                ui.separator();
                match app.sel_asset {
                    Some(k) if k < app.assets.len() => {
                        let (nm, secs) = (app.assets[k].name.clone(), app.assets[k].data.len() as f64 / app.sample_rate.max(1) as f64);
                        ui.label(RichText::new(format!("asset: {}", nm)).small());
                        ui.label(RichText::new(format!("{:.2} s", secs)).weak().small());
                        ui.horizontal(|ui| {
                            if ui.small_button("Quantize").on_hover_text(format!("Snap the content to the {} grid", SUBDIVISIONS[app.q_sub].1)).clicked() {
                                app.transform_asset(k, 0);
                            }
                            if ui.small_button("Trim").on_hover_text("Clip silence off the start and end").clicked() {
                                app.transform_asset(k, 1);
                            }
                            if ui.small_button("Remove").on_hover_text("Remove this asset and its clips").clicked() {
                                app.remove_asset_at(k);
                            }
                        });
                    }
                    _ => {
                        ui.label(RichText::new("select an asset to quantize, trim or remove").weak().small());
                    }
                }
            });
        });
}

/// The mixer dock: every track's controls, as one strip per track, pinned
/// to the right like the asset rack. Strip heights come from [`App::track_h`]
/// and the column is offset by the grid's scroll — so each strip sits exactly
/// level with its track, and clips/waveforms get the grid's full height.
fn mixer_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::right("mixer")
        .resizable(true)
        .default_size(250.0)
        .min_size(210.0)
        .show(ui, |ui| {
            ui.heading(RichText::new("MIXER").strong());
            ui.separator();
            let base = ui.available_rect_before_wrap();
            // Clip to the visible column, then lay strips out on absolute
            // rects shifted by the grid's vertical scroll (shared offset —
            // the alignment is the whole point of this panel). The column
            // starts at the timeline track stack's top edge (measured last
            // frame by `central_panels`), so strips sit level with tracks
            // even though the two panels' headers differ in height; any
            // leftover room between panels is intentionally left empty.
            ui.shrink_clip_rect(base);
            let top = if app.grid_anchor > 0.0 {
                app.grid_anchor.min(base.max.y)
            } else {
                base.min.y
            };
            let mut y = top - app.vscroll;
            for t in 0..MAX_LOOPS {
                let th = app.track_h(t);
                if y + th >= base.min.y && y <= base.max.y {
                    let r = egui::Rect::from_min_size(
                        egui::pos2(base.min.x, y),
                        egui::vec2(base.width(), th),
                    );
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .id_salt(("mixstrip", t))
                            .max_rect(r)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| mixer_strip(ui, app, t),
                    );
                }
                y += th + TRACK_SEP_HIT;
            }
            let _ = ui.allocate_exact_size(base.size(), Sense::hover());
        });
}

/// One track's mixer strip: select / record / mute / child lanes, then the
/// vol-gain-pan stack and the FX chain chips.
fn mixer_strip(ui: &mut egui::Ui, app: &mut App, t: usize) {
    let sel = app.selected == t;
    let rec = app.backend.engine.rec_state_of(t) == TR_REC;
    let on = app.backend.engine.track_on(t);
    let nl = app.arr.tracks[t].lanes.len();
    let clips: usize = app.arr.tracks[t].lanes.iter().map(|l| l.clips.len()).sum();
    ui.horizontal(|ui| {
        let head = ui
            .selectable_label(sel, RichText::new(format!("Track {}", t + 1)).strong())
            .on_hover_text("Click to select \u{b7} right-click this track's grid clears its clips");
        if head.clicked() {
            app.selected = t;
        }
        let rec_lbl = if rec { "STOP" } else { "REC" };
        if ui
            .small_button(RichText::new(rec_lbl).color(if rec {
                Color32::from_rgb(255, 90, 90)
            } else {
                Color32::from_rgb(220, 180, 120)
            }))
            .on_hover_text(
                "Record: the take becomes an asset plus a clip at the playhead; \
                 recording over an occupied lane opens a child lane automatically",
            )
            .clicked()
        {
            app.toggle_record(t);
        }
        if ui
            .small_button(if on { "on" } else { "mute" })
            .on_hover_text(
                "While the transport runs, a muted track contributes nothing \
                 (its clips stay on the timeline)",
            )
            .clicked()
        {
            app.backend.cmd(Cmd::ToggleTrackOn { i: t });
        }
        if ui
            .small_button("+")
            .on_hover_text("Add a child lane (stacks additively)")
            .clicked()
        {
            app.arr.add_lane(t);
            app.grow_for_lanes(t, nl);
            app.push_project();
        }
        if nl > 1
            && ui
                .small_button("-")
                .on_hover_text("Remove the last child lane")
                .clicked()
        {
            let per = app.track_h(t) / nl.max(1) as f32;
            app.arr.remove_lane(t, nl - 1);
            app.set_track_h(t, per * (nl - 1).max(1) as f32);
            app.push_project();
        }
        ui.label(RichText::new(format!("{clips}c/{nl}L")).weak().small());
    });
    for kind in [KIND_VOL, KIND_GAIN, KIND_PAN] {
        level_row(ui, app, t, kind);
    }
    ui.horizontal(|ui| {
        if ui
            .small_button("FX")
            .on_hover_text("This track's effects: enable scripts, reorder the chain, open editors")
            .clicked()
        {
            app.fx_window = Some(t);
        }
        fx_buttons(ui, app, t);
    });
}

/// The toolbar row above the timeline: quantize / snap controls, asset
/// actions, loop and zoom.
fn grid_toolbar(ui: &mut egui::Ui, app: &mut App) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("grid:").strong());
        if ui
            .checkbox(&mut app.auto_quantize, "auto-quantize new recordings")
            .on_hover_text("Quantize each fresh recording's content onto the metronome grid")
            .changed()
        {
            app.save_config();
        }
        ui.label("quantize");
        egui::ComboBox::from_id_salt("q_sub_cb")
            .selected_text(SUBDIVISIONS[app.q_sub].1)
            .show_ui(ui, |ui| {
                for (n, s) in SUBDIVISIONS.iter().enumerate() {
                    if ui.selectable_value(&mut app.q_sub, n, s.1).changed() {
                        app.save_config();
                    }
                }
            });
        ui.label("snap");
        egui::ComboBox::from_id_salt("snap_sub_cb")
            .selected_text(SUBDIVISIONS[app.snap_sub].1)
            .show_ui(ui, |ui| {
                for (n, s) in SUBDIVISIONS.iter().enumerate() {
                    if ui.selectable_value(&mut app.snap_sub, n, s.1).changed() {
                        app.save_config();
                    }
                }
            });
        ui.separator();
        // ---- looping (grid header) ----
        let mut lt = app.loop_tl;
        if ui
            .checkbox(&mut lt, "loop")
            .on_hover_text(
                "Wrap the playhead at the end of the loop region instead of \
                 parking at the project end",
            )
            .changed()
        {
            app.loop_tl = lt;
            app.backend.cmd(Cmd::SetLoop(app.loop_tl));
            app.send_loop_region();
            app.save_config();
        }
        let mut mch = false;
        ui.add_enabled_ui(app.loop_tl, |ui| {
            egui::ComboBox::from_id_salt("loop_mode_cb")
                .selected_text(LOOP_MODES[app.loop_mode.min(2)])
                .show_ui(ui, |ui| {
                    for (n, s) in LOOP_MODES.iter().enumerate() {
                        if ui.selectable_value(&mut app.loop_mode, n, *s).changed() {
                            mch = true;
                        }
                    }
                });
        });
        if mch {
            app.send_loop_region();
            app.save_config();
        }
        if app.loop_tl && app.loop_mode == 0 {
            ui.label(RichText::new("(drag the ruler handles)").weak().small());
        }
        ui.separator();
        ui.label("zoom");
        // Logarithmic so the default (160 px/s) sits near the centre of the
        // 3..2400 range: max-out shows ~100 bars, max-in edits samples.
        if ui
            .add(
                Slider::new(&mut app.px_per_sec, ZOOM_MIN..=ZOOM_MAX)
                    .logarithmic(true)
                    .show_value(false),
            )
            .changed()
        {}
    });
}

fn central_panels(ui: &mut egui::Ui, app: &mut App) {
    egui::CentralPanel::default().show(ui, |ui| {
        grid_toolbar(ui, app);
        ui.separator();
        let sr = app.sample_rate.max(1) as f64;
        let total = app.total_len().max(1);
        let px = app.px_per_sec as f64;
        let w = ((total as f64 / sr) * px).max(640.0) as f32;
        if ui.input(|i| !i.pointer.any_down()) {
            app.scrub_drag = false;
        }
        // Plain horizontal wheel (no modifiers) pans the timeline; the
        // modifier combos (Ctrl = zoom, Shift = pan) are intercepted at the
        // top of `ui_root` before egui converts them to scroll deltas.
        let area = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(area) {
            let dx = ui.input(|i| i.smooth_scroll_delta.x);
            if dx != 0.0 && !ui.input(|i| i.modifiers.shift) {
                app.hscroll -= dx;
            }
        }
        let avail = ui.available_width().max(200.0);
        app.hscroll = app.hscroll.clamp(0.0, (w - avail).max(0.0));
        // Pinned ruler: always visible above the track scroll (it used to
        // scroll away with the content), same width + x origin as the lanes.
        ruler_row(ui, app, avail, px, sr);
        let grid_top = ui.cursor().min;
        app.grid_anchor = grid_top.y;
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for t in 0..MAX_LOOPS {
                    track_block(ui, app, t, avail, px, sr);
                }
            });
        // The mixer panel mirrors this offset so strips stay aligned.
        app.vscroll = out.state.offset.y;
        // One continuous playhead across the whole track stack (drawn over
        // lanes AND separation bars, so it shows no break).
        let gx = x_of(grid_top.x - app.hscroll, app.backend.engine.pos(), sr, px);
        let gr = egui::Rect::from_min_max(grid_top, ui.max_rect().right_bottom());
        if gx >= gr.min.x && gx <= gr.max.x {
            ui.painter().line_segment(
                [egui::pos2(gx, gr.min.y), egui::pos2(gx, gr.max.y)],
                egui::Stroke::new(1.6, COL_PLAYHEAD),
            );
        }
    });
}

/// Seconds -> x within a strip starting at `x0`.
fn x_of(x0: f32, pos: u64, sr: f64, px: f64) -> f32 {
    x0 + (pos as f64 / sr * px) as f32
}

/// Samples at x within a strip starting at `x0`.
fn s_at(x0: f32, x: f32, sr: f64, px: f64) -> u64 {
    (((x - x0).max(0.0) as f64 / px) * sr) as u64
}

/// Paint the metronome grid (bar lines bold, beat lines faint, sub-beat
/// snap lines even fainter — drawn at the snap subdivision) plus the
/// playhead across `rect`. `x0` is where timeline position 0 sits in point
/// coordinates — left of `rect` whenever the view is panned — so every strip
/// (pinned ruler and lane alike) maps samples through the identical window
/// and the playhead renders as one straight line across the whole grid.
fn paint_grid(
    ui: &mut egui::Ui,
    app: &App,
    rect: egui::Rect,
    px: f64,
    sr: f64,
    ruler: bool,
    x0: f32,
) {
    let p = ui.painter();
    p.rect_filled(
        rect,
        0.0,
        if ruler {
            Color32::from_rgb(34, 34, 40)
        } else {
            Color32::from_rgb(24, 24, 29)
        },
    );
    let beat_px = quantize::beat_samples(sr as u32, app.bpm()) * (px / sr);
    let num = app.beats() as u64;
    // Sub-beat snap tier: follow the snap combo, but only while the step
    // stays readable (≥4 px apart) and actually subdivides the beat.
    let steps = SUBDIVISIONS[app.snap_sub].0;
    let snap_on = steps > 1 && (beat_px / steps as f64) > 4.0;
    if beat_px > 3.0 {
        // Only the beats inside this strip's visible window (cheap at any
        // project length, correct under panning).
        let b_lo = (((rect.min.x - x0).max(0.0) as f64 / beat_px) as u64).saturating_sub(1);
        let b_hi = (((rect.max.x - x0).max(0.0) as f64 / beat_px) as u64) + 2;
        for b in b_lo..b_hi.min(b_lo + 4096) {
            let x = x0 + (b as f64 * beat_px) as f32;
            if x < rect.min.x - 1.0 || x > rect.max.x + 1.0 {
                continue;
            }
            // Snap steps between this beat and the next (k = 0 is the beat
            // line itself; snap subdivisions never coincide with a beat).
            if snap_on {
                for k in 1..steps {
                    let sx = x0 + ((b as f64 + k as f64 / steps as f64) * beat_px) as f32;
                    if sx < rect.min.x - 1.0 || sx > rect.max.x + 1.0 {
                        continue;
                    }
                    p.line_segment(
                        [egui::pos2(sx, rect.min.y), egui::pos2(sx, rect.max.y)],
                        egui::Stroke::new(0.6, COL_SNAP_LINE),
                    );
                }
            }
            let bar = b % num == 0;
            p.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(
                    if bar { 1.3 } else { 0.6 },
                    if bar { COL_BAR_LINE } else { COL_BEAT_LINE },
                ),
            );
            if ruler && bar && beat_px * num as f64 > 44.0 {
                p.text(
                    egui::pos2(x + 3.0, rect.min.y + 3.0),
                    egui::Align2::LEFT_TOP,
                    format!("{}", b / num + 1),
                    egui::FontId::proportional(10.0),
                    Color32::GRAY,
                );
            }
        }
    }
    // Playhead: the ruler paints its own segment; the track stack gets ONE
    // continuous line painted over lanes and separation bars together (see
    // `central_panels`), so it never breaks at a track boundary.
    if ruler {
        let pos = app.backend.engine.pos();
        let x = x_of(x0, pos, sr, px);
        if x >= rect.min.x && x <= rect.max.x {
            p.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(1.6, COL_PLAYHEAD),
            );
        }
    }
}

/// Timeline ruler: measure grid, bar numbers, and the scrub handle — press
/// or drag anywhere here to move the global playhead. Pinned above the track
/// scroll so it is always visible; shares its x window with the lane strips.
fn ruler_row(ui: &mut egui::Ui, app: &mut App, avail: f32, px: f64, sr: f64) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(avail, 20.0), Sense::click_and_drag());
    let x0 = rect.min.x - app.hscroll;
    paint_grid(ui, app, rect, px, sr, true, x0);
    // Loop region on the ruler ("grid header"): a shaded span, plus two
    // draggable handles in area mode. Handles follow the snap setting.
    let (ls, le) = app.loop_region_eff();
    let hx0 = x_of(x0, ls, sr, px);
    let hx1 = x_of(x0, le, sr, px);
    if app.loop_tl && le > ls {
        let band = egui::Rect::from_min_max(
            egui::pos2(hx0.max(rect.min.x), rect.min.y),
            egui::pos2(hx1.min(rect.max.x), rect.max.y),
        );
        if band.width() > 0.5 {
            ui.painter().rect_filled(
                band,
                0.0,
                Color32::from_rgba_premultiplied(90, 150, 230, 55),
            );
        }
        if app.loop_mode == 0 {
            // Grab bars in loud yellow: this is where the user drags the
            // loop's start/end. They brighten while one is being dragged.
            let dragging = app.drag_loop_handle.is_some();
            for (n, hx) in [hx0, hx1].into_iter().enumerate() {
                if hx > rect.min.x - 4.0 && hx < rect.max.x + 4.0 {
                    let held = dragging && app.drag_loop_handle == Some(n as u8);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(hx - 3.0, rect.min.y),
                            egui::vec2(6.0, rect.height()),
                        ),
                        1.0,
                        if held {
                            Color32::from_rgb(255, 235, 120)
                        } else {
                            Color32::from_rgb(250, 200, 50)
                        },
                    );
                }
            }
        }
    }
    if let Some(pt) = resp.interact_pointer_pos() {
        if resp.drag_started_by(egui::PointerButton::Primary) {
            app.drag_loop_handle = if app.loop_tl
                && app.loop_mode == 0
                && (pt.x - hx0).abs().min((pt.x - hx1).abs()) <= 7.0
            {
                Some(if (pt.x - hx0).abs() <= (pt.x - hx1).abs() {
                    0
                } else {
                    1
                })
            } else {
                None
            };
        }
        if let Some(h) = app.drag_loop_handle {
            if resp.dragged() {
                let snapu = app.snap_unit();
                let pos = snap_start(s_at(x0, pt.x, sr, px), snapu);
                let bar = app.bar_samples();
                let (mut s, mut e) = (
                    app.loop_area.0,
                    if app.loop_area.1 == 0 {
                        2 * bar
                    } else {
                        app.loop_area.1
                    },
                );
                if h == 0 {
                    s = pos.min(e.saturating_sub(1));
                } else {
                    e = pos.max(s + 1);
                }
                app.loop_area = (s, e);
                app.send_loop_region();
            }
            if resp.drag_stopped() {
                app.drag_loop_handle = None;
                app.save_config();
            }
        } else if (resp.dragged() || resp.clicked()) && rect.contains(pt) {
            let pos = s_at(x0, pt.x, sr, px);
            app.backend.cmd(Cmd::Seek { pos });
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
}

/// One track's grid block: every lane strip, edge to edge with zero spacing,
/// closed by a thin separation bar. All controls live in the mixer panel —
/// this block's height is exactly [`App::track_h`], which the mixer shares.
fn track_block(ui: &mut egui::Ui, app: &mut App, t: usize, w: f32, px: f64, sr: f64) {
    let th = app.track_h(t);
    let nl = app.arr.tracks[t].lanes.len().max(1);
    let lane_h = th / nl as f32;
    for k in 0..nl {
        lane_row(ui, app, t, k, w, px, sr, lane_h);
    }
    // The separation bar doubles as the resize handle: dragging it changes
    // ONLY the track above (children inside share the new height equally).
    let (r, sep) = ui.allocate_exact_size(egui::vec2(w, TRACK_SEP_HIT), Sense::drag());
    let hot = sep.hovered() || app.sep_drag.is_some_and(|(d, _, _)| d == t);
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(r.min.x, r.center().y - TRACK_SEP / 2.0),
            egui::vec2(r.width(), TRACK_SEP),
        ),
        0.0,
        if hot {
            Color32::from_rgb(96, 96, 116)
        } else {
            Color32::from_rgb(56, 56, 64)
        },
    );
    if sep.drag_started_by(egui::PointerButton::Primary) {
        if let Some(pt) = sep.interact_pointer_pos() {
            app.sep_drag = Some((t, app.track_h(t), pt.y));
        }
    }
    if sep.dragged() {
        if let Some((tt, h0, y0)) = app.sep_drag {
            if tt == t {
                if let Some(pt) = sep.interact_pointer_pos() {
                    app.set_track_h(t, h0 + (pt.y - y0));
                }
            }
        }
    }
    if sep.drag_stopped() && app.sep_drag.is_some() {
        app.sep_drag = None;
        app.save_config();
    }
    if sep.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
}

/// One lane strip: grid + clips + interactions, clipped to its own rect and
/// sharing the pinned ruler's x window (pan offset included). Pressing empty
/// grid scrubs the playhead; clips drag with snap; rack drags arrive through
/// egui's native drag-and-drop (the old cross-response handoff dropped
/// nothing because the rack owned the pointer).
#[allow(clippy::too_many_arguments)] // (grid geometry) + (view state) tuples read worse
fn lane_row(
    ui: &mut egui::Ui,
    app: &mut App,
    t: usize,
    k: usize,
    w: f32,
    px: f64,
    sr: f64,
    h: f32,
) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::click_and_drag());
    let x0 = rect.min.x - app.hscroll;
    let snapu = app.snap_unit();
    let sel_clip = app.sel_clip;
    let clips: Vec<(Clip, String)> = app.arr.tracks[t].lanes[k]
        .clips
        .iter()
        .map(|c| {
            (
                *c,
                app.assets
                    .get(c.asset as usize)
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();

    // ---- paint (clipped to this strip) ----
    let old_clip = ui.clip_rect();
    ui.shrink_clip_rect(rect);
    paint_grid(ui, app, rect, px, sr, false, x0);
    // Lane tag rides inside the strip's left edge: it must NOT take layout
    // room, or the lane grids would sit right of the ruler's.
    ui.painter().text(
        rect.min + egui::vec2(4.0, 1.0),
        egui::Align2::LEFT_TOP,
        format!("T{} {}", t + 1, if k == 0 { "track" } else { "child" }),
        egui::FontId::proportional(9.0),
        Color32::from_gray(110),
    );
    for (ci, (c, name)) in clips.iter().enumerate() {
        let len = app.asset_len(c.asset);
        let cx0 = x_of(x0, c.pos, sr, px);
        let cx1 = x_of(x0, c.pos + len, sr, px);
        if cx1 < rect.min.x || cx0 > rect.max.x {
            continue;
        }
        let sel = sel_clip == Some((t, k, ci));
        draw_asset_clip(
            ui,
            app,
            c.asset,
            egui::Rect::from_min_max(
                egui::pos2(cx0, rect.min.y + 1.0),
                egui::pos2(cx1, rect.max.y - 1.0),
            ),
            COL_CLIP,
            sel,
        );
        if cx1 - cx0 > 36.0 && !name.is_empty() {
            ui.painter().text(
                egui::pos2(cx0 + 3.0, rect.min.y + 2.0),
                egui::Align2::LEFT_TOP,
                name,
                egui::FontId::proportional(10.0),
                Color32::from_gray(220),
            );
        }
        if sel {
            // Selection hint: this clip is what Delete removes.
            ui.painter().text(
                egui::pos2(cx1 - 3.0, rect.min.y + 1.0),
                egui::Align2::RIGHT_TOP,
                "\u{d7}",
                egui::FontId::proportional(13.0),
                Color32::from_rgb(235, 120, 120),
            );
        }
    }
    // Rack drag: ghost preview while a native egui drag hovers this strip
    // (contains_pointer keeps working even while the source is dragged).
    let drop_in: Option<usize> = resp.dnd_hover_payload::<usize>().map(|a| *a);
    if let (Some(a), Some(pt)) = (drop_in, ui.input(|i| i.pointer.hover_pos())) {
        let pt = pt.clamp(
            egui::pos2(rect.min.x, rect.min.y),
            egui::pos2(rect.max.x, rect.max.y),
        );
        let ns = snap_start(s_at(x0, pt.x, sr, px), snapu);
        let len = app.asset_len(a as u32);
        let gx0 = x_of(x0, ns, sr, px).clamp(rect.min.x, rect.max.x);
        let gx1 = x_of(x0, ns + len, sr, px).clamp(rect.min.x + 2.0, rect.max.x);
        ui.painter().rect_stroke(
            egui::Rect::from_min_max(
                egui::pos2(gx0, rect.min.y + 1.0),
                egui::pos2(gx1, rect.max.y - 1.0),
            ),
            2.0,
            egui::Stroke::new(2.0, COL_GHOST),
            egui::StrokeKind::Inside,
        );
    }
    ui.set_clip_rect(old_clip);

    // ---- interactions ----
    // Release of a rack drag over this strip places the clip (the strip
    // receives it directly; the old manual cross-panel handoff never did).
    if let Some(a) = resp.dnd_release_payload::<usize>().map(|a| *a) {
        if let Some(pt) = ui.input(|i| i.pointer.hover_pos()) {
            if rect.contains(pt) {
                let ns = snap_start(s_at(x0, pt.x, sr, px), snapu);
                app.arr.insert(
                    t,
                    k,
                    Clip {
                        asset: a as u32,
                        pos: ns,
                    },
                );
                let li = app.arr.tracks[t].lanes[k].clips.len() - 1;
                app.sel_clip = Some((t, k, li));
                app.push_project();
                app.status = format!(
                    "clip placed on track {} \u{b7} {}",
                    t + 1,
                    if k == 0 { "track" } else { "child lane" }
                );
            }
        }
        return; // the drop consumed this frame's pointer
    }
    // Any primary press in the grid remembers this track as the paste
    // target ("paste into the nearest track to the last click").
    if resp.drag_started_by(egui::PointerButton::Primary) {
        app.last_click_track = Some(t);
        if let Some(pt) = resp.interact_pointer_pos() {
            if rect.contains(pt) {
                let pos = s_at(x0, pt.x, sr, px);
                let hit = clips
                    .iter()
                    .position(|(c, _)| pos >= c.pos && pos < c.pos + app.asset_len(c.asset));
                match hit {
                    Some(ci) => {
                        app.sel_clip = Some((t, k, ci));
                        let cpos = app.arr.tracks[t].lanes[k].clips[ci].pos;
                        app.drag_clip = Some(ClipDrag {
                            t,
                            lane: k,
                            c: ci,
                            grab: cpos.saturating_sub(pos),
                        });
                    }
                    None => {
                        // Empty grid = playhead handle: press-jumps on
                        // release (below), drag keeps scanning; the movement
                        // area spans the grid of every track and matches the
                        // pinned ruler position 1:1.
                        app.sel_clip = None;
                        app.scrub_drag = true;
                        app.backend.cmd(Cmd::Seek { pos });
                    }
                }
            }
        }
    } else if resp.dragged_by(egui::PointerButton::Primary) {
        if let Some(pt) = resp.interact_pointer_pos() {
            if app.scrub_drag {
                if rect.contains(pt) {
                    let pos = s_at(x0, pt.x, sr, px);
                    app.backend.cmd(Cmd::Seek { pos });
                }
            } else if let Some(d) = app.drag_clip {
                if d.t == t && d.lane == k && rect.contains(pt) {
                    let pos = snap_start(s_at(x0, pt.x, sr, px).saturating_add(d.grab), snapu);
                    app.arr.move_clip(t, k, d.c, pos);
                }
            }
        }
    }
    if resp.drag_stopped() && app.drag_clip.take().is_some() {
        app.push_project();
    }
    if resp.clicked() && !resp.dragged() && !app.scrub_drag && app.drag_clip.is_none() {
        // Plain click (no drag happened): select the clip under it, or scrub
        // if the click landed on empty grid.
        if let Some(pt) = resp.interact_pointer_pos() {
            if rect.contains(pt) {
                app.last_click_track = Some(t);
                let pos = s_at(x0, pt.x, sr, px);
                match clips
                    .iter()
                    .position(|(c, _)| pos >= c.pos && pos < c.pos + app.asset_len(c.asset))
                {
                    Some(ci) => app.sel_clip = Some((t, k, ci)),
                    None => {
                        app.sel_clip = None;
                        app.backend.cmd(Cmd::Seek { pos });
                    }
                }
            }
        }
    }
    // Right-click anywhere in the track's grid clears its clips — the old
    // header gesture (headers moved to the mixer). Read from the global
    // pointer state: 0.36 has no secondary-only Sense, and a click-sensing
    // overlay would steal primary clicks from the widgets underneath.
    let rmb = ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary));
    if rmb
        && ui
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|p| rect.contains(p))
    {
        app.clear_track(t);
    }
}

/// Draw one clip's rectangle with its asset's waveform, normalized by the
/// asset's own peak so quiet takes stay legible.
fn draw_asset_clip(
    ui: &mut egui::Ui,
    app: &mut App,
    asset: u32,
    rect: egui::Rect,
    fill: Color32,
    sel: bool,
) {
    let p = ui.painter();
    p.rect_filled(rect, 2.0, fill.linear_multiply(0.45));
    if rect.width() >= 4.0 {
        if let Some(wave) = asset_wave(app, asset, rect.width() as usize) {
            let data = wave;
            let n = data.len().max(1);
            let col_w = rect.width() / n as f32;
            let cy = rect.center().y;
            let amp = rect.height() * 0.46;
            let col = if sel {
                COL_CLIP_SEL
            } else {
                Color32::from_rgb(210, 225, 250)
            };
            let stroke = egui::Stroke::new((col_w * 0.7).max(0.8), col);
            for (i, [lo, hi]) in data.iter().enumerate() {
                let x = rect.min.x + (i as f32 + 0.5) * col_w;
                p.line_segment(
                    [egui::pos2(x, cy - hi * amp), egui::pos2(x, cy - lo * amp)],
                    stroke,
                );
            }
        }
    }
    let stroke = if sel {
        egui::Stroke::new(2.0, COL_CLIP_SEL)
    } else {
        egui::Stroke::new(1.0, fill)
    };
    p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
}

/// Cached min/max columns for one asset at the requested width.
fn asset_wave(app: &mut App, asset: u32, cols: usize) -> Option<Vec<[f32; 2]>> {
    let cols = cols.clamp(1, 4096);
    if let Some(c) = app.wave.get(&asset) {
        if c.cols == cols {
            return Some(c.data.clone());
        }
    }
    let a = app.assets.get(asset as usize)?;
    let data = wave_cols(&a.data, cols);
    let mut peak = 1e-4f32;
    let stride = (a.data.len() / 4096).max(1);
    for &v in a.data.iter().step_by(stride) {
        peak = peak.max(v.abs());
    }
    // Normalize the columns by the peak so drawing only scales by height.
    let mut norm = Vec::with_capacity(data.len());
    for [lo, hi] in data {
        norm.push([lo / peak, hi / peak]);
    }
    app.wave.insert(
        asset,
        AssetWave {
            cols,
            data: norm.clone(),
        },
    );
    Some(norm)
}

const KIND_VOL: u8 = 0;
const KIND_GAIN: u8 = 1;
const KIND_PAN: u8 = 2;

/// One stacked level row: `label | editable value box (centred) | slider |
/// raw MIDI readout`. The slider always runs the raw MIDI range 0..=127
/// (what a bound knob sends); the text box shows the human value — dB for
/// volume and gain, -100..=100 for pan (0 = centre) — and is typed into
/// directly, committed with Enter. Text hugs its slider with no filler
/// padding; every row's slider starts at the same x, so the three line up.
fn level_row(ui: &mut egui::Ui, app: &mut App, i: usize, kind: u8) {
    let label = ["vol ", "gain", "pan "][kind as usize];
    let midi = match kind {
        KIND_VOL => app.trk_vol[i],
        KIND_GAIN => app.trk_gain[i],
        _ => app.trk_pan[i],
    };
    let hover = match kind {
        KIND_VOL => "Track playback volume: -inf…+6 dB (0 dB = three-quarters). The number at the right is the raw MIDI value. Bind it to a MIDI knob in Settings → MIDI bindings → per track.",
        KIND_GAIN => "Track gain trim: 0 = silence, 80 = 0 dB, 127 = +10 dB. The number at the right is the raw MIDI value. Bind it to a MIDI knob in Settings → MIDI bindings → per track.",
        _ => "Track pan: -100 = hard left, 0 = centre, +100 = hard right (equal power across the L/R outputs). The number at the right is the raw MIDI value. Bind it to a MIDI knob in Settings → MIDI bindings → per track.",
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(label).monospace().small());
        // Editable value box (number only), centred text, flush against the
        // slider.
        let key = (kind, i);
        let mut txt = app
            .lv_edit
            .get(&key)
            .cloned()
            .unwrap_or_else(|| level_display(kind, midi));
        let rt = ui
            .add(
                egui::TextEdit::singleline(&mut txt)
                    .desired_width(46.0)
                    .horizontal_align(egui::Align::Center)
                    .font(egui::TextStyle::Monospace),
            )
            .on_hover_text(match kind {
                KIND_PAN => "Pan position -100..=100 (0 = centre). Press Enter to apply.",
                _ => "Level in dB (e.g. -6.5). Press Enter to apply.",
            });
        // The unit is plain text outside the box — the box holds numbers
        // only, so typing never has to fight the "dB" suffix.
        if kind != KIND_PAN {
            ui.add(
                egui::Label::new(RichText::new("dB").monospace().weak().small()).selectable(false),
            );
        }
        if rt.has_focus() {
            app.lv_edit.insert(key, txt.clone());
        } else {
            app.lv_edit.remove(&key);
        }
        if rt.lost_focus() && ui.input(|u| u.key_pressed(egui::Key::Enter)) {
            if let Some(v) = level_parse(kind, &txt) {
                match kind {
                    KIND_VOL => app.set_trk_vol(i, v),
                    KIND_GAIN => app.set_trk_gain(i, v),
                    _ => app.set_trk_pan(i, v),
                }
                app.levels_dirty = true;
            }
            app.lv_edit.remove(&key);
        }
        // Slider: raw MIDI range, no value bubble (the box shows the value).
        // Sized to the text box's height so the slider track sits exactly at
        // the text's vertical centre — the rows were misaligned when the
        // slider kept its own taller default height. Leave room for the
        // raw-value readout and the reset button.
        let mut v = midi;
        let sw = (ui.available_width() - if kind == KIND_PAN { 56.0 } else { 76.0 }).max(30.0);
        let bh = rt.rect.height();
        let r = ui
            .add_sized(
                egui::vec2(sw, bh),
                Slider::new(&mut v, 0..=127).show_value(false),
            )
            .on_hover_text(hover);
        if r.changed() {
            match kind {
                KIND_VOL => app.set_trk_vol(i, v),
                KIND_GAIN => app.set_trk_gain(i, v),
                _ => app.set_trk_pan(i, v),
            }
            app.levels_dirty = true;
        }
        ui.add(
            egui::Label::new(RichText::new(format!("{v:>3}")).monospace().weak().small())
                .selectable(false),
        )
        .on_hover_text("Raw MIDI value (0..=127) sent by a bound knob.");
        // Reset this strip straight to its "0" position.
        if ui
            .small_button(RichText::new("0").monospace())
            .on_hover_text(
                [
                    "Reset volume to 0 dB",
                    "Reset gain to 0 dB",
                    "Center the pan",
                ][kind as usize],
            )
            .clicked()
        {
            match kind {
                KIND_VOL => app.set_trk_vol(i, VOL_UNITY_MIDI),
                KIND_GAIN => app.set_trk_gain(i, GAIN_UNITY_MIDI),
                _ => app.set_trk_pan(i, 64),
            }
            app.levels_dirty = true;
        }
    });
}

/// What a level text box shows when it is not being edited (number only;
/// the "dB" unit is plain text drawn right after the box).
fn level_display(kind: u8, midi: u8) -> String {
    match kind {
        KIND_VOL => db_num(vol_amp(midi)),
        KIND_GAIN => db_num(gain_amp(midi)),
        _ => format!("{}", pan_text(midi)),
    }
}

/// Bare dB number for the level text box ("-inf", "+0.0", "-12.3").
fn db_num(amp: f32) -> String {
    if amp <= 0.0 {
        "-inf".into()
    } else {
        format!("{:+.1}", 20.0 * amp.log10())
    }
}

/// Parse a level text box back to a MIDI value ("−12.3 dB", "-inf", "50").
/// Volume / gain floor at MIDI 1 for finite values (0 stays reserved for
/// the exact "-inf"); pan maps -100..=100 -> 0..=127.
fn level_parse(kind: u8, s: &str) -> Option<u8> {
    let t: String = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let t = t.strip_suffix("db").unwrap_or(&t);
    match kind {
        KIND_PAN => t.parse::<f32>().ok().map(pan_midi),
        _ => {
            if t == "-inf" {
                return Some(0);
            }
            let db = t.parse::<f32>().ok()?;
            if !db.is_finite() {
                return None;
            }
            let v = match kind {
                KIND_VOL => vol_midi(db) as f32,
                _ => GAIN_UNITY_MIDI as f32 + db / 0.215,
            };
            Some(v.round().clamp(1.0, 127.0) as u8)
        }
    }
}

/// Enabled effects drawn on the loop row as plain name buttons, left to
/// right in chain order. The buttons replace the old inline sliders: a
/// click opens the effect's child editor window (auto sliders, or the
/// effect's own custom GUI if it scripts one).
fn fx_buttons(ui: &mut egui::Ui, app: &mut App, i: usize) {
    let mut open_slot: Option<usize> = None;
    let mut k = 0usize; // chain index (enabled entries only)
    for e in app.fx_entries[i].iter() {
        if !e.enabled {
            continue;
        }
        let Some(desc) = app.fx_registry.desc(&e.stem).cloned() else {
            continue;
        };
        let mut hover = String::new();
        if let Some(d) = desc.description.as_deref() {
            hover.push_str(d);
            hover.push('\n');
        }
        hover.push_str("Click to open this effect's controls window.");
        let color = if app.fx_live[i].get(k).is_some() {
            Color32::from_rgb(150, 200, 255)
        } else {
            Color32::from_gray(140)
        };
        if ui
            .button(RichText::new(&desc.name).small().color(color))
            .on_hover_text(hover)
            .clicked()
        {
            open_slot = Some(k);
        }
        k += 1;
    }
    if let Some(k) = open_slot {
        app.open_fx_editor(i, k);
    }
}

fn wave_cols(buf: &[f32], cols: usize) -> Vec<[f32; 2]> {
    let n = buf.len();
    if n == 0 || cols == 0 {
        return Vec::new();
    }
    let mut out: Vec<[f32; 2]> = vec![[0.0, 0.0]; cols];
    let stride = (n / (cols * 8)).max(1);
    for (c, col) in out.iter_mut().enumerate() {
        let lo = c * n / cols;
        let hi = ((c + 1) * n / cols).max(lo + 1);
        let mut mn = f32::MAX;
        let mut mx = f32::MIN;
        let mut k = lo;
        while k < hi {
            let v = buf[k];
            if v < mn {
                mn = v;
            }
            if v > mx {
                mx = v;
            }
            k += stride;
        }
        *col = [mn, mx];
    }
    out
}

/// Min/max per column for the dependent-mode tiled view: each column covers a
/// window of the ensemble period, sampled modulo the recording length — every
/// pixel column maps to a sample of the (repeating) loop, never to silence.
/// Same bounded stride sampling as [`wave_cols`], so the whole tiled view is
/// computed once per cache rebuild instead of once per frame.
/// Three-step knob-learn sub-window (opened by the knob buttons in
/// `bind_row`).
///
/// 1. Move any control: the axis — message TYPE + CHANNEL — is detected.
///    While it is not locked, moving a different control re-picks it;
///    **Lock axis** freezes it and advances to step 2.
/// 2. Listen on that axis for ANY 0..=127 data value; **Lock min** freezes
///    the raw minimum and advances to step 3.
/// 3. Listen again for ANY value; **Finish** commits. If the maximum read
///    lower than the minimum the axis is inverted — `scale_axis` flips the
///    translation so the knob still maps continuously low\u{2192}high, and
///    the window says so in plain words.
///
/// The raw lo/hi is stored per slot and incoming values are rescaled with
/// `scale_axis`, so a knob wired to a partial range (or inverted) still
/// covers the full program travel.
fn knob_learn_window(ui: &mut egui::Ui, app: &mut App) {
    if app.knob_learn.is_none() {
        return;
    }
    egui::Window::new("Learn knob")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ui, |ui| {
            ui.set_width(380.0);
            enum Act {
                None,
                LockAxis,
                LockMin,
                Finish,
                Cancel,
                Restart,
            }
            let mut act = Act::None;
            {
                let kl = app.knob_learn.as_ref().unwrap();
                let action = MidiAction::ALL[kl.slot].label();
                ui.label(RichText::new(format!("Bind knob: {action}")).strong());
                let axis = kl.bind.axis_label();
                if !kl.has_axis {
                    ui.label("1. Move the knob or slider you want to bind.");
                    ui.label(
                        RichText::new(
                            "The axis is the message TYPE + CHANNEL; its data byte (0-127) \
                             is the value that gets learned. Waiting for the control's \
                             first message\u{2026}",
                        )
                        .weak()
                        .small()
                        .italics(),
                    );
                } else if !kl.axis_locked {
                    ui.label(RichText::new(format!("1. Axis found: {axis}")).strong());
                    ui.label(
                        RichText::new(format!("last message seen: {}", kl.bind.label()))
                            .monospace()
                            .small(),
                    );
                    ui.label(
                        RichText::new(
                            "Wrong knob? Just move another one \u{2014} the axis follows the \
                             most recent control until you lock it. Lock the axis when it \
                             shows the type + channel you want.",
                        )
                        .weak()
                        .small(),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Lock axis").clicked() {
                            act = Act::LockAxis;
                        }
                        ui.label(
                            RichText::new("\u{2192} then capture the minimum")
                                .weak()
                                .small(),
                        );
                    });
                } else if !kl.locked {
                    ui.label(RichText::new(format!("2. Axis locked: {axis}")).strong());
                    ui.horizontal(|ui| {
                        ui.label(format!("minimum raw value: {}", kl.lo));
                        if ui.button("Lock min").clicked() {
                            act = Act::LockMin;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Move the control to its LOWEST position \u{2014} the number \
                             follows every message on this axis (any 0-127 value) \u{2014} \
                             then Lock min.",
                        )
                        .weak()
                        .small(),
                    );
                } else {
                    ui.label(RichText::new(format!("3. Axis locked: {axis}")).strong());
                    ui.label(
                        RichText::new(format!("minimum raw value: {} (locked)", kl.lo))
                            .weak()
                            .small(),
                    );
                    ui.horizontal(|ui| {
                        ui.label(format!("maximum raw value: {}", kl.hi));
                        let ok = kl.hi != kl.lo;
                        let fin = ui
                            .add_enabled(ok, egui::Button::new("Finish"))
                            .on_hover_text("needs a maximum different from the minimum");
                        if fin.clicked() {
                            act = Act::Finish;
                        }
                    });
                    let hint = if kl.lo > kl.hi {
                        format!(
                            "Min {} > max {}: this control is INVERTED. Finish flips the \
                             translation so turning the knob still sweeps low\u{2192}high \
                             across the full range.",
                            kl.lo, kl.hi
                        )
                    } else {
                        "Move the control to its HIGHEST position, then Finish.".to_string()
                    };
                    ui.label(RichText::new(hint).weak().small());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        act = Act::Cancel;
                    }
                    if kl.has_axis && ui.button("Restart").clicked() {
                        act = Act::Restart;
                    }
                });
            }
            match act {
                Act::None => {}
                Act::LockAxis => app.knob_lock_axis(),
                Act::LockMin => app.knob_lock_min(),
                Act::Finish => app.knob_finish(),
                Act::Cancel => {
                    let kl = app.knob_learn.take().unwrap();
                    app.status =
                        format!("cancelled knob learn: {}", MidiAction::ALL[kl.slot].label());
                }
                Act::Restart => {
                    let slot = app.knob_learn.as_ref().unwrap().slot;
                    app.knob_learn = Some(KnobLearn {
                        slot,
                        bind: MidiBind {
                            ty: 0xff,
                            ch: 0,
                            d1: 0,
                        },
                        has_axis: false,
                        axis_locked: false,
                        last_val: None,
                        lo: 0,
                        hi: 127,
                        locked: false,
                    });
                    app.status = format!(
                        "knob learn restarted: {} \u{2014} move the knob",
                        MidiAction::ALL[slot].label()
                    );
                }
            }
        });
}

/// One row of the bindings table: action label + MIDI capture button +
/// keyboard button; persists on change. Both columns work like "press a key":
/// click the cell to arm it, then send the trigger — the next matching input
/// becomes the binding (click again or Esc cancels).
///
/// Continuous (knob) actions bind differently: they need the axis minimum
/// and maximum, so their button opens the two-step learn window instead of
/// grabbing the first message that arrives.
fn bind_row(ui: &mut egui::Ui, app: &mut App, a: MidiAction) {
    let idx = MidiAction::idx(a);
    ui.horizontal(|ui| {
        ui.set_min_width(150.0);
        ui.label(RichText::new(a.label()).weak());

        // MIDI binding: live capture (no dropdown — the controller is the
        // menu). Knobs get the calibrated two-step flow.
        let knob = a.is_continuous();
        let learning_knob = app.knob_learn.as_ref().is_some_and(|k| k.slot == idx);
        let listening = app.midi_learning_slot == Some(idx) && !learning_knob;
        let txt = if learning_knob {
            "learning knob…".to_string()
        } else if listening {
            "listening…".to_string()
        } else {
            match app.midi_binds[idx] {
                // A knob binding owns its AXIS (type + channel) — the data
                // byte is never part of it, so show the axis, not a CC id.
                Some(b) if knob => match app.knob_cal.get(idx).copied().flatten() {
                    Some(c) => format!("{}  [{}/{}]", b.axis_label(), c.lo, c.hi),
                    None => b.axis_label(),
                },
                Some(b) => match app.knob_cal.get(idx).copied().flatten() {
                    // Show the captured raw range next to the axis id.
                    Some(c) => format!("{}  [{}/{}]", b.label(), c.lo, c.hi),
                    None => b.label(),
                },
                None => "—".to_string(),
            }
        };
        let mut btn =
            egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(150.0, 0.0));
        if listening || learning_knob {
            btn = btn.fill(Color32::from_rgb(60, 90, 120));
        }
        let hover = if knob {
            "Click to open the knob wizard: move the control to its minimum, \
             lock it, move to the maximum, finish. Captures the full travel of \
             the knob (min and max must be on the same MIDI channel). \
             Esc cancels; Delete clears the binding and its calibration."
        } else {
            "Click to listen: the next NOTE ON / CC / PROGRAM CHANGE \
             from your controller becomes this binding. Click again (or Esc) \
             to cancel; Delete clears the binding."
        };
        let resp = ui.add(btn).on_hover_text(hover);
        if resp.clicked() {
            if learning_knob {
                app.knob_learn = None;
            } else if knob {
                app.arm_knob_learn(idx);
            } else if listening {
                app.midi_learning_slot = None;
            } else {
                app.arm_midi_slot(idx);
            }
        }

        // Keyboard binding for the same action.
        ui.add_space(4.0);
        let learning = app.learning_slot == Some(idx);
        let txt = if learning {
            "press a key…".to_string()
        } else {
            match app.short_keys[idx] {
                Some(k) => key_display(k),
                None => "—".to_string(),
            }
        };
        let mut btn =
            egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(44.0, 0.0));
        if learning {
            btn = btn.fill(Color32::from_rgb(60, 90, 120));
        }
        if ui.add(btn).clicked() {
            app.arm_key_slot(idx);
        }
    });
}

/// Central dashboard: mode checkbox + the loop track rows.
/// Export prompt (Export button / bound key): pick one loop or the whole
/// project, plus destination file and format. Whole-project export forces
/// dependent mode on before running — it only exists there.
fn export_window(ui: &mut egui::Ui, app: &mut App) {
    // Local copy so the closure can borrow `app` while `.open()` holds the
    // window's open-state; written back after show.
    let mut open = app.show_export;
    let mut cancelled = false;
    let mut done = false;
    egui::Window::new(RichText::new("EXPORT").strong())
        .open(&mut open)
        .min_width(430.0)
        .show(ui, |ui| {
            ui.label(RichText::new("What to export:").strong());
            ui.radio_value(
                &mut app.export_whole,
                false,
                format!("Track {} (selected)", app.selected + 1),
            );
            ui.radio_value(&mut app.export_whole, true, "Whole project");
            if app.export_whole {
                ui.label(
                    RichText::new("Whole project = every sounding track, all lanes, mixer trims, pans and effects.")
                        .weak()
                        .small(),
                );
            }
            ui.add_space(6.0);
            ui.label("Destination:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.export_path).desired_width(290.0));
                if ui.button("Browse…").clicked() {
                    // No extension here — the Format dropdown adds it at
                    // export time (see `do_export`), so changing the format
                    // never means re-picking the destination.
                    let name = if app.export_whole {
                        "slooper-project".to_string()
                    } else {
                        format!("slooper-track-{}", app.selected + 1)
                    };
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("Export destination")
                        .set_file_name(name)
                        .save_file()
                    {
                        app.export_path = p.to_string_lossy().into_owned();
                    }
                }
            });
            ui.label("Format:");
            egui::ComboBox::from_id_salt("export_fmt")
                .selected_text(crate::audio_io::ExportFormat::ALL[app.export_fmt].label())
                .show_ui(ui, |ui| {
                    for (k, f) in crate::audio_io::ExportFormat::ALL.iter().enumerate() {
                        ui.selectable_value(&mut app.export_fmt, k, f.label());
                    }
                });
            if crate::audio_io::ExportFormat::ALL[app.export_fmt].needs_ffmpeg() {
                ui.label(RichText::new("MP3 / OGG encoding needs ffmpeg on PATH.").weak().small());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                if ui.button("Export").clicked() {
                    app.do_export();
                    done = true;
                }
            });
        });
    app.show_export = open && !cancelled && !done;
}

/// The per-loop effects window: folder picker ("effects location"), one
/// row per script with a toggle (disabled by default), reorder arrows for
/// enabled ones (enabled effects snap to the top of the list, in chain
/// order), plus rescan / add-template helpers. Rejected scripts are listed
/// with their compile/load error so effect authors can fix them.
fn effects_window(ui: &mut egui::Ui, app: &mut App) {
    let Some(loop_i) = app.fx_window else { return };
    let mut open = true;
    egui::Window::new(RichText::new(format!("EFFECTS — track {}", loop_i + 1)).strong())
        .open(&mut open)
        .min_width(520.0)
        .default_height(420.0)
        .show(ui, |ui| {
            ui.label("Effects location:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.fx_dir_edit).desired_width(330.0));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("Effects folder").pick_folder()
                    {
                        app.fx_dir_edit = p.to_string_lossy().into_owned();
                        app.apply_fx_dir();
                    }
                }
                if ui.button("Apply").clicked() {
                    app.apply_fx_dir();
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Rescan").clicked() {
                    app.rescan_fx();
                }
                if ui.button("Add template").clicked() {
                    match app.fx_registry.add_template() {
                        Ok(stem) => {
                            let stems = app.fx_registry.stems();
                            for i in 0..MAX_LOOPS {
                                app.fx_entries[i] =
                                    fx::merge_entries(&app.fx_entries[i], &stems, &[]);
                                app.rebuild_fx(i);
                            }
                            app.fx_note =
                                format!("created {stem}.rs — enable it below to hear it");
                        }
                        Err(e) => app.fx_note = e,
                    }
                }
            });
            if !fx::rustc_available() {
                ui.label(
                    RichText::new(
                        "rustc is not on PATH — effect scripts cannot compile. Install Rust (rustup).",
                    )
                    .color(Color32::YELLOW),
                );
            }
            if !app.fx_note.is_empty() {
                ui.label(RichText::new(&app.fx_note).weak().small());
            }
            ui.separator();

            // One row per script: valid ones get a toggle (+ arrows when
            // enabled); invalid ones show the rejection reason instead.
            let mut act: Option<(u8, usize)> = None; // (0 toggle, 1 up, 2 down)
            for (k, e) in app.fx_entries[loop_i].clone().iter().enumerate() {
                let desc = app.fx_registry.desc(&e.stem).cloned();
                let err = app
                    .fx_registry
                    .items()
                    .iter()
                    .find(|it| it.stem == e.stem)
                    .and_then(|it| it.error.clone());
                ui.horizontal(|ui| {
                    let mut en = e.enabled;
                    let cb = ui.checkbox(&mut en, "");
                    if cb.changed() {
                        act = Some((0, k));
                    }
                    let label = desc.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| e.stem.clone());
                    let txt = if e.enabled && desc.is_some() {
                        RichText::new(&label).strong()
                    } else {
                        RichText::new(&label)
                    };
                    let hover = match (&desc, &err) {
                        (Some(d), _) => {
                            let mut h = String::new();
                            if let Some(a) = &d.author {
                                h.push_str(&format!("by {a}\n"));
                            }
                            if let Some(t) = &d.description {
                                h.push_str(t);
                            }
                            if d.params.is_empty() {
                                h.push_str("\n(no parameters)");
                            } else {
                                h.push_str(&format!("\nparams: {}", d.params.len()));
                            }
                            h
                        }
                        (None, Some(t)) => t.clone(),
                        (None, None) => label.clone(),
                    };
                    ui.add_enabled_ui(desc.is_some(), |ui| {
                        ui.label(txt).on_hover_text(hover);
                        if e.enabled {
                            if arrow_button(ui, true).clicked() {
                                act = Some((1, k));
                            }
                            if arrow_button(ui, false).clicked() {
                                act = Some((2, k));
                            }
                        }
                    });
                    if let Some(t) = &err {
                        let short: String = t.chars().take(72).collect();
                        ui.label(RichText::new(format!("× {short}")).color(Color32::from_rgb(220, 120, 120)).small());
                    }
                });
            }
            // Scripts that failed to compile or load: dimmed rows with the
            // rejection reason, so an effect author can fix the script in
            // place and Rescan.
            for it in app.fx_registry.items().iter().filter(|it| it.desc.is_none()) {
                ui.horizontal(|ui| {
                    let mut dim = false;
                    ui.add_enabled_ui(false, |ui| {
                        ui.checkbox(&mut dim, "");
                        ui.label(RichText::new(&it.stem).weak().monospace());
                    });
                    let t = it.error.clone().unwrap_or_else(|| "invalid".into());
                    let short: String = t.chars().take(140).collect();
                    ui.label(
                        RichText::new(format!("× {}", short.replace('\n', " · ")))
                            .color(Color32::from_rgb(220, 120, 120))
                            .small(),
                    )
                    .on_hover_text(t);
                });
            }
            if let Some((m, k)) = act {
                let changed = if m == 0 {
                    app.fx_entries[loop_i][k].enabled ^= true;
                    let stems = app.fx_registry.stems();
                    fx::resort_entries(&mut app.fx_entries[loop_i], &stems);
                    true
                } else {
                    fx::move_enabled(&mut app.fx_entries[loop_i], k, m == 1)
                };
                if changed {
                    app.rebuild_fx(loop_i);
                    app.save_config();
                }
            }
            ui.separator();
            ui.label(
                RichText::new(
                    "Enabled effects run in list order (top first) on this loop's playback, \
                     after gain and volume. Drop a Rust script into the folder above, \
                     press Rescan, and toggle it — see template.rs for the contract.",
                )
                .weak()
                .small(),
            );
        });
    app.fx_window = if open { Some(loop_i) } else { None };
}

/// Small ↑/↓ reorder button with a DRAWN arrow. egui's default font stack
/// (Ubuntu-Light/NotoEmoji/emoji-icon-font) has no arrow or triangle glyphs
/// at all — "▲"/"▼" (and even "↑"/"↓") render as hollow tofu boxes — so the
/// icon is vector-painted into the button rect instead.
fn arrow_button(ui: &mut egui::Ui, up: bool) -> egui::Response {
    let resp = ui
        .add(egui::Button::new(" ").min_size(egui::vec2(20.0, 16.0)))
        .on_hover_text(if up {
            "Move earlier in the chain"
        } else {
            "Move later in the chain"
        });
    let c = resp.rect.center();
    let col = if resp.hovered() {
        ui.visuals().widgets.active.fg_stroke.color
    } else {
        ui.visuals().widgets.noninteractive.fg_stroke.color
    };
    let s = egui::Stroke::new(1.4, col);
    let p = ui.painter();
    // shaft
    p.line_segment([egui::pos2(c.x, c.y - 4.0), egui::pos2(c.x, c.y + 4.5)], s);
    // head (tip up or down)
    let tip = if up { c.y - 4.5 } else { c.y + 4.5 };
    let jaw = if up { tip + 3.5 } else { tip - 3.5 };
    p.line_segment([egui::pos2(c.x, tip), egui::pos2(c.x - 3.5, jaw)], s);
    p.line_segment([egui::pos2(c.x, tip), egui::pos2(c.x + 3.5, jaw)], s);
    resp
}

fn port_list(
    ui: &mut egui::Ui,
    title: &str,
    hint: &str,
    avail: &[PortInfo],
    sel: &mut HashSet<String>,
) {
    ui.label(RichText::new(format!("{}  ({})", title, avail.len())).strong());
    ui.label(RichText::new(hint).weak().small());
    if avail.is_empty() {
        ui.label(RichText::new("no remote audio ports found").weak());
        return;
    }
    let mut last_client = String::new();
    for p in avail {
        if p.client != last_client {
            last_client = p.client.clone();
            ui.add_space(4.0);
            ui.label(RichText::new(format!("▸ {}", p.client)).strong());
        }
        let short = p.name.split(':').nth(1).unwrap_or(&p.name);
        let mut checked = sel.contains(&p.name);
        if ui.checkbox(&mut checked, short).clicked() {
            if checked {
                sel.insert(p.name.clone());
            } else {
                sel.remove(&p.name);
            }
        }
        ui.label(
            RichText::new(format!("  {}", p.name))
                .weak()
                .small()
                .monospace(),
        );
    }
}

/// Like [`port_list`], but each output row carries two checkboxes — the
/// left and right side of the stereo mix the port plays. Both ticked =
/// mono downmix `(L + R)/sqrt2` (the classic behaviour); one ticked routes
/// only that side. Unticking both deselects the port.
fn port_list_lr(
    ui: &mut egui::Ui,
    title: &str,
    hint: &str,
    avail: &[PortInfo],
    sel: &mut HashMap<String, u8>,
) {
    ui.label(RichText::new(format!("{}  ({})", title, avail.len())).strong());
    ui.label(RichText::new(hint).weak().small());
    if avail.is_empty() {
        ui.label(RichText::new("no remote audio ports found").weak());
        return;
    }
    let mut last_client = String::new();
    for p in avail {
        if p.client != last_client {
            last_client = p.client.clone();
            ui.add_space(4.0);
            ui.label(RichText::new(format!("▸ {}", p.client)).strong());
        }
        let short = p.name.split(':').nth(1).unwrap_or(&p.name);
        let entry = sel.get(&p.name).copied().unwrap_or(0u8);
        let (mut l, mut r) = (entry & 1 != 0, entry & 2 != 0);
        ui.horizontal(|ui| {
            let rl = ui.checkbox(&mut l, "L").on_hover_text("play the LEFT side");
            let rr = ui
                .checkbox(&mut r, "R")
                .on_hover_text("play the RIGHT side");
            ui.label(RichText::new(short).strong().small());
            ui.label(
                RichText::new(format!("  {}", p.name))
                    .weak()
                    .small()
                    .monospace(),
            );
            if rl.changed() || rr.changed() {
                let bits = (l as u8) | ((r as u8) << 1);
                if bits == 0 {
                    sel.remove(&p.name);
                } else {
                    sel.insert(p.name.clone(), bits);
                }
            }
        });
    }
}

fn midi_monitor(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::bottom("midi")
        .resizable(true)
        .min_size(140.0)
        .default_size(220.0)
        .show(ui, |ui| {
            ui.label(RichText::new("MIDI MONITOR").strong());
            ui.label(
                RichText::new("last detected MIDI input (also printed to stdout)")
                    .weak()
                    .small(),
            );

            if let Some(last) = &app.last_midi {
                ui.add_space(4.0);
                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(35, 40, 50))
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("[{}]", last.ts)).weak().monospace());
                            ui.label(RichText::new(last.type_name).strong().monospace());
                            ui.label(RichText::new(&last.describe).monospace());
                            ui.separator();
                            ui.label(
                                RichText::new(format!("| {}", last.raw_hex))
                                    .weak()
                                    .monospace(),
                            );
                        });
                    });
            } else {
                ui.add_space(4.0);
                ui.label(RichText::new("waiting for MIDI input…").weak());
                ui.label(
                    RichText::new(
                        "the audio thread reads the JACK port slooper:midi_in and \
                         listens on every channel — link your device to it (e.g. \
                         pw-link 'your-midi-port' 'slooper:midi_in') and send a \
                         message; new source ports appear below automatically",
                    )
                    .weak()
                    .small(),
                );
            }

            if !app.midi_sources.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("sources linked to slooper:midi_in")
                        .weak()
                        .small(),
                );
                for s in &app.midi_sources {
                    ui.label(RichText::new(format!("  {s}")).weak().small().monospace());
                }
            }

            egui::ScrollArea::vertical()
                .min_scrolled_height(f32::INFINITY)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    // Newest first (log is push_front).
                    for line in app.midi_log.iter() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("[{}]", line.ts)).weak().monospace());
                            ui.label(RichText::new(line.type_name).monospace());
                            ui.label(RichText::new(&line.describe).weak().monospace());
                            ui.label(
                                RichText::new(format!("| {}", line.raw_hex))
                                    .weak()
                                    .monospace(),
                            );
                        });
                    }
                });
        });
}

fn settings_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::left("settings").resizable(true).default_size(410.0).min_size(330.0).show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(RichText::new("SETTINGS").strong());

            // ---- GUI -----------------------------------------------------
            ui.separator();
            ui.label(RichText::new("GUI").strong());
            ui.horizontal(|ui| {
                let r = ui.add(egui::Slider::new(&mut app.gui_scale, 0.7..=1.6).suffix("×"));
                if r.changed() {
                    app.save_config();
                }
            });
            ui.label(RichText::new("Scales the whole interface; applied live.").weak().small());

            // ---- latency compensation -------------------------------------
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("latency (ms):");
                ui.add(egui::TextEdit::singleline(&mut app.latency_ms).desired_width(60.0));
                if ui.button("Apply").clicked() {
                    let ms = app.latency_ms.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1000.0);
                    app.latency_ms = format!("{ms}");
                    app.backend.cmd(Cmd::SetLatency { ms });
                    app.save_config();
                    app.status = format!("latency compensation: {ms} ms");
                }
            });
            ui.label(
                RichText::new(
                    "Each fresh recording discards its first this-many milliseconds of \
                     input, trimming the silence gap between pressing REC and that button \
                     click reaching your mic. Set it to your system's capture latency and \
                     the take begins with the hit, not with dead air. Monitoring and the \
                     output mix are never delayed.",
                )
                    .weak()
                    .small(),
            );

            // ---- asset preview --------------------------------------------
            ui.separator();
            ui.label(RichText::new("ASSET PREVIEW").strong());
            if ui
                .checkbox(&mut app.preview_enabled, "audition rack entries on click")
                .on_hover_text("Clicking an asset-rack entry plays it at the master output (never recorded). Click the entry again to stop.")
                .changed()
            {
                app.save_config();
            }
            ui.horizontal(|ui| {
                ui.label("preview length (s):");
                let prt = ui
                    .add(egui::TextEdit::singleline(&mut app.preview_secs).desired_width(60.0))
                    .on_hover_text("Seconds of audio to audition: shorter entries play in full, longer ones are cut after this many seconds.");
                if prt.lost_focus() {
                    let v = app.preview_secs_f();
                    app.preview_secs = v.to_string();
                    app.save_config();
                }
            });
            ui.label(
                RichText::new(
                    "Entries at most this long play in full; longer ones are auditioned \
                     from the start for exactly this many seconds.",
                )
                .weak()
                .small(),
            );

            // ---- projects --------------------------------------------------
            ui.separator();
            ui.label(RichText::new("PROJECTS").strong());
            ui.horizontal(|ui| {
                ui.label("folder:");
                if ui
                    .add(egui::TextEdit::singleline(&mut app.project_dir).desired_width(220.0))
                    .lost_focus()
                {
                    app.save_config();
                }
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("Projects folder").pick_folder()
                    {
                        app.project_dir = p.to_string_lossy().into_owned();
                        app.save_config();
                    }
                }
            });
            ui.label(
                RichText::new(format!(
                    "Projects ({} files) save into this folder: “{}” — empty means ~/slooper-projects. Each project keeps its audio next to it in “<name> audio”.",
                    project::EXT,
                    app.projects_dir().display()
                ))
                .weak()
                .small(),
            );

            // ---- MIDI + keyboard bindings -----------------------------------
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("MIDI + KEYBOARD BINDINGS").strong());
                if ui.button("Reset to Defaults").clicked() {
                    for (k, d) in app.short_keys.iter_mut().zip(SHORTCUT_DEFAULTS.iter()) {
                        *k = *d;
                    }
                    for m in app.midi_binds.iter_mut() {
                        *m = None;
                    }
                    app.save_config();
                    app.status = "bindings reset to defaults".into();
                }
            });
            ui.label(
                RichText::new(
                    "Click a MIDI cell, then send a NOTE ON / CC / PROGRAM \
                     CHANGE from your controller — the first matching message \
                     becomes that binding (click again or Esc to cancel; while \
                     a MIDI cell listens, Delete clears its binding and no \
                     other key does anything). A bound action fires on every \
                     matching message (value/velocity ignored). The key column \
                     works the same way: click, then press the new key (Esc \
                     cancels); Delete clears that cell's binding and is never \
                     bindable itself. Keys are ignored while a text field has \
                     focus.",
                )
                .weak()
                .small(),
            );
            ui.label(RichText::new("global").weak().small());
            for a in [
                MidiAction::RecSelected,
                MidiAction::ClearSelected,
                MidiAction::PlaySelected,
                MidiAction::PlayAll,
                MidiAction::StopAll,
                MidiAction::Export,
                MidiAction::Metronome,
                MidiAction::SelectNext,
                MidiAction::SelectPrev,
            ] {
                bind_row(ui, app, a);
            }
            ui.label(RichText::new("per track bindings").weak().small());
            ui.horizontal(|ui| {
                ui.label("Track");
                egui::ComboBox::from_id_salt("bind_loop_sel")
                    .selected_text(format!("{}", app.bind_loop_sel + 1))
                    .show_ui(ui, |ui| {
                        for n in 0..MAX_LOOPS {
                            ui.selectable_value(&mut app.bind_loop_sel, n, format!("{}", n + 1));
                        }
                    });
            });
            for off in 0..7usize {
                let a = match off {
                    0 => MidiAction::RecLoop(app.bind_loop_sel),
                    1 => MidiAction::ClearLoop(app.bind_loop_sel),
                    2 => MidiAction::PlayLoop(app.bind_loop_sel),
                    3 => MidiAction::Select(app.bind_loop_sel),
                    4 => MidiAction::VolLoop(app.bind_loop_sel),
                    5 => MidiAction::GainLoop(app.bind_loop_sel),
                    _ => MidiAction::PanLoop(app.bind_loop_sel),
                };
                bind_row(ui, app, a);
            }

            // ---- audio ports -------------------------------------------------
            ui.separator();
            ui.label(RichText::new("AUDIO PORTS").strong());
            port_list(ui, "INPUTS", "remote output ports (sources)", &app.avail_in, &mut app.sel_in);
            ui.add_space(8.0);
            port_list_lr(
                ui,
                "OUTPUTS",
                "remote input ports (sinks) — tick L and/or R for what each port plays",
                &app.avail_out,
                &mut app.sel_out,
            );
            ui.horizontal(|ui| {
                if ui.button("Apply Selection").clicked() {
                    // Sort for deterministic slot assignment, so the same
                    // selection maps to the same slots across restarts.
                    let mut ins: Vec<String> = app.sel_in.iter().cloned().collect();
                    ins.sort();
                    let mut outs: Vec<OutSel> = app
                        .sel_out
                        .iter()
                        .filter(|(_, b)| **b != 0)
                        .map(|(n, b)| (n.clone(), *b))
                        .collect();
                    outs.sort();
                    app.applied_ports = Some((ins.clone(), outs.clone()));
                    app.backend.cmd(Cmd::Apply { ins, outs });
                    app.save_config();
                }
                if ui.button("Stop").clicked() {
                    app.backend.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
                }
                if ui.button("Refresh").clicked() {
                    app.backend.cmd(Cmd::Refresh);
                }
            });
        });
    });
}

/// First-save (or Save As) name prompt.
fn save_prompt_window(ui: &mut egui::Ui, app: &mut App) {
    if !app.show_save_prompt {
        return;
    }
    let mut open = true;
    let dir = app.projects_dir();
    let mut do_save = false;
    let mut enter_hit = false;
    egui::Window::new(RichText::new("SAVE PROJECT").strong())
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ui, |ui| {
            ui.set_width(360.0);
            ui.label("Project name:");
            let re = ui.add(egui::TextEdit::singleline(&mut app.save_draft).desired_width(320.0));
            enter_hit = re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.label(
                RichText::new(format!("→ {}/<name>.{}", dir.display(), project::EXT))
                    .weak()
                    .small(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    do_save = true;
                }
                if ui.button("Cancel").clicked() {
                    app.show_save_prompt = false;
                }
            });
        });
    if do_save || enter_hit {
        let n = app.save_draft.clone();
        app.do_save(&n);
    }
    if !open {
        app.show_save_prompt = false;
    }
}

/// Browse the projects folder; one button per saved project.
fn load_window(ui: &mut egui::Ui, app: &mut App) {
    if !app.show_load {
        return;
    }
    let mut open = true;
    let mut load: Option<PathBuf> = None;
    egui::Window::new(RichText::new("LOAD PROJECT").strong())
        .open(&mut open)
        .default_width(420.0)
        .show(ui, |ui| {
            let dir = app.projects_dir();
            ui.label(RichText::new(format!("{}", dir.display())).weak().small());
            let list = project::list_projects(&dir);
            if list.is_empty() {
                ui.label("No projects here yet — Ctrl+S saves the first one.");
            }
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .show(ui, |ui| {
                    for (name, path) in &list {
                        let cur = *name == app.proj_name;
                        let b = ui
                            .add_sized(
                                [390.0, 24.0],
                                egui::Button::new(RichText::new(name).strong()),
                            )
                            .on_hover_text(path.display().to_string());
                        if cur {
                            ui.label(RichText::new("current project").weak().small());
                        }
                        if b.clicked() {
                            load = Some(path.clone());
                        }
                    }
                });
        });
    if let Some(p) = load {
        app.do_load(&p);
    }
    app.show_load = open;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_table_matches_actions() {
        assert_eq!(SHORTCUT_DEFAULTS.len(), N_ACTIONS);
        assert_eq!(N_ACTIONS, 121);
        // Space is the global transport toggle; knob slots are never keyed.
        assert_eq!(
            SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::PlayAll)],
            Some(egui::Key::Space)
        );
        assert!(SHORTCUT_DEFAULTS[73..].iter().all(|k| k.is_none()));
        // Every default key is unique (learn mode enforces the same).
        let mut seen: Vec<egui::Key> = SHORTCUT_DEFAULTS.iter().flatten().copied().collect();
        seen.sort_by_key(|k| format!("{k:?}"));
        let n = seen.len();
        seen.dedup_by_key(|k| format!("{k:?}"));
        assert_eq!(seen.len(), n);
    }

    // ---- headless window tests (real widget tree, synthetic pointer) ----

    /// Fresh App with null audio, a scratch config dir, and one rack folder
    /// holding a generated kick.wav. Returns the app and the file's y-row
    /// finder left to the caller (layout is deterministic at px 1.0).
    fn headless_app(tag: &str) -> App {
        std::env::set_var("SLOOPER_NULL_AUDIO", "1");
        let dir = std::env::temp_dir().join(format!("sp-uity-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("slooper")).unwrap();
        std::fs::create_dir_all(dir.join("slooper/effects")).unwrap();
        let samples = dir.join("samples");
        std::fs::create_dir_all(&samples).unwrap();
        let data: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        crate::audio_io::export(
            &samples.join("kick.wav"),
            48000,
            &data,
            crate::audio_io::ExportFormat::Wav,
        )
        .unwrap();
        std::fs::write(
            dir.join("slooper/config.json"),
            format!(r#"{{"rack_folders":["{}"]}}"#, samples.display()),
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let mut app = App::new(Backend::start());
        app.drain_events(); // discard the null-audio notice
        app.sample_rate = 48_000; // null audio never sends Evt::Ready
        app
    }

    /// One UI frame with the given pointer events.
    fn frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
    }

    fn moved(p: egui::Pos2) -> egui::Event {
        egui::Event::PointerMoved(p)
    }
    fn btn(p: egui::Pos2, b: egui::PointerButton, down: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: p,
            button: b,
            pressed: down,
            modifiers: Default::default(),
        }
    }

    /// SETTINGS must dock to the LEFT of the asset rack (add-order test:
    /// both panels are left panels; whichever is added first sits outermost,
    /// so its heading text renders further left).
    #[test]
    fn settings_panel_pops_up_left_of_the_rack() {
        let mut app = headless_app("settings");
        let ctx = egui::Context::default();
        app.show_settings = true;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        // Read headings from the pre-tessellate shapes (0.36 tessellates
        // text away into callbacks, but the Shape list still carries galleys).
        let mut x_settings = f32::MAX;
        let mut x_rack = f32::MAX;
        for cs in &out.shapes {
            if let egui::epaint::Shape::Text(t) = &cs.shape {
                let txt = t.galley.text();
                if txt.contains("SETTINGS") {
                    x_settings = x_settings.min(t.pos.x);
                }
                if txt.contains("ASSET RACK") {
                    x_rack = x_rack.min(t.pos.x);
                }
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        assert!(
            x_settings < f32::MAX && x_rack < f32::MAX,
            "both headings painted"
        );
        assert!(
            x_settings < x_rack,
            "SETTINGS x={x_settings} must be left of ASSET RACK x={x_rack}"
        );
    }

    /// The full user gesture: press a rack row, drag onto a track's lane
    /// strip, release -> a clip lands on that track (and Delete-selection
    /// is set). Then a right-click on the track header clears it again.
    /// Drives the real egui DnD plugin end to end — this is the regression
    /// test for "dragging from the rack dropped nothing".
    #[test]
    fn rack_drag_drops_a_clip_and_header_rmb_clears() {
        let mut app = headless_app("drop");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]); // layout pass

        // Click-probe the rack rows to locate kick.wav deterministically.
        let mut y_row = None;
        for y in (96..220).step_by(3) {
            let p = egui::pos2(40.0, y as f32);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            if app.sel_asset.is_some() {
                y_row = Some(y as f32);
                break;
            }
        }
        let y_row = y_row.expect("a rack row was clickable (kick.wav)");

        // Drag: press, move past the drag threshold, hover the lane strip of
        // track 1, release there (well inside the lane: the separation bar
        // under track 0 owns a few pixels above it now).
        let start = egui::pos2(40.0, y_row);
        let lane = egui::pos2(500.0, 240.0);
        frame(
            &mut app,
            &ctx,
            vec![moved(start), btn(start, egui::PointerButton::Primary, true)],
        );
        frame(&mut app, &ctx, vec![moved(egui::pos2(120.0, y_row + 6.0))]);
        frame(&mut app, &ctx, vec![moved(egui::pos2(320.0, 170.0))]);
        frame(&mut app, &ctx, vec![moved(lane)]);
        assert!(
            egui::DragAndDrop::has_payload_of_type::<usize>(&ctx),
            "drag carries the asset"
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(lane, egui::PointerButton::Primary, false)],
        );

        let total = |a: &App| -> usize {
            a.arr
                .tracks
                .iter()
                .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
                .sum()
        };
        assert_eq!(total(&app), 1, "drag-drop placed exactly one clip");
        assert!(app.sel_clip.is_some(), "the dropped clip is selected");
        assert_eq!(app.status, "clip placed on track 2 \u{b7} track");

        // Right-click the Track-1 header (probe the small header band): it
        // must clear the clip.
        let mut cleared = false;
        // Right-click now lives on the track's grid strip (headers moved to
        // the mixer): sweep the drop lane's vertical neighbourhood.
        for y in (214..=300).step_by(3) {
            let p = egui::pos2(500.0, y as f32);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Secondary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Secondary, false)],
            );
            if total(&app) == 0 {
                cleared = true;
                break;
            }
        }
        assert!(cleared, "right-click on the track header cleared its clips");
    }

    #[test]
    fn loop_region_follows_mode() {
        let mut app = headless_app("loopmode");
        // 120 bpm, 4/4 at 48 kHz -> one bar = 96_000 samples.
        app.metro_bpm = "120".into();
        app.metro_num = "4".into();
        assert_eq!(app.bar_samples(), 96_000);
        app.loop_mode = 0;
        app.loop_area = (0, 0);
        assert_eq!(app.loop_region_eff(), (0, 192_000), "area default = 2 bars");
        app.loop_area = (24_000, 96_000);
        assert_eq!(
            app.loop_region_eff(),
            (24_000, 96_000),
            "dragged handles win"
        );
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        let len = app.asset_len(a);
        app.loop_mode = 1;
        app.arr.insert(
            0,
            0,
            Clip {
                asset: a,
                pos: 48_000,
            },
        );
        assert_eq!(
            app.loop_region_eff(),
            (0, 48_000 + len),
            "longest = last clip end"
        );
        app.loop_mode = 2;
        assert_eq!(app.loop_region_eff(), (0, app.total_len()));
    }

    #[test]
    fn track_height_grows_with_lanes() {
        let mut app = headless_app("trackh");
        assert_eq!(app.track_h(0), 96.0, "one lane = the mixer minimum");
        app.arr.add_lane(0);
        assert_eq!(app.track_h(0), 96.0, "two lanes still fit the minimum");
        app.arr.add_lane(0);
        assert_eq!(app.track_h(0), 144.0, "three lanes extend the track");
    }

    #[test]
    fn key_names_roundtrip() {
        for k in [
            egui::Key::A,
            egui::Key::Num3,
            egui::Key::F7,
            egui::Key::Space,
            egui::Key::M,
        ] {
            assert_eq!(key_from_name(&key_debug(k)), Some(k), "roundtrip {k:?}");
        }
    }

    #[test]
    fn level_text_roundtrip() {
        // Volume spans -inf..=+6 dB: 0 = -inf, VOL_UNITY_MIDI = 0 dB,
        // 127 = +6 dB, and every dB text parses back to the same knob.
        assert_eq!(db_num(vol_amp(0)), "-inf");
        assert_eq!(level_display(KIND_VOL, VOL_UNITY_MIDI), "+0.0");
        assert_eq!(level_display(KIND_VOL, 127), "+6.0");
        assert_eq!(level_parse(KIND_VOL, "+0.0"), Some(VOL_UNITY_MIDI));
        assert_eq!(level_parse(KIND_VOL, "+6.0"), Some(127));
        assert_eq!(level_parse(KIND_VOL, "-inf"), Some(0));
        assert_eq!(level_parse(KIND_VOL, "-12.3"), Some(vol_midi(-12.3)));
        // Monotone over the whole range, and unity sits at 0 dB exactly.
        for v in 1..127u8 {
            assert!(vol_amp(v) < vol_amp(v + 1), "vol_amp not monotone at {v}");
        }
        assert!((20.0 * vol_amp(VOL_UNITY_MIDI).log10()).abs() < 0.02);
        // Pan: centre reads 0.
        let s = level_display(KIND_PAN, 64);
        assert_eq!(s, "0");
        assert_eq!(level_parse(KIND_PAN, &s), Some(64));
    }

    #[test]
    fn grid_units_follow_subdivision() {
        // 120 BPM: quarter = 2× eighth; snap unit ordering is monotonic.
        let (sr, bpm) = (48_000, 120);
        assert!(quantize::unit_samples(sr, bpm, 1) > quantize::unit_samples(sr, bpm, 2));
        assert!(quantize::unit_samples(sr, bpm, 2) > quantize::unit_samples(sr, bpm, 3));
    }

    #[test]
    fn projects_dir_defaults_to_home() {
        let cfg = Config::default();
        assert!(cfg.projects_dir().ends_with("slooper-projects"));
    }

    // ---- track resizing, lane growth, wheel routing ----

    #[test]
    fn separator_resize_is_per_track_and_clamped() {
        let mut app = headless_app("resize");
        assert_eq!(app.track_h(0), TRACK_MIN_H, "automatic height");
        app.set_track_h(0, 200.0);
        assert_eq!(app.track_h(0), 200.0);
        assert_eq!(
            app.track_h(1),
            TRACK_MIN_H,
            "a sibling keeps its own height"
        );
        app.set_track_h(0, 10.0);
        assert_eq!(app.track_h(0), TRACK_MIN_H, "clamped to the lane floor");
    }

    #[test]
    fn new_lane_duplicates_a_lanes_height() {
        // The child lane must not steal room from the parent: the track
        // grows by one old-lane height so every lane keeps its size.
        let mut app = headless_app("grow");
        app.set_track_h(0, 200.0);
        app.arr.tracks[0].add_lane();
        app.grow_for_lanes(0, 1);
        assert_eq!(app.track_h(0), 400.0, "two lanes at the old lane height");
        // One number for the family: the lanes share whatever the user sets.
        app.set_track_h(0, 120.0);
        assert_eq!(app.track_h(0), 120.0);
        // ... but never below the track floor (two squeeze-able lanes
        // still can't shrink the block under TRACK_MIN_H).
        app.set_track_h(0, 40.0);
        assert_eq!(app.track_h(0), TRACK_MIN_H);
    }

    #[test]
    fn wheel_modifiers_zoom_and_pan() {
        // Ctrl+wheel zooms the timeline, Shift+wheel pans it horizontally —
        // the regression test for "ctrl/shift + scrollwheel does nothing".
        let mut app = headless_app("wheel");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let z0 = app.px_per_sec;
        let wheel = |dy: f32, m: egui::Modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: m,
        };
        frame(&mut app, &ctx, vec![wheel(3.0, egui::Modifiers::COMMAND)]);
        assert!(app.px_per_sec > z0, "ctrl+wheel zooms in");
        frame(&mut app, &ctx, vec![wheel(-3.0, egui::Modifiers::COMMAND)]);
        assert!((app.px_per_sec - z0).abs() < 1.0, "and back out again");
        // Zoom far in first: horizontal scroll is clamped to the content
        // width, and an empty timeline is narrower than the viewport.
        app.px_per_sec = 1200.0;
        frame(&mut app, &ctx, vec![wheel(-2.0, egui::Modifiers::SHIFT)]);
        assert!(app.hscroll > 0.0, "shift+wheel pans the timeline");
        // A plain wheel must NOT zoom (ScrollAreas keep vertical scrolling).
        let z1 = app.px_per_sec;
        frame(&mut app, &ctx, vec![wheel(5.0, egui::Modifiers::NONE)]);
        assert_eq!(app.px_per_sec, z1, "plain wheel leaves zoom alone");
    }

    // ---- regression pins: zoom range, sensitivity & centred default ----

    #[test]
    fn zoom_is_fast_falls_far_and_centers_default() {
        let mut app = headless_app("zoom");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let wheel = |dy: f32, m: egui::Modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: m,
        };
        // One notch must move the view visibly (~12 %, was 0.6 % — the
        // "ctrl+wheel is too slow" complaint).
        let z0 = app.px_per_sec;
        frame(&mut app, &ctx, vec![wheel(1.0, egui::Modifiers::COMMAND)]);
        assert!(
            app.px_per_sec >= z0 * 1.08,
            "one ctrl+wheel notch zoomed only {:.1} %",
            (app.px_per_sec / z0 - 1.0) * 100.0
        );
        // One notch out lands back (log-space accumulation is symmetric).
        frame(&mut app, &ctx, vec![wheel(-1.0, egui::Modifiers::COMMAND)]);
        assert!(
            (app.px_per_sec - z0).abs() < 0.5,
            "notch in then out cancels"
        );
        // Max zoom-out shows at least 100 measures in the ~936 pt viewport
        // of the 1440×853 window at 80 BPM 4/4 (3 s per bar — the exact
        // geometry the report quoted: 13 bars at the old 24 px/s floor).
        let bar_s = 60.0 * 4.0 / 80.0;
        let viewport_pt = 936.0;
        let bars = viewport_pt / (ZOOM_MIN * bar_s);
        assert!(bars >= 100.0, "ZOOM_MIN shows only {bars:.0} bars");
        // Wheeling all the way out lands exactly on the floor.
        app.px_per_sec = ZOOM_DEFAULT;
        for _ in 0..60 {
            frame(&mut app, &ctx, vec![wheel(-3.0, egui::Modifiers::COMMAND)]);
        }
        assert_eq!(app.px_per_sec, ZOOM_MIN);
        // The logarithmic slider keeps the untouched default near centre.
        let t = (ZOOM_DEFAULT.ln() - ZOOM_MIN.ln()) / (ZOOM_MAX.ln() - ZOOM_MIN.ln());
        assert!(
            (0.45..=0.75).contains(&t),
            "default sits at {t:.2} of the slider"
        );
    }

    // ---- regression pins: metronome editor + start/stop button ----

    /// Center of the first painted glyph galley exactly `want` (top-level
    /// draw order: the top bar paints before every dock).
    fn label_center(app: &mut App, ctx: &egui::Context, want: &str) -> Option<egui::Pos2> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let mut c = None;
        for cs in &out.shapes {
            if let egui::epaint::Shape::Text(t) = &cs.shape {
                if t.galley.text() == want {
                    c = Some(t.pos + t.galley.rect.center().to_vec2());
                    break;
                }
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        c
    }

    fn key_ev(key: egui::Key, m: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: m,
        }
    }

    /// Click, select-all, replace with `value`, Enter. `find` answers
    /// whether the click landed on the widget under test after typing "7".
    fn type_into_focused(app: &mut App, ctx: &egui::Context, value: &str) {
        frame(app, ctx, vec![key_ev(egui::Key::Home, Default::default())]);
        frame(
            app,
            ctx,
            vec![key_ev(egui::Key::End, egui::Modifiers::SHIFT)],
        );
        for c in value.chars() {
            frame(app, ctx, vec![egui::Event::Text(c.to_string())]);
        }
        // Enter: single-line TextEdits commit by losing focus (egui 0.36).
        frame(app, ctx, vec![key_ev(egui::Key::Enter, Default::default())]);
    }

    /// The regression test for "bpm and beats/measure can never be edited
    /// — the box always snaps back to 120/4": typing must stick to the app
    /// state, Enter commits, and the saved config follows.
    #[test]
    fn metro_tempo_boxes_are_editable_and_commit() {
        let mut app = headless_app("metroedit");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);

        // Find the bpm box: click-scan the top bar, type a digit, and the
        // app field that moved says which box got focus.
        let mut found = None;
        for x in (160..1000).step_by(4) {
            let p = egui::pos2(x as f32, 22.0);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            let before = (app.metro_bpm.clone(), app.metro_num.clone());
            frame(&mut app, &ctx, vec![egui::Event::Text("7".into())]);
            if app.metro_bpm != before.0 {
                found = Some(("bpm", x));
                break;
            }
            app.metro_bpm = before.0;
            app.metro_num = before.1;
        }
        let (_, x_bpm) = found.expect("a click in the top bar focuses the bpm box");
        type_into_focused(&mut app, &ctx, "96");
        assert_eq!(app.metro_bpm, "96", "typed bpm survives the frame loop");

        // Same trick for the beats box, scanning right of the bpm box.
        let mut found = None;
        for x in (x_bpm + 10..1040).step_by(4) {
            let p = egui::pos2(x as f32, 22.0);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            let before = (app.metro_bpm.clone(), app.metro_num.clone());
            frame(&mut app, &ctx, vec![egui::Event::Text("7".into())]);
            if app.metro_num != before.1 {
                found = Some(x);
                break;
            }
            app.metro_bpm = before.0;
            app.metro_num = before.1;
        }
        assert!(found.is_some(), "a click focuses the beats box");
        type_into_focused(&mut app, &ctx, "3");
        assert_eq!(app.metro_num, "3", "typed beats-per-measure survives");

        // Commit landed in the config (send_metro saves on commit).
        let cfg_txt = std::fs::read_to_string(Config::path()).unwrap();
        assert!(cfg_txt.contains("\"metro_bpm\": 96"), "config: {cfg_txt}");
        assert!(cfg_txt.contains("\"metro_num\": 3"), "config: {cfg_txt}");
    }

    /// The metro checkbox became a start/stop button: clicking it toggles
    /// the click and relabels (METRO → STOP → METRO).
    #[test]
    fn metro_button_starts_and_stops() {
        let mut app = headless_app("metrobtn");
        let ctx = egui::Context::default();
        let p = label_center(&mut app, &ctx, "METRO").expect("METRO button painted");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert!(app.metro_on, "click starts the click track");
        let p = label_center(&mut app, &ctx, "STOP").expect("running button reads STOP");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert!(!app.metro_on, "clicking again stops it");
    }

    // ---- regression pins: asset preview, master widgets, snap grid ----

    /// Clicking a rack row auditions the sample; clicking the same row
    /// again stops it; the Settings checkbox disables the whole feature.
    #[test]
    fn rack_click_previews_and_toggles() {
        let mut app = headless_app("rackprev");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let click = |app: &mut App, ctx: &egui::Context, y: f32| {
            let p = egui::pos2(40.0, y);
            frame(
                app,
                ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(app, ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
        };
        // Find kick.wav's row: the click that starts a preview.
        let mut y_row = None;
        for y in (96..220).step_by(3) {
            click(&mut app, &ctx, y as f32);
            if app.preview_playing.is_some() {
                y_row = Some(y as f32);
                break;
            }
        }
        let y_row = y_row.expect("clicking a rack row previews it");
        assert!(app
            .preview_playing
            .as_deref()
            .is_some_and(|p| p.ends_with("kick.wav")));
        assert!(app.status.starts_with("previewing kick"), "{}", app.status);
        // Second click on the same entry stops (toggle).
        click(&mut app, &ctx, y_row);
        assert_eq!(
            app.preview_playing, None,
            "clicking the auditioning row stops it"
        );
        // Disabled in Settings → clicks never preview.
        app.preview_enabled = false;
        click(&mut app, &ctx, y_row);
        assert_eq!(app.preview_playing, None, "preview disabled in settings");
        // Re-enabled → clicks audition again.
        app.preview_enabled = true;
        click(&mut app, &ctx, y_row);
        assert!(app.preview_playing.is_some(), "re-enabled preview works");
    }

    /// Preview length parses/clamps and defaults to 5 s (config defaults
    /// are pinned in config::tests; the cut logic lives in preview_asset).
    #[test]
    fn preview_secs_parses_and_clamps() {
        let mut app = headless_app("prevsecs");
        assert_eq!(app.preview_secs_f(), 5.0, "default 5 s");
        app.preview_secs = "12.5".into();
        assert_eq!(app.preview_secs_f(), 12.5);
        app.preview_secs = "junk".into();
        assert_eq!(app.preview_secs_f(), 5.0, "garbage falls back to 5");
        app.preview_secs = "9999".into();
        assert_eq!(app.preview_secs_f(), 600.0, "clamped");
    }

    /// Master volume grew the per-track row widgets: an editable dB box
    /// and a "0" zeroize button resetting to 0 dB.
    #[test]
    fn master_volume_has_text_box_and_zero_button() {
        let mut app = headless_app("mastervol");
        let ctx = egui::Context::default();
        app.master_midi = 50;
        frame(&mut app, &ctx, vec![]);
        // Zeroize button (first painted "0" galley is the top bar's —
        // panels draw in add order, and the top bar is first).
        let z = label_center(&mut app, &ctx, "0").expect("a \"0\" button is painted");
        frame(
            &mut app,
            &ctx,
            vec![moved(z), btn(z, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(z, egui::PointerButton::Primary, false)],
        );
        assert_eq!(
            app.master_midi, VOL_UNITY_MIDI,
            "zeroize resets master to 0 dB"
        );

        // Editable box: click exactly on the painted dB value, type, commit.
        let want = db_num(vol_amp(app.master_midi));
        let p = label_center(&mut app, &ctx, &want).expect("master volume box is painted");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![egui::Event::Text("9".into())]);
        assert!(
            app.lv_edit.contains_key(&(KIND_VOL, usize::MAX)),
            "click+type holds a scratch edit on the master box"
        );
        type_into_focused(&mut app, &ctx, "-12");
        assert_eq!(
            app.master_midi,
            vol_midi(-12.0),
            "typed dB value applies to master"
        );
    }

    /// Sub-beat snap lines: a third, fainter grid tier drawn at the snap
    /// subdivision; gone when snap is the plain beat or the steps would
    /// crowd (< 4 px apart).
    #[test]
    fn snap_grid_lines_follow_snap_setting() {
        let mut app = headless_app("snapgrid");
        let ctx = egui::Context::default();
        let snap_lines = |app: &mut App, ctx: &egui::Context| -> usize {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let n = out
                .shapes
                .iter()
                .filter(|cs| {
                    matches!(&cs.shape, egui::epaint::Shape::LineSegment { stroke, .. }
                        if stroke.color == COL_SNAP_LINE)
                })
                .count();
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            n
        };
        // Default snap = sixteenth notes at the default zoom: the sub tier
        // paints (ruler + lanes).
        assert!(
            snap_lines(&mut app, &ctx) > 0,
            "snap tier painted at snap subdivision"
        );
        // Quarter-note snap: the beat line IS the snap grid → no sub tier.
        app.snap_sub = 0;
        assert_eq!(
            snap_lines(&mut app, &ctx),
            0,
            "no sub tier for quarter-note snap"
        );
        // Zoomed to the floor the steps fall under 4 px → tier hidden.
        app.snap_sub = SUBDIVISIONS.iter().position(|s| s.0 == 8).unwrap();
        app.px_per_sec = ZOOM_MIN;
        assert_eq!(
            snap_lines(&mut app, &ctx),
            0,
            "sub tier hides when steps crowd"
        );
    }
}
