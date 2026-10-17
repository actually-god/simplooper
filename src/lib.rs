//! simplooper — a minimalist live audio looper for Linux (JACK / PipeWire).
//!
//! Library surface:
//! - [`audio`]: JACK/PipeWire-JACK backend (manager thread + realtime handler,
//!   loop engine).
//! - [`midi`]: MIDI message parsing, display formatting and trigger bindings.
//! - [`quantize`]: easy-quantization math (grid snap, zero crossing, seam).
//! - [`config`]: persistent settings (shortcuts, MIDI bindings, root loops).
//! - [`fx`]: runtime-compiled per-loop effects (the effect framework).
//! - [`ports`]: enabled-device port filtering for the input/output pickers.
//! - [`ui`]: the egui front-end (used by the binary).

pub mod audio;
pub mod audio_io;
pub mod config;
pub mod fx;
pub mod midi;
pub mod ports;
pub mod quantize;
pub mod ui;

pub use midi::{MidiAction, MidiBind, MidiMsg};
