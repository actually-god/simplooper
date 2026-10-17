// ============================================================ simplooper ==
// effect_api.rs — the simplooper effect ABI, version 1.
//
// THIS FILE IS NOT AUTHORED BY HAND AS A STANDALONE PROGRAM. It is a shared
// contract, written once into every effects folder by simplooper itself and
// embedded in the binary (src/effect_api.rs is the source of truth). Effect
// scripts pull it in with:
//
//     include!("effect_api.rs");
//
// because that resolves next to the script, where simplooper keeps a fresh
// copy. Never edit the copy inside your effects folder — if simplooper
// rewrites it (or the version number below changes), your effect adapts.
//
// ---------------------------------------------------------------------------
// What an effect is
// ---------------------------------------------------------------------------
// An effect script is one standalone Rust file. simplooper compiles each
// valid `*.rs` file in the effects folder with:
//
//     rustc --edition 2021 --crate-type cdylib <file>
//
// loads the result, and calls your functions per audio sample, on the audio
// thread, for every loop that has the effect enabled. Per loop, per effect
// slot, an independent *instance* is created via `create` and torn down via
// `destroy`; each instance owns whatever state it needs (counters, delay
// lines, filters …).
//
// ---------------------------------------------------------------------------
// REQUIRED pieces (an effect without these is not valid)
// ---------------------------------------------------------------------------
// 1. `pub static DESC: EffectInfoV1` — the description block below.
//    `name` is the only field you must give real text: it is what the GUI
//    shows next to the toggle and in the loop row.
// 2. `#[no_mangle] pub extern "C" fn simplooper_effect_info() -> *const EffectInfoV1`
//    — the entry point simplooper looks for. One line, see the template.
// 3. The three functions wired into DESC:
//    - `create`     (sample_rate) -> state pointer. Called once per enabled
//                   instance. Return core::ptr::null_mut() to signal failure
//                   (the effect then passes audio through).
//    - `destroy`    (state)       — free what `create` allocated.
//    - `process`    (state, input_sample, params) -> output_sample.
//                   Called once per sample on the audio thread. MUST be
//                   allocation-free, must never block (no locks, I/O, sleep),
//                   and must never panic. `params` points to `n_params`
//                   f32 values (the sliders; read them, never write).
//
// ---------------------------------------------------------------------------
// OPTIONAL pieces
// ---------------------------------------------------------------------------
// - `author`, `description`: shown as hover text in the effects window.
//   Use core::ptr::null() when you have nothing to show.
// - Parameters: up to SIMPLOOPER_MAX_PARAMS entries in the `params` array.
//   Each one becomes a labelled slider in the GUI, clamped to [min, max],
//   seeded with `default`. To ADD a parameter: append an `EffectParamV1`
//   entry, raise `n_params`, and read `*params.add(K)` in `process`.
//   To REMOVE one: delete the entry, lower `n_params`, drop its use.
//   Keep the array indices and the `process` reads in sync — simplooper
//   validates `n_params <= SIMPLOOPER_MAX_PARAMS` but cannot know which
//   index you meant. A slider with min == max renders inert; that is fine.
//
// ---------------------------------------------------------------------------
// How to check your effect is VALID
// ---------------------------------------------------------------------------
// Simplooper marks a script valid only when ALL of this succeeds:
//   * it compiles with the command above (no `extern crate`s beyond std/core;
//     include! of effect_api.rs must resolve — keep the script in the folder),
//   * the library exports `simplooper_effect_info`,
//   * DESC.abi == SIMPLOOPER_EFFECT_ABI, name is non-null UTF-8 text,
//     n_params <= SIMPLOOPER_MAX_PARAMS, and all three function pointers are
//     non-null.
// You can dry-run the compile yourself in the effects folder:
//     rustc --edition 2021 --crate-type cdylib my_effect.rs
// and see every rejection reason in the effects window (invalid files are
// listed with the compiler/loader error). Inside simplooper, toggling the
// effect on a loop proves `create` + `process` run live.
// ---------------------------------------------------------------------------

/// ABI version. Bumped only when this struct layout or the callback contract
/// changes incompatibly; simplooper refuses effects built for another version.
pub const SIMPLOOPER_EFFECT_ABI: u32 = 1;

/// Hard cap on sliders per effect (keeps the realtime param snapshot fixed
/// size; the GUI will not show more).
pub const SIMPLOOPER_MAX_PARAMS: usize = 8;

/// One slider the user can ride on the loop row: label + range + default.
#[repr(C)]
pub struct EffectParamV1 {
    /// Display label (required, non-null).
    pub label: *const core::ffi::c_char,
    /// Lowest allowed value; also the floor passed to `process`.
    pub min: f32,
    /// Highest allowed value. Must be >= `min`.
    pub max: f32,
    /// Value used when the effect is first enabled on a loop.
    pub default: f32,
}

/// The whole effect, as a C-ABI vtable. Plain data + plain function pointers
/// only — nothing from outside core/std may cross this boundary.
#[repr(C)]
pub struct EffectInfoV1 {
    /// Must be [`SIMPLOOPER_EFFECT_ABI`].
    pub abi: u32,
    /// Display name in the GUI (REQUIRED, non-null).
    pub name: *const core::ffi::c_char,
    /// Optional author line (or null).
    pub author: *const core::ffi::c_char,
    /// Optional one-paragraph description (or null).
    pub description: *const core::ffi::c_char,
    /// Number of valid entries in `params` (0..=SIMPLOOPER_MAX_PARAMS).
    pub n_params: u32,
    /// Param descriptors — null exactly when `n_params == 0`.
    pub params: *const EffectParamV1,
    /// Per-instance constructor. `sample_rate` is the engine rate in Hz (may
    /// be 0.0 when the audio backend was not up yet — size state lazily or
    /// guard divisions). Return null to fail (effect then passes audio
    /// through and shows an error).
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    /// Per-instance destructor. Always called once per successful `create`.
    pub destroy: extern "C" fn(state: *mut core::ffi::c_void),
    /// Per-sample processing on the audio thread: one mono sample in, one
    /// out. `params` holds `n_params` f32 values, updated lock-free by the
    /// GUI at audio-block boundaries. Keep it cheap: this runs at the full
    /// sample rate while the loop plays.
    pub process:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
}

// The description blocks live in read-only statics and are only ever shared
// one way (effect -> loader); nothing in Rust mutates them after the library
// is loaded, so cross-thread sharing is exactly the contract.
unsafe impl Sync for EffectParamV1 {}
unsafe impl Sync for EffectInfoV1 {}
