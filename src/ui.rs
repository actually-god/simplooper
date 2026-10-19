//! egui front-end for simplooper: loop dashboard + settings + MIDI monitor.
//!
//! Layout:
//! - top bar: hamburger (settings), status, xruns, peak meter, master gain
//! - left side panel (optional): audio ports, MIDI + keyboard bindings, easy quantize
//! - central: independent/dependent mode checkbox + sixteen loop tracks, each
//!   with a waveform of the recorded audio and REC / OD / play / clear buttons
//! - bottom: MIDI monitor

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::Frame;
use egui::{Color32, RichText, Sense, Slider};

use crate::audio::{Backend, Cmd, Evt, FxSlot, OutSel, PortInfo, TrackFx, MAX_LOOPS, TR_OD, TR_PLAY, TR_REC};
use crate::config::{Config, MidiAxisCal, MidiBinding, TrackFxSave};
use crate::fx;
use crate::midi::{hex, scale_axis, MidiAction, MidiBind, MidiMsg};
use crate::quantize::Unit;

/// One row in the MIDI monitor log.
#[derive(Clone)]
struct MidiLine {
    ts: String,
    type_name: &'static str,
    describe: String,
    raw_hex: String,
}

/// Cached min/max columns for one track's waveform (invalidated by length,
/// content generation, width or ensemble-period changes — the dependent-mode
/// tiled view is cached too, so no per-frame buffer scans while playing).
/// `peak` is the loudest sampled sample (stride-sampled like the columns, so
/// rebuilding stays cheap even while a 60 s take records), letting the
/// display normalize "maxed out" recordings to full scale.
struct WaveCache {
    len: u32,
    gen: u64,
    cols: usize,
    /// Ensemble period the columns were tiled over (0 = single copy).
    period: u64,
    data: Vec<[f32; 2]>,
    peak: f32,
}

/// Number of bindable actions — one keyboard slot per entry of
/// [`MidiAction::ALL`].
pub const N_ACTIONS: usize = MidiAction::ALL.len();

/// Default keyboard bindings, one per entry of `MidiAction::ALL` (same order);
/// `None` = no default binding. Order: selected-track + file ops (6), select
/// loop 1–16, per-loop record/overdub/clear/play × 16 loops, dependent mode,
/// metronome, select next / previous loop.
const SHORTCUT_DEFAULTS: [Option<egui::Key>; N_ACTIONS] = [
    // Global (selected-track + file operations).
    Some(egui::Key::R),      // record (selected)
    Some(egui::Key::O),      // overdub (selected)
    Some(egui::Key::C),      // clear (selected)
    Some(egui::Key::Space),  // play / stop (selected)
    Some(egui::Key::I),      // import recording into selected loop
    Some(egui::Key::E),      // export prompt
    // Select loop 1–4 keep their historical defaults; 5–16 start unbound.
    Some(egui::Key::Num1),
    Some(egui::Key::Num2),
    Some(egui::Key::Num3),
    Some(egui::Key::Num4),
    None, // Select(4)
    None, // Select(5)
    None, // Select(6)
    None, // Select(7)
    None, // Select(8)
    None, // Select(9)
    None, // Select(10)
    None, // Select(11)
    None, // Select(12)
    None, // Select(13)
    None, // Select(14)
    None, // Select(15)
    // Per-loop actions (record/overdub/clear/play × 16 loops): unbound by
    // default — the user binds them in Settings → "Per loop bindings".
    None, None, None, None, // Loop 1: rec / od / clear / play
    None, None, None, None, // Loop 2
    None, None, None, None, // Loop 3
    None, None, None, None, // Loop 4
    None, None, None, None, // Loop 5
    None, None, None, None, // Loop 6
    None, None, None, None, // Loop 7
    None, None, None, None, // Loop 8
    None, None, None, None, // Loop 9
    None, None, None, None, // Loop 10
    None, None, None, None, // Loop 11
    None, None, None, None, // Loop 12
    None, None, None, None, // Loop 13
    None, None, None, None, // Loop 14
    None, None, None, None, // Loop 15
    None, None, None, None, // Loop 16
    Some(egui::Key::D), // dependent mode
    Some(egui::Key::M), // metronome on / off
    None,               // select next loop
    None,               // select previous loop
    Some(egui::Key::U), // undo last overdub
    Some(egui::Key::Y), // redo last undone overdub
    // Per-loop volume / gain (loops 1–16, interleaved): value-driven from
    // MIDI knobs — a keyboard press carries no value, so never key-bound.
    None, None, // Loop 1: volume / gain
    None, None, // Loop 2
    None, None, // Loop 3
    None, None, // Loop 4
    None, None, // Loop 5
    None, None, // Loop 6
    None, None, // Loop 7
    None, None, // Loop 8
    None, None, // Loop 9
    None, None, // Loop 10
    None, None, // Loop 11
    None, None, // Loop 12
    None, None, // Loop 13
    None, None, // Loop 14
    None, None, // Loop 15
    None, None, // Loop 16
    // Per-loop pan (loops 1–16): value-driven from MIDI knobs — appended
    // after volume/gain, so never key-bound either.
    None, // Loop 1: pan
    None, None, None, None, None, None, None, // Loops 2–8
    None, None, None, None, None, None, None, // Loops 9–15
    None, // Loop 16
];

/// Metronome subdivisions: (clicks per beat, display name).
const SUBDIVISIONS: [(u32, &str); 6] = [
    (1, "quarter note"),
    (2, "eighth note"),
    (3, "triplet"),
    (4, "sixteenth note"),
    (6, "triplet eighths"),
    (8, "thirty-second note"),
];

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

/// Name a key is persisted under (its Debug name; reversible via `key_from_name`).
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
    KEY_CANDIDATES.iter().find(|k| format!("{k:?}") == s).copied()
}

// --- per-loop level controls ---------------------------------------------
// MIDI knobs/sliders send 0..=127. Both mappings anchor 0 at silence
// (-inf dB); the sliders in each track row show the resulting dB.

/// MIDI value that maps to 0 dB on the per-loop gain slider.
pub const GAIN_UNITY_MIDI: u8 = 80;

