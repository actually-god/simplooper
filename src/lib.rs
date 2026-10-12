//! simplooper — a minimalist live audio looper for Linux (JACK / PipeWire).
//!
//! Library surface:
//! - [`audio`]: JACK/PipeWire-JACK backend (manager thread + realtime handler,
//!   loop engine).
//! - [`midi`]: MIDI message parsing, display formatting and trigger bindings.
//! - [`quantize`]: easy-quantization math (grid snap, zero crossing, seam).
//! - [`ui`]: the egui front-end (used by the binary).

pub mod audio;
pub mod midi;
pub mod quantize;
pub mod ui;
