//! Persistent configuration: rebindable keyboard shortcuts, MIDI bindings and
//! the dependent-mode "root loops" count.
//!
//! Stored as JSON at `$XDG_CONFIG_HOME/simplooper/config.json` (falling back to
//! `~/.config/simplooper/config.json`). A missing or corrupt file yields
//! defaults; saving is best-effort (UI keeps working if the write fails).

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

/// One enabled effect in a loop's saved chain (script stem + slider values,
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
    /// of `MidiAction::ALL` (same order); null = unbound.
    #[serde(default)]
    pub shortcuts: Vec<Option<String>>,
    /// MIDI bindings (only bound actions are stored).
    #[serde(default)]
    pub midi: Vec<MidiBinding>,
    /// Axis calibrations for continuous bindings (knob min/max sweep).
    /// Slots without an entry use the controller's full 0..=127 travel.
    #[serde(default)]
    pub midi_axis: Vec<MidiAxisCal>,
    /// Dependent-mode ensemble length in units of the longest loop (>= 1).
    #[serde(default = "default_root_loops")]
    pub root_loops: u32,
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
    /// take lands exactly at the loop start. Monitoring, overdubs and the
    /// output mix are never delayed.
    #[serde(default)]
    pub latency_ms: f32,
    /// Per-loop playback volume, MIDI-knob style (0..=127; 0 = -inf, 127 =
    /// 0 dB). Missing entries fall back to 127.
    #[serde(default)]
    pub trk_vol: Vec<u8>,
    /// Per-loop gain trim, MIDI-knob style (0..=127; 0 = -inf, 80 = 0 dB,
    /// 127 = +10 dB). Missing entries fall back to 80 (unity).
    #[serde(default)]
    pub trk_gain: Vec<u8>,
    /// Per-loop stereo pan, MIDI-knob style (0..=127; 0 = hard left,
    /// 64 = centre, 127 = hard right). Missing entries fall back to 64.
    #[serde(default)]
    pub trk_pan: Vec<u8>,
    /// Metronome tempo in BPM.
    #[serde(default = "default_metro_bpm")]
    pub metro_bpm: u32,
    /// Metronome subdivision as clicks per beat (1 = quarter, 2 = eighth, ...).
    #[serde(default = "default_metro_sub")]
    pub metro_sub: u32,
    /// Metronome beats per measure (time-signature numerator); the first
    /// quarter note of every measure gets the downbeat tone.
    #[serde(default = "default_metro_num")]
    pub metro_num: u32,
    /// Easy quantize: snap to the metronome grid instead of the root loop.
    #[serde(default)]
    pub q_metro: bool,
    /// Effects folder ("" = default: `effects/` next to the installed
    /// package). The folder is created and seeded on first use.
    #[serde(default)]
    pub fx_dir: String,
    /// Per-loop effect chains: index = track, entries = enabled scripts in
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
            // Must match serde's field default — the derived `Default` would
            // give 0, which the UI clamps but is not a valid ensemble length.
            root_loops: default_root_loops(),
            ports_in: Vec::new(),
            ports_out: Vec::new(),
            ports_out_lr: Vec::new(),
            gui_scale: default_gui_scale(),
            latency_ms: 0.0,
            trk_vol: Vec::new(),
            trk_gain: Vec::new(),
            trk_pan: Vec::new(),
            metro_bpm: default_metro_bpm(),
            metro_sub: default_metro_sub(),
            metro_num: default_metro_num(),
            q_metro: false,
            fx_dir: String::new(),
            track_fx: Vec::new(),
        }
    }
}

fn default_root_loops() -> u32 {
    1
}

fn default_gui_scale() -> f32 {
    1.0
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

impl Config {
    /// `$XDG_CONFIG_HOME/simplooper/config.json` (or `~/.config/...`).
    pub fn path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"));
        base.join("simplooper").join("config.json")
    }

    /// Load the config, falling back to defaults on any error.
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
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
    fn roundtrip() {
        let cfg = Config {
            shortcuts: vec![Some("R".into()), None, Some("Space".into())],
            midi: vec![MidiBinding {
                action: MidiAction::RecLoop(0),
                bind: Some(MidiBind { ty: 0, ch: 1, d1: 60 }),
            }],
            midi_axis: Vec::new(),
            root_loops: 3,
            ports_in: vec!["remote:out_1".into()],
            ports_out: vec!["remote:in_1".into(), "remote:in_2".into()],
            ports_out_lr: vec![1, 2],
            gui_scale: 1.25,
            latency_ms: 12.5,
            metro_bpm: 96,
            metro_sub: 3,
            metro_num: 7,
            q_metro: true,
            trk_vol: vec![127, 100],
            trk_gain: vec![80, 60],
            trk_pan: vec![64, 20],
            fx_dir: "/tmp/effects".into(),
            track_fx: vec![vec![TrackFxSave { stem: "template".into(), params: vec![0.5, 4.0] }]],
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.shortcuts, cfg.shortcuts);
        assert_eq!(back.midi, cfg.midi);
        assert_eq!(back.root_loops, 3);
        assert!((back.gui_scale - 1.25).abs() < f32::EPSILON);
        assert!((back.latency_ms - 12.5).abs() < f32::EPSILON);
        assert_eq!(back.metro_bpm, 96);
        assert_eq!(back.metro_sub, 3);
        assert_eq!(back.metro_num, 7);
        assert!(back.q_metro);
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
        assert_eq!(back.root_loops, 1);
        assert!(back.shortcuts.is_empty());
    }

    #[test]
    fn missing_fields_default() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(back.root_loops, 1);
        assert!(back.midi.is_empty());
        // Per-loop levels are #[serde(default)] — old configs load as empty
        // and App::new falls them back to 127 (vol) / 80 (gain).
        assert!(back.trk_vol.is_empty());
        assert!(back.trk_gain.is_empty());
        assert!(back.trk_pan.is_empty());
        assert!(back.ports_out_lr.is_empty());
    }
}