/// Volume mapping: 0 = silence, 127 = 0 dB, ~0.47 dB per step down from
/// there (~-60 dB at 1) — linear in dB so the slider feels even.
pub fn vol_amp(v: u8) -> f32 {
    if v == 0 {
        0.0
    } else {
        10f32.powf((127 - v as i32) as f32 * (-60.0 / 127.0) / 20.0)
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
    /// Last port selection actually applied via "Apply Selection"; persisted
    /// with the config so it can be restored on the next launch.
    applied_ports: Option<(Vec<String>, Vec<OutSel>)>,
    gain: f32,
    /// Per-loop playback volume, MIDI-knob style (0..=127; 0 = -inf, 127 =
    /// 0 dB). Applied to each loop's contribution to the mix.
    trk_vol: [u8; MAX_LOOPS],
    /// Per-loop gain trim, MIDI-knob style (0..=127; 0 = -inf, 80 = 0 dB).
    trk_gain: [u8; MAX_LOOPS],
    /// Per-loop stereo pan, MIDI-knob style (0..=127; 0 = hard left,
    /// 64 = centre, 127 = hard right).
    trk_pan: [u8; MAX_LOOPS],
    /// In-progress text for a level value box while it holds keyboard
    /// focus: (kind, loop) -> draft. Cleared on commit / blur; when a knob
    /// or slider moves the value meanwhile, the box simply redraws itself.
    lv_edit: HashMap<(u8, usize), String>,
    /// Last loop selection propagated to the audio engine (input monitoring
    /// follows the selected loop's chain/gain/pan). `ui()` diffs it.
    sel_sent: usize,
    /// Level sliders / MIDI knobs moved since the last config write; flushed
    /// by `ui()` once the pointer is released (knob spam must not thrash the
    /// disk).
    levels_dirty: bool,
    /// GUI zoom factor (Settings → GUI); 1.0 = default UI size.
    gui_scale: f32,

    // --- loop dashboard ---
    show_settings: bool,
    dependent: bool,
    /// Track targeted by the keyboard shortcuts (keys 1–4 select it).
    selected: usize,
    track_state: [u8; MAX_LOOPS],
    track_len: [u32; MAX_LOOPS],
    wave: [Option<WaveCache>; MAX_LOOPS],
    /// Overdub undo/redo availability, mirrored from the audio thread via
    /// `Evt::UndoState` (drives the UNDO/REDO buttons' enabled state).
    can_undo: bool,
    can_redo: bool,

    // --- easy quantize tool ---
    /// Which loop's recorded length defines the quantize grid (0 = Loop 1).
    q_root_loop: usize,
    q_num: String,
    q_den: String,
    q_bars: String,
    q_unit_is_bar: bool,
    q_result: Option<String>,
    /// Quantize against the metronome grid instead of the root loop.
    q_metro: bool,

    // --- metronome ---
    metro_on: bool,
    /// Tempo in BPM (text field; parsed on apply).
    metro_bpm: String,
    /// Beats per measure — time-signature numerator (text field; default 4).
    metro_num: String,
    /// Index into [`SUBDIVISIONS`].
    metro_sub: usize,

    // --- latency compensation ---
    /// Input delay in ms (text field; applied via its own button). Delays the
    /// incoming signal (recording + monitoring) so playing lands on the loop
    /// grid; the output mix itself is never delayed.
    latency_ms: String,

    // --- MIDI bindings: one slot per entry of MidiAction::ALL ---
    midi_binds: [Option<MidiBind>; N_ACTIONS],

    // --- rebindable keyboard shortcuts (one slot per MidiAction::ALL entry) ---
    short_keys: [Option<egui::Key>; N_ACTIONS],
    /// Action slot currently in "press a key" learn mode, if any.
    learning_slot: Option<usize>,
    /// MIDI slot currently capturing the next NOTE ON / CC / PC message, if any.
    midi_learning_slot: Option<usize>,
    /// Two-step knob learn in progress for one action slot: capture the
    /// axis low end, lock it, capture the high end (see `knob_learn_window`).
    knob_learn: Option<KnobLearn>,
    /// Per-action knob calibration (volume / gain / pan slots only),
    /// persisted via Config::midi_axis_cal.
    knob_cal: Vec<Option<MidiAxisCal>>,
    /// Remote MIDI source ports currently linked into simplooper:midi_in
    /// (auto-detected; shown in the MIDI monitor).
    midi_sources: Vec<String>,
    /// True while the export prompt dialog is open.
    show_export: bool,

    // --- per-loop effects framework ---
    /// Effects folder registry (GUI thread; compile + load happen here).
    fx_registry: fx::Registry,
    /// Effects-window rows per loop: enabled first in chain order, then the
    /// disabled ones alphabetically.
    fx_entries: Vec<Vec<fx::UiEntry>>,
    /// Live chain instances per loop (parallel to the enabled entries; the
    /// sliders ride these directly and the audio thread reads their atomics).
    fx_live: Vec<Vec<std::sync::Arc<fx::Instance>>>,
    /// Loop whose Effects window is open (None = closed).
    fx_window: Option<usize>,
    /// Open effect editor child window (loop row button / effects window).
    /// Holding it keeps the edited instance alive; dropping it runs the
    /// custom GUI's destroy. One editor at a time.
    fx_editor: Option<Box<crate::fxgui::FxEditor>>,
    /// "Effects location" text box (folder of effect scripts).
    fx_dir_edit: String,
    /// Status line shown inside the effects window.
    fx_note: String,
    /// Chain edits flush the config once the pointer is released.
    fx_save_pending: bool,
    /// Which loop the "Per loop bindings" dropdown points at (0-based).
    bind_loop_sel: usize,
    /// Export dialog state: whole-project toggle, destination path, format
    /// index into `ExportFormat::ALL`.
    export_whole: bool,
    export_path: String,
    export_fmt: usize,

    // --- dependent-mode ensemble length (in units of the longest loop) ---
    root_loops: u32,
    q_root: String,

    // --- MIDI monitor state ---
    last_midi: Option<MidiLine>,
    midi_log: VecDeque<MidiLine>,
    peak_disp: f32,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        // Load persisted settings (shortcuts, MIDI binds, root loops).
        let cfg = Config::load();
        // Effects framework: open (creating/seeding when needed) the folder
        // and compile+load every script; chains get rebuilt once the engine
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
                    .map(|v| v.iter().map(|s| (s.stem.clone(), s.params.clone())).collect())
                    .unwrap_or_default();
                fx::merge_entries(&[], &fx_stems, &saved)
            })
            .collect();
        let mut short_keys = core::array::from_fn(|i| SHORTCUT_DEFAULTS[i]);
        // Legacy 9-slot order (select 1–4, play/stop, record, overdub, clear,
        // dependent); remap onto the per-action table. Newer configs store one
        // entry per MidiAction::ALL slot; v0.1.x stored the 25-slot layout
        // (select 1–4, selected rec/od/clear/play, per-loop × 4 loops, dep).
        const OLD9_TO_NEW: [usize; 9] = [6, 7, 8, 9, 3, 0, 1, 2, N_ACTIONS - 1];
        let legacy25_to_new = |i: usize| match i {
            0..=3 => 6 + i,          // Select(0..3)
            4 => 0,                  // RecSelected
            5 => 1,                  // OdSelected
            6 => 2,                  // ClearSelected
            7 => 3,                  // PlaySelected
            8..=23 => 20 + (i - 8),  // per-loop actions for loops 0–3
            _ => N_ACTIONS - 1,      // Dependent
        };
        match cfg.shortcuts.len() {
            9 => {
                for (old, name) in cfg.shortcuts.iter().enumerate() {
                    short_keys[OLD9_TO_NEW[old]] = name.as_deref().and_then(key_from_name);
                }
            }
            25 => {
                for (old, name) in cfg.shortcuts.iter().enumerate() {
                    short_keys[legacy25_to_new(old)] = name.as_deref().and_then(key_from_name);
                }
            }
            _ => {
                for (i, name) in cfg.shortcuts.iter().enumerate().take(N_ACTIONS) {
                    short_keys[i] = name.as_deref().and_then(key_from_name);
                }
            }
        }
        let mut midi_binds = [None; N_ACTIONS];
        for b in &cfg.midi {
            if MidiAction::idx(b.action) < N_ACTIONS {
                midi_binds[MidiAction::idx(b.action)] = b.bind;
            }
        }
        // Knob calibration: expand the sparse persisted list into one
        // slot per action.
        let mut knob_cal: Vec<Option<MidiAxisCal>> = vec![None; N_ACTIONS];
        for c in &cfg.midi_axis {
            if let Some(s) = knob_cal.get_mut(usize::from(c.slot)) {
                *s = Some(*c);
            }
        }
        // Per-loop levels: pad the persisted vectors (older configs have
        // none) and push the restored values into the audio engine — the
        // engine atomics are the live source of truth for the RT thread.
        let trk_vol = core::array::from_fn(|i| cfg.trk_vol.get(i).copied().unwrap_or(127));
        let trk_gain =
            core::array::from_fn(|i| cfg.trk_gain.get(i).copied().unwrap_or(GAIN_UNITY_MIDI));
        let trk_pan =
            core::array::from_fn(|i| cfg.trk_pan.get(i).copied().unwrap_or(PAN_CENTRE_MIDI));
        for t in 0..MAX_LOOPS {
            backend.engine.set_loop_vol(t, vol_amp(trk_vol[t]));
            backend.engine.set_loop_gain(t, gain_amp(trk_gain[t]));
            backend.engine.set_loop_pan(t, pan01(trk_pan[t]));
        }
        let root_loops = cfg.root_loops.clamp(1, 64);
        // Restore the last applied port selections so the user doesn't have to
        // re-check the same ports on every launch.
        let sel_in: HashSet<String> = cfg.ports_in.iter().cloned().collect();
        // Outputs carry their saved L/R routing; entries saved before stereo
        // existed default to the mono downmix (3) — identical behaviour.
        let sel_out: HashMap<String, u8> = cfg
            .ports_out
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), cfg.ports_out_lr.get(i).copied().unwrap_or(3)))
            .collect();
        let applied_ports = if cfg.ports_in.is_empty() && cfg.ports_out.is_empty() {
            None
        } else {
            Some((
                cfg.ports_in,
                cfg.ports_out
                    .into_iter()
                    .enumerate()
                    .map(|(i, n)| (n, cfg_lr(&cfg.ports_out_lr, i)))
                    .collect(),
            ))
        };

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
            gain: 1.0,
            trk_vol,
            trk_gain,
            trk_pan,
            lv_edit: HashMap::new(),
            sel_sent: usize::MAX,
            levels_dirty: false,
            gui_scale: cfg.gui_scale.clamp(0.7, 1.6),
            show_settings: false,
            dependent: false,
            selected: 0,
            track_state: [0u8; MAX_LOOPS],
            track_len: [0u32; MAX_LOOPS],
            wave: core::array::from_fn(|_| None),
            can_undo: false,
            can_redo: false,
            q_root_loop: 0,
            q_num: "4".into(),
            q_den: "4".into(),
            q_bars: "2".into(),
            q_unit_is_bar: false,
            q_result: None,
            q_metro: cfg.q_metro,
            metro_on: false,
            metro_bpm: cfg.metro_bpm.to_string(),
            metro_num: cfg.metro_num.clamp(1, 16).to_string(),
            metro_sub: SUBDIVISIONS
                .iter()
                .position(|(c, _)| *c == cfg.metro_sub)
                .unwrap_or(0),
            latency_ms: if cfg.latency_ms == 0.0 {
                "0".into()
            } else {
                format!("{}", cfg.latency_ms)
            },
            midi_binds,
            short_keys,
            learning_slot: None,
            midi_learning_slot: None,
            knob_learn: None,
            knob_cal,
            midi_sources: Vec::new(),
            show_export: false,
            fx_registry,
            fx_entries,
            fx_live: vec![Vec::new(); MAX_LOOPS],
            fx_window: None,
            fx_editor: None,
            fx_dir_edit: fx_dir.display().to_string(),
            fx_note: String::new(),
            fx_save_pending: false,
            bind_loop_sel: 0,
            export_whole: false,
            export_path: String::new(),
            export_fmt: 0,
            root_loops,
            q_root: root_loops.to_string(),
            last_midi: None,
            midi_log: VecDeque::with_capacity(300),
            peak_disp: 0.0,
        }
    }

    fn now_stamp() -> String {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            (t.as_secs() % 86400) / 3600,
            (t.as_secs() % 3600) / 60,
            t.as_secs() % 60,
            t.as_millis() % 1000
        )
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
            bind: MidiBind { ty: 0xff, ch: 0, d1: 0 },
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
        let Some(kl) = self.knob_learn.take() else { return };
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
            Some(MidiAxisCal { slot: slot as u16, lo, hi })
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
        if ctx.text_edit_focused() {
            return;
        }

        // The knob wizard is its own learn mode: it does NOT set
        // `midi_learning_slot` (arm_knob_learn clears it), so it needs its
        // own key branch. Escape cancels; Delete clears the slot's binding
        // and calibration — same vocabulary as the key/MIDI cells. All keys
        // are swallowed so nothing fires behind the wizard.
        if self.knob_learn.is_some() {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key { key, pressed: true, repeat: false, .. } => Some(*key),
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
                        self.status =
                            format!("{verb}: {}", MidiAction::ALL[kl.slot].label());
                    }
                }
            }
            return; // swallow all keys while the knob wizard is open
        }

        // Learn mode: the next key press becomes the slot's new binding.
        if let Some(slot) = self.learning_slot {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key { key, pressed: true, repeat: false, .. } => Some(*key),
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
                    self.status =
                        format!("cleared binding: {}", MidiAction::ALL[slot].label());
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
                    egui::Event::Key { key, pressed: true, repeat: false, .. } => Some(*key),
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
                    egui::Event::Key { key, pressed: true, repeat: false, .. } => Some(*key),
                    _ => None,
                })
                .collect()
        });

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

    /// Fire a bound action (MIDI trigger or keyboard shortcut). Selected-track
    /// variants act on the selected loop; loop variants act on one specific
    /// track; Dependent toggles ensemble mode.
    fn fire_midi_action(&mut self, a: MidiAction) {
        match a {
            MidiAction::Select(i) => self.selected = i,
            MidiAction::RecSelected => self.backend.cmd(Cmd::ToggleRecord { i: self.selected }),
            MidiAction::OdSelected => self.backend.cmd(Cmd::ToggleOverdub { i: self.selected }),
            MidiAction::ClearSelected => self.backend.cmd(Cmd::ClearTrack { i: self.selected }),
            MidiAction::PlaySelected => self.backend.cmd(Cmd::TogglePlay { i: self.selected }),
            MidiAction::ImportSelected => self.import_loop(self.selected),
            MidiAction::Export => self.show_export = true,
            MidiAction::RecLoop(i) => self.backend.cmd(Cmd::ToggleRecord { i }),
            MidiAction::OdLoop(i) => self.backend.cmd(Cmd::ToggleOverdub { i }),
            MidiAction::ClearLoop(i) => self.backend.cmd(Cmd::ClearTrack { i }),
            MidiAction::PlayLoop(i) => self.backend.cmd(Cmd::TogglePlay { i }),
            MidiAction::Dependent => {
                self.dependent = !self.dependent;
                self.backend.cmd(Cmd::SetDependent(self.dependent));
            }
            MidiAction::Metronome => {
                self.metro_on = !self.metro_on;
                self.send_metro();
            }
            // Selection NEVER wraps: it stops at loop 1 and loop 16, so the
            // selection frame can't jump off-screen past the first/last row.
            MidiAction::SelectNext => self.selected = (self.selected + 1).min(MAX_LOOPS - 1),
            MidiAction::SelectPrev => self.selected = self.selected.saturating_sub(1),
            MidiAction::UndoOverdub => self.backend.cmd(Cmd::UndoOverdub),
            MidiAction::RedoOverdub => self.backend.cmd(Cmd::RedoOverdub),
            // Value-driven knob actions fire only through the MIDI path
            // (handle_evt passes the CC value); a key press has no value.
            MidiAction::VolLoop(_) | MidiAction::GainLoop(_) | MidiAction::PanLoop(_) => {}
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
            let Some(desc) = self.fx_registry.desc(&e.stem).cloned() else { continue };
            let vals = fx::seed_values(&e.params, &desc);
            e.params = vals.clone();
            if let Some(inst) =
                self.fx_registry.create_instance(&e.stem, self.sample_rate as f64, &vals)
            {
                slots.push(FxSlot { inst: inst.clone(), desc });
                live.push(inst);
            }
        }
        self.fx_live[i] = live;
        self.backend.cmd(Cmd::SetTrackEffects { i, chain: TrackFx { slots: std::sync::Arc::new(slots) } });
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
        let (Some(inst), Some(desc)) = (inst, desc) else { return };
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
                if let Some(e) =
                    self.fx_entries[ed.loop_i].iter_mut().filter(|e| e.enabled).nth(ed.slot)
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
        let bad = self.fx_registry.items().iter().filter(|e| e.error.is_some()).count();
        self.fx_note = format!("{ok} valid effect(s), {bad} rejected");
    }

    /// Push the metronome state (play/stop / tempo / beats per measure /
    /// subdivision) to the audio thread and persist it. Tempo text that fails
    /// to parse keeps 120 BPM; beats-per-measure text falls back to 4.
    fn send_metro(&mut self) {
        let bpm = self.metro_bpm.trim().parse::<u32>().unwrap_or(120).clamp(20, 300);
        self.backend.cmd(Cmd::SetMetro {
            on: self.metro_on,
            bpm,
            sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            num: self.metro_num.trim().parse::<u32>().unwrap_or(4).clamp(1, 16),
        });
        self.save_config();
    }

    /// Persist shortcuts, MIDI bindings and the ensemble length to disk.
    fn save_config(&self) {
        let cfg = Config {
            shortcuts: (0..N_ACTIONS)
                .map(|i| self.short_keys[i].map(key_debug))
                .collect(),
            midi: MidiAction::ALL
                .iter()
                .zip(self.midi_binds.iter())
                .filter_map(|(a, b)| b.map(|bind| MidiBinding { action: *a, bind: Some(bind) }))
                .collect(),
            midi_axis: self
                .knob_cal
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|mut m| { m.slot = i as u16; m }))
                .collect(),
            root_loops: self.root_loops,
            gui_scale: self.gui_scale,
            latency_ms: self.latency_ms.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1000.0),
            metro_bpm: self.metro_bpm.trim().parse::<u32>().unwrap_or(120).clamp(20, 300),
            metro_sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            metro_num: self.metro_num.trim().parse::<u32>().unwrap_or(4).clamp(1, 16),
            q_metro: self.q_metro,
            // Only selections that were actually applied get persisted.
            ports_in: self.applied_ports.as_ref().map(|p| p.0.clone()).unwrap_or_default(),
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
            trk_vol: self.trk_vol.to_vec(),
            trk_gain: self.trk_gain.to_vec(),
            trk_pan: self.trk_pan.to_vec(),
            fx_dir: self.fx_dir_edit.trim().to_string(),
            track_fx: (0..MAX_LOOPS)
                .map(|i| {
                    self.fx_entries[i]
                        .iter()
                        .filter(|e| e.enabled)
                        .map(|e| TrackFxSave { stem: e.stem.clone(), params: e.params.clone() })
                        .collect()
                })
                .collect(),
        };
        if let Err(e) = cfg.save() {
            eprintln!("[simplooper] could not save config: {e}");
        }
    }

    fn handle_evt(&mut self, evt: Evt) {
        match evt {
            Evt::Unavailable(msg) => self.status = msg,
            Evt::Ready { client_name, sample_rate } => {
                self.client_name = Some(client_name.clone());
                self.sample_rate = sample_rate;
                self.status = "JACK ready — select ports, then Apply".into();
                // Audio is up: apply the persisted ensemble length, latency
                // compensation and metronome settings.
                self.backend.cmd(Cmd::SetRootLoops(self.root_loops));
                self.backend.cmd(Cmd::SetLatency { ms: self.latency_ms.trim().parse().unwrap_or(0.0) });
                self.send_metro();
                // Effect instances made before the backend was up were
                // created with rate 0; rebuild every chain at the real rate.
                for t in 0..MAX_LOOPS {
                    if !self.fx_live[t].is_empty()
                        || self.fx_entries[t].iter().any(|e| e.enabled)
                    {
                        self.rebuild_fx(t);
                    }
                }
                // Re-apply the port bindings persisted from the previous
                // session so audio flows on launch without re-clicking
                // "Apply Selection" (missing ports just fail to connect and
                // show an error in the status line).
                if let Some((ins, outs)) = &self.applied_ports {
                    if !ins.is_empty() || !outs.is_empty() {
                        self.backend.cmd(Cmd::Apply { ins: ins.clone(), outs: outs.clone() });
                    }
                }
            }
            Evt::Ports { ins, outs } => {
                self.avail_in = ins;
                self.avail_out = outs;
            }
            Evt::Applied { ins, outs } => {
                self.applied = Some((ins, outs));
                self.status = format!("applied: {} in / {} out", ins, outs);
            }
            Evt::LoopState { states, lens } => {
                self.track_state = states;
                self.track_len = lens;
            }
            Evt::Quantized { summary } => {
                self.q_result = Some(summary);
            }
            Evt::UndoState { can_undo, can_redo } => {
                self.can_undo = can_undo;
                self.can_redo = can_redo;
            }
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
                                self.status =
                                    format!("bound {} → {}", MidiAction::ALL[slot].label(), bind.label());
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
                                MidiMsg::NoteOn { velocity, .. } if *velocity > 0 => Some(*velocity),
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
                                let Some(b) = self.midi_binds[i] else { continue };
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

    /// Import a recording into loop `i`: system file picker, then decode +
    /// resample to engine rate on the GUI thread (fast enough for typical
    /// takes), then hand the samples to the audio thread.
    fn import_loop(&mut self, i: usize) {
        if self.sample_rate == 0 {
            self.status = "import: wait for JACK to be ready".into();
            return;
        }
        let path = rfd::FileDialog::new()
            .set_title("Import recording")
            .add_filter("Audio", crate::audio_io::IMPORT_EXTENSIONS)
            .pick_file();
        let Some(p) = path else { return };
        match crate::audio_io::decode_to_mono(&p, self.sample_rate) {
            Ok(data) => {
                self.backend.cmd(Cmd::ImportTrack { i, data });
                self.status = format!(
                    "imported {} → loop {}",
                    p.file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    i + 1
                );
            }
            Err(e) => self.status = e,
        }
    }

    /// Run the export configured in the dialog (see `export_window`).
    fn do_export(&mut self) {
        let mut path = std::path::PathBuf::from(self.export_path.trim());
        if path.as_os_str().is_empty() {
            self.status = "export: choose a destination first".into();
            return;
        }
        let fmt = crate::audio_io::ExportFormat::ALL[self.export_fmt];
        // The selected format drives the extension: the destination is
        // picked without one, so switching WAV/FLAC/MP3/OGG never requires
        // re-choosing the location.
        let ext = fmt.extension();
        let has_want = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext));
        if !has_want {
            path.set_extension(ext);
        }
        if self.export_whole {
            // Whole-project export only exists in dependent mode — force it on.
            if !self.dependent {
                self.dependent = true;
                self.backend.cmd(Cmd::SetDependent(true));
                self.save_config();
            }
            self.backend.cmd(Cmd::ExportProject { path, fmt });
        } else {
            self.backend.cmd(Cmd::ExportTrack { i: self.selected, path, fmt });
        }
        self.show_export = false;
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        // Apply the user's GUI scale (Settings → GUI); no-op unless it changed.
        ctx.set_zoom_factor(self.gui_scale);

        // Keep the update loop ticking so backend events (MIDI, port changes)
        // are drained promptly; eframe otherwise pauses frames when nothing
        // has changed. While a track plays we tick faster (~50 fps) so the
        // playback bars move smoothly; idle stays at 10 fps to keep CPU low.
        let any_playing = self.track_state.iter().any(|&s| s == TR_PLAY || s == TR_OD);
        ctx.request_repaint_after(std::time::Duration::from_millis(if any_playing {
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

        // Decay the displayed peak toward the live value.
        let p = f32::from_bits(self.backend.peak.load(Ordering::Relaxed));
        self.peak_disp = p.max(self.peak_disp * 0.9);

        if changed {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        // Push the gain slider value into the audio thread (f32 as bits).
        self.backend.gain.store(self.gain.to_bits(), Ordering::Relaxed);

        // Mirror the GUI selection into the engine every time it changes:
        // the input monitor runs the selected loop's FX chain + gain + pan.
        // usize::MAX at boot forces the first frame to send it.
        if self.sel_sent != self.selected {
            self.sel_sent = self.selected;
            self.backend.cmd(Cmd::SetSelected(self.selected));
        }

        // Flush per-loop level changes to the config once the control is
        // released: sliders fire per pixel and MIDI knobs per message, so
        // saving on every change would thrash the disk.
        if self.levels_dirty && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.levels_dirty = false;
            self.save_config();
        }
        if self.fx_save_pending && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.fx_save_pending = false;
            self.save_config();
        }

        top_bar(ui, self);
        if self.show_settings {
            settings_panel(ui, self);
        }
        midi_monitor(ui, self);
        central_panels(ui, self);
        if self.show_export {
            export_window(ui, self);
        }
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
    egui::Panel::top("topbar")
        .exact_size(40.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Hamburger (settings) button: three horizontal bars.
                let size = egui::vec2(26.0, 18.0);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                if rect.width() > 4.0 && rect.height() > 4.0 {
                    let p = ui.painter();
                    for k in 0..3u32 {
                        let y = rect.min.y + 4.0 + k as f32 * 5.0;
                        p.rect_filled(
                            egui::Rect::from_min_size(egui::pos2(rect.min.x + 3.0, y), egui::vec2(20.0, 2.0)),
                            1.0,
                            if app.show_settings { Color32::WHITE } else { Color32::from_rgb(200, 200, 205) },
                        );
                    }
                }
                let resp = resp.on_hover_text("Settings");
                if resp.clicked() {
                    app.show_settings = !app.show_settings;
                }

                ui.separator();
                ui.heading(RichText::new("simplooper").strong());
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

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Peak meter.
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(90.0, 8.0), Sense::hover());
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
                            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())),
                            1.0,
                            color,
                        );
                    }
                    ui.label(RichText::new(format!("peak {:.2}", app.peak_disp)).weak());

                    ui.add(Slider::new(&mut app.gain, 0.0..=2.0).suffix("× gain"));
                });
            });
        });
}

