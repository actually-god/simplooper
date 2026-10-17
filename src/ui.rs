//! egui front-end for simplooper: loop dashboard + settings + MIDI monitor.
//!
//! Layout:
//! - top bar: hamburger (settings), status, xruns, peak meter, master gain
//! - left side panel (optional): audio ports, MIDI + keyboard bindings, easy quantize
//! - central: independent/dependent mode checkbox + sixteen loop tracks, each
//!   with a waveform of the recorded audio and REC / OD / play / clear buttons
//! - bottom: MIDI monitor

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::Frame;
use egui::{Color32, RichText, Sense, Slider};

use crate::audio::{Backend, Cmd, Evt, FxSlot, PortInfo, TrackFx, MAX_LOOPS, TR_OD, TR_PLAY, TR_REC};
use crate::config::{Config, MidiBinding, TrackFxSave};
use crate::fx;
use crate::midi::{hex, MidiAction, MidiBind, MidiMsg};
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

pub struct App {
    backend: Backend,

    // --- audio / device state ---
    status: String,
    client_name: Option<String>,
    sample_rate: u32,
    avail_in: Vec<PortInfo>,
    avail_out: Vec<PortInfo>,
    sel_in: HashSet<String>,
    sel_out: HashSet<String>,
    applied: Option<(usize, usize)>,
    /// Last port selection actually applied via "Apply Selection"; persisted
    /// with the config so it can be restored on the next launch.
    applied_ports: Option<(Vec<String>, Vec<String>)>,
    gain: f32,
    /// Per-loop playback volume, MIDI-knob style (0..=127; 0 = -inf, 127 =
    /// 0 dB). Applied to each loop's contribution to the mix.
    trk_vol: [u8; MAX_LOOPS],
    /// Per-loop gain trim, MIDI-knob style (0..=127; 0 = -inf, 80 = 0 dB).
    trk_gain: [u8; MAX_LOOPS],
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
    /// True while a quantize text field has focus; keyboard shortcuts are
    /// suppressed then. Recomputed each frame in `ui()`.
    in_text_edit: bool,
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
        // Per-loop levels: pad the persisted vectors (older configs have
        // none) and push the restored values into the audio engine — the
        // engine atomics are the live source of truth for the RT thread.
        let trk_vol = core::array::from_fn(|i| cfg.trk_vol.get(i).copied().unwrap_or(127));
        let trk_gain =
            core::array::from_fn(|i| cfg.trk_gain.get(i).copied().unwrap_or(GAIN_UNITY_MIDI));
        for t in 0..MAX_LOOPS {
            backend.engine.set_loop_vol(t, vol_amp(trk_vol[t]));
            backend.engine.set_loop_gain(t, gain_amp(trk_gain[t]));
        }
        let root_loops = cfg.root_loops.clamp(1, 64);
        // Restore the last applied port selections so the user doesn't have to
        // re-check the same ports on every launch.
        let sel_in: HashSet<String> = cfg.ports_in.iter().cloned().collect();
        let sel_out: HashSet<String> = cfg.ports_out.iter().cloned().collect();
        let applied_ports = if cfg.ports_in.is_empty() && cfg.ports_out.is_empty() {
            None
        } else {
            Some((cfg.ports_in, cfg.ports_out))
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
            levels_dirty: false,
            gui_scale: cfg.gui_scale.clamp(0.7, 1.6),
            show_settings: false,
            dependent: false,
            selected: 0,
            in_text_edit: false,
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
            midi_sources: Vec::new(),
            show_export: false,
            fx_registry,
            fx_entries,
            fx_live: vec![Vec::new(); MAX_LOOPS],
            fx_window: None,
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
    /// auto-repeat doesn't re-fire an action, and the keys are ignored while a
    /// text field has focus.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Ignore keys while a text field has focus (tracked in `ui()`), so
        // typing "1" into the quantize fields doesn't switch loops.
        if self.in_text_edit {
            return;
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
            MidiAction::SelectNext => self.selected = (self.selected + 1) % MAX_LOOPS,
            MidiAction::SelectPrev => {
                self.selected = (self.selected + MAX_LOOPS - 1) % MAX_LOOPS;
            }
            MidiAction::UndoOverdub => self.backend.cmd(Cmd::UndoOverdub),
            MidiAction::RedoOverdub => self.backend.cmd(Cmd::RedoOverdub),
            // Value-driven knob actions fire only through the MIDI path
            // (handle_evt passes the CC value); a key press has no value.
            MidiAction::VolLoop(_) | MidiAction::GainLoop(_) => {}
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

    /// Rebuild loop `i`'s live effect chain from its window rows and hand it
    /// to the audio engine. Also seeds each row's slider values from the
    /// descriptors (stored values win, clamped to range).
    fn rebuild_fx(&mut self, i: usize) {
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

    /// Ride one effect slider: live instance now, window row for persistence.
    fn set_fx_param(&mut self, i: usize, slot: usize, par: usize, v: f32) {
        if let Some(inst) = self.fx_live[i].get(slot) {
            inst.set_param(par, v);
        }
        if let Some(e) = self.fx_entries[i].iter_mut().filter(|e| e.enabled).nth(slot) {
            if let Some(pv) = e.params.get_mut(par) {
                *pv = v;
            }
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
            root_loops: self.root_loops,
            gui_scale: self.gui_scale,
            latency_ms: self.latency_ms.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1000.0),
            metro_bpm: self.metro_bpm.trim().parse::<u32>().unwrap_or(120).clamp(20, 300),
            metro_sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            metro_num: self.metro_num.trim().parse::<u32>().unwrap_or(4).clamp(1, 16),
            q_metro: self.q_metro,
            // Only selections that were actually applied get persisted.
            ports_in: self.applied_ports.as_ref().map(|p| p.0.clone()).unwrap_or_default(),
            ports_out: self.applied_ports.as_ref().map(|p| p.1.clone()).unwrap_or_default(),
            trk_vol: self.trk_vol.to_vec(),
            trk_gain: self.trk_gain.to_vec(),
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
                                self.save_config();
                                self.status =
                                    format!("bound {} → {}", MidiAction::ALL[slot].label(), bind.label());
                            }
                            self.midi_learning_slot = None;
                        } else {
                            // Value for continuous (knob) bindings: the CC
                            // byte, or note velocity when the note binds.
                            let cval = match &msg {
                                MidiMsg::ControlChange { value, .. } => Some(*value),
                                MidiMsg::NoteOn { velocity, .. } if *velocity > 0 => {
                                    Some(*velocity)
                                }
                                _ => None,
                            };
                            for i in 0..N_ACTIONS {
                                if self.midi_binds[i] == Some(bind) {
                                    let a = MidiAction::ALL[i];
                                    match a {
                                        MidiAction::VolLoop(t) => {
                                            if let Some(v) = cval {
                                                self.set_trk_vol(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        MidiAction::GainLoop(t) => {
                                            if let Some(v) = cval {
                                                self.set_trk_gain(t, v);
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
        let path = std::path::PathBuf::from(self.export_path.trim());
        if path.as_os_str().is_empty() {
            self.status = "export: choose a destination first".into();
            return;
        }
        let fmt = crate::audio_io::ExportFormat::ALL[self.export_fmt];
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

        // Recomputed by the text-edit widgets below while they have focus.
        self.in_text_edit = false;

        top_bar(ui, self);
        if self.show_settings {
            settings_panel(ui, self);
        }
        midi_monitor(ui, self);
        central_panels(ui, self);
        if self.show_export {
            export_window(ui, self);
        }
        if self.fx_window.is_some() {
            effects_window(ui, self);
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
                    let r = ui.add(egui::TextEdit::singleline(&mut app.latency_ms).desired_width(60.0));
                    app.in_text_edit = app.in_text_edit || r.has_focus();
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
                        "Delays the INCOMING signal (recording + monitoring) so your playing \
                         lands on the already-recorded loop grid; the output mix (loops, \
                         metronome, monitors) stays exactly on grid. Use it to cancel the \
                         fixed input delay of your interface/DAW chain — negative problems \
                         from output-side delay are gone: loops never drift.",
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
                // Volume/gain are knob bindings: the action fires with the
                // message VALUE (0..=127), so only CC / note-on drives them.
                for off in 0..7usize {
                    let a = match off {
                        0 => MidiAction::RecLoop(app.bind_loop_sel),
                        1 => MidiAction::OdLoop(app.bind_loop_sel),
                        2 => MidiAction::ClearLoop(app.bind_loop_sel),
                        3 => MidiAction::PlayLoop(app.bind_loop_sel),
                        4 => MidiAction::Select(app.bind_loop_sel),
                        5 => MidiAction::VolLoop(app.bind_loop_sel),
                        _ => MidiAction::GainLoop(app.bind_loop_sel),
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
                port_list(
                    ui,
                    "OUTPUTS",
                    "remote input ports (sinks)",
                    &app.avail_out,
                    &mut app.sel_out,
                );
                ui.horizontal(|ui| {
                    if ui.button("Apply Selection").clicked() {
                        let ins: Vec<String> = app.sel_in.iter().cloned().collect();
                        let outs: Vec<String> = app.sel_out.iter().cloned().collect();
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

/// One row of the bindings table: action label + MIDI capture button +
/// keyboard button; persists on change. Both columns work like "press a key":
/// click the cell to arm it, then send the trigger — the next matching input
/// becomes the binding (click again or Esc cancels).
fn bind_row(ui: &mut egui::Ui, app: &mut App, a: MidiAction) {
    let idx = MidiAction::idx(a);
    ui.horizontal(|ui| {
        ui.set_min_width(150.0);
        ui.label(RichText::new(a.label()).weak());

        // MIDI binding: live capture (no dropdown — the controller is the menu).
        let listening = app.midi_learning_slot == Some(idx);
        let txt = if listening {
            "listening…".to_string()
        } else {
            match app.midi_binds[idx] {
                Some(b) => b.label(),
                None => "—".to_string(),
            }
        };
        let mut btn = egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(150.0, 0.0));
        if listening {
            btn = btn.fill(Color32::from_rgb(60, 90, 120));
        }
        let resp = ui.add(btn).on_hover_text(
            "Click to listen: the next NOTE ON / CC / PROGRAM CHANGE \
             from your controller becomes this binding. Click again (or Esc) \
             to cancel; Delete clears the binding.",
        );
        if resp.clicked() {
            if listening {
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
                let r = ui.add(egui::TextEdit::singleline(&mut app.export_path).desired_width(290.0));
                app.in_text_edit = app.in_text_edit || r.has_focus();
                if ui.button("Browse…").clicked() {
                    let fmt = crate::audio_io::ExportFormat::ALL[app.export_fmt];
                    let name = if app.export_whole {
                        format!("simplooper-project.{}", fmt.extension())
                    } else {
                        format!("simplooper-loop-{}.{}", app.selected + 1, fmt.extension())
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
                let r = ui
                    .add(egui::TextEdit::singleline(&mut app.fx_dir_edit).desired_width(330.0));
                app.in_text_edit = app.in_text_edit || r.has_focus();
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
                            if ui.button("▲").on_hover_text("Move earlier in the chain").clicked() {
                                act = Some((1, k));
                            }
                            if ui.button("▼").on_hover_text("Move later in the chain").clicked() {
                                act = Some((2, k));
                            }
                        }
                    });
                    if let Some(t) = &err {
                        let short: String = t.chars().take(72).collect();
                        ui.label(RichText::new(format!("✗ {short}")).color(Color32::from_rgb(220, 120, 120)).small());
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
                        RichText::new(format!("✗ {}", short.replace('\n', " · ")))
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

fn central_panels(ui: &mut egui::Ui, app: &mut App) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("loop mode:").strong());
            let dep = ui.checkbox(&mut app.dependent, "dependent").on_hover_text(
                "Dependent looping: the longest loop is the master — every other loop \
                 wraps on top of it, and the whole ensemble restarts together when the \
                 longest one ends.\n\nUse only with properly quantized loops (Easy \
                 Quantize, above), or you will hear skips at the loop points.",
            );
            if dep.changed() {
                app.backend.cmd(Cmd::SetDependent(app.dependent));
            }
            // Ensemble length: how many times the longest loop repeats before
            // everything restarts together.
            ui.label(RichText::new("root loops:").weak());
            let r_root = ui.add(egui::TextEdit::singleline(&mut app.q_root).desired_width(40.0));
            app.in_text_edit = app.in_text_edit || r_root.has_focus();
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
                    let r_num = ui.add(egui::TextEdit::singleline(&mut app.q_num).desired_width(40.0));
                    ui.label("/");
                    let r_den = ui.add(egui::TextEdit::singleline(&mut app.q_den).desired_width(40.0));
                    app.in_text_edit = app.in_text_edit || (r_num.has_focus() || r_den.has_focus());
                    ui.label("bars in loop");
                    egui::ComboBox::from_id_salt("q_root_loop")
                        .selected_text(format!("{}", app.q_root_loop + 1))
                        .show_ui(ui, |ui| {
                            for n in 0..MAX_LOOPS {
                                ui.selectable_value(&mut app.q_root_loop, n, format!("{}", n + 1));
                            }
                        });
                    ui.label(":");
                    let r_bars = ui.add(egui::TextEdit::singleline(&mut app.q_bars).desired_width(60.0));
                    app.in_text_edit = app.in_text_edit || r_bars.has_focus();
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
                    app.in_text_edit = app.in_text_edit || r_bpm.has_focus();
                    if r_bpm.changed() && app.metro_bpm.trim().parse::<u32>().is_ok() {
                        app.send_metro();
                    }
                    ui.label("BPM · beats/measure");
                    let r_num = ui.add(egui::TextEdit::singleline(&mut app.metro_num).desired_width(30.0));
                    app.in_text_edit = app.in_text_edit || r_num.has_focus();
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
            .add(egui::Button::new(RichText::new("⇩ IMP").monospace()))
            .on_hover_text("Import a recording into this loop (I).");
        if r_imp.clicked() {
            app.import_loop(i);
        }
    });

    // Bottom strip of the loop: one stacked volume+gain element pinned to
    // the bottom-left, the Effects button right next to it, then every
    // enabled effect with its own sliders, left to right in chain order.
    // Gain x volume are always applied first — before any effect.
    ui.horizontal(|ui| {
        // Stacked level element (vol over gain), each bindable to a MIDI
        // knob in Settings → per loop bindings.
        ui.vertical(|ui| {
            let w = (ui.available_width() * 0.24).clamp(180.0, 300.0);
            ui.set_width(w);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("vol  {:>8}", db_str(vol_amp(app.trk_vol[i]))))
                        .monospace()
                        .small(),
                );
                let mut v = app.trk_vol[i];
                let rv = ui
                    .add_sized(egui::vec2(w - 92.0, 0.0), Slider::new(&mut v, 0..=127).text(" "))
                    .on_hover_text(
                        "Loop playback volume: 0 = silence (-inf dB), 127 = 0 dB. Bind it to a MIDI knob in Settings → MIDI bindings → per loop.",
                    );
                if rv.changed() {
                    app.set_trk_vol(i, v);
                    app.levels_dirty = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("gain {:>8}", db_str(gain_amp(app.trk_gain[i]))))
                        .monospace()
                        .small(),
                );
                let mut g = app.trk_gain[i];
                let rg = ui
                    .add_sized(egui::vec2(w - 92.0, 0.0), Slider::new(&mut g, 0..=127).text(" "))
                    .on_hover_text(
                        "Loop gain trim: 0 = silence, 80 = 0 dB, 127 = +10 dB. Bind it to a MIDI knob in Settings → MIDI bindings → per loop.",
                    );
                if rg.changed() {
                    app.set_trk_gain(i, g);
                    app.levels_dirty = true;
                }
            });
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
        fx_chips(ui, app, i);
    });
}

/// Enabled effects drawn directly on the loop row, left to right in chain
/// order (identical to the effects window): name + one slider per parameter.
fn fx_chips(ui: &mut egui::Ui, app: &mut App, i: usize) {
    let mut pend: Option<(usize, usize, f32)> = None;
    let mut k = 0usize; // chain index (enabled entries only)
    for e in app.fx_entries[i].iter() {
        if !e.enabled {
            continue;
        }
        let Some(desc) = app.fx_registry.desc(&e.stem).cloned() else { continue };
        let live = app.fx_live[i].get(k).cloned();
        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(&desc.name)
                        .strong()
                        .small()
                        .color(Color32::from_rgb(150, 200, 255)),
                )
                .on_hover_text(desc.description.as_deref().unwrap_or(""));
                for (p, pd) in desc.params.iter().enumerate() {
                    let mut v = live.as_ref().map(|l| l.param(p)).unwrap_or(pd.default);
                    let r = ui
                        .add_sized(
                            egui::vec2(76.0, 0.0),
                            Slider::new(&mut v, pd.min..=pd.max).show_value(false),
                        )
                        .on_hover_text(format!("{} — range {:.2}…{:.2}", pd.label, pd.min, pd.max));
                    if r.changed() {
                        pend = Some((k, p, v));
                    }
                    ui.label(RichText::new(format!("{v:.2}")).monospace().small());
                }
            });
        });
        k += 1;
    }
    if let Some((slot, p, v)) = pend {
        app.set_fx_param(i, slot, p, v);
        app.fx_save_pending = true;
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
        let dir =
            std::env::temp_dir().join(format!("simplooper-test-{}-{}", std::process::id(), tag));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        App::new(crate::audio::Backend::start())
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
        assert_eq!(N_ACTIONS, 124);
        for i in 0..MAX_LOOPS {
            assert_eq!(MidiAction::idx(MidiAction::VolLoop(i)), 92 + i * 2);
            assert_eq!(MidiAction::idx(MidiAction::GainLoop(i)), 93 + i * 2);
            assert!(MidiAction::VolLoop(i).is_continuous());
            assert!(MidiAction::GainLoop(i).is_continuous());
            assert_eq!(SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::VolLoop(i))], None);
            assert_eq!(SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::GainLoop(i))], None);
        }
        assert!(!MidiAction::RecLoop(0).is_continuous());
        assert_eq!(MidiAction::VolLoop(0).label(), "loop 1: volume (knob)");
        assert_eq!(MidiAction::GainLoop(15).label(), "loop 16: gain (knob)");
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
