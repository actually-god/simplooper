//! slooper — a minimalist clip-based audio DAW for Linux (JACK / PipeWire).
//!
//! Library surface:
//! - [`audio`]: JACK/PipeWire-JACK backend (manager thread + realtime
//!   handler, transport, clip mixing, take recording).
//! - [`arrange`]: arrangement model — tracks → lanes → clips, placement,
//!   snapping, child-lane rules.
//! - [`assets`]: asset-rack folder store (scan, take saves, resolution).
//! - [`project`]: the `.slooper` save format and projects folder.
//! - [`midi`]: MIDI message parsing, trigger bindings, shortcut migration.
//! - [`quantize`]: metronome grid math, content quantize, silence trim.
//! - [`config`]: persistent settings (rack folders, projects dir, mixer,
//!   shortcuts, MIDI bindings).
//! - [`fx`]: runtime-compiled per-track effects (the effect framework).
//! - [`ports`]: enabled-device port filtering for the input/output pickers.
//! - [`ui`]: the egui front-end (used by the binary).

pub mod arrange;
pub mod assets;
pub mod audio;
pub mod audio_io;
pub mod config;
pub mod fx;
pub mod fxgui;
pub mod midi;
pub mod midipat;
pub mod ports;
pub mod project;
pub mod quantize;
pub mod samples808;
pub mod synth;
pub mod vsthost;
pub mod synth_api;
pub mod ui;

pub use midi::{MidiAction, MidiBind, MidiMsg};