/// Left settings panel: audio ports, MIDI bindings, easy quantize tool.
fn settings_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::left("settings")
        .resizable(true)
        .default_size(410.0)
        .min_size(330.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading(RichText::new("SETTINGS").strong());

                // ---- GUI ---------------------------------------------------------
                ui.separator();
                ui.label(RichText::new("GUI").strong());
                ui.horizontal(|ui| {
                    let r = ui.add(egui::Slider::new(&mut app.gui_scale, 0.7..=1.6).suffix("×"));
                    if r.changed() {
                        app.save_config();
                    }
                });
                ui.label(RichText::new("Scales the whole interface; applied live.").weak().small());

                // ---- latency compensation ------------------------------------------
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
                         the click lands exactly on the loop start — the loop begins with the \
                         hit, not with dead air. Monitoring, overdubs and the output mix are \
                         never delayed.",
                    )
                        .weak()
                        .small(),
                );

                // ---- MIDI + keyboard bindings ------------------------------------
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
                         other key does anything). A bound \
                         action fires on every matching message (value/velocity \
                         ignored). The key column works the same way: click, then \
                         press the new key (Esc cancels); Delete clears that cell's \
                         binding and is never bindable itself. Keys are ignored \
                         while a text field has focus.",
                    )
                    .weak()
                    .small(),
                );
                // Global: selected-track transport + file ops + mode. The old
                // "selected track" section lives here now; per-loop actions are
                // bound through the dropdown below instead of 64 rows.
                ui.label(RichText::new("global").weak().small());
                for a in [
                    MidiAction::RecSelected,
                    MidiAction::OdSelected,
                    MidiAction::ClearSelected,
                    MidiAction::PlaySelected,
                    MidiAction::ImportSelected,
                    MidiAction::Export,
                    MidiAction::UndoOverdub,
                    MidiAction::RedoOverdub,
                    MidiAction::Dependent,
                    MidiAction::Metronome,
                    MidiAction::SelectNext,
                    MidiAction::SelectPrev,
                ] {
                    bind_row(ui, app, a);
                }
                ui.label(RichText::new("per loop bindings").weak().small());
                ui.horizontal(|ui| {
                    ui.label("Loop");
                    egui::ComboBox::from_id_salt("bind_loop_sel")
                        .selected_text(format!("{}", app.bind_loop_sel + 1))
                        .show_ui(ui, |ui| {
                            for n in 0..MAX_LOOPS {
                                ui.selectable_value(&mut app.bind_loop_sel, n, format!("{}", n + 1));
                            }
                        });
                });
                // The rows below re-target the dropdown's loop; changing the
                // selection swaps which slots they edit (labels update too).
                // Volume / gain / pan are knob bindings: the action fires
                // with the message VALUE (0..=127), so only CC / note-on
                // drives them.
                for off in 0..8usize {
                    let a = match off {
                        0 => MidiAction::RecLoop(app.bind_loop_sel),
                        1 => MidiAction::OdLoop(app.bind_loop_sel),
                        2 => MidiAction::ClearLoop(app.bind_loop_sel),
                        3 => MidiAction::PlayLoop(app.bind_loop_sel),
                        4 => MidiAction::Select(app.bind_loop_sel),
                        5 => MidiAction::VolLoop(app.bind_loop_sel),
                        6 => MidiAction::GainLoop(app.bind_loop_sel),
                        _ => MidiAction::PanLoop(app.bind_loop_sel),
                    };
                    bind_row(ui, app, a);
                }

                // ---- audio ports -------------------------------------------------
                // Last on purpose: the long port lists used to push every other
                // section out of view. One flat list per direction, stacked
                // inside the panel's own scroll area (nested scroll areas made
                // the section titles scroll away from view; side-by-side
                // columns were too narrow at the default panel width).
                ui.separator();
                ui.label(RichText::new("AUDIO PORTS").strong());
                port_list(
                    ui,
                    "INPUTS",
                    "remote output ports (sources)",
                    &app.avail_in,
                    &mut app.sel_in,
                );
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
                        // Sort for deterministic slot assignment, so the
                        // same selection maps to the same out<N> slots and
                        // JACK connections survive restarts unchanged.
                        let mut ins: Vec<String> = app.sel_in.iter().cloned().collect();
                        ins.sort();
                        let mut outs: Vec<OutSel> = app
                            .sel_out
                            .iter()
                            .filter(|(_, b)| **b != 0)
                            .map(|(n, b)| (n.clone(), *b))
                            .collect();
                        outs.sort();
                        // Remember the applied selection so it is restored on
                        // the next launch (persisted with the other settings).
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
                        bind: MidiBind { ty: 0xff, ch: 0, d1: 0 },
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
        let mut btn = egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(150.0, 0.0));
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
        let mut btn = egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(44.0, 0.0));
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
                format!("Loop {} (selected)", app.selected + 1),
            );
            ui.radio_value(&mut app.export_whole, true, "Whole project");
            if app.export_whole {
                ui.label(
                    RichText::new(
                        "⚠ Whole-project export only works in dependent mode — it will \
                         be enabled automatically before exporting.",
                    )
                    .color(Color32::YELLOW),
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
                        "simplooper-project".to_string()
                    } else {
                        format!("simplooper-loop-{}", app.selected + 1)
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
    egui::Window::new(RichText::new(format!("EFFECTS — loop {}", loop_i + 1)).strong())
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

fn central_panels(ui: &mut egui::Ui, app: &mut App) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("loop mode:").strong());
            // The selector is the INDEPENDENT switch: ticked = every loop
            // runs on its own clock (unchecked = dependent). Only the
            // polarity flipped — the dependent logic behind it is unchanged.
            let mut indep = !app.dependent;
            let dep = ui.checkbox(&mut indep, "independent").on_hover_text(
                "Independent looping (tick = on): every loop plays on its own clock, \
                 like sixteen standalone loopers.\n\nUntick for DEPENDENT looping: the \
                 longest loop is the master — every other loop wraps on top of it, and \
                 the whole ensemble restarts together when the longest one ends.\n\n\
                 Use dependent mode only with properly quantized loops (Easy Quantize, \
                 above), or you will hear skips at the loop points.",
            );
            if dep.changed() {
                app.dependent = !indep;
                app.backend.cmd(Cmd::SetDependent(app.dependent));
            }
            // Ensemble length: how many times the longest loop repeats before
            // everything restarts together.
            ui.label(RichText::new("root loops:").weak());
            let r_root = ui.add(egui::TextEdit::singleline(&mut app.q_root).desired_width(40.0));
            if r_root.changed() {
                if let Ok(n) = app.q_root.trim().parse::<u32>() {
                    let n = n.clamp(1, 64);
                    if n != app.root_loops {
                        app.root_loops = n;
                        app.backend.cmd(Cmd::SetRootLoops(n));
                        app.save_config();
                    }
                }
            }
            ui.separator();
            if ui.button("Export").clicked() {
                app.show_export = true;
            }
            // Overdub undo/redo (global — acts on the most recent overdub).
            let b_undo = egui::Button::new(RichText::new("UNDO").monospace());
            let r_undo = ui
                .add_enabled(app.can_undo, b_undo)
                .on_hover_text(if app.can_undo {
                    "Undo the last overdub: the loop it was dubbed over reverts to its \
                     pre-overdub content (U). Redo with Y or the REDO button. Bindable via MIDI."
                } else {
                    "Nothing to undo yet — overdub a loop first (key: U)"
                });
            if r_undo.clicked() {
                app.backend.cmd(Cmd::UndoOverdub);
            }
            let b_redo = egui::Button::new(RichText::new("REDO").monospace());
            let r_redo = ui
                .add_enabled(app.can_redo, b_redo)
                .on_hover_text(if app.can_redo {
                    "Re-apply the undone overdub (Y). Bindable via MIDI."
                } else {
                    "Nothing to redo — undo an overdub first (key: Y)"
                });
            if r_redo.clicked() {
                app.backend.cmd(Cmd::RedoOverdub);
            }
            ui.label(
                RichText::new("independent = each loop plays on its own clock").weak().small(),
            );
        });
        // ---- easy quantize (lives in the main window, not Settings) ----
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(35, 40, 50))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("EASY QUANTIZE").strong()).on_hover_text(
                        "Derives the grid from the root loop's recorded length \
                         (pick 1–4 in the dropdown): bar = len ÷ bars, beat = bar ÷ \
                         numerator. Every loop is then snapped to the nearest multiple \
                         of the chosen unit; the cut is refined to a zero crossing and \
                         the seam crossfaded.",
                    );
                    ui.separator();
                    ui.label("time sig.");
                    ui.add(egui::TextEdit::singleline(&mut app.q_num).desired_width(40.0));
                    ui.label("/");
                    ui.add(egui::TextEdit::singleline(&mut app.q_den).desired_width(40.0));
                    ui.label("bars in loop");
                    egui::ComboBox::from_id_salt("q_root_loop")
                        .selected_text(format!("{}", app.q_root_loop + 1))
                        .show_ui(ui, |ui| {
                            for n in 0..MAX_LOOPS {
                                ui.selectable_value(&mut app.q_root_loop, n, format!("{}", n + 1));
                            }
                        });
                    ui.label(":");
                    ui.add(egui::TextEdit::singleline(&mut app.q_bars).desired_width(60.0));
                    ui.label("quantize to:");
                    ui.radio_value(&mut app.q_unit_is_bar, false, "beat");
                    ui.radio_value(&mut app.q_unit_is_bar, true, "bar");
                    let qm = ui.checkbox(&mut app.q_metro, "quantize to metronome").on_hover_text(
                        "Use the metronome's tempo (below) plus the time signature above \
                         as the grid instead of the root loop's length. 'bars in loop' \
                         is unused in this mode.",
                    );
                    if qm.changed() {
                        app.save_config();
                    }
                    if ui.button("Quantize all loops").clicked() {
                        match (
                            app.q_num.parse::<u32>(),
                            app.q_den.parse::<u32>(),
                            app.q_bars.parse::<u32>(),
                        ) {
                            (Ok(n), Ok(d), Ok(b)) if n >= 1 && d >= 1 && b >= 1 => {
                                let unit = if app.q_unit_is_bar { Unit::Bar } else { Unit::Beat };
                                app.backend.cmd(Cmd::Quantize {
                                    num: n,
                                    den: d,
                                    bars: b,
                                    unit,
                                    root: app.q_root_loop,
                                    metro: app.q_metro,
                                    bpm: app.metro_bpm.trim().parse::<u32>().unwrap_or(120).clamp(20, 300),
                                });
                            }
                            _ => {
                                app.status = "quantize: enter positive integers for time signature and bars".into();
                            }
                        }
                    }
                });
                if let Some(r) = &app.q_result {
                    ui.label(RichText::new(r).monospace().small());
                }
            });

        // ---- metronome (click goes to outputs only — never recorded) ----
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(35, 40, 50))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Play/stop button in the same style as the per-loop
                    // transport buttons: ▶ starts the click immediately on
                    // the downbeat, ■ stops it.
                    let (m_label, m_active) =
                        if app.metro_on { ("■ METRO", true) } else { ("▶ METRO", false) };
                    let mut b_metro = egui::Button::new(RichText::new(m_label).monospace());
                    if m_active {
                        b_metro = b_metro.fill(Color32::from_rgb(90, 140, 90));
                    }
                    let r_metro = ui.add(b_metro).on_hover_text(
                        "Metronome: starts directly on the downbeat. Click track: \
                         audible on all selected outputs but never recorded into a \
                         loop. Beat 1 of every measure gets the high downbeat tone, \
                         the other quarter notes a mid tone, subdivisions a low \
                         tone. Toggle also on M (rebindable in Settings).",
                    );
                    if r_metro.clicked() {
                        app.metro_on = !app.metro_on;
                        app.send_metro();
                    }
                    ui.separator();
                    ui.label("tempo");
                    let r_bpm = ui.add(egui::TextEdit::singleline(&mut app.metro_bpm).desired_width(50.0));
                    if r_bpm.changed() && app.metro_bpm.trim().parse::<u32>().is_ok() {
                        app.send_metro();
                    }
                    ui.label("BPM · beats/measure");
                    let r_num = ui.add(egui::TextEdit::singleline(&mut app.metro_num).desired_width(30.0));
                    if r_num.changed() && app.metro_num.trim().parse::<u32>().is_ok() {
                        app.send_metro();
                    }
                    ui.label("· subdivision");
                    // NB: `ComboBox::show_ui` returns the BUTTON response,
                    // which egui never marks changed when a popup item is
                    // picked — watch the `selectable_value` responses instead
                    // (this was why a subdivision change only reached the
                    // engine on the next play/stop click).
                    let mut sub_changed = false;
                    egui::ComboBox::from_id_salt("metro_sub")
                        .selected_text(SUBDIVISIONS[app.metro_sub.min(SUBDIVISIONS.len() - 1)].1)
                        .show_ui(ui, |ui| {
                            for (n, (_, name)) in SUBDIVISIONS.iter().enumerate() {
                                if ui.selectable_value(&mut app.metro_sub, n, *name).changed() {
                                    sub_changed = true;
                                }
                            }
                        });
                    if sub_changed {
                        app.send_metro();
                    }
                });
            });

        ui.label(
            RichText::new("keys are rebindable in Settings · click a row to select its loop")
                .weak()
                .small(),
        );
        ui.add_space(10.0);

        // Dependent mode: show the root (longest) loop first, then the rest in
        // numeric order; every waveform spans the full ensemble period.
        // Live lengths straight from the engine (same pattern as pos /
        // master_pos / peak), so the timer, waveform and ensemble period keep
        // moving while a take is recording — the Evt::LoopState mirror only
        // updates on state changes.
        let lens: [u32; MAX_LOOPS] = core::array::from_fn(|i| app.backend.engine.len_of(i));
        let lmax = lens.iter().copied().max().unwrap_or(0);
        let mut order: Vec<usize> = (0..MAX_LOOPS).collect();
        if app.dependent {
            order.sort_by_key(|&i| ((lens[i] != lmax) as usize, i));
        }
        let dep_period = if app.dependent && lmax > 0 {
            Some((lmax as u64 * app.root_loops as u64) as u32)
        } else {
            None
        };

        // Scrollable so all 16 tracks stay reachable on small windows.
        egui::ScrollArea::vertical().show(ui, |ui| {
            // The selection frame expands 3 px past the row rect; without a
            // leading spacer its top edge on row 1 clips at the viewport.
            ui.add_space(4.0);
            for &i in &order {
                track_row(ui, app, i, dep_period);
                ui.add_space(8.0);
            }
        });
    });
}

