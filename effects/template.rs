// ============================================================ slooper ==
// template.rs — starter effect for the slooper effect framework.
//
// Copy this file inside your effects folder, rename it, and edit. Every
// effect is a single standalone Rust file that slooper compiles and
// loads at runtime. This template starts as an audible-but-subtle tremolo
// (an amplitude wobble); with depth = 0 it is an exact pass-through, so
// swap its guts for your own algorithm and keep the scaffolding.
//
// ---------------------------------------------------------------------------
// THE FIVE THINGS AN EFFECT NEEDS (see effect_api.rs for the full contract)
// ---------------------------------------------------------------------------
// 1. `include!("effect_api.rs");` — pulls in the shared ABI structs. The
//    file lives next to this script (slooper writes it there); keep this
//    script inside the effects folder or the include stops resolving.
// 2. A state struct — everything this effect remembers per loop-instance
//    (one instance per loop that enables it; loops never share state).
// 3. Three `extern "C"` functions: create / destroy / process.
// 4. A `DESC: EffectInfoV1` static wiring it together — the ONLY required
//    field with content is `name` (shown in the GUI). author/description
//    are optional hover text; null when unused.
// 5. The `slooper_effect_info` entry point — one fixed line; do not edit.
//
// ---------------------------------------------------------------------------
// ADDING / REMOVING SETTINGS (sliders the user rides per loop)
// ---------------------------------------------------------------------------
// Each slider = one EffectParamV1 entry in DESC.params: label, min, max,
// default. The GUI renders them (in order) on the loop row. Read them in
// `process` as `*params.add(K)` where K is the entry index. Adding a slider:
//   1. append an EffectParamV1 { .. } to the array below,
//   2. raise `n_params` in DESC,
//   3. read the new index in `process`.
// Removing: the reverse. Values arrive pre-clamped to [min, max]; the engine
// updates them between audio blocks, never mid-`process`. Up to 8 total.
//
// ---------------------------------------------------------------------------
// HOW SLOOPER DECIDES THIS FILE IS "VALID"
// ---------------------------------------------------------------------------
// - it compiles:  rustc --edition 2021 --crate-type cdylib template.rs
//   (run that inside the effects folder to check without restarting; only
//   std/core are available — no external crates),
// - the library exports slooper_effect_info,
// - DESC.abi == 1, name non-empty, n_params <= 8, all fn pointers set.
// Anything else (typo, bad type, missing entry point) and the effects
// window lists the file in red with the compiler/loader error — the app and
// every other effect stay fine. Re-edit and hit Rescan.
//
// ---------------------------------------------------------------------------
// REALTIME RULES (process runs on the audio thread, every sample)
// ---------------------------------------------------------------------------
// No allocations, no locks, no I/O, no panicking, keep the math small.
// `create`/`destroy` may allocate. Read `sample_rate` in `create` to size
// time-based state (it can be 0.0 when the backend was not up yet — the
// effect is rebuilt with the real rate once audio starts, but guard maths
// against 0 just in case).
// ---------------------------------------------------------------------------

// The shared ABI (structs, constants, validity rules) — written next to
// this file by slooper on every start.
include!("effect_api.rs");

// ---------------------------------------------------------------------------
// 2. State — one per enabled (loop, effect-slot) instance.
// ---------------------------------------------------------------------------
struct Tremolo {
    /// Engine sample rate captured at create (Hz; 0 until known).
    sr: f64,
    /// LFO phase in radians, advanced by `process`.
    phase: f64,
}

/// Index of each slider below — keep in sync with DESC.n_params.
const P_DEPTH: usize = 0; // 0..1   tremolo depth (0 = pass-through)
const P_RATE: usize = 1; //  0.1..20 Hz wobble speed
const P_MIX: usize = 2; //  0..1   dry/wet (1 = fully wobbled)
const P_OUT: usize = 3; //  0..2   output trim after the effect

// ---------------------------------------------------------------------------
// 3a. create — allocate instance state. Return null to fail.
// ---------------------------------------------------------------------------
extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    // Box into the heap; the raw pointer is the effect's opaque state.
    Box::into_raw(Box::new(Tremolo { sr: sample_rate.max(1.0), phase: 0.0 }))
        as *mut core::ffi::c_void
}

// ---------------------------------------------------------------------------
// 3b. destroy — free what create allocated (called once per instance).
// ---------------------------------------------------------------------------
extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        // Reclaim the Box from create; dropping it frees the memory.
        drop(unsafe { Box::from_raw(state as *mut Tremolo) });
    }
}

// ---------------------------------------------------------------------------
// 3c. process — the audio thread hot path, one mono sample at a time.
//     `params` points at the slider values, indexed as declared in DESC.
// ---------------------------------------------------------------------------
extern "C" fn process(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
) -> f32 {
    if state.is_null() {
        return input; // never happen (engine drops null instances) — be safe
    }
    let st = unsafe { &mut *(state as *mut Tremolo) };

    // Slider values (pre-clamped by the engine).
    let depth = unsafe { *params.add(P_DEPTH) }.clamp(0.0, 1.0);
    let rate = unsafe { *params.add(P_RATE) }.clamp(0.1, 20.0);
    let mix = unsafe { *params.add(P_MIX) }.clamp(0.0, 1.0);
    let out = unsafe { *params.add(P_OUT) }.clamp(0.0, 2.0);

    // Depth 0 => exact pass-through: skip the oscillator entirely.
    if depth <= 0.0 || mix <= 0.0 {
        return input * out;
    }

    // Advance the LFO (one 32-bit sin per sample is fine for a looper;
    // keep heavier math out of here or amortise it across samples).
    let phase_step = core::f64::consts::TAU * rate as f64 / st.sr;
    st.phase = (st.phase + phase_step) % core::f64::consts::TAU;
    let lfo = 1.0 - depth as f32 * (0.5 - 0.5 * (st.phase as f32).cos());

    // Dry/wet blend, then output trim.
    (input * (1.0 - mix) + input * lfo * mix) * out
}

// ---------------------------------------------------------------------------
// 4. Description — what the GUI shows. REQUIRED fields: name + the three
//    functions. Set author/description to null to omit them.
// ---------------------------------------------------------------------------
static PARAMS: [EffectParamV1; 4] = [
    EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 1.0, default: 0.25 },
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.1, max: 20.0, default: 4.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Template Tremolo".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Starter effect: depth 0 is an exact pass-through. Copy template.rs and swap the maths in process().".as_ptr(),
    n_params: 4,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

// ---------------------------------------------------------------------------
// 5. Entry point — fixed signature; slooper's loader looks this symbol up
//    by name. Never rename, never make it generic, never add args.
// ---------------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}
