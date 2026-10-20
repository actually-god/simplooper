//! Persistent configuration: rebindable keyboard shortcuts, MIDI bindings,
//! per-track mixer levels, metronome/grid settings and folder locations.
//!
//! Stored as JSON at `$XDG_CONFIG_HOME/slooper/config.json` (falling back to
//! `~/.config/slooper/config.json`). A missing or corrupt file yields
//! defaults; saving is best-effort (UI keeps working if the write fails).
//! Unknown fields are ignored and removed fields (root loops, the old
//! metronome-quantize toggle) simply drop out, so configs from any older
//! build load cleanly.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::midi::{MidiAction, MidiBind};

/// One saved MIDI binding: an action plus its trigger (None = unbound).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiBinding {
    pub action: MidiAction,
    pub bind: Option<MidiBind>,
}

/// Axis calibration for one continuous (knob/slider) binding slot: the raw
/// controller values captured when the user swept the control to its minimum
/// and maximum. Values between are scaled onto the program's 0..=127 space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiAxisCal {
    pub slot: u16,
    pub lo: u8,
    pub hi: u8,
}

/// One enabled effect in a track's saved chain (script stem + slider values,
/// in chain order). The stem is the key: dropping scripts is always safe.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackFxSave {
    pub stem: String,
    #[serde(default)]
    pub params: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Keyboard shortcuts as key names (e.g. `"R"`, `"Space"`), one per entry
    /// of `MidiAction::ALL` (same order); null = unbound. Tables from older
    /// layouts are remapped on load (`midi::migrate_shortcuts`).
    #[serde(default)]
    pub shortcuts: Vec<Option<String>>,
    /// MIDI bindings (only bound actions are stored).
    #[serde(default)]
    pub midi: Vec<MidiBinding>,
    /// Axis calibrations for continuous bindings (knob min/max sweep).
    /// Slots without an entry use the controller's full 0..=127 travel.
    #[serde(default)]
    pub midi_axis: Vec<MidiAxisCal>,
    /// Last applied audio port selections — restored on the next launch so the
    /// user doesn't have to re-check the same ports every time.
    #[serde(default)]
    pub ports_in: Vec<String>,
    #[serde(default)]
    pub ports_out: Vec<String>,
    /// Stereo routing for `ports_out` (same order): bit 0 = left bus,
    /// bit 1 = right bus (3 = mono downmix). Missing/short vectors fall
    /// back to 3 — pre-stereo configs keep their exact mono behaviour.
    #[serde(default)]
    pub ports_out_lr: Vec<u8>,
    /// GUI zoom factor (egui `zoom_factor`): 1.0 = default UI size, >1 larger.
    #[serde(default = "default_gui_scale")]
    pub gui_scale: f32,
    /// Latency compensation in milliseconds: each fresh recording discards
    /// this much input at its start, so the button click that started the
    /// take lands exactly at the take's first sample. Monitoring and the
    /// output mix are never delayed.
    #[serde(default)]
    pub latency_ms: f32,
    /// Per-track playback volume, MIDI-knob style (0..=127; 0 = -inf, 127 =
    /// 0 dB). Missing entries fall back to 127.
    #[serde(default)]
    pub trk_vol: Vec<u8>,
    /// Per-track grid block height in points, written when the user drags a
    /// separation bar (0 = automatic: lanes × lane height). Missing entries
    /// fall back to automatic.
    #[serde(default)]
    pub trk_h: Vec<f32>,
    /// Per-track gain trim, MIDI-knob style (0..=127; 0 = -inf, 80 = 0 dB,
    /// 127 = +10 dB). Missing entries fall back to 80 (unity).
    #[serde(default)]
    pub trk_gain: Vec<u8>,
    /// Per-track stereo pan, MIDI-knob style (0..=127; 0 = hard left,
    /// 64 = centre, 127 = hard right). Missing entries fall back to 64.
    #[serde(default)]
    pub trk_pan: Vec<u8>,
    /// Metronome tempo in BPM — also the tempo of the project grid.
    #[serde(default = "default_metro_bpm")]
    pub metro_bpm: u32,
    /// Metronome click subdivision as clicks per beat (1 = quarter, 2 = eighth).
    #[serde(default = "default_metro_sub")]
    pub metro_sub: u32,
    /// Metronome beats per measure (time-signature numerator); the first
    /// quarter note of every measure gets the downbeat tone and defines the
    /// measure the timeline grid (and the ruler) is drawn on.
    #[serde(default = "default_metro_num")]
    pub metro_num: u32,
    /// Quantize grid: evenly-spaced steps per beat (index into the UI's
    /// SUBDIVISIONS table; see `quantize::unit_samples`).
    #[serde(default = "default_grid_sub")]
    pub q_sub: u32,
    /// Drag/drop snap grid: evenly-spaced steps per beat.
    #[serde(default = "default_grid_sub")]
    pub snap_sub: u32,
    /// Snap freshly recorded takes' placement to the quantize grid.
    #[serde(default = "default_true")]
    pub auto_quantize: bool,
    /// Timeline loop toggle: wrap the playhead at the end of the loop
    /// region (see `loop_mode` below).
    #[serde(default = "default_true")]
    pub loop_tl: bool,
    /// Loop region mode: 0 = area (dragged handles on the ruler),
    /// 1 = current longest (0 → last clip end), 2 = whole song.
    #[serde(default = "default_loop_mode")]
    pub loop_mode: usize,
    /// Area loop bounds in samples (mode 0; end 0 = "not set yet" → 2 bars
    /// once the sample rate is known).
    #[serde(default)]
    pub loop_area_start: u64,
    #[serde(default)]
    pub loop_area_end: u64,
    /// Master output volume, MIDI range (0 = -inf, 115 = 0 dB, 127 = +6 dB
    /// — the same `vol_amp` mapping every track volume uses).
    #[serde(default = "default_master_vol")]
    pub master_vol: u8,
    /// False only in configs written before 0.3.0, where volume was
    /// MIDI-linear (127 = 0 dB). `load` converts the stored faders once.
    #[serde(default)]
    pub vol_scale_v2: bool,
    /// Where `.slooper` project files live (empty = `~/slooper-projects`).
    #[serde(default)]
    pub project_dir: String,
    /// Asset rack folders, in order. The first entry is the *primary* folder:
    /// recordings are written there.
    #[serde(default)]
    pub rack_folders: Vec<String>,
    /// Effects folder ("" = default: `effects/` next to the installed
    /// package). The folder is created and seeded on first use.
    #[serde(default)]
    pub fx_dir: String,
    /// Per-track effect chains: index = track, entries = enabled scripts in
    /// chain order (each stem + its slider values).
    #[serde(default)]
    pub track_fx: Vec<Vec<TrackFxSave>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shortcuts: Vec::new(),
            midi: Vec::new(),
            midi_axis: Vec::new(),
            ports_in: Vec::new(),
            ports_out: Vec::new(),
            ports_out_lr: Vec::new(),
            gui_scale: default_gui_scale(),
            latency_ms: 0.0,
            trk_vol: Vec::new(),
            trk_h: Vec::new(),
            trk_gain: Vec::new(),
            trk_pan: Vec::new(),
            metro_bpm: default_metro_bpm(),
            metro_sub: default_metro_sub(),
            metro_num: default_metro_num(),
            q_sub: default_grid_sub(),
            snap_sub: default_grid_sub(),
            auto_quantize: default_true(),
            loop_tl: default_true(),
            loop_mode: default_loop_mode(),
            loop_area_start: 0,
            loop_area_end: 0,
            master_vol: default_master_vol(),
            vol_scale_v2: true,
            project_dir: String::new(),
            rack_folders: Vec::new(),
            fx_dir: String::new(),
            track_fx: Vec::new(),
        }
    }
}