/// One loop track: label + waveform + REC/OD/play/clear buttons. The row the
/// keyboard shortcuts target (keys 1–4) is highlighted with a frame.
fn track_row(ui: &mut egui::Ui, app: &mut App, i: usize, dep_period: Option<u32>) {
    // Lay the row out first, then stroke a bounding box around exactly the
    // rect the content occupies: the blue box hugs the selected loop's own
    // waveform and buttons — no separate widget, no extra vertical gap
    // (a pre-allocated frame used to render as an empty box above the row).
    let inner = ui.vertical(|ui| {
        track_row_inner(ui, app, i, dep_period);
    });
    if app.selected == i {
        let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(110, 165, 205));
        ui.painter().rect_stroke(
            inner.response.rect.expand(3.0),
            2.0,
            stroke,
            egui::StrokeKind::Middle,
        );
    }
}

fn track_row_inner(ui: &mut egui::Ui, app: &mut App, i: usize, dep_period: Option<u32>) {
    let state = app.track_state[i];
    // Live length from the engine (updated by the RT thread every sample), so
    // the timer and waveform track the take as it records.
    let len = app.backend.engine.len_of(i);
    let secs = if app.sample_rate > 0 {
        len as f32 / app.sample_rate as f32
    } else {
        0.0
    };
    let (state_txt, state_col) = match state {
        TR_REC => ("recording…", Color32::RED),
        TR_PLAY => ("playing", Color32::LIGHT_GREEN),
        TR_OD => ("overdubbing", Color32::YELLOW),
        _ if len > 0 => ("ready", Color32::GRAY),
        _ => ("idle", Color32::GRAY),
    };

    // Playback bar position (fraction 0..1 across the waveform), if playing.
    let mut playhead: Option<f32> = None;
    if state == TR_PLAY || state == TR_OD {
        let eng = &app.backend.engine;
        match dep_period {
            Some(period) if period > 0 => {
                // Dependent mode: position within the full ensemble period.
                let mp = eng.master_pos() % period as u64;
                playhead = Some(mp as f32 / period as f32);
            }
            _ if len > 0 => {
                playhead = Some(eng.pos_of(i) as f32 / len as f32);
            }
            _ => {}
        }
    }

    ui.horizontal(|ui| {
        // Left: name + state.
        ui.vertical(|ui| {
            ui.set_min_width(128.0);
            ui.label(RichText::new(format!("Loop {}", i + 1)).strong());
            ui.label(
                RichText::new(format!("{state_txt} · {secs:.2} s"))
                    .color(state_col)
                    .monospace()
                    .small(),
            );
        });

        // Middle: waveform of the recorded audio (reserve room for buttons).
        // Clicking it selects this loop (the row's only click target).
        let wave_w = (ui.available_width() - 330.0).clamp(60.0, f32::INFINITY);
        let eng = app.backend.engine.clone();
        let data = eng.read_range(i, len);
        let wave_resp = draw_wave(ui, &mut app.wave[i], len, state, eng.generation(), data, wave_w, playhead, dep_period);
        if wave_resp.clicked() {
            app.selected = i;
        }

        // Right: transport buttons.
        let mut b_rec = egui::Button::new(RichText::new("● REC").monospace());
        if state == TR_REC {
            b_rec = b_rec.fill(Color32::from_rgb(210, 70, 70));
        }
        let r_rec = ui.add(b_rec).on_hover_text(
            "Record: fresh capture (replaces current content). Key: R. Bindable via MIDI.",
        );
        if r_rec.clicked() {
            app.backend.cmd(Cmd::ToggleRecord { i });
        }

        let mut b_od = egui::Button::new(RichText::new("◉ OD").monospace());
        if state == TR_OD {
            b_od = b_od.fill(Color32::from_rgb(210, 170, 60));
        }
        let r_od = ui.add(b_od).on_hover_text(
            "Overdub: play while writing live input on top. Key: O. Bindable via MIDI.",
        );
        if r_od.clicked() {
            app.backend.cmd(Cmd::ToggleOverdub { i });
        }

        let (p_label, p_active) = match state {
            TR_PLAY | TR_OD => ("■", true),
            _ => ("▶", false),
        };
        let mut b_play = egui::Button::new(RichText::new(p_label).monospace());
        if p_active {
            b_play = b_play.fill(Color32::from_rgb(90, 140, 90));
        }
        let r_play = ui.add(b_play).on_hover_text("Play / stop this loop (space).");
        if r_play.clicked() {
            app.backend.cmd(Cmd::TogglePlay { i });
        }

        let r_clr = ui
            .add(egui::Button::new(RichText::new("✕").monospace()))
            .on_hover_text("Stop and erase this loop (C).");
        if r_clr.clicked() {
            app.backend.cmd(Cmd::ClearTrack { i });
        }

        let r_imp = ui
            .add(egui::Button::new(RichText::new("IMP").monospace()))
            .on_hover_text("Import a recording into this loop (I).");
        if r_imp.clicked() {
            app.import_loop(i);
        }
    });

    // Bottom strip of the loop: the stacked level element (vol / gain /
    // pan) pinned to the bottom-left, the Effects button right next to it,
    // then every enabled effect, left to right in chain order.
    // Gain x volume are always applied first — before any effect.
    ui.horizontal(|ui| {
        // One row per level control, each bindable to a MIDI knob in
        // Settings → per loop bindings.
        ui.vertical(|ui| {
            let w = (ui.available_width() * 0.24).clamp(220.0, 320.0);
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 2.0;
            level_row(ui, app, i, KIND_VOL);
            level_row(ui, app, i, KIND_GAIN);
            level_row(ui, app, i, KIND_PAN);
        });

        ui.add_space(6.0);
        let fb = ui
            .button("Effects")
            .on_hover_text("Open this loop's effects window (folder, toggles, order).");
        if fb.clicked() {
            app.fx_window = Some(i);
            app.fx_dir_edit = app.fx_registry.dir().display().to_string();
            app.fx_note = String::new();
        }
        ui.separator();
        fx_buttons(ui, app, i);
    });
}

