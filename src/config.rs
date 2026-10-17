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

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Keyboard shortcuts as key names (e.g. `"R"`, `"Space"`), one per entry
    /// of `MidiAction::ALL` (same order); null = unbound.
    #[serde(default)]
    pub shortcuts: Vec<Option<String>>,
    /// MIDI bindings (only bound actions are stored).
    #[serde(default)]
    pub midi: Vec<MidiBinding>,
    /// Dependent-mode ensemble length in units of the longest loop (>= 1).
    #[serde(default = "default_root_loops")]
    pub root_loops: u32,
    /// Last applied audio port selections — restored on the next launch so the
    /// user doesn't have to re-check the same ports every time.
    #[serde(default)]
    pub ports_in: Vec<String>,
    #[serde(default)]
    pub ports_out: Vec<String>,
    /// GUI zoom factor (egui `zoom_factor`): 1.0 = default UI size, >1 larger.
    #[serde(default = "default_gui_scale")]
    pub gui_scale: f32,
    /// Input/output latency compensation in milliseconds: the whole output
    /// mix (monitoring + loops + metronome) is delayed by this much.
    #[serde(default)]
    pub latency_ms: f32,
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shortcuts: Vec::new(),
            midi: Vec::new(),
            // Must match serde's field default — the derived `Default` would
            // give 0, which the UI clamps but is not a valid ensemble length.
            root_loops: default_root_loops(),
            ports_in: Vec::new(),
            ports_out: Vec::new(),
            gui_scale: default_gui_scale(),
            latency_ms: 0.0,
            metro_bpm: default_metro_bpm(),
            metro_sub: default_metro_sub(),
            metro_num: default_metro_num(),
            q_metro: false,
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
            root_loops: 3,
            ports_in: vec!["remote:out_1".into()],
            ports_out: vec!["remote:in_1".into(), "remote:in_2".into()],
            gui_scale: 1.25,
            latency_ms: 12.5,
            metro_bpm: 96,
            metro_sub: 3,
            metro_num: 7,
            q_metro: true,
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
    }
}
