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
//   Each one becomes a labelled slider in the effect's AUTO child window,
//   clamped to [min, max], seeded with `default`. To ADD a parameter:
//   append an `EffectParamV1` entry, raise `n_params`, and read
//   `*params.add(K)` in `process`.
//   To REMOVE one: delete the entry, lower `n_params`, drop its use.
//   Keep the array indices and the `process` reads in sync — simplooper
//   validates `n_params <= SIMPLOOPER_MAX_PARAMS` but cannot know which
//   index you meant. A slider with min == max renders inert; that is fine.
// - A CUSTOM child GUI (curve displays, drag handles, live meters): export
//   the optional `simplooper_effect_gui` symbol — see the GUI contract at
//   the bottom of this file. Everything else stays exactly as-is, so old
//   scripts are untouched by it.
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

/// Hard cap on params per effect (keeps the realtime param snapshot fixed
/// size; the auto GUI will not show more). Effects with a CUSTOM child GUI
/// (see the GUI contract at the bottom of this file) lay params out however
/// they like — the cap is on param count, not on widgets.
pub const SIMPLOOPER_MAX_PARAMS: usize = 32;

/// One slider the auto child window rides per param: label + range + default.
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

// ---------------------------------------------------------------------------
// OPTIONAL: the child GUI (editor window) contract
// ---------------------------------------------------------------------------
// By default simplooper gives every effect a child window with one slider
// per param — effects window / loop-row button → window, no GUI code
// needed. An effect that wants a CUSTOM editor exports ONE extra symbol:
//
//     #[no_mangle]
//     pub extern "C" fn simplooper_effect_gui() -> *const EffectGuiV1 { &GUI }
//
// with a `GUI: EffectGuiV1` static (its own version field:
// SIMPLOOPER_EFFECT_GUI_ABI). Effects without this symbol keep the auto
// slider window; everything else in this file is unchanged for them.
//
// The child GUI is IMMEDIATE MODE, like the primitives below suggest:
// simplooper calls your `draw` once per frame while the window is open.
// You receive
//   * `gs`     — your own per-window state (from `gui_create`), and
//   * `state`  — the AUDIO instance state (the same pointer `create`
//                returned for this loop's instance), so you can show live
//                data `process` publishes, and
//   * `frame`  — window size, mouse, time — and a `GuiApiV1` vtable of
//                drawing/interaction calls. Call them at the coordinates
//                you want (origin = canvas top-left, y down, points).
// Interaction returns 1 the frame the user changed something; you then
// push new values with `api.set_param(k, v)` — the same params `process`
// reads, so audio follows automatically (and simplooper persists them).
//
// GUI-thread rules (relaxed vs. process, but real):
//   * Allocation, format!, Vec are OK here (60 fps UI code).
//   * NEVER block, and NEVER free instance state from here.
//   * `state` is owned by the audio thread: read/write ONLY through
//     atomics you placed in it (levels for a spectrum, a "gui open" flag
//     …). The editor holds an instance reference for the whole window
//     lifetime, so the pointer stays valid until `gui_destroy` returns.
//   * Keep drawing inside 0..frame.width / 0..frame.height.
// ---------------------------------------------------------------------------

/// Child-GUI ABI version (independent of the audio ABI above).
pub const SIMPLOOPER_EFFECT_GUI_ABI: u32 = 1;

/// Everything one `draw` call knows about the current frame. Plain data.
#[repr(C)]
pub struct GuiFrameV1 {
    /// Canvas size in points; lay out relative to these to stay resizable.
    pub width: f32,
    pub height: f32,
    /// Pointer position in canvas coordinates (valid when `mouse_inside`).
    pub mouse_x: f32,
    pub mouse_y: f32,
    /// 1 while the pointer is over the canvas / button pressed / pressed
    /// this frame / released this frame (left button).
    pub mouse_inside: u32,
    pub mouse_down: u32,
    pub mouse_pressed: u32,
    pub mouse_released: u32,
    /// Seconds since the window opened, and the previous frame's duration
    /// (clamped to sane values) — drive animations and envelope decays.
    pub time: f64,
    pub dt: f64,
    /// Frames drawn so far in this window.
    pub frame: u64,
    /// Audio engine sample rate in Hz (0.0 until the backend is up).
    pub sample_rate: f64,
}