fn default_gui_scale() -> f32 {
    1.0
}

/// "Current longest" is the default loop region mode (index 1 of
/// `["area", "current longest", "whole song"]`).
fn default_loop_mode() -> usize {
    1
}

/// Master volume default = 0 dB on the `vol_amp` MIDI mapping.
fn default_master_vol() -> u8 {
    115
}

/// Convert a pre-0.3 MIDI-linear fader (0 = silence, 127 = 0 dB, plain
/// linear amplitude) onto the current dB-scale MIDI value. Shared by the
/// config migration and the v1 project loader.
pub fn vol_midi_from_legacy(v: u8) -> u8 {
    if v == 0 {
        return 0;
    }
    let db = 20.0 * (v as f32 / 127.0).log10();
    crate::ui::vol_midi(db)
}

fn default_metro_bpm() -> u32 {
    120
}

fn default_metro_sub() -> u32 {
    1
}

fn default_metro_num() -> u32 {
    4
}

fn default_grid_sub() -> u32 {
    4
}

fn default_true() -> bool {
    true
}

impl Config {
    /// `$XDG_CONFIG_HOME/slooper/config.json` (or `~/.config/...`).
    pub fn path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"));
        base.join("slooper").join("config.json")
    }

    /// Load the config, falling back to defaults on any error.
    pub fn load() -> Self {
        let raw = std::fs::read_to_string(Self::path());
        let mut cfg: Self = match &raw {
            Ok(s) => match serde_json::from_str(s) {
                Ok(c) => c,
                Err(e) => {
                    // A torn/invalid write must never be silently replaced:
                    // keep it next to the config for recovery, start fresh.
                    let _ = std::fs::rename(Self::path(), Self::path().with_extension("corrupt"));
                    eprintln!("[slooper] config invalid ({e}) — saved as config.corrupt, using defaults");
                    return Self::default();
                }
            },
            Err(_) => return Self::default(),
        };
        if !cfg.vol_scale_v2 {
            // Pre-0.3 config: volumes were MIDI-linear (127 = 0 dB).
            cfg.master_vol = vol_midi_from_legacy(cfg.master_vol);
            for v in &mut cfg.trk_vol {
                *v = vol_midi_from_legacy(*v);
            }
            cfg.vol_scale_v2 = true;
            cfg.save().ok();
        }
        cfg
    }

    /// Save the config; creates the directory if needed.
    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&p, s).map_err(|e| e.to_string())
    }

    /// Where `.slooper` files live: the configured folder, or the
    /// `slooper-projects` folder in the user's home when unset.
    pub fn projects_dir(&self) -> PathBuf {
        if self.project_dir.trim().is_empty() {
            home_dir().join("slooper-projects")
        } else {
            PathBuf::from(self.project_dir.trim())
        }
    }

    /// MIDI binding for `action`, if any.
    pub fn midi_bind(&self, a: MidiAction) -> Option<MidiBind> {
        self.midi.iter().find(|b| b.action == a).and_then(|b| b.bind)
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_volume_faders_migrate_to_db_scale() {
        use crate::ui::VOL_UNITY_MIDI;
        // Old scale: 127 = 0 dB, linear amplitude. New: 115 = 0 dB.
        assert_eq!(vol_midi_from_legacy(127), VOL_UNITY_MIDI);
        assert_eq!(vol_midi_from_legacy(0), 0);
        assert_eq!(vol_midi_from_legacy(64), 104); // -5.94 dB
        // Monotone and never exceeds unity below 127.
        let mut prev = 0u8;
        for v in 0..=127u8 {
            let m = vol_midi_from_legacy(v);
            assert!(m >= prev, "not monotone at {v}");
            prev = m;
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg = Config {
            shortcuts: vec![Some("R".into()), None, Some("Space".into())],
            midi: vec![MidiBinding {
                action: MidiAction::RecLoop(0),
                bind: Some(MidiBind { ty: 0, ch: 1, d1: 60 }),
            }],
            midi_axis: Vec::new(),
            ports_in: vec!["remote:out_1".into()],
            ports_out: vec!["remote:in_1".into(), "remote:in_2".into()],
            ports_out_lr: vec![1, 2],
            gui_scale: 1.25,
            latency_ms: 12.5,
            metro_bpm: 96,
            metro_sub: 3,
            metro_num: 7,
            q_sub: 6,
            snap_sub: 2,
            auto_quantize: false,
            loop_tl: false,
            loop_mode: 1,
            loop_area_start: 24,
            loop_area_end: 192_000,
            master_vol: 100,
            vol_scale_v2: true,
            project_dir: "/tmp/projects".into(),
            rack_folders: vec!["/tmp/samples".into(), "/tmp/takes".into()],
            fx_dir: "/tmp/effects".into(),
            track_fx: vec![vec![TrackFxSave { stem: "template".into(), params: vec![0.5, 4.0] }]],
            trk_vol: vec![127, 100],
            trk_h: vec![120.0],
            trk_gain: vec![80, 60],
            trk_pan: vec![64, 20],
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.shortcuts, cfg.shortcuts);
        assert_eq!(back.midi, cfg.midi);
        assert!((back.gui_scale - 1.25).abs() < f32::EPSILON);
        assert!((back.latency_ms - 12.5).abs() < f32::EPSILON);
        assert_eq!(back.metro_bpm, 96);
        assert_eq!(back.metro_sub, 3);
        assert_eq!(back.metro_num, 7);
        assert_eq!(back.q_sub, 6);
        assert_eq!(back.snap_sub, 2);
        assert!(!back.auto_quantize && !back.loop_tl);
        assert_eq!(back.project_dir, "/tmp/projects");
        assert_eq!(back.rack_folders, cfg.rack_folders);
        assert_eq!(back.ports_in, cfg.ports_in);
        assert_eq!(back.ports_out, cfg.ports_out);
        assert_eq!(back.ports_out_lr, cfg.ports_out_lr);
        assert_eq!(back.trk_vol, cfg.trk_vol);
        assert_eq!(back.trk_gain, cfg.trk_gain);
        assert_eq!(back.trk_pan, cfg.trk_pan);
        assert_eq!(back.fx_dir, "/tmp/effects");
        assert_eq!(back.track_fx[0][0].stem, "template");
        assert_eq!(back.track_fx[0][0].params, vec![0.5, 4.0]);
    }

    #[test]
    fn defaults_on_corrupt() {
        let back: Config = serde_json::from_str("{not json").unwrap_or_default();
        assert_eq!(back.q_sub, 4);
        assert!(back.shortcuts.is_empty());
    }

    #[test]
    fn missing_fields_default_and_removed_fields_tolerated() {
        // An old config: removed fields (root_loops, q_metro) must be
        // ignored, new ones must land on their documented defaults.
        let back: Config =
            serde_json::from_str(r#"{"root_loops": 3, "q_metro": true, "unknown": 1}"#).unwrap();
        assert_eq!(back.q_sub, 4);
        assert_eq!(back.snap_sub, 4);
        assert!(back.auto_quantize, "recordings snap to the grid unless unticked");
        assert!(back.loop_tl, "the timeline loops by default");
        assert!(back.midi.is_empty());
        // Per-track levels are #[serde(default)] — old configs load as empty
        // and App::new falls them back to 127 (vol) / 80 (gain) / 64 (pan).
        assert!(back.trk_vol.is_empty());
        assert!(back.trk_gain.is_empty());
        assert!(back.trk_pan.is_empty());
        assert!(back.ports_out_lr.is_empty());
        assert!(back.rack_folders.is_empty());
    }

    #[test]
    fn removed_midi_actions_become_reserved() {
        // A persisted binding naming an action this build removed must not
        // poison the whole config load.
        let back: Config = serde_json::from_str(
            r#"{"midi":[{"action":"OdSelected","bind":{"ty":0,"ch":0,"d1":60}},
                       {"action":{"RecLoop":3},"bind":null}]}"#,
        )
        .expect("old action names still deserialize");
        assert_eq!(back.midi[0].action, MidiAction::Reserved);
        assert_eq!(back.midi[1].action, MidiAction::RecLoop(3));
    }

    #[test]
    fn projects_dir_fallback() {
        let mut cfg = Config::default();
        assert!(cfg.projects_dir().ends_with("slooper-projects"));
        cfg.project_dir = "/srv/music".into();
        assert_eq!(cfg.projects_dir(), PathBuf::from("/srv/music"));
    }
}