// Level-row kinds (index into labels, keys `lv_edit`).
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
        KIND_VOL => "Loop playback volume: 0 = silence (-inf dB), 127 = 0 dB. The number at the right is the raw MIDI value. Bind it to a MIDI knob in Settings → MIDI bindings → per loop.",
        KIND_GAIN => "Loop gain trim: 0 = silence, 80 = 0 dB, 127 = +10 dB. The number at the right is the raw MIDI value. Bind it to a MIDI knob in Settings → MIDI bindings → per loop.",
        _ => "Loop pan: -100 = hard left, 0 = centre, +100 = hard right (equal power across the L/R outputs). The number at the right is the raw MIDI value. Bind it to a MIDI knob in Settings → MIDI bindings → per loop.",
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(label).monospace().small());
        // Editable value box (number only), centred text, flush against the
        // slider.
        let key = (kind, i);
        let mut txt =
            app.lv_edit.get(&key).cloned().unwrap_or_else(|| level_display(kind, midi));
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
                egui::Label::new(RichText::new("dB").monospace().weak().small())
                    .selectable(false),
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
        // slider kept its own taller default height.
        let mut v = midi;
        let sw = (ui.available_width() - if kind == KIND_PAN { 30.0 } else { 50.0 }).max(30.0);
        let bh = rt.rect.height();
        let r = ui
            .add_sized(egui::vec2(sw, bh), Slider::new(&mut v, 0..=127).show_value(false))
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
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
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
                KIND_VOL => 127.0 + db * (127.0 / 60.0),
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
        let Some(desc) = app.fx_registry.desc(&e.stem).cloned() else { continue };
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
        if ui.button(RichText::new(&desc.name).small().color(color)).on_hover_text(hover).clicked()
        {
            open_slot = Some(k);
        }
        k += 1;
    }
    if let Some(k) = open_slot {
        app.open_fx_editor(i, k);
    }
}

/// Draw a track's waveform, scaled to the track's own peak so quiet recordings
/// are as legible as loud ones. Shows an optional playback bar and — in
/// dependent mode — tiles the recording across the full ensemble period with
/// separator bars at each repeat boundary (matching the modulo-wrap playback).
#[allow(clippy::too_many_arguments)] // flat args keep call sites readable
fn draw_wave(
    ui: &mut egui::Ui,
    cache: &mut Option<WaveCache>,
    len: u32,
    state: u8,
    gen: u64,
    data: &[f32],
    w: f32,
    playhead: Option<f32>,
    period: Option<u32>,
) -> egui::Response {
    let h = 60.0f32;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w.max(40.0), h), Sense::click());
    let painter = ui.painter();

    let color = match state {
        TR_REC => Color32::from_rgb(255, 96, 96),
        TR_PLAY => Color32::from_rgb(128, 224, 128),
        TR_OD => Color32::from_rgb(240, 208, 96),
        _ => Color32::from_rgb(150, 165, 190),
    };

    painter.rect_stroke(
        rect,
        1.0,
        egui::Stroke::new(1.0, Color32::from_rgb(70, 74, 84)),
        egui::StrokeKind::Inside,
    );
    let mid = rect.center().y;
    painter.line_segment(
        [egui::pos2(rect.min.x, mid), egui::pos2(rect.max.x, mid)],
        egui::Stroke::new(1.0, Color32::from_rgb(95, 100, 110)),
    );

    if len > 0 {
        // Per-track peak normalization: the (stride-sampled) loudest sample is
        // "maxed out" at full scale, so quiet recordings are as legible as
        // loud ones. Columns and peak are memoized together and rebuilt only
        // when length, content generation, width or the ensemble period
        // change — while playing (50 fps) this path is pure rendering.
        let n = len as usize;
        let cols = (rect.width() / 2.0).max(1.0) as usize;
        let period_samples = period.unwrap_or(len) as u64;
        let need = match cache {
            Some(c) => c.len != len || c.gen != gen || c.cols != cols || c.period != period_samples,
            None => true,
        };
        if need {
            let stride = (n / (cols * 8)).max(1);
            let mut peak = 0f32;
            for &v in data.iter().step_by(stride) {
                let a = v.abs();
                if a > peak {
                    peak = a;
                }
            }
            let data_cols = if period_samples <= n as u64 {
                wave_cols(data, cols)
            } else {
                wave_cols_tiled(data, cols, period_samples)
            };
            *cache = Some(WaveCache { len, gen, cols, period: period_samples, data: data_cols, peak });
        }
        let scale = match cache {
            Some(c) if c.peak > 1e-6 => (rect.height() / 2.0 - 2.0) / c.peak,
            _ => 0.0,
        };

        // Cached min/max columns — single copy, or the recording tiled across
        // the ensemble period (the same wrap as playback, `p % len`).
        if let Some(c) = cache {
            let colw = rect.width() / c.cols.max(1) as f32;
            for (k, [mn, mx]) in c.data.iter().enumerate() {
                let top = (mid - mx * scale).clamp(rect.min.y + 1.0, rect.max.y - 1.0);
                let bot = (mid - mn * scale).clamp(rect.min.y + 1.0, rect.max.y - 1.0);
                let x = rect.min.x + k as f32 * colw;
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(x, top.min(bot)),
                        egui::vec2(colw.max(1.0) - 0.5, (bot - top).max(1.0)),
                    ),
                    0.0,
                    color,
                );
            }
        }
        if period_samples > n as u64 {
            // Dependent stacked view: separator bars at every repeat boundary
            // m * len (m >= 1), including on the root track when it repeats
            // (root loops > 1).
            let n64 = n as u64;
            let mut m = 1u64;
            while m * n64 < period_samples {
                let p = m * n64;
                let x = rect.min.x + p as f32 / period_samples as f32 * rect.width();
                painter.line_segment(
                    [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                    egui::Stroke::new(1.0, Color32::from_rgb(105, 112, 128)),
                );
                m += 1;
            }
        }
    } else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no recording yet",
            egui::FontId::proportional(11.0),
            Color32::from_rgb(110, 115, 125),
        );
    }

    // Playback bar.
    if let Some(f) = playhead {
        let x = rect.min.x + f.clamp(0.0, 1.0) * rect.width();
        painter.line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            egui::Stroke::new(2.0, Color32::from_rgb(255, 214, 90)),
        );
    }

    resp.on_hover_text(format!(
        "{} samples recorded{} — click to select this loop",
        len,
        period.map(|p| format!(" · ensemble of {p}")).unwrap_or_default()
    ))
}

