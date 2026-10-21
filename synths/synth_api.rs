// ============================================================ slooper ==
// synth_api.rs — the slooper synthesizer ABI, version 1.
//
// THIS FILE IS NOT AUTHORED BY HAND AS A STANDALONE PROGRAM. It is a shared
// contract, written into every synths folder by slooper itself and embedded
// in the binary (src/synth_api.rs is the source of truth). Synth scripts
// pull it in with:
//
//     include!("synth_api.rs");
//
// ---------------------------------------------------------------------------
// What a synth is
// ---------------------------------------------------------------------------
// A synth script is one standalone Rust file, compiled by slooper with
//
//     rustc --edition 2021 --crate-type cdylib -C opt-level=2 <file>
//
// and instantiated PER TRACK (each track plays its MIDI clips through its
// own synth; no binding means the built-in `piano`). Where an effect turns
// audio into audio, a synth turns NOTES into audio:
//
//   * `create(sr, host)`      — allocate your voice state. `host` may be
//                              null (never used outside a track); keep the
//                              pointer only if you need host.load_asset.
//   * `destroy(s)`            — free it.
//   * `note_on(s, ch, note, vel)`  — start a voice (vel 1..=127).
//   * `note_off(s, ch, note)`      — release it.
//   * `all_off(s)`            — panic stop: every voice off, immediately.
//   * `cc(s, ch, num, val)`   — a controller change from a clip.
//   * `press(s, ch, val)`     — channel pressure from a clip.
//   * `process(s, out, n, params)` — append n mono samples to `out`
//     (already zeroed), on the audio thread. Allocation-free, no locks,
//     no I/O, must never panic. `params` points to n_params f32 sliders.
//
// Notes arrive with sample-accurate batching: slooper calls note_on /
// note_off / cc / press immediately BEFORE the process call that covers
// that audio block segment, so you may start a voice directly in note_on.
//
// ---------------------------------------------------------------------------
// The host table (optional, for samplers)
// ---------------------------------------------------------------------------
// `SynthHostV1::load_asset(host, id, out)` copies the current description
// of audio asset `id` from the rack into `out` (data pointer, length,
// sample rate) and returns 1, or 0 when the id is unknown. The pointer is
// ONLY valid until the next load_asset call on the same thread — copy what
// you need into your own state (that is what the bundled sampler does).
//
// REQUIRED pieces (mirrors the effect ABI):
//   1. `pub static DESC: SynthInfoV1`
//   2. `#[no_mangle] pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1`
//   3. every function pointer in DESC non-null.
// ---------------------------------------------------------------------------

pub const SLOOPER_SYNTH_ABI: u32 = 1;
pub const SLOOPER_MAX_SYNTH_PARAMS: usize = 16;

#[repr(C)]
pub struct SynthParamV1 {
    pub label: *const core::ffi::c_char,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

/// A rack asset handed to a sampler (see SynthHostV1::load_asset).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SynthAssetV1 {
    pub data: *const f32,
    pub len: u32,
    pub sample_rate: u32,
}

/// Host services available to synths at create time.
#[repr(C)]
pub struct SynthHostV1 {
    pub abi: u32,
    /// Fill `out` with asset `id`; 1 = filled, 0 = unknown id. The pointer
    /// stays valid only until the next load_asset on this thread.
    pub load_asset: extern "C" fn(id: u32, out: *mut SynthAssetV1) -> u8,
}

#[repr(C)]
pub struct SynthInfoV1 {
    pub abi: u32,
    pub name: *const core::ffi::c_char,
    pub author: *const core::ffi::c_char,
    pub description: *const core::ffi::c_char,
    pub n_params: u32,
    pub params: *const SynthParamV1,
    pub create: extern "C" fn(sample_rate: f64, host: *const SynthHostV1) -> *mut core::ffi::c_void,
    pub destroy: extern "C" fn(s: *mut core::ffi::c_void),
    pub note_on: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8, vel: u8),
    pub note_off: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8),
    pub all_off: extern "C" fn(s: *mut core::ffi::c_void),
    pub cc: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, num: u8, val: u8),
    pub press: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, val: u8),
    pub process: extern "C" fn(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32),
}

unsafe impl Sync for SynthParamV1 {}
unsafe impl Sync for SynthInfoV1 {}
unsafe impl Sync for SynthHostV1 {}
unsafe impl Sync for SynthAssetV1 {}