/// Immediate-mode canvas + param access. Every function takes the `ctx`
/// from [`GuiApiV1::ctx`] as its first argument — pass it through
/// untouched. Coordinates are canvas points (origin top-left, y down).
/// Colors are RGBA floats 0..1.
#[repr(C)]
pub struct GuiApiV1 {
    /// Must be [`SIMPLOOPER_EFFECT_GUI_ABI`] (checked for you).
    pub abi: u32,
    /// Per-frame context token. Pass it as the first argument of EVERY
    /// call below: `(api.line)(api.ctx, …)`. Valid only during `draw`.
    pub ctx: *mut core::ffi::c_void,
    // ---- pure drawing (no interaction, drawn this frame only) ----
    /// One line segment.
    pub line: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x0: f32, y0: f32, x1: f32, y1: f32,
        width: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    /// Polyline through `n` xy pairs (interleaved `[x0,y0,x1,y1,…]`);
    /// `closed` != 0 closes it (polygon outline).
    pub polyline: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        xy: *const f32, n: u32, closed: u32,
        width: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    /// Filled / outlined rectangle at (x, y) with size (w, h).
    pub rect_filled: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, h: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    pub rect_stroke: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, h: f32,
        width: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    /// Text at (x, y), `size` points tall; `align` 0=left 1=center 2=right
    /// around x, vertically centred on y. UTF-8.
    pub text: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, size: f32, align: u32,
        r: f32, g: f32, b: f32, a: f32, s: *const core::ffi::c_char,
    ),
    /// Width of `s` in points at `size` — lay out labels without guessing.
    pub text_width: extern "C" fn(
        ctx: *mut core::ffi::c_void, size: f32, s: *const core::ffi::c_char,
    ) -> f32,
    // ---- interaction widgets (return 1 the frame the user changed) ----
    /// Horizontal slider `w × h` at (x, y), label baked in. `curve`:
    /// 1.0 linear, 0.0 logarithmic (needs min > 0), else exponent.
    /// Writes the new value through `value` when it returns 1.
    pub slider: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, h: f32, label: *const core::ffi::c_char,
        value: *mut f32, min: f32, max: f32, curve: f32,
    ) -> u32,
    /// Round knob of diameter `size` at (x, y) (drag up/down to edit;
    /// label under it). Same value mechanics as `slider`.
    pub knob: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, size: f32, label: *const core::ffi::c_char,
        value: *mut f32, min: f32, max: f32, curve: f32,
    ) -> u32,
    /// Push button `w` wide (auto height) at (x, y). Returns 1 when clicked.
    pub button: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, label: *const core::ffi::c_char,
    ) -> u32,
    /// Checkbox at (x, y); `on` != 0 draws checked, toggled writes 0/1.
    pub checkbox: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, label: *const core::ffi::c_char, on: *mut f32,
    ) -> u32,
    // ---- audio param access (the ONLY channel to `process`) ----
    /// Current value of param `k` (0..`n_params`); 0.0 out of range.
    pub get_param: extern "C" fn(ctx: *mut core::ffi::c_void, k: u32) -> f32,
    /// Ride param `k` (clamped to its declared range by simplooper;
    /// remembers the change for persistence).
    pub set_param: extern "C" fn(ctx: *mut core::ffi::c_void, k: u32, v: f32),
    /// How many params this effect has.
    pub n_params: u32,
}

/// The optional editor window, as a C-ABI vtable. Like `EffectInfoV1`:
/// plain data + plain function pointers, no Rust types cross the boundary.
#[repr(C)]
pub struct EffectGuiV1 {
    /// Must be [`SIMPLOOPER_EFFECT_GUI_ABI`].
    pub abi: u32,
    /// Preferred canvas size in points (0 = let simplooper choose ~480×320).
    pub want_width: f32,
    pub want_height: f32,
    /// Per-open-window constructor (called when the child window opens;
    /// NOT per loop — one editor window at a time). Null to fall back to
    /// the auto slider window. Return null to fail (auto window used).
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    /// Draw one frame. `gs` is `create`'s state, `state` the audio
    /// instance state of the loop being edited (never free it here!),
    /// `frame` the frame info, `api` valid for this call only.
    pub draw: extern "C" fn(
        gs: *mut core::ffi::c_void,
        state: *mut core::ffi::c_void,
        frame: *const GuiFrameV1,
        api: *const GuiApiV1,
    ),
    /// Window closed. Free what `create` allocated. The audio instance is
    /// guaranteed alive until this returns.
    pub destroy: extern "C" fn(gs: *mut core::ffi::c_void),
}

// The description blocks live in read-only statics and are only ever shared
// one way (effect -> loader); nothing in Rust mutates them after the library
// is loaded, so cross-thread sharing is exactly the contract.
unsafe impl Sync for EffectParamV1 {}
unsafe impl Sync for EffectInfoV1 {}
unsafe impl Sync for GuiApiV1 {}
unsafe impl Sync for EffectGuiV1 {}