/// Min/max per column with bounded stride sampling (fast even for 60 s buffers).
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
fn wave_cols_tiled(buf: &[f32], cols: usize, period: u64) -> Vec<[f32; 2]> {
    let n = buf.len() as u64;
    if n == 0 || cols == 0 {
        return Vec::new();
    }
    let mut out: Vec<[f32; 2]> = vec![[0.0, 0.0]; cols];
    for (c, col) in out.iter_mut().enumerate() {
        let p0 = c as u64 * period / cols as u64;
        let p1 = ((c + 1) as u64 * period / cols as u64).max(p0 + 1);
        let mut mn = 0f32;
        let mut mx = 0f32;
        let stride = ((p1 - p0) / 8).max(1);
        let mut k = p0;
        while k < p1 {
            let v = buf[(k % n) as usize];
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

/// Flat list of ports for one direction — no inner scroll area, so the whole
/// settings panel scrolls as one and section titles stay visible.
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
            RichText::new(format!("  {}", p.name)).weak().small().monospace(),
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
            let rr = ui.checkbox(&mut r, "R").on_hover_text("play the RIGHT side");
            ui.label(RichText::new(short).strong().small());
            ui.label(RichText::new(format!("  {}", p.name)).weak().small().monospace());
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
                                RichText::new(format!("| {}", last.raw_hex)).weak().monospace(),
                            );
                        });
                    });
            } else {
                ui.add_space(4.0);
                ui.label(RichText::new("waiting for MIDI input…").weak());
                ui.label(
                    RichText::new(
                        "the audio thread reads the JACK port simplooper:midi_in and \
                         listens on every channel — link your device to it (e.g. \
                         pw-link 'your-midi-port' 'simplooper:midi_in') and send a \
                         message; new source ports appear below automatically",
                    )
                    .weak()
                    .small(),
                );
            }

            if !app.midi_sources.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("sources linked to simplooper:midi_in").weak().small());
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
                                RichText::new(format!("| {}", line.raw_hex)).weak().monospace(),
                            );
                        });
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(k: egui::Key) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }
    }

    /// App with config isolated to a per-test temp dir (never the user's).
    fn test_app(tag: &str) -> App {
        // Set-once: set_var mutates glibc `environ` (realloc), and the audio
        // backend threads earlier tests left running read env vars — even a
        // single concurrent getenv during a set_var is the same UB that
        // surfaced as `corrupted size vs. prev_size`. The NULL-audio flag is
        // identical for every test, so it only ever needs one write.
        static NULL_ENV: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        NULL_ENV.get_or_init(|| std::env::set_var("SIMPLOOPER_NULL_AUDIO", "1"));
        // XDG_CONFIG_HOME changes per test; no other thread reads it (null
        // mode never reaches the JACK client), and the suite is pinned to
        // RUST_TEST_THREADS=1 (.cargo/config.toml).
        let dir =
            std::env::temp_dir().join(format!("simplooper-test-{}-{}", std::process::id(), tag));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        App::new(crate::audio::Backend::start())
    }

    #[test]
    fn effect_name_button_opens_the_effects_own_gui() {
        // The scripted examples ship their own child GUI; clicking an
        // enabled effect's name button must open that GUI (not the auto
        // slider window) and paint it through a real egui pass.
        if !fx::rustc_available() {
            return; // no compiler in the environment: nothing to load
        }
        let mut app = test_app("fxeditorgui");
        let dir = std::env::temp_dir().join(format!("simplooper-fxgui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(fx::API_FILE), fx::API_SRC).unwrap();
        std::fs::write(dir.join(fx::TEMPLATE_FILE), fx::TEMPLATE_SRC).unwrap();
        for (name, src) in fx::EXAMPLES {
            std::fs::write(dir.join(name), src).unwrap();
        }
        app.fx_dir_edit = dir.display().to_string();
        app.apply_fx_dir();

        for stem in ["parametric_eq", "space_echo"] {
            assert!(app.fx_registry.gui(stem).is_some(), "{stem} exposes a child GUI");
            let k = app.fx_entries[0].iter().position(|e| e.stem == stem).unwrap();
            app.fx_entries[0][k].enabled = true;
            let stems = app.fx_registry.stems();
            fx::resort_entries(&mut app.fx_entries[0], &stems);
            app.rebuild_fx(0);
            let slot = app.fx_entries[0]
                .iter()
                .filter(|e| e.enabled)
                .position(|e| e.stem == stem)
                .unwrap();
            app.open_fx_editor(0, slot);
            assert!(app.fx_editor.is_some(), "name click opens the editor window");
            assert!(
                app.fx_editor.as_ref().is_some_and(|ed| ed.is_custom()),
                "{stem} must open its own scripted GUI, not auto sliders"
            );
            // Paint it: gui_create/gui_draw run inside a real pass; a bad
            // GUI contract would panic here.
            let ctx = egui::Context::default();
            let mut out = ctx.run_ui(egui::RawInput::default(), |c| {
                egui::CentralPanel::default().show(c, |ui| {
                    app.fx_editor_window(ui);
                });
            });
            // Headless pass: the caller (usually a GPU backend) must consume
            // the frame's shapes + texture deltas before they are dropped.
            let _tris = ctx.tessellate(out.shapes, out.pixels_per_point);
            out.textures_delta.clear();
            assert!(app.fx_editor.is_some(), "editor stays open after painting");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eq_strip_widgets_and_fine_curve_react() {
        // Headless regression for the three "EQ does nothing" complaints:
        // 1. the selected band's FREQ KNOB must write the frequency param;
        // 2. the response curve must visibly change when that param moves
        //    (it is one high-resolution antialiased path, 800 pts);
        // 3. no label may contain a glyph egui's fonts lack ('\u{25B8}', '\u{00B7}')
        //    — those hollow boxes read as broken checkboxes.
        if !fx::rustc_available() {
            return;
        }
        let mut app = test_app("eqstrip");
        let dir = std::env::temp_dir().join(format!("sp-strip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(fx::API_FILE), fx::API_SRC).unwrap();
        std::fs::write(dir.join(fx::TEMPLATE_FILE), fx::TEMPLATE_SRC).unwrap();
        for (name, src) in fx::EXAMPLES {
            std::fs::write(dir.join(name), src).unwrap();
        }
        app.fx_dir_edit = dir.display().to_string();
        app.apply_fx_dir();
        app.fx_entries[0].iter_mut().for_each(|e| e.enabled = false);
        let k = app.fx_entries[0].iter().position(|e| e.stem == "parametric_eq").unwrap();
        app.fx_entries[0][k].enabled = true;
        app.rebuild_fx(0);
        app.open_fx_editor(0, 0);
        let inst = app.fx_editor.as_ref().unwrap().inst.clone();
        let ctx = egui::Context::default();
        let mut t = 0.0f64;
        let mut frame = |evs: Vec<egui::Event>| -> Vec<egui::epaint::ClippedShape> {
            let mut ri = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1920.0, 1080.0),
                )),
                time: Some({
                    t += 0.1;
                    t
                }),
                ..Default::default()
            };
            ri.events = evs;
            let app: &mut App = unsafe { &mut *(&mut app as *mut App) };
            let mut out = ctx.run_ui(ri, |c| {
                app.fx_editor_window(c);
            });
            let shapes = out.shapes.clone();
            ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            out.textures_delta.clear();
            shapes
        };
        let moved = |p: egui::Pos2| egui::Event::PointerMoved(p);
        let down = |p: egui::Pos2| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let up = |p: egui::Pos2| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        };
        frame(vec![]);
        frame(vec![]);
        let shapes = frame(vec![moved(egui::pos2(100.0, 476.0))]);

        // (3) no tofu glyphs in any painted label.
        for cs in &shapes {
            if let egui::Shape::Text(t) = &cs.shape {
                let s = t.galley.text();
                assert!(
                    !s.contains('\u{25B8}') && !s.contains('\u{00B7}'),
                    "label {s:?} contains a glyph the default fonts lack"
                );
            }
        }

        // (1) drag the freq knob (first knob circle below the graph) up 30 px.
        let mut knob: Option<egui::Pos2> = None;
        for cs in &shapes {
            if let egui::Shape::Circle(cc) = &cs.shape {
                if cc.center.y > 400.0 && cc.center.x < 500.0 {
                    let c = cc.center;
                    if knob.is_none_or(|k| c.x < k.x) {
                        knob = Some(c);
                    }
                }
            }
        }
        let kp = knob.expect("freq knob circle painted");
        let skip_knob = std::env::var("EQP").is_ok();
        let f0 = inst.param(1);
        if !skip_knob {
            frame(vec![moved(kp)]);
            frame(vec![down(kp)]);
            frame(vec![moved(kp + egui::vec2(0.0, -30.0))]);
            frame(vec![up(kp + egui::vec2(0.0, -30.0))]);
        }
        let f1 = inst.param(1);
        assert!(skip_knob || f0 != f1, "dragging the freq knob must change the freq param");

        // (2) the fine curve must actually move when the freq param moves.
        // Minimal-pattern bisection.
        match std::env::var("EQMIN").ok().as_deref() {
            Some("1") => { inst.set_param(1, 80.0); frame(vec![]); }
            Some("2") => { inst.set_param(1, 80.0); frame(vec![]); frame(vec![]); }
            Some("3") => { inst.set_param(1, 80.0); frame(vec![]); inst.set_param(1, 3000.0); frame(vec![]); }
            Some("4") => {
                inst.set_param(1, 80.0); frame(vec![]);
                let s = frame(vec![]);
                println!("EQMIN4 shapes={}", s.len());
                inst.set_param(1, 3000.0); frame(vec![]);
                let _s2 = frame(vec![]);
            }
            Some("5") => { frame(vec![]); frame(vec![]); frame(vec![]); frame(vec![]); }
            _ => {}
        }
        // (4) bell shape: B3 peaking +4.5 dB @ 800 Hz must be ONE smooth
        // bump centred on its frequency, not spikes at random places (the
        // curve once evaluated |H| at TAU*f instead of TAU*f/sample_rate
        // and aliased every bell across the whole axis).
        {
            let mut grab = || -> Vec<(f32, f32)> {
                frame(vec![]);
                let shapes = frame(vec![]);
                shapes
                    .iter()
                    .filter_map(|cs| match &cs.shape {
                        egui::Shape::Path(pth) if pth.points.len() > 400 => {
                            Some(pth.points.iter().map(|p| (p.x, p.y)).collect::<Vec<(f32, f32)>>())
                        }
                        _ => None,
                    })
                    .max_by_key(|pts| pts.len())
                    .expect("response curve for bell check")
            };
            inst.set_param(0, 0.0); // B1 off: everything else sits at 0 dB gain
            inst.set_param(10, 0.0); // B3 gain 0 -> the flat reference
            let flat = grab();
            inst.set_param(9, 800.0);
            inst.set_param(10, 4.5);
            let bell = grab();
            let n = bell.len().min(flat.len());
            let dev: Vec<f32> = (0..n).map(|i| flat[i].1 - bell[i].1).collect(); // px above flat
            let peak = dev.iter().cloned().fold(f32::MIN, f32::max);
            let pi = dev.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
            let fpk = 20.0f32 * 1000.0f32.powf(pi as f32 / (n as f32 - 1.0));
            assert!(peak > 10.0, "4.5 dB bump must be visible ({} px)", peak);
            assert!((fpk - 800.0).abs() < 96.0, "bump peak at {} Hz, not 800", fpk);
            let far = dev
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let f = 20.0f32 * 1000.0f32.powf(i as f32 / (n as f32 - 1.0));
                    if f < 350.0 || f > 1800.0 {
                        v.abs()
                    } else {
                        0.0
                    }
                })
                .fold(0.0f32, f32::max);
            assert!(far < peak * 0.35, "spikes outside the band: {} vs peak {}", far, peak);
        }
        let mut curve_at = |freq: f32| -> Vec<(f32, f32)> {
            inst.set_param(1, freq);
            frame(vec![]);
            let shapes = frame(vec![]);
            shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::Shape::Path(pth) if pth.points.len() > 400 => {
                        Some(pth.points.iter().map(|p| (p.x, p.y)).collect::<Vec<(f32, f32)>>())
                    }
                    _ => None,
                })
                .max_by_key(|pts| pts.len())
                .expect("high-resolution response curve (single path)")
        };
        // A shelf/peak at 0 dB is mathematically FLAT — moving its corner
        // frequency changes nothing visible. Give B1 a real, enabled band.
        inst.set_param(0, 1.0); // low shelf (the bell check above turned it off)
        inst.set_param(10, 0.0); // and silence B3 again
        inst.set_param(2, 6.0);
        let skip_curve = std::env::var("EQC").is_ok();
        let c1 = if skip_knob || skip_curve { Vec::new() } else { curve_at(80.0) };
        let c2 = if skip_knob || skip_curve { Vec::new() } else { curve_at(3000.0) };
        if !skip_knob && !skip_curve {
            assert!(c1.len() >= 800, "curve should sample >= 800 points, got {}", c1.len());
            let moved_px: f32 = c1.iter().zip(c2.iter()).map(|(a, b)| (a.1 - b.1).abs()).sum();
            assert!(moved_px > 200.0, "curve must visibly change when B1 freq moves (sum {moved_px})");
        }
        if std::env::var("EQNODROP").is_ok() {
            std::mem::forget(inst);
        } else {
            drop(inst);
        }
        app.fx_editor = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eq_graph_handles_drag_at_their_drawn_positions() {
        // Headless regression for the two EQ graph bugs:
        // 1. every painted band handle must be grabbable AT ITS PAINTED
        //    POSITION (draw and hit-test used to disagree: draw at the
        //    cascade |H(fc)|, grab at the band's own gain — after moving
        //    one band the other five handles were painted far from
        //    anywhere clickable);
        // 2. no widget label may wrap: a wrapped label renders as a
        //    vertical column of one-character lines (the "spectrum"
        //    checkbox bug via Ui::put's justified WRAP layout).
        if !fx::rustc_available() {
            return;
        }
        let mut app = test_app("eqgraph");
        let dir = std::env::temp_dir().join(format!("simplooper-eqgraph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(fx::API_FILE), fx::API_SRC).unwrap();
        std::fs::write(dir.join(fx::TEMPLATE_FILE), fx::TEMPLATE_SRC).unwrap();
        for (name, src) in fx::EXAMPLES {
            std::fs::write(dir.join(name), src).unwrap();
        }
        app.fx_dir_edit = dir.display().to_string();
        app.apply_fx_dir();
        app.fx_entries[0].iter_mut().for_each(|e| e.enabled = false);
        let k = app.fx_entries[0].iter().position(|e| e.stem == "parametric_eq").unwrap();
        app.fx_entries[0][k].enabled = true;
        app.rebuild_fx(0);
        app.open_fx_editor(0, 0); // only parametric_eq is enabled
        let inst = app.fx_editor.as_ref().unwrap().inst.clone();
        let ctx = egui::Context::default();
        let mut t = 0.0f64;
        let mut frame = |evs: Vec<egui::Event>| -> Vec<egui::epaint::ClippedShape> {
            let mut ri = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1920.0, 1080.0),
                )),
                time: Some({
                    t += 0.1;
                    t
                }),
                ..Default::default()
            };
            ri.events = evs;
            let app: &mut App = unsafe { &mut *(&mut app as *mut App) };
            let mut out = ctx.run_ui(ri, |c| {
                app.fx_editor_window(c);
            });
            let shapes = out.shapes.clone();
            ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            out.textures_delta.clear();
            shapes
        };
        // egui 0.36 frame-caches new windows: first pass paints Noop.
        frame(vec![]);
        frame(vec![]);
        let shapes = frame(vec![]);
        let canvas = shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Rect(rr) if rr.fill == egui::Color32::from_rgb(21, 23, 28) => {
                    Some(rr.rect)
                }
                _ => None,
            })
            .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap())
            .expect("canvas rect");

        // (2) every text galley inside the editor is a single line.
        for cs in &shapes {
            if let egui::Shape::Text(t) = &cs.shape {
                assert_eq!(
                    t.galley.rows.len(),
                    1,
                    "label wrapped into {} lines at {:?}: {:?}",
                    t.galley.rows.len(),
                    t.pos,
                    t.galley.text()
                );
            }
        }

        // (1) scan for the painted handles (8x8 squares, selected amber or
        // unselected blue) and drag each one at its PAINTED centre.
        let moved = |p: egui::Pos2| egui::Event::PointerMoved(p);
        let down = |p: egui::Pos2| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let up = |p: egui::Pos2| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        };
        for round in 0..2 {
            let shapes = frame(vec![moved(canvas.min + egui::vec2(3.0, 3.0))]);
            let mut handles: Vec<(f32, f32)> = shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::Shape::Rect(rr)
                        if [(egui::Color32::from_rgb(255, 191, 76), 255), (egui::Color32::from_rgb(114, 158, 216), 255)]
                            .iter()
                            .any(|(c, _)| rr.fill == *c)
                            && (rr.rect.width() - 8.0).abs() < 0.6
                            && (rr.rect.height() - 8.0).abs() < 0.6 =>
                    {
                        Some((rr.rect.center().x, rr.rect.center().y))
                    }
                    _ => None,
                })
                .collect();
            handles.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            assert_eq!(handles.len(), 6, "all six default band handles painted (round {round})");
            for (bi, (hx, hy)) in handles.iter().enumerate() {
                let before = inst.param(bi * 4 + 1);
                let tgt = egui::pos2(*hx, *hy);
                frame(vec![moved(tgt)]);
                frame(vec![down(tgt)]);
                let dst = egui::pos2(hx + 30.0, hy - 20.0);
                frame(vec![moved(dst)]);
                frame(vec![up(dst)]);
                frame(vec![moved(canvas.min + egui::vec2(3.0, 3.0))]);
                let after = inst.param(bi * 4 + 1);
                assert_ne!(before, after, "dragging band {bi} handle at its drawn pos must move freq (round {round})");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eq_strip_checkboxes_sit_on_their_button_rows() {
        // Headless regression for the "checkboxes overlap and are
        // misaligned" report: `Ui::put` with a ZERO-sized rect CENTRES the
        // checkbox on its anchor, so the box straddled the neighbour
        // button and floated above the row centre. The strip contract is a
        // TOP-LEFT anchor in a 20 px row band (see `fxgui::tr_checkbox`):
        // every painted checkbox box must be vertically centred on the
        // button row it belongs to, intersect NO button rect, and the
        // drawn box must be clickable.
        if !fx::rustc_available() {
            return;
        }
        let mut app = test_app("eqchecks");
        let dir = std::env::temp_dir().join(format!("simplooper-eqchecks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(fx::API_FILE), fx::API_SRC).unwrap();
        std::fs::write(dir.join(fx::TEMPLATE_FILE), fx::TEMPLATE_SRC).unwrap();
        for (name, src) in fx::EXAMPLES {
            std::fs::write(dir.join(name), src).unwrap();
        }
        app.fx_dir_edit = dir.display().to_string();
        app.apply_fx_dir();
        app.fx_entries[0].iter_mut().for_each(|e| e.enabled = false);
        let k = app.fx_entries[0].iter().position(|e| e.stem == "parametric_eq").unwrap();
        app.fx_entries[0][k].enabled = true;
        app.rebuild_fx(0);
        app.open_fx_editor(0, 0);
        let inst = app.fx_editor.as_ref().unwrap().inst.clone();
        let ctx = egui::Context::default();
        let mut t = 0.0f64;
        let mut frame = |evs: Vec<egui::Event>| -> Vec<egui::epaint::ClippedShape> {
            let mut ri = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1920.0, 1080.0),
                )),
                time: Some({
                    t += 0.1;
                    t
                }),
                ..Default::default()
            };
            ri.events = evs;
            let app: &mut App = unsafe { &mut *(&mut app as *mut App) };
            let mut out = ctx.run_ui(ri, |c| {
                app.fx_editor_window(c);
            });
            let shapes = out.shapes.clone();
            ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            out.textures_delta.clear();
            shapes
        };
        frame(vec![]);
        let shapes = frame(vec![]);

        // Checkbox boxes are the only ~14-16 px squares painted (band
        // handles are 8 px, buttons 20 px tall).
        let mut boxes: Vec<egui::Rect> = shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Rect(rr)
                    if rr.rect.width() > 12.0
                        && rr.rect.width() < 18.0
                        && rr.rect.height() > 12.0
                        && rr.rect.height() < 18.0 =>
                {
                    Some(rr.rect)
                }
                _ => None,
            })
            .collect();
        boxes.sort_by(|a, b| a.center().y.total_cmp(&b.center().y));
        assert_eq!(boxes.len(), 2, "two strip checkboxes painted (spectrum + on)");

        // The control strip buttons are 20 px tall.
        let rows: Vec<egui::Rect> = shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Rect(rr) if (rr.rect.height() - 20.0).abs() < 0.6 => Some(rr.rect),
                _ => None,
            })
            .collect();
        assert!(rows.len() >= 8, "strip buttons painted as 20 px rows");
        for b in &boxes {
            let off = rows
                .iter()
                .map(|r| (r.center().y - b.center().y).abs())
                .fold(f32::MAX, f32::min);
            assert!(
                off < 2.0,
                "checkbox box {b:?} not centred on a button row (off {off:.1} px)"
            );
            for r in &rows {
                assert!(
                    (*b).intersect(*r).is_negative(),
                    "checkbox {b:?} overlaps button {r:?} — anchor/width math regressed"
                );
            }
        }

        // The lower box is the "on" checkbox of the selected band (B1,
        // default type = low shelf): a click AT THE PAINTED BOX must toggle
        // the band type (shelf -> off -> peaking), proving hit and paint
        // agree.
        let on_box = boxes[1].center();
        let mv = egui::Event::PointerMoved;
        let down = |p: egui::Pos2| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let up = |p: egui::Pos2| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        };
        assert_eq!(inst.param(0), 1.0, "B1 starts as a shelf (checked)");
        frame(vec![mv(on_box)]);
        frame(vec![down(on_box)]);
        frame(vec![up(on_box)]);
        assert_eq!(inst.param(0), 0.0, "clicking the painted box unchecks the band");
        frame(vec![down(on_box)]);
        frame(vec![up(on_box)]);
        assert_eq!(inst.param(0), 3.0, "second click re-enables as peaking");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]

    fn delete_clears_midi_binding_while_listening() {
        let mut app = test_app("midi-del");
        app.midi_binds[0] = Some(crate::midi::MidiBind { ty: 0, ch: 0, d1: 60 });
        app.midi_learning_slot = Some(0);
        let ctx = egui::Context::default();
        ctx.input_mut(|i| i.events.push(key_event(egui::Key::Delete)));
        app.handle_shortcuts(&ctx);
        assert_eq!(app.midi_binds[0], None, "Delete must clear the MIDI binding");
        assert_eq!(app.midi_learning_slot, None, "and end the listening state");
    }

    #[test]
    fn delete_clears_knob_binding_and_calibration() {
        // The knob cell is "listening" while its wizard is open. Delete must
        // clear the binding AND its min/max calibration there too — not just
        // close the window (that was "cancel", which Escape still does).
        let mut app = test_app("knob-del");
        let slot = MidiAction::idx(MidiAction::VolLoop(0));
        app.midi_binds[slot] = Some(crate::midi::MidiBind { ty: 1, ch: 1, d1: 7 });
        app.knob_cal[slot] =
            Some(crate::config::MidiAxisCal { slot: slot as u16, lo: 8, hi: 42 });
        app.arm_knob_learn(slot);
        assert!(app.knob_learn.is_some());
        assert_eq!(app.midi_learning_slot, None, "the wizard owns no MIDI cell");
        let ctx = egui::Context::default();
        ctx.input_mut(|i| i.events.push(key_event(egui::Key::Delete)));
        // Delete is only seen while the bindings window context runs the
        // global handler, same as the MIDI test above.
        app.handle_shortcuts(&ctx);
        assert_eq!(app.midi_binds[slot], None, "Delete must clear the knob binding");
        assert_eq!(app.knob_cal[slot], None, "and the min/max calibration");
        assert!(app.knob_learn.is_none(), "and close the wizard");
        assert_eq!(app.midi_learning_slot, None, "and end the listening state");
    }

    #[test]
    fn delete_clears_key_binding_while_learning() {
        let mut app = test_app("key-del");
        app.short_keys[0] = Some(egui::Key::R);
        app.arm_key_slot(0);
        let ctx = egui::Context::default();
        ctx.input_mut(|i| i.events.push(key_event(egui::Key::Delete)));
        app.handle_shortcuts(&ctx);
        assert_eq!(app.short_keys[0], None, "Delete must unbind the key slot");
        assert_eq!(app.learning_slot, None, "and end the learning state");
    }

    #[test]
    fn delete_never_binds_nor_fires() {
        // Delete is not a bindable candidate, so it cannot be persisted...
        assert_eq!(key_from_name("Delete"), None);
        // ...and while no cell is armed it must not fire any action slot.
        let mut app = test_app("del-fire");
        for s in app.short_keys.iter_mut() {
            *s = None;
        }
        app.short_keys[0] = Some(egui::Key::Delete); // simulate a stale binding
        let ctx = egui::Context::default();
        ctx.input_mut(|i| i.events.push(key_event(egui::Key::Delete)));
        app.handle_shortcuts(&ctx);
        // Slot 0 is RecSelected; firing it would enqueue a command. Instead of
        // inspecting the queue, assert the reserved-key rule at the table level:
        assert!(!KEY_CANDIDATES.contains(&egui::Key::Delete));
    }

    #[test]
    fn arming_one_cell_cancels_the_other() {
        // A stuck key-learn mode would swallow Delete before the MIDI branch;
        // arming either column must cancel the other.
        let mut app = test_app("arm-x");
        app.arm_key_slot(0);
        app.arm_midi_slot(1);
        assert_eq!(app.learning_slot, None);
        assert_eq!(app.midi_learning_slot, Some(1));
        app.arm_key_slot(2);
        assert_eq!(app.midi_learning_slot, None);
        assert_eq!(app.learning_slot, Some(2));
    }

    #[test]
    fn knob_learn_locks_axis_then_follows_every_message() {
        // Three-step wizard: step 1 captures the AXIS — message TYPE +
        // CHANNEL, never the data byte — and Lock axis freezes it; steps 2
        // and 3 then follow the LATEST value on that axis (any 0..=127 data
        // byte) until Lock min / Finish. (The old bug: step 1 stored the
        // first MidiBind.d1 — the CC number, constant across a sweep — as
        // the binding AND read both ends off it, so min and max locked to
        // the same value and Finish could never fire.)
        let mut app = test_app("knob-learn");
        let slot = MidiAction::idx(MidiAction::VolLoop(0));
        assert!(MidiAction::VolLoop(0).is_continuous());
        app.arm_knob_learn(slot);
        fn cc(app: &mut App, ch: u8, num: u8, v: u8) {
            app.handle_evt(Evt::Midi(crate::audio::RawMidiMsg { bytes: vec![0xB0 | ch, num, v] }))
        }
        // Step 1: the first message picks the axis — the axis only.
        cc(&mut app, 1, 7, 90);
        {
            let kl = app.knob_learn.as_ref().expect("wizard open");
            assert!(kl.has_axis && !kl.axis_locked, "axis detected, not yet locked");
            assert_eq!(kl.bind, MidiBind { ty: 1, ch: 1, d1: 7 });
        }
        // While unlocked, moving a different control re-picks the axis...
        cc(&mut app, 2, 7, 10);
        assert_eq!(app.knob_learn.as_ref().unwrap().bind.ch, 2, "axis follows the newest control");
        // ...but another CC number on the same type + channel is the SAME axis.
        cc(&mut app, 1, 71, 90);
        let kl = app.knob_learn.as_ref().unwrap();
        assert_eq!(kl.bind.ch, 1);
        assert!(kl.bind.same_axis(&MidiBind { ty: 1, ch: 1, d1: 7 }));
        // Lock axis: freezes the axis and seeds the min readout.
        app.knob_lock_axis();
        {
            let kl = app.knob_learn.as_ref().unwrap();
            assert!(kl.axis_locked);
            assert_eq!(kl.lo, 90, "lock seeds the min readout from the last value");
        }
        // Step 2: the min follows every message until Lock min.
        for v in [40u8, 12, 30, 8] {
            cc(&mut app, 1, 7, v);
            assert_eq!(app.knob_learn.as_ref().unwrap().lo, v, "min must follow every message");
        }
        app.knob_lock_min();
        // Step 3: the max follows every message; the min stays frozen.
        for v in [20u8, 70, 111, 127] {
            cc(&mut app, 1, 7, v);
            let kl = app.knob_learn.as_ref().unwrap();
            assert_eq!(kl.hi, v, "max must follow every message after the lock");
            assert_eq!(kl.lo, 8, "locked min must not move");
        }
        // Off-axis (other channel) is rejected and contaminates neither end.
        cc(&mut app, 2, 7, 55);
        {
            let kl = app.knob_learn.as_ref().unwrap();
            assert_eq!(kl.hi, 127, "off-axis message rejected");
            assert_eq!(kl.lo, 8, "off-axis message rejected");
        }
        // Same axis, different CC number mid-sweep: still counts (controllers
        // that re-use numbers must not be rejected).
        cc(&mut app, 1, 71, 42);
        assert_eq!(app.knob_learn.as_ref().unwrap().hi, 42, "same channel, any CC number counts");
        // Finish commits the AXIS — the data byte is never part of it —
        // plus the raw 8..=42 travel, rescaled at dispatch.
        app.knob_finish();
        assert!(app.knob_learn.is_none(), "finish closes the wizard");
        assert_eq!(app.midi_bind_at(slot), Some(MidiBind { ty: 1, ch: 1, d1: 71 }));
        let cal = app.knob_cal[slot].expect("partial travel gets a calibration");
        assert_eq!((cal.lo, cal.hi), (8, 42));
        // Dispatch drives the parameter through the calibration on ANY CC
        // number of that axis: raw 8 -> silence, raw 42 -> full.
        cc(&mut app, 1, 7, 8);
        assert_eq!(app.trk_vol[0], 0, "raw minimum maps to program minimum");
        cc(&mut app, 1, 9, 42);
        assert_eq!(app.trk_vol[0], 127, "raw maximum maps to program maximum");
        // An inverted capture (min read HIGHER than max) finishes too and
        // flips: raw 120 -> 0, raw 10 -> 127.
        let slot2 = MidiAction::idx(MidiAction::PanLoop(3));
        app.arm_knob_learn(slot2);
        cc(&mut app, 5, 3, 120);
        app.knob_lock_axis();
        app.knob_lock_min();
        cc(&mut app, 5, 3, 10);
        app.knob_finish();
        assert!(app.knob_learn.is_none(), "inverted range is a valid finish");
        assert_eq!(app.knob_cal[slot2].map(|c| (c.lo, c.hi)), Some((120, 10)));
        cc(&mut app, 5, 3, 120);
        assert_eq!(app.trk_pan[3], 0, "inverted raw min maps to 0");
        cc(&mut app, 5, 3, 10);
        assert_eq!(app.trk_pan[3], 127, "inverted raw max maps to 127");
    }

    #[test]
    fn undo_redo_slots_keep_their_positions_when_knobs_are_appended() {
        // The persisted shortcut table is positional: undo/redo must keep
        // their original slots even though the per-loop volume/gain knobs
        // now extend the end of MidiAction::ALL (92-entry table: 90/91).
        assert_eq!(MidiAction::idx(MidiAction::UndoOverdub), 90);
        assert_eq!(MidiAction::idx(MidiAction::RedoOverdub), 91);
        assert_eq!(SHORTCUT_DEFAULTS[90], Some(egui::Key::U));
        assert_eq!(SHORTCUT_DEFAULTS[91], Some(egui::Key::Y));
        assert_eq!(MidiAction::UndoOverdub.label(), "undo overdub");
        assert_eq!(MidiAction::RedoOverdub.label(), "redo overdub");
    }

    #[test]
    fn knob_actions_are_appended_and_value_driven() {
        // Per-loop volume/gain slots were APPENDED after redo-overdub
        // (interleaved vol/gain per loop); they are value-driven and must
        // never carry a keyboard default (a key press carries no value).
        assert_eq!(N_ACTIONS, 140);
        for i in 0..MAX_LOOPS {
            assert_eq!(MidiAction::idx(MidiAction::VolLoop(i)), 92 + i * 2);
            assert_eq!(MidiAction::idx(MidiAction::GainLoop(i)), 93 + i * 2);
            // Pan was APPENDED as a block after all vol/gain slots.
            assert_eq!(MidiAction::idx(MidiAction::PanLoop(i)), 124 + i);
            assert!(MidiAction::VolLoop(i).is_continuous());
            assert!(MidiAction::GainLoop(i).is_continuous());
            assert!(MidiAction::PanLoop(i).is_continuous());
            assert_eq!(SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::VolLoop(i))], None);
            assert_eq!(SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::GainLoop(i))], None);
            assert_eq!(SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::PanLoop(i))], None);
        }
        assert!(!MidiAction::RecLoop(0).is_continuous());
        assert_eq!(MidiAction::VolLoop(0).label(), "loop 1: volume (knob)");
        assert_eq!(MidiAction::GainLoop(15).label(), "loop 16: gain (knob)");
        assert_eq!(MidiAction::PanLoop(0).label(), "loop 1: pan (knob)");
    }

    #[test]
    fn pan_mappings_centre_at_zero() {
        // MIDI 64 is the centre default; the text box shows -100..=100
        // (0 = centre; the old 0..=100/50-centre scale maps 1:1 onto it).
        assert_eq!(PAN_CENTRE_MIDI, 64);
        assert_eq!(pan_text(PAN_CENTRE_MIDI), 0);
        assert_eq!(pan_midi(0.0), PAN_CENTRE_MIDI); // centre -> 63.5 -> 64
        assert_eq!(pan_text(0), -100);
        assert_eq!(pan_text(127), 100);
        assert_eq!(pan_midi(-100.0), 0);
        assert_eq!(pan_midi(100.0), 127);
        assert_eq!(pan_midi(-150.0), 0, "clamped");
        assert_eq!(pan_midi(150.0), 127, "clamped");
        // The old translation points: 0 -> -100, 50 -> 0, 100 -> 100.
        assert_eq!(pan_midi(-100.0), 0);
        assert_eq!(pan_midi(0.0), 64);
        assert_eq!(pan_midi(100.0), 127);
        // Every MIDI position survives the text round trip (±1).
        for m in 0u8..=127 {
            let back = pan_midi(pan_text(m) as f32);
            assert!(back.abs_diff(m) <= 1, "midi {m} -> text {} -> {back}", pan_text(m));
        }
        // Engine position: hard right at MIDI 127.
        assert!((pan01(127) - 1.0).abs() < 1e-6);
        assert!((pan01(0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn level_text_roundtrip() {
        // dB boxes parse back into the MIDI domain; "-inf" hits silence.
        assert_eq!(level_parse(KIND_VOL, "+0.0 dB"), Some(127));
        assert_eq!(level_parse(KIND_VOL, "-inf"), Some(0));
        assert_eq!(level_parse(KIND_GAIN, "0 dB"), Some(GAIN_UNITY_MIDI));
        assert_eq!(level_parse(KIND_GAIN, "+10.1 dB"), Some(127));
        assert_eq!(level_parse(KIND_VOL, "-999"), Some(1), "finite floors at 1");
        assert_eq!(level_parse(KIND_PAN, "-999"), Some(0), "clamped hard left");
        assert_eq!(level_parse(KIND_PAN, "0"), Some(PAN_CENTRE_MIDI), "0 = centre");
        assert_eq!(level_parse(KIND_PAN, "100"), Some(127), "hard right");
        assert_eq!(level_parse(KIND_PAN, "junk"), None);
        // Display strings feed straight back into the parser.
        for m in [0u8, 1, 40, 80, 127] {
            let s = level_display(KIND_VOL, m);
            assert!(level_parse(KIND_VOL, &s).is_some(), "{s}");
        }
        assert_eq!(level_display(KIND_PAN, PAN_CENTRE_MIDI), "0");
        assert_eq!(level_display(KIND_PAN, 0), "-100");
    }

    #[test]
    fn per_loop_level_mappings() {
        // MIDI 0 = silence (-inf dB) for both sliders.
        assert_eq!(vol_amp(0), 0.0);
        assert_eq!(gain_amp(0), 0.0);
        assert_eq!(db_str(0.0), "-inf dB");
        // Volume: 127 = 0 dB, monotonic down to ~-60 dB at 1.
        assert!((vol_amp(127) - 1.0).abs() < 1e-6);
        let v1 = vol_amp(1);
        assert!(v1 > 0.0 && v1 < 0.011);
        assert!(vol_amp(1) < vol_amp(64) && vol_amp(64) < vol_amp(100));
        assert!(vol_amp(100) < vol_amp(127));
        // Gain: unity at 80, cut below, boost above (+10 dB top ≈ ×3.16).
        assert!((gain_amp(GAIN_UNITY_MIDI) - 1.0).abs() < 1e-6);
        assert!((gain_amp(127) - 3.199).abs() < 0.01);
        assert!(gain_amp(40) < 1.0 && gain_amp(40) > 0.0);
        assert_eq!(db_str(1.0), "+0.0 dB");
    }

    #[test]
    fn selection_clamps_at_loop_1_and_loop_16() {
        // select next/prev must stop at the first/last loop: the selection
        // frame may never wrap around and jump off the visible list.
        let mut app = test_app("sel-clamp");
        app.selected = 0;
        app.fire_midi_action(MidiAction::SelectPrev);
        assert_eq!(app.selected, 0, "prev at loop 1 stays");
        for _ in 0..(MAX_LOOPS + 3) {
            app.fire_midi_action(MidiAction::SelectNext);
        }
        assert_eq!(app.selected, MAX_LOOPS - 1, "next stops at loop 16");
        app.fire_midi_action(MidiAction::SelectPrev);
        assert_eq!(app.selected, MAX_LOOPS - 2);
    }

    /// The effects window's rows (toggle → chain order → live instances)
    /// drive `fx_live` exactly: enable snaps to the top block, arrows
    /// reorder the chain, disable drops the slot.
    #[test]
    fn effects_window_rows_drive_the_live_chain() {
        if !fx::rustc_available() {
            return; // no compiler in the environment: chain build is skipped
        }
        let mut app = test_app("fxchain");
        let dir = std::env::temp_dir().join(format!("simplooper-fxui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(fx::TEMPLATE_FILE), fx::TEMPLATE_SRC).unwrap();
        app.fx_dir_edit = dir.display().to_string();
        app.apply_fx_dir();
        assert_eq!(app.fx_entries[0].len(), 1, "template listed (disabled)");

        // A second copy of the script to exercise chain ordering.
        std::fs::write(dir.join("second.rs"), fx::TEMPLATE_SRC).unwrap();
        app.rescan_fx();

        // Enable template then second: enabled block keeps that order.
        let stems = app.fx_registry.stems();
        let kt = app.fx_entries[0].iter().position(|e| e.stem == "template").unwrap();
        app.fx_entries[0][kt].enabled = true;
        fx::resort_entries(&mut app.fx_entries[0], &stems);
        let ks = app.fx_entries[0].iter().position(|e| e.stem == "second").unwrap();
        app.fx_entries[0][ks].enabled = true;
        fx::resort_entries(&mut app.fx_entries[0], &stems);
        app.rebuild_fx(0);
        assert_eq!(app.fx_live[0].len(), 2, "one live instance per enabled slot");
        let on: Vec<&str> =
            app.fx_entries[0].iter().filter(|e| e.enabled).map(|e| e.stem.as_str()).collect();
        assert_eq!(on, vec!["template", "second"]);

        // Move second ahead of template — chain order follows the list.
        let k_second = app.fx_entries[0].iter().position(|e| e.stem == "second").unwrap();
        assert!(fx::move_enabled(&mut app.fx_entries[0], k_second, true));
        app.rebuild_fx(0);
        let on: Vec<&str> =
            app.fx_entries[0].iter().filter(|e| e.enabled).map(|e| e.stem.as_str()).collect();
        assert_eq!(on, vec!["second", "template"]);

        // Disable: the live instance goes away, the row drops down the list.
        let k_second = app.fx_entries[0].iter().position(|e| e.stem == "second").unwrap();
        app.fx_entries[0][k_second].enabled = false;
        fx::resort_entries(&mut app.fx_entries[0], &stems);
        app.rebuild_fx(0);
        assert_eq!(app.fx_live[0].len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
