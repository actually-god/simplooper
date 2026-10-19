//! simplooper effect framework: runtime-compiled Rust effects per loop.
//!
//! Every `*.rs` file in the effects folder is a self-contained effect script
//! (contract: [`effect_api.rs`], embedded below and also written next to the
//! scripts as `effect_api.rs`, which the scripts `include!`). The registry
//! enumerates the folder, compiles each script with `rustc --crate-type
//! cdylib` into a `.build/` cache (recompiling only when the source is newer
//! than its artifact), loads the result with libloading and validates the
//! exported `simplooper_effect_info` description.
//!
//! Instances ([`Instance`]) are created on the GUI thread, ref-counted with
//! Arc and handed to the audio engine through `Cmd::SetTrackEffects`. The
//! realtime thread only clones Arcs and calls the C-ABI `process` callback;
//! slider values ride on per-instance atomics bumped through a generation
//! counter, so live param edits never take a lock on the audio path.

use std::collections::hash_map::DefaultHasher;
use std::ffi::{CStr, OsStr};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

// The C-ABI contract. Compiled twice: once into simplooper (this include)
// and once into every effect script (via its `include!("effect_api.rs")`),
// so both sides agree on the layout by construction.
mod api {
    #![allow(dead_code)]
    include!("effect_api.rs");
}
pub use api::{
    EffectGuiV1, EffectInfoV1, EffectParamV1, GuiApiV1, GuiFrameV1,
    SIMPLOOPER_EFFECT_ABI, SIMPLOOPER_EFFECT_GUI_ABI, SIMPLOOPER_MAX_PARAMS,
};

/// Param count cap mirrors the ABI (RT param snapshots are fixed-size).
pub const MAX_PARAMS: usize = SIMPLOOPER_MAX_PARAMS;
/// Effects per loop chain (bounds the RT snapshot arrays).
pub const MAX_FX_PER_TRACK: usize = 8;

/// ABI version this build speaks.
pub const ABI_VERSION: u32 = SIMPLOOPER_EFFECT_ABI;
/// Child-GUI ABI version this build speaks.
pub const GUI_ABI_VERSION: u32 = SIMPLOOPER_EFFECT_GUI_ABI;

/// The generated shared-contract file every script includes.
pub const API_FILE: &str = "effect_api.rs";
/// The contract text (single source of truth: src/effect_api.rs).
pub const API_SRC: &str = include_str!("effect_api.rs");
/// The documented starter effect, offered by the "Add template" button.
pub const TEMPLATE_SRC: &str = include_str!("../effects/template.rs");
/// File name the template is written under.
pub const TEMPLATE_FILE: &str = "template.rs";
/// Bundled example effects with custom child GUIs (parametric EQ with a
/// response/spectrum graph, and the delay+reverb with an animated canvas).
/// Written into brand-new effects folders so the child-GUI features are
/// discoverable; never written over an existing (user-owned) copy.
pub const EXAMPLES: &[(&str, &str)] = &[
    ("parametric_eq.rs", include_str!("../effects/parametric_eq.rs")),
    ("space_echo.rs", include_str!("../effects/space_echo.rs")),
];

/// Symbol every effect library must export (NUL-terminated: libloading 0.8
/// takes the raw name bytes).
const ENTRY: &[u8] = b"simplooper_effect_info\0";
/// Optional symbol exporting the custom child-GUI vtable. Absent = the
/// effect gets the auto slider window; the audio ABI does not care.
const GUI_ENTRY: &[u8] = b"simplooper_effect_gui\0";

// ---------------------------------------------------------------------------
// Directory handling
// ---------------------------------------------------------------------------

/// The folder effects live in when nothing was configured: next to the
/// installed package. Exes under a `bin/` directory use the FHS-style
/// sibling (`../share/simplooper/effects` → `/usr/share/...` or
/// `~/.local/share/...`); dev builds get `effects/` beside the binary
/// (repo checkout). Falls back to `$XDG_DATA_HOME/simplooper/effects` when
/// the first choice cannot be created.
pub fn default_effects_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let under_bin = exe_dir.file_name() == Some(std::ffi::OsStr::new("bin"));
    let primary = if under_bin {
        exe_dir
            .parent()
            .map(|root| root.join("share").join("simplooper").join("effects"))
            .unwrap_or_else(|| exe_dir.join("effects"))
    } else {
        exe_dir.join("effects")
    };
    if std::fs::create_dir_all(&primary).is_ok() {
        return primary;
    }
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("share"));
    let fallback = base.join("simplooper").join("effects");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve the effects folder and make it usable: create it when missing,
/// refresh the generated `effect_api.rs` when it differs (silently skipping
/// read-only folders that already ship a matching copy). `SIMPLOOPER_EFFECTS_DIR`
/// overrides everything (used by tests and for quick experimentation).
pub fn prepare_effects_dir(cfg_dir: Option<&str>) -> PathBuf {
    let env_dir = std::env::var("SIMPLOOPER_EFFECTS_DIR")
        .ok()
        .filter(|p| !p.trim().is_empty());
    let dir = match env_dir.as_deref().or(cfg_dir.map(str::trim)) {
        Some(d) if !d.is_empty() => {
            let p = PathBuf::from(d);
            let _ = std::fs::create_dir_all(&p);
            p
        }
        _ => default_effects_dir(),
    };
    seed_api(&dir);
    // The user-preset folder lives beside the scripts and is (re)created at
    // every launch — plugins find it ready-made the first time they save.
    let _ = ensure_user_presets_dir(&dir);
    dir
}

// ---------------------------------------------------------------------------
// User presets
// ---------------------------------------------------------------------------
//
// Users save the current settings of any effect as a named preset. Files
// live in `<effects>/user presets/` and are named
// `{plugin stem}-{preset name}.json` — the stem prefix is the ownership
// marker: a plugin lists and reads ONLY files starting with its own
// `{stem}-`, so one folder serves every effect. Content is flat JSON:
// `{"plugin": stem, "name": preset, "params": {"label": value, …}}`.

/// Folder name (fixed by convention) inside the effects location.
pub const USER_PRESET_DIR: &str = "user presets";

pub fn user_presets_dir(fx_dir: &Path) -> PathBuf {
    fx_dir.join(USER_PRESET_DIR)
}

/// Create the user-preset folder if missing and return its path.
pub fn ensure_user_presets_dir(fx_dir: &Path) -> std::io::Result<PathBuf> {
    let d = user_presets_dir(fx_dir);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Filesystem-safe preset name: keep letters/digits/space/dot/dash/underscore
/// (UTF-8 aware so accented names survive), collapse runs of rejected chars
/// to a single dash, trim. Empty result becomes "preset".
pub fn sanitize_preset_name(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_alphanumeric() || ch == ' ' || ch == '.' || ch == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    while out.ends_with(' ') || out.ends_with('-') || out.ends_with('.') {
        out.pop();
    }
    // No hidden files, no confusion with relative paths: a preset name never
    // starts with a dot or a separator.
    while out.starts_with(' ') || out.starts_with('-') || out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out.push_str("preset");
    }
    out
}

/// The preset file for (stem, name) — exact round-trip of `list_user_presets`.
pub fn user_preset_path(fx_dir: &Path, stem: &str, name: &str) -> PathBuf {
    user_presets_dir(fx_dir).join(format!("{stem}-{}.json", sanitize_preset_name(name)))
}

/// Preset names owned by `stem`, sorted. Only `{stem}-{name}.json` files.
pub fn list_user_presets(fx_dir: &Path, stem: &str) -> Vec<String> {
    let mut out = Vec::new();
    let prefix = format!("{stem}-");
    if let Ok(rd) = std::fs::read_dir(user_presets_dir(fx_dir)) {
        for e in rd.flatten() {
            let Some(n) = e.file_name().to_str().map(str::to_string) else { continue };
            if let Some(rest) = n.strip_prefix(&prefix) {
                if let Some(name) = rest.strip_suffix(".json") {
                    if !name.is_empty() {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Delete a preset. Missing file counts as success (idempotent).
pub fn delete_user_preset(fx_dir: &Path, stem: &str, name: &str) -> Result<(), String> {
    let p = user_preset_path(fx_dir, stem, name);
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not delete preset: {e}")),
    }
}

/// Minimal flat JSON writer for a preset. Labels come from effect metadata;
/// still escape the mandatory characters, and reject non-finite values —
/// JSON has no NaN and a poisoned file would load garbage later.
pub fn preset_to_json(stem: &str, name: &str, params: &[(String, f32)]) -> Result<String, String> {
    let mut s = format!(
        "{{\n  \"plugin\": \"{}\",\n  \"name\": \"{}\",\n  \"params\": {{\n",
        json_escape(stem),
        json_escape(name)
    );
    for (i, (label, v)) in params.iter().enumerate() {
        if !v.is_finite() {
            return Err(format!("parameter '{label}' is not a number"));
        }
        s.push_str(&format!(
            "    \"{}\": {}{}",
            json_escape(label),
            f64::from(*v),
            if i + 1 < params.len() { "," } else { "" }
        ));
        s.push('\n');
    }
    s.push_str("  }\n}\n");
    Ok(s)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Pull every `"label": number` pair out of a preset file. Deliberately
/// lenient (string values like "plugin"/"name" simply don't parse as
/// numbers and are skipped): a hand-tweaked preset should still load its
/// numbers. Garbage that yields no pairs at all is still an error.
pub fn preset_from_json(text: &str) -> Result<Vec<(String, f64)>, String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        // Read one string literal starting at i (this position is a key
        // candidate; string VALUES fall through the colon test below).
        let mut j = i + 1;
        let mut key = String::new();
        let mut closed = false;
        while j < b.len() {
            if b[j] == b'\\' && j + 1 < b.len() {
                key.push(text[j + 1..].chars().next().unwrap_or(' '));
                j += 2;
            } else if b[j] == b'"' {
                closed = true;
                break;
            } else {
                let ch = text[j..].chars().next().unwrap();
                key.push(ch);
                j += ch.len_utf8();
            }
        }
        if !closed {
            break;
        }
        i = j + 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b':' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
            i += 1;
        }
        if let Ok(v) = text[start..i].parse::<f64>() {
            out.push((key, v));
        }
    }
    if out.is_empty() {
        return Err("no numeric parameters found in preset file".into());
    }
    Ok(out)
}

/// Write `effect_api.rs` next to the scripts when absent or stale.
fn seed_api(dir: &Path) {
    let p = dir.join(API_FILE);
    match std::fs::read_to_string(&p) {
        Ok(cur) if cur == API_SRC => {}
        _ => {
            let _ = std::fs::write(&p, API_SRC);
        }
    }
    // The template is written once for a brand-new folder.
    let fresh = !has_scripts(dir);
    let t = dir.join(TEMPLATE_FILE);
    if fresh && !t.exists() {
        let _ = std::fs::write(&t, TEMPLATE_SRC);
    }
    // Bundled examples that are ALREADY PRESENT are updated with every app
    // release, not just seeded once: the previous write-only-if-absent rule
    // meant plugin fixes shipped in later releases never reached an existing
    // install — users ran the day-one scripts forever ("EQ still has all
    // bugs" while the repo copy was fixed). A file about to change is copied
    // to `<name>.rs.bak` (single generation) first, so nothing is lost; a
    // user customization that must survive updates belongs under its own
    // file name. Files that are NOT present are never added to an existing
    // folder — only a fresh folder gets the full starter set.
    for (name, src) in EXAMPLES {
        let p = dir.join(name);
        if !p.exists() && !fresh {
            continue;
        }
        match std::fs::read_to_string(&p) {
            Ok(cur) if cur == *src => {}
            Ok(cur) => {
                let _ = std::fs::write(p.with_extension("rs.bak"), cur.as_bytes());
                let _ = std::fs::write(&p, src);
            }
            Err(_) => {
                let _ = std::fs::write(&p, src);
            }
        }
    }
}

/// Any effect script in the folder (ignoring the API file and drafts)?
fn has_scripts(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if stem != API_FILE.strip_suffix(".rs").unwrap_or(API_FILE)
            && !stem.starts_with('.')
            && !stem.starts_with('_')
        {
            return true;
        }
    }
    false
}

/// True when `rustc` can be launched (effects cannot compile without it).
pub fn rustc_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("rustc")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Descriptors & instances
// ---------------------------------------------------------------------------

/// Safe copy of one slider description.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamDesc {
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

/// Safe copy of the effect description shown in the GUI.
#[derive(Clone, Debug)]
pub struct EffectDesc {
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub params: Vec<ParamDesc>,
}

/// The three live callbacks, copied out of the library (plain fn pointers —
/// Send + Sync by construction).
#[derive(Clone, Copy)]
struct EffectFns {
    create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    destroy: extern "C" fn(state: *mut core::ffi::c_void),
    process: extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
}

/// Owning wrapper for a loaded library (raw handle inside ⇒ manual impls;
/// only ever touched while building instances or dropping them).
struct LibHandle(std::mem::ManuallyDrop<libloading::Library>);
// Intentional leak: dropping libloading::Library calls dlclose, and closing
// a plugin that embeds its own Rust std runtime frees that runtime's TLS
// entries and lazy pool bookkeeping while the process's own teardown
// destructors are still running — dangling references land as
// `corrupted size vs. prev_size` heap aborts at shutdown (reproduced in the
// headless EQ test). Effects are few and small; keep them mapped for good.
unsafe impl Send for LibHandle {}
unsafe impl Sync for LibHandle {}

/// One live effect instance: created per (loop, chain slot), destroyed when
/// the last Arc drops. Slider values are atomics so the GUI can ride them
/// while the audio thread reads generation-stamped snapshots of them.
pub struct Instance {
    fns: EffectFns,
    state: *mut core::ffi::c_void,
    sample_rate: f64,
    n_params: usize,
    params: Box<[AtomicU32; MAX_PARAMS]>,
    param_gen: AtomicU64,
    _lib: Arc<LibHandle>,
}
// `state` is only dereferenced by the audio thread (or the render thread on
// a private instance); params are atomics; the library Arc keeps the code
// mapped for as long as any thread can call into it.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    /// Create via the effect's `create`. `values` seeds the sliders
    /// (missing entries fall back to the defaults in `desc`).
    fn new(fns: EffectFns, lib: Arc<LibHandle>, desc: &EffectDesc, sample_rate: f64, values: &[f32]) -> Option<Arc<Self>> {
        let state = (fns.create)(sample_rate);
        if state.is_null() {
            return None;
        }
        let params = core::array::from_fn(|k| {
            let d = desc.params.get(k);
            let v = values
                .get(k)
                .copied()
                .or(d.map(|p| p.default))
                .unwrap_or(0.0);
            // Clamp to the DECLARED range, never at 0: bipolar params
            // (EQ band gain -24..+24 dB) must survive loading a saved
            // negative value — the same trap `set_param` already fixed.
            let v = match d {
                Some(p) => v.clamp(p.min, p.max),
                None => v,
            };
            AtomicU32::new(v.to_bits())
        });
        Some(Arc::new(Self {
            fns,
            state,
            sample_rate,
            n_params: desc.params.len().min(MAX_PARAMS),
            params: Box::new(params),
            // Start at 1 so an RT snapshot that last saw 0 always picks the
            // seeded values up on its first block.
            param_gen: AtomicU64::new(1),
            _lib: lib,
        }))
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Raw pointer to the effect's per-instance state (what `create`
    /// returned). Only ever handed to a live child GUI's `draw` while the
    /// editor holds an `Arc<Instance>` clone — the instance (and its
    /// library) cannot be destroyed underneath the GUI.
    pub fn state_ptr(&self) -> *mut core::ffi::c_void {
        self.state
    }

    pub fn n_params(&self) -> usize {
        self.n_params
    }

    pub fn param(&self, k: usize) -> f32 {
        self.params
            .get(k)
            .map(|p| f32::from_bits(p.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    /// Ride one slider (GUI thread; audio picks it up next block).
    /// Values keep their sign — many params are bipolar (EQ band gain runs
    /// -24..+24 dB) — only non-finite writes are ignored. Range clamping is
    /// the caller's job (fxgui clamps by the declared param bounds).
    pub fn set_param(&self, k: usize, v: f32) {
        if let Some(p) = self.params.get(k) {
            if !v.is_finite() {
                return;
            }
            p.store(v.to_bits(), Ordering::Relaxed);
            self.param_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn param_gen(&self) -> u64 {
        self.param_gen.load(Ordering::Relaxed)
    }

    /// Copy the slider values into `out` (used by the audio thread on
    /// generation change and by the render path).
    pub fn read_params(&self, out: &mut [f32; MAX_PARAMS]) {
        for (o, p) in out.iter_mut().zip(self.params.iter()) {
            *o = f32::from_bits(p.load(Ordering::Relaxed));
        }
    }

    /// Process one sample. ONLY from the audio thread (or with a private
    /// instance during offline render); `params` must hold current values.
    ///
    /// # Safety
    /// The caller must be the single thread that owns this instance's state
    /// (the RT thread for live chains, the render thread for private
    /// clones) and `params` must point at `MAX_PARAMS` live slider values.
    #[inline]
    pub unsafe fn call_sample(&self, input: f32, params: &[f32; MAX_PARAMS]) -> f32 {
        (self.fns.process)(self.state, input, params.as_ptr())
    }

    /// A brand-new instance of the same effect at the same rate, seeded with
    /// this instance's current slider values. For offline renders, which
    /// must never share mutable state with the live audio thread.
    pub fn spawn_fresh(&self, desc: &EffectDesc) -> Option<Arc<Self>> {
        Self::new(self.fns, self._lib.clone(), desc, self.sample_rate, &self.snapshot_params())
    }

    /// Process one sample with this instance's own current slider values.
    /// Call only from a single thread that owns the instance (offline
    /// renders); the live audio path uses the generation-stamped snapshot.
    pub fn process_private(&self, input: f32) -> f32 {
        let mut p = [0.0f32; MAX_PARAMS];
        self.read_params(&mut p);
        unsafe { self.call_sample(input, &p) }
    }

    fn snapshot_params(&self) -> Vec<f32> {
        (0..MAX_PARAMS).map(|k| self.param(k)).collect()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        (self.fns.destroy)(self.state);
    }
}

// ---------------------------------------------------------------------------
// Custom child GUI handles
// ---------------------------------------------------------------------------

/// A validated child-GUI vtable from a loaded effect, plus the library ref
/// that keeps its code mapped. Cheap to clone; the editor window owns one.
#[derive(Clone)]
pub struct EffectGuiRef {
    v: &'static EffectGuiV1,
    _lib: Arc<LibHandle>,
}
// Same reasoning as LibHandle: the Arc keeps the dlopen'd library mapped
// while any clone exists, and the vtable static is read-only.
unsafe impl Send for EffectGuiRef {}
unsafe impl Sync for EffectGuiRef {}

impl EffectGuiRef {
    /// Preferred canvas size (0/NaN hints fall back to a sane default).
    pub fn size_hint(&self) -> (f32, f32) {
        let w = self.v.want_width;
        let h = self.v.want_height;
        let w = if w.is_finite() && w >= 120.0 { w } else { 480.0 };
        let h = if h.is_finite() && h >= 90.0 { h } else { 320.0 };
        (w, h)
    }

    /// Open a per-window GUI state. Null = effect declined (auto window).
    pub fn create(&self, sample_rate: f64) -> *mut core::ffi::c_void {
        (self.v.create)(sample_rate)
    }

    /// Draw one frame. `state` is the audio instance state (valid while
    /// the editor's instance Arc lives); `api`/`frame` are per-call data.
    ///
    /// # Safety
    /// `gs` must come from `create` on this same handle (not yet destroyed)
    /// and `state` must belong to a live [`Instance`]; `frame`/`api` must
    /// stay valid for the duration of the call.
    pub unsafe fn draw(
        &self,
        gs: *mut core::ffi::c_void,
        state: *mut core::ffi::c_void,
        frame: &GuiFrameV1,
        api: &GuiApiV1,
    ) {
        (self.v.draw)(gs, state, frame, api)
    }

    /// Close the window's GUI state. The instance must still be alive.
    pub fn destroy(&self, gs: *mut core::ffi::c_void) {
        (self.v.destroy)(gs)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// One script found in the effects folder. Exactly one of `fns`/`error` is
/// meaningful: valid effects carry descriptors + callbacks; everything else
/// is listed in the GUI with its rejection reason.
pub struct EffectEntry {
    /// File stem (display id + persistence key).
    pub stem: String,
    pub desc: Option<EffectDesc>,
    fns: Option<EffectFns>,
    /// Custom child GUI exported by this effect (None = auto slider window).
    gui: Option<&'static EffectGuiV1>,
    pub error: Option<String>,
    /// Keep the library mapped while instances may exist.
    _lib: Option<Arc<LibHandle>>,
}

/// The effects folder plus everything loaded from it. Lives on the GUI
/// thread; Send/Sync is asserted for the engine handoff of its Arcs (the
/// registry itself is not shared across threads in practice, but the
/// wrapper types must be Send for the Arcs it produces).
pub struct Registry {
    dir: PathBuf,
    items: Vec<EffectEntry>,
}
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

impl Registry {
    pub fn open(dir: PathBuf) -> Self {
        Registry { dir, items: Vec::new() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// All scripts, valid and invalid, in folder order (alphabetical stem).
    pub fn items(&self) -> &[EffectEntry] {
        &self.items
    }

    /// Valid effects in alphabetical stem order.
    pub fn stems(&self) -> Vec<String> {
        self.items.iter().filter(|e| e.fns.is_some()).map(|e| e.stem.clone()).collect()
    }

    /// Loadable entry by stem.
    fn find(&self, stem: &str) -> Option<&EffectEntry> {
        self.items.iter().find(|e| e.stem == stem && e.fns.is_some())
    }

    pub fn desc(&self, stem: &str) -> Option<&EffectDesc> {
        self.find(stem).and_then(|e| e.desc.as_ref())
    }

    /// True when the folder holds a script with this stem (any state).
    pub fn has_file(&self, stem: &str) -> bool {
        self.items.iter().any(|e| e.stem == stem)
    }

    pub fn has_template(&self) -> bool {
        self.dir.join(TEMPLATE_FILE).is_file()
    }

    /// Write the starter template into the folder (never overwriting).
    /// Returns the file stem created.
    pub fn add_template(&mut self) -> Result<String, String> {
        if self.has_template() {
            return Err("template.rs already exists in the effects folder".into());
        }
        std::fs::write(self.dir.join(TEMPLATE_FILE), TEMPLATE_SRC)
            .map_err(|e| format!("could not write template: {e}"))?;
        self.rescan();
        Ok(TEMPLATE_FILE.trim_end_matches(".rs").to_string())
    }

    /// Re-point the registry at another folder (effects window "Apply").
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.items.clear();
    }

    /// Compile stale scripts, (re)load and validate everything. Compile
    /// errors are per-file: a broken script never hides the healthy ones.
    /// Runs on the GUI thread — rustc takes a moment per *changed* file.
    pub fn rescan(&mut self) {
        self.items.clear();
        seed_api(&self.dir);
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for ent in rd.flatten() {
                let path = ent.path();
                if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let stem = match path.file_stem() {
                    Some(s) => s.to_string_lossy().into_owned(),
                    None => continue,
                };
                // Skip the generated contract and dot/underscore scratch files.
                if stem == Path::new(API_FILE).file_stem().and_then(|s| s.to_str()).unwrap_or(API_FILE)
                    || stem.starts_with('_')
                    || stem.starts_with('.')
                {
                    continue;
                }
                files.push((stem, path));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (stem, path) in files {
            let item = self.compile_and_load(&stem, &path);
            self.items.push(item);
        }
    }

    fn compile_and_load(&self, stem: &str, src: &Path) -> EffectEntry {
        let mk = |error: String| EffectEntry { stem: stem.to_string(), desc: None, fns: None, gui: None, error: Some(error), _lib: None };
        if !rustc_available() {
            return mk("rustc not found — install rust (or rustup) to compile effects".into());
        }
        let so = match self.stamped_artifact(stem, src) {
            Ok(so) => so,
            Err(e) => return mk(e),
        };
        if !so.is_file() {
            if let Err(e) = compile_effect(src, &so) {
                return mk(e);
            }
            self.prune_stale(stem, &so);
        }
        let lib = match unsafe { libloading::Library::new(&so) } {
            Ok(l) => Arc::new(LibHandle(std::mem::ManuallyDrop::new(l))),
            Err(e) => return mk(format!("could not load {}: {e}", so.display())),
        };
        match unsafe { validate(&lib.0) } {
            Ok((desc, fns, gui)) => EffectEntry { stem: stem.to_string(), desc: Some(desc), fns: Some(fns), gui, error: None, _lib: Some(lib) },
            Err(e) => mk(e),
        }
    }

    /// `.build/<stem>-<stamp>.so`; the stamp folds source mtime+size so an
    /// edited script builds to a fresh path (fresh dlopen, no in-place
    /// overwrites of mapped files).
    fn stamped_artifact(&self, stem: &str, src: &Path) -> Result<PathBuf, String> {
        let meta = std::fs::metadata(src).map_err(|e| format!("stat failed: {e}"))?;
        let mut h = DefaultHasher::new();
        stem.hash(&mut h);
        meta.len().hash(&mut h);
        meta.modified().map(|t| format!("{t:?}")).unwrap_or_default().hash(&mut h);
        let stamp = h.finish();
        Ok(self.dir.join(".build").join(format!("{stem}-{stamp:016x}.so")))
    }

    /// Delete older artifacts of the same stem (best-effort; deleting a
    /// still-mapped file is safe on Linux).
    fn prune_stale(&self, stem: &str, keep: &Path) {
        let Some(dir) = keep.parent() else { return };
        if let Ok(rd) = std::fs::read_dir(dir) {
            let prefix = format!("{stem}-");
            for ent in rd.flatten() {
                let p = ent.path();
                let name = p.file_name().unwrap_or_else(|| OsStr::new("")).to_string_lossy().into_owned();
                if name.starts_with(&prefix) && p != keep {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }

    /// Create a live instance of a valid effect (null-create ⇒ None).
    pub fn create_instance(&self, stem: &str, sample_rate: f64, values: &[f32]) -> Option<Arc<Instance>> {
        let e = self.find(stem)?;
        let fns = e.fns?;
        let desc = e.desc.as_ref()?;
        let lib = e._lib.as_ref()?.clone();
        Instance::new(fns, lib, desc, sample_rate, values)
    }

    /// The effect's custom child GUI, if it exported a valid one. The
    /// handle keeps the library mapped independently of the registry.
    pub fn gui(&self, stem: &str) -> Option<EffectGuiRef> {
        let e = self.find(stem)?;
        let v = e.gui?;
        Some(EffectGuiRef { v, _lib: e._lib.as_ref()?.clone() })
    }
}

/// rustc invocation shared by the loader and its tests. Output paths are
/// unique so parallel compiles never collide.
fn compile_effect(src: &Path, out: &Path) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create build dir: {e}"))?;
    }
    let crate_name = sanitize_crate_name(
        out.file_stem().and_then(|s| s.to_str()).unwrap_or("simplooper_effect"),
    );
    let tmp = out.with_extension("so.tmp");
    let output = Command::new("rustc")
        .args(["--edition", "2021", "--crate-type", "cdylib", "-C", "opt-level=2"])
        .arg("--crate-name")
        .arg(&crate_name)
        .arg("-o")
        .arg(&tmp)
        .arg(src)
        .output()
        .map_err(|e| format!("could not run rustc: {e} — install rust or rustup"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let err = trim_head(err.trim(), 1800);
        std::fs::write(out.with_extension("so.failed"), &err).ok();
        return Err(format!("compile failed:\n{err}"));
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("could not store {}: {e}", out.display()))
}

/// rustc crate names are `[a-z0-9_]`, cannot be empty or a keyword prefix
/// issue for our purposes — fold anything else to `_` and prefix so a stem
/// like `test` or `2s` still builds.
fn sanitize_crate_name(stem: &str) -> String {
    let mut s = String::from("fx_");
    for c in stem.chars() {
        s.push(if c.is_ascii_alphanumeric() || c == '_' { c.to_ascii_lowercase() } else { '_' });
    }
    s
}

/// Read + validate the exported description from a loaded library. Also
/// picks up the optional custom child-GUI vtable (an effect that exports a
/// broken one is rejected outright — silent auto-fallback would hide the
/// very bug the author needs to see in the effects window).
unsafe fn validate(
    lib: &libloading::Library,
) -> Result<(EffectDesc, EffectFns, Option<&'static EffectGuiV1>), String> {
    let sym: libloading::Symbol<extern "C" fn() -> *const EffectInfoV1> =
        lib.get(ENTRY).map_err(|_| "missing entry point simplooper_effect_info — see template.rs".to_string())?;
    let info = sym();
    if info.is_null() {
        return Err("simplooper_effect_info returned null".into());
    }
    let d = &*info;
    if d.abi != ABI_VERSION {
        return Err(format!("effect ABI version {} (this simplooper speaks {ABI_VERSION})", d.abi));
    }
    let name = read_cstr(d.name).ok_or("effect has no name — DESC.name must be text")?;
    let author = d.author.cast_non_null().and_then(read_cstr);
    let description = d.description.cast_non_null().and_then(read_cstr);
    if d.n_params as usize > MAX_PARAMS {
        return Err(format!("effect declares {} params (max {MAX_PARAMS})", d.n_params));
    }
    if d.n_params > 0 && d.params.is_null() {
        return Err("n_params > 0 but the params array is null".into());
    }
    let mut params = Vec::with_capacity(d.n_params as usize);
    for k in 0..d.n_params as usize {
        let p = &*d.params.add(k);
        let label = read_cstr(p.label).ok_or_else(|| format!("param {k} has no label"))?;
        let (min, max) = if p.min <= p.max { (p.min, p.max) } else { (p.max, p.min) };
        params.push(ParamDesc { label, min, max, default: p.default.clamp(min, max) });
    }
    let gui = match lib.get(GUI_ENTRY) {
        Ok(gsym) => {
            let gsym: libloading::Symbol<extern "C" fn() -> *const EffectGuiV1> = gsym;
            let g = gsym();
            if g.is_null() {
                return Err("simplooper_effect_gui returned null".into());
            }
            let g = &*g;
            if g.abi != GUI_ABI_VERSION {
                return Err(format!("child GUI ABI version {} (this simplooper speaks {GUI_ABI_VERSION})", g.abi));
            }
            // create/draw/destroy are non-nullable fn types in the shared
            // ABI file — a script cannot put null in them and compile, so
            // (like EffectInfoV1) there is nothing to null-check.
            Some(g)
        }
        // No GUI symbol: the auto slider window handles this effect.
        Err(_) => None,
    };
    Ok((
        EffectDesc { name, author, description, params },
        EffectFns { create: d.create, destroy: d.destroy, process: d.process },
        gui,
    ))
}

/// `*const T` non-null helper (kept tiny; `Option::from` is for refs).
trait CastNonNull {
    fn cast_non_null(self) -> Option<*const core::ffi::c_char>;
}
impl CastNonNull for *const core::ffi::c_char {
    fn cast_non_null(self) -> Option<*const core::ffi::c_char> {
        (!self.is_null()).then_some(self)
    }
}

fn read_cstr(p: *const core::ffi::c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())).flatten()
}

fn trim_head(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n…", &s[..cut])
    }
}

// ---------------------------------------------------------------------------
// GUI-facing chain model (plain data; ordering lives here, audio is fed
// Arc<Instance> snapshots from the UI layer)
// ---------------------------------------------------------------------------

/// One row of the effects window: a script, whether this loop runs it, and
/// the current slider values (empty = not yet seeded; the UI fills defaults
/// from the descriptor).
#[derive(Clone, Debug)]
pub struct UiEntry {
    pub stem: String,
    pub enabled: bool,
    pub params: Vec<f32>,
}

/// Merge the folder contents into a loop's window list:
/// - enabled effects first, keeping their previous signal order (and dropping
///   ones whose script disappeared or failed to compile),
/// - then the disabled ones, alphabetically.
///
/// `saved` is the persisted chain for this loop (enabled order + values);
/// pass an empty slice on rescan (the live list already carries state).
pub fn merge_entries(existing: &[UiEntry], stems: &[String], saved: &[(String, Vec<f32>)]) -> Vec<UiEntry> {
    let mut out: Vec<UiEntry> = Vec::new();
    // Enabled block, in chain order: previous GUI list first (rescan), else
    // the saved config order (startup).
    let order: Vec<String> = if existing.iter().any(|e| e.enabled) {
        existing.iter().filter(|e| e.enabled).map(|e| e.stem.clone()).collect()
    } else {
        saved.iter().map(|(s, _)| s.clone()).collect()
    };
    for stem in order {
        if !stems.iter().any(|s| s == &stem) {
            continue; // script gone or now broken: silently dropped
        }
        let params = existing
            .iter()
            .find(|e| e.stem == stem)
            .map(|e| e.params.clone())
            .filter(|p| !p.is_empty())
            .or_else(|| saved.iter().find(|(s, _)| *s == stem).map(|(_, p)| p.clone()))
            .unwrap_or_default();
        out.push(UiEntry { stem, enabled: true, params });
    }
    // Disabled block: everything valid that is not enabled, alphabetical
    // (`stems` arrives alphabetical).
    for s in stems {
        if !out.iter().any(|e| e.stem == *s) {
            let keep = existing.iter().find(|e| &e.stem == s).map(|e| e.params.clone()).unwrap_or_default();
            out.push(UiEntry { stem: s.clone(), enabled: false, params: keep });
        }
    }
    out
}

/// GUI order after a toggle: enable → snap to the end of the enabled block,
/// disable → drop into the alphabetical disabled block.
pub fn resort_entries(entries: &mut Vec<UiEntry>, stems: &[String]) {
    let enabled: Vec<UiEntry> = entries.iter().filter(|e| e.enabled).cloned().collect();
    let mut v = merge_entries(&enabled, stems, &[]);
    // merge_entries starts from `enabled` (all enabled) and appends nothing;
    // the disabled half must come from `stems` fresh:
    for s in stems {
        if !v.iter().any(|e| e.stem == *s) {
            v.push(UiEntry { stem: s.clone(), enabled: false, params: Vec::new() });
        }
    }
    *entries = v;
}

/// Swap `k` with the neighbouring enabled entry (chain reordering in the
/// window). Returns true when the list changed.
pub fn move_enabled(entries: &mut [UiEntry], k: usize, up: bool) -> bool {
    let mut idx: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| e.enabled).map(|(i, _)| i).collect();
    let pos = match idx.iter().position(|&i| i == k) {
        Some(p) => p,
        None => return false,
    };
    let new_pos = if up {
        if pos == 0 {
            return false;
        }
        pos - 1
    } else {
        if pos + 1 >= idx.len() {
            return false;
        }
        pos + 1
    };
    idx.swap(pos, new_pos);
    entries.swap(idx[pos], idx[new_pos]);
    true
}

/// Values a chain slot should use: stored ones, padded with the descriptor
/// defaults.
pub fn seed_values(saved: &[f32], desc: &EffectDesc) -> Vec<f32> {
    desc.params
        .iter()
        .enumerate()
        .map(|(k, p)| saved.get(k).copied().unwrap_or(p.default).clamp(p.min, p.max))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("simplooper-fx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bundled_scripts_update_in_place_with_backup() {
        // A stale copy of a bundled script is refreshed on seed (app fixes
        // must reach existing installs), and the outgoing contents survive
        // as `<name>.rs.bak`.
        let dir = temp_dir("seedupdate");
        std::fs::write(dir.join(API_FILE), API_SRC).unwrap();
        let (name, src) = EXAMPLES[0];
        let p = dir.join(name);
        std::fs::write(&p, "// old stale copy\n").unwrap();
        seed_api(&dir);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), *src, "{name} not refreshed");
        assert_eq!(std::fs::read_to_string(p.with_extension("rs.bak")).unwrap(), "// old stale copy\n");
        // A current copy is left untouched (and no spurious backup).
        let bak = p.with_extension("rs.bak");
        std::fs::remove_file(&bak).unwrap();
        seed_api(&dir);
        assert!(!bak.exists(), "unchanged script must not be re-backed-up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_files_stay_in_sync() {
        // The repo copies (what PKGBUILD installs) must equal the embedded
        // sources the app writes/creates.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(std::fs::read_to_string(root.join("effects/effect_api.rs")).unwrap(), API_SRC);
        assert_eq!(std::fs::read_to_string(root.join("effects/template.rs")).unwrap(), TEMPLATE_SRC);
        for (name, src) in EXAMPLES {
            assert_eq!(std::fs::read_to_string(root.join("effects").join(name)).unwrap(), *src, "{name} out of sync");
        }
    }

    #[test]
    fn bundled_examples_load_with_custom_guis() {
        // A fresh effects folder seeds template + examples; every one must
        // compile through the real loader, and the two examples must carry
        // a valid child GUI while the plain template must not.
        if !rustc_available() {
            return; // CI without rustc: nothing to verify
        }
        let dir = temp_dir("examples");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let want = ["template", "parametric_eq", "space_echo"];
        for name in want {
            let it = reg.items().iter().find(|e| e.stem == name).unwrap_or_else(|| {
                panic!("missing {name}: {:?}", reg.items().iter().map(|e| (&e.stem, e.error.clone())).collect::<Vec<_>>())
            });
            assert!(it.error.is_none(), "{name} failed to load: {}", it.error.clone().unwrap_or_default());
        }
        assert_eq!(reg.desc("parametric_eq").unwrap().params.len(), 26);
        assert_eq!(reg.desc("space_echo").unwrap().params.len(), 11);
        assert!(reg.gui("parametric_eq").is_some(), "EQ must expose a child GUI");
        assert!(reg.gui("space_echo").is_some(), "space echo must expose a child GUI");
        assert!(reg.gui("template").is_none(), "template keeps the auto slider window");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bipolar_params_survive_loading() {
        // Band gain on the EQ is bipolar (-24..+24 dB). Both the initial
        // seed (loading a saved chain) and live slider moves must keep
        // negative values — a `clamp(0, ..)` here made the EQ "do nothing"
        // for cuts, the original user complaint.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("bipolar");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let inst = reg
            .create_instance("parametric_eq", 48000.0, &[1.0, 1000.0, -12.0, 1.0])
            .expect("eq instance");
        assert_eq!(inst.param(2), -12.0, "saved -12 dB gain must load");
        inst.set_param(2, -6.5);
        assert_eq!(inst.param(2), -6.5, "live set must keep the sign");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_echo_holds_a_wet_tail() {
        // After a single impulse, a wet echo must keep producing decaying
        // output for at least one delay time — proof the effect state and
        // feedback run at the instance level (what the serial chain feeds
        // through to whatever slot comes next).
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("echotail");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // time .25 s, fb .5, tone 8 k, wobble 0, rate 0, decay .5, damp 8 k, wet 1, out 1
        let inst = reg
            .create_instance(
                "space_echo",
                48000.0,
                &[0.25, 0.5, 8000.0, 0.0, 0.05, 0.5, 8000.0, 1.0, 1.0],
            )
            .expect("echo instance");
        let _ = inst.process_private(1.0);
        // Energy strictly after the impulse: the tail. Windows are wide
        // enough to bracket wherever the first tap lands (the delay line
        // reserves headroom beyond `time`).
        let win: Vec<f32> = (0..8)
            .map(|_| (0..4800).map(|_| inst.process_private(0.0).abs()).sum())
            .collect();
        let total: f32 = win.iter().sum();
        assert!(total > 1e-3, "wet echo produced no tail (sum {total})");
        let peak = win.iter().cloned().fold(0.0f32, f32::max);
        assert!(win[7] < peak, "tail must decay: {win:?}");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_echo_tail_modes_are_dense_not_metallic() {
        // Objective guard against the metallic "ping" on transients: after
        // an impulse, the LATE reverb tail must be a dense wash, not a
        // handful of knife-edge resonances. Method: impulse -> settle 0.4 s
        // -> Hann-windowed Goertzel of the next 0.4 s at 97 log-spaced
        // frequencies, count resonances standing out of the average floor.
        // Four bunched combs produced a handful of huge isolated peaks; the
        // eight-comb Freeverb bank must produce many more, much flatter
        // ones.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("echomodes");
        let probe = |dir: &Path, stem: &str| -> (usize, f64, f64, f64) {
            let mut reg = Registry::open(dir.to_path_buf());
            reg.rescan();
            // cathedral-ish: long decay, moderate damp, echo level 0 so the
            // FDN alone is measured.
            let inst = reg
                .create_instance(
                    stem,
                    48000.0,
                    &[0.25, 0.5, 8000.0, 0.0, 0.0, 0.95, 2800.0, 1.0, 1.0, 0.0, 1.0],
                )
                .expect("echo instance");
            let _ = inst.process_private(1.0);
            for _ in 0..24000 {
                let _ = inst.process_private(0.0);
            }
            // Goertzel magnitudes of one window at 161 log-spaced freqs.
            // 2 s windows give fine (0.5 Hz) resolution so INDIVIDUAL comb
            // modes resolve — that is exactly where sparse banks look
            // metallic and dense ones look like a wash.
            let window_mags = |x: &[f64]| -> Vec<f64> {
                let n = x.len();
                let mut mags = Vec::with_capacity(161);
                for k in 0..161 {
                    let f = 150.0f64 * (14000.0f64 / 150.0).powf(k as f64 / 160.0);
                    let w = core::f64::consts::TAU * f / 48000.0;
                    let cw = w.cos();
                    let (mut s1, mut s2) = (0.0f64, 0.0f64);
                    for (i, &v) in x.iter().enumerate() {
                        let h = 0.5 - 0.5 * (core::f64::consts::TAU * i as f64 / (n as f64 - 1.0)).cos();
                        let s0 = v * h + 2.0 * cw * s1 - s2;
                        s2 = s1;
                        s1 = s0;
                    }
                    mags.push((s1 * s1 + s2 * s2 - 2.0 * cw * s1 * s2).max(0.0).sqrt());
                }
                mags
            };
            const N: usize = 96000;
            let mut early = Vec::with_capacity(N);
            for _ in 0..N {
                early.push(inst.process_private(0.0) as f64);
            }
            let mags = window_mags(&early);
            // Late slice, ~1 s further in: where air absorption should have
            // eaten the treble (the thing that made the old tail "tinny").
            for _ in 0..48000 {
                let _ = inst.process_private(0.0);
            }
            let mut late = Vec::with_capacity(N);
            for _ in 0..N {
                late.push(inst.process_private(0.0) as f64);
            }
            let lmags = window_mags(&late);
            let band_mean = |m: &[f64], lo: usize, hi: usize| -> f64 {
                let s: f64 = m[lo..hi].iter().sum();
                s / (hi - lo) as f64
            };
            // hi (≈5.5–14 k) vs mid (≈1–4 k): natural tails darken.
            let hm_e = band_mean(&mags, 127, 161) / band_mean(&mags, 67, 116).max(1e-30);
            let hm_l = band_mean(&lmags, 127, 161) / band_mean(&lmags, 67, 116).max(1e-30);
            let mean = mags.iter().sum::<f64>() / mags.len() as f64;
            let mut peaks = 0usize;
            // Tilt-removed concentration: each bin over a ±4-bin moving
            // average of its neighbours. A dark high-frequency rolloff (what
            // a *good* tail does) cancels in the ratio; a lone ringing mode
            // does not. Raw neighbour ratios punish the shelf edge instead
            // (1000x drops across 290 Hz at 9 kHz on a deep tilt).
            let smooth: Vec<f64> = (0..mags.len())
                .map(|i| {
                    let lo = i.saturating_sub(4);
                    let hi = (i + 5).min(mags.len());
                    mags[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
                })
                .collect();
            let mut conc = 1.0f64;
            for i in 1..mags.len() - 1 {
                if mags[i] > mags[i - 1] && mags[i] >= mags[i + 1] && mags[i] > mean * 2.0 {
                    peaks += 1;
                }
                conc = conc.max(mags[i] / smooth[i].max(1e-30));
            }
            drop(reg);
            (peaks, conc, hm_e, hm_l)
        };
        let (peaks, conc, hm_e, hm_l) = probe(&dir, "space_echo");
        println!("MODEPROBE current: peaks={peaks} concentration={conc:.1} hi/mid {hm_e:.3}→{hm_l:.3}");
        // Calibration (tilt-removed concentration): current bank ~7.5,
        // deliberately bunched combs through the same damping stack ~8.7,
        // the original unmodulated metallic bank (no damping, no diffusion)
        // far higher; density (peaks) and the hi/mid darkening below are
        // the primary guards — the tail must be a wash AND must lose
        // treble as it decays ("tinny" was the complaint).
        assert!(peaks >= 5, "late tail empty? only {peaks} resonances visible");
        assert!(conc < 12.0, "late tail metallic: one mode {conc:.1}x over its local envelope");
        assert!(hm_l < hm_e * 0.65, "tail stays tinny: hi/mid {hm_e:.3} → {hm_l:.3} barely darkens");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diag_whine_probe() {
        // TEMPORARY diagnostic — high-pitched whine report. Removed before ship.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("diagwhine");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let mut rng = 0x2545F491u64;
        let mut rnd = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            ((rng as f64) / (u64::MAX as f64)) * 2.0 - 1.0
        };
        let window_mags = |x: &[f64]| -> Vec<(f64, f64)> {
            let n = x.len();
            let mut out = Vec::new();
            for k in 0..161 {
                let f = 150.0f64 * (16000.0f64 / 150.0).powf(k as f64 / 160.0);
                let w = core::f64::consts::TAU * f / 48000.0;
                let cw = w.cos();
                let (mut s1, mut s2) = (0.0f64, 0.0f64);
                for (i, &v) in x.iter().enumerate() {
                    let h = 0.5 - 0.5 * (core::f64::consts::TAU * i as f64 / (n as f64 - 1.0)).cos();
                    let s0 = v * h + 2.0 * cw * s1 - s2;
                    s2 = s1;
                    s1 = s0;
                }
                out.push((f, (s1 * s1 + s2 * s2 - 2.0 * cw * s1 * s2).max(0.0).sqrt()));
            }
            out
        };
        // (a) sustain test: noise burst, then silence; RMS every 100 ms.
        // A self-oscillating tone shows as an RMS PLATEAU instead of decay.
        let sets: [(&str, [f32; 11]); 4] = [
            ("default-Echo", [0.38, 0.62, 3600.0, 0.0, 0.6, 0.84, 2800.0, 0.4, 1.0, 1.0, 1.0]),
            ("cathedral", [0.32, 0.5, 2200.0, 0.25, 0.14, 0.95, 1500.0, 0.7, 0.9, 0.18, 1.0]),
            ("verb-only-hot", [0.25, 0.5, 8000.0, 0.0, 0.0, 0.95, 16000.0, 1.0, 1.0, 0.0, 1.5]),
            ("bright-damp-max", [0.25, 0.5, 8000.0, 0.0, 0.0, 0.95, 16000.0, 1.0, 1.0, 1.0, 1.5]),
        ];
        for (name, params) in sets {
            let inst = reg.create_instance("space_echo", 48000.0, &params).expect("inst");
            for _ in 0..4800 {
                let _ = inst.process_private(rnd() as f32 * 0.3);
            }
            let mut rms = Vec::new();
            for _ in 0..40 {
                let e: f64 = (0..4800).map(|_| {
                    let y = inst.process_private(0.0) as f64;
                    y * y
                }).sum();
                rms.push((e / 4800.0).sqrt());
            }
            let head = rms[0].max(1e-12);
            let ratios: Vec<String> = rms.iter().step_by(4).map(|r| format!("{:.2}", r / head)).collect();
            println!("SUSTAIN {name}: ratios(0.4s steps) {ratios:?}");
            // late-window spectrum 1.6–2.6 s in: dominant peaks over local floor
            for _ in 0..28800 {
                let _ = inst.process_private(0.0);
            }
            let late: Vec<f64> = (0..48000).map(|_| inst.process_private(0.0) as f64).collect();
            let mags = window_mags(&late);
            let mm: Vec<f64> = mags.iter().map(|m| m.1).collect();
            let smooth: Vec<f64> = (0..mm.len()).map(|i| {
                let lo = i.saturating_sub(3);
                let hi = (i + 4).min(mm.len());
                mm[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
            }).collect();
            let mut peaks: Vec<(f64, f64)> = (1..mm.len() - 1)
                .filter(|&i| mm[i] > mm[i - 1] && mm[i] >= mm[i + 1])
                .map(|i| (mags[i].0, mm[i] / smooth[i].max(1e-30)))
                .collect();
            peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let top: Vec<String> = peaks.iter().take(6).map(|(f, c)| format!("{f:.0}Hz x{c:.1}")).collect();
            let late_peak = late.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            println!("LATE {name}: peak={late_peak:.2e} top-peaks {top:?}");
            drop(inst);
        }
        // (b) tone sensitivity: echo tail hi/lo energy vs tone param.
        for tone in [200.0f32, 3600.0, 16000.0] {
            let inst = reg
                .create_instance("space_echo", 48000.0, &[0.25, 0.9, tone, 0.0, 0.05, 0.5, 8000.0, 1.0, 1.0, 1.0, 0.0])
                .expect("inst");
            for _ in 0..2400 {
                let _ = inst.process_private(rnd() as f32 * 0.5);
            }
            let tail: Vec<f64> = (0..48000).map(|_| inst.process_private(0.0) as f64).collect();
            let mags = window_mags(&tail);
            let mean = |lo: usize, hi: usize| -> f64 {
                mags[lo..hi].iter().map(|m| m.1).sum::<f64>() / (hi - lo) as f64
            };
            let hilo = mean(120, 161) / mean(20, 70).max(1e-30);
            let peak = tail.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            println!("TONE {tone:.0}: tail peak={peak:.3} hi/lo={hilo:.4}");
            drop(inst);
        }
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diag_sine_adds_nothing() {
        // TEMPORARY diagnostic - sine in, anything else out? A whine is energy
        // the plugin invents away from the input. Removed before ship.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("diagsine");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let goertzel = |x: &[f64], f: f64| -> f64 {
            let w = core::f64::consts::TAU * f / 48000.0;
            let cw = w.cos();
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for &v in x {
                let s0 = v + 2.0 * cw * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            (s1 * s1 + s2 * s2 - 2.0 * cw * s1 * s2).max(0.0).sqrt()
        };
        let sets: [(&str, [f32; 11]); 4] = [
            ("user-opera", [0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0]),
            ("opera-no-wob", [0.2, 0.42, 3800.0, 0.0, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0]),
            ("opera-no-verb", [0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 0.0]),
            ("opera-no-echo", [0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.0, 1.0]),
        ];
        for (name, params) in sets {
            let inst = reg.create_instance("space_echo", 48000.0, &params).expect("inst");
            for i in 0..144000u64 {
                let x = (core::f64::consts::TAU * 1000.0 * i as f64 / 48000.0).sin() * 0.4;
                let _ = inst.process_private(x as f32);
            }
            let win: Vec<f64> = (0..48000u64)
                .map(|i| {
                    let x = (core::f64::consts::TAU * 1000.0 * i as f64 / 48000.0).sin() * 0.4;
                    inst.process_private(x as f32) as f64
                })
                .collect();
            let norm = 2.0 / win.len() as f64; // single-bin amplitude from Goertzel
            let fund = goertzel(&win, 1000.0) * norm;
            let mut sum_out = 0.0f64;
            let mut hi_out = 0.0f64;
            let mut top = (0.0f64, 0.0f64);
            for k in 0..120 {
                let f = 60.0f64 * (16000.0f64 / 60.0).powf(k as f64 / 119.0);
                if (f - 1000.0).abs() < 30.0 {
                    continue; // skip the fundamental bin neighbourhood
                }
                let m = goertzel(&win, f) * norm;
                sum_out += m * m;
                if f > 4000.0 {
                    hi_out += m * m;
                }
                if m > top.1 {
                    top = (f, m);
                }
            }
            println!(
                "SINE {name}: fundAmp={fund:.4} out-of-band rms={:.4} hi-band rms={:.4} top={:.0}Hz amp={:.5} ratio-to-fund={:.4}",
                sum_out.sqrt(),
                hi_out.sqrt(),
                top.0,
                top.1,
                top.1 / fund.max(1e-12)
            );
            drop(inst);
        }
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_echo_verb_and_echo_levels_are_independent() {
        // The two level params pick between the two effect halves:
        // with VERB at 0 nothing exists between the impulse and the first
        // echo tap (delay 250 ms = 12000 samples); with ECHO at 0 the FDN
        // still fills that window from the dry feed — a room with no slap.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("echoverb");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let mk = |echo: f32, verb: f32| {
            reg.create_instance(
                "space_echo",
                48000.0,
                &[0.25, 0.5, 8000.0, 0.0, 0.05, 0.9, 8000.0, 1.0, 1.0, echo, verb],
            )
            .expect("echo instance")
        };
        let window_sum = |inst: &std::sync::Arc<Instance>| -> f32 {
            let _ = inst.process_private(1.0);
            // Samples 2800..11900: after the FDN fills (the diffuser chain's
            // group delay puts the first verb sample around 2700) and before
            // the first echo tap (0.25 s = 12000 samples).
            (0..11900).skip(2800).map(|_| inst.process_private(0.0).abs()).sum()
        };
        let echo_only = window_sum(&mk(1.0, 0.0));
        let verb_only = window_sum(&mk(0.0, 1.0));
        assert!(echo_only < 1e-6, "echo-only must be silent before the first tap (sum {echo_only})");
        assert!(verb_only > 1e-3, "verb-only must fill the pre-echo window (sum {verb_only})");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eq_peaking_bands_are_stable_and_correct() {
        // The peaking (bell) denominator must be (1 + α/A, −2cos, 1 − α/A).
        // A sign swap there (1 − α/A … 1 + α/A) parks the poles OUTSIDE the
        // unit circle, so a peaking band self-oscillates to ±inf for ANY
        // nonzero alpha — even at 0 dB gain. With four peaking bands at the
        // EQ's defaults, enabling it pegged the output meter at inf.
        // Also covers the shelf-slope α: the shelf-SLOPE form goes
        // imaginary at high gain + high Q; clamped to 0 it parked shelf
        // poles exactly ON the unit circle (perpetual ringing).
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("eqstab");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let flat_off = [0.0f32, 1000.0, 0.0, 1.0];
        let mut vals = Vec::new();
        vals.extend_from_slice(&flat_off); // band 1: off
        for _ in 1..6 {
            vals.extend_from_slice(&flat_off);
        }
        vals.extend_from_slice(&[1.0, 1.0]); // mix, out
        // 1) defaults-equivalent cascade + impulse: finite, near pass-through.
        let inst = reg.create_instance("parametric_eq", 48000.0, &vals).expect("eq");
        // impulse + settle, then a sine; the pass-through must keep the
        // impulse energy at ~1 head, dead-silent tail, and unity sine gain.
        let head: f32 = (0..240).map(|_| inst.process_private(1.0).abs()).fold(0.0, f32::max);
        let tail: f32 = (0..4560).map(|_| inst.process_private(0.0).abs()).fold(0.0, f32::max);
        assert!(head > 0.5 && tail < 0.05, "flat EQ impulse head {head} tail {tail}");
        let out: Vec<f32> = (0..48000)
            .map(|i| {
                let x = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin() * 0.3;
                inst.process_private(x + if i % 7 == 0 { 0.05 } else { -0.05 })
            })
            .collect();
        assert!(out.iter().all(|y| y.is_finite()), "non-finite EQ output");
        let peak = out.iter().fold(0.0f32, |a, y| a.max(y.abs()));
        assert!((0.25..0.5).contains(&peak), "flat EQ must pass ~unity, peak {peak}");
        // 2) a +6 dB peaking band at 1 kHz must lift a 1 kHz sine by ~2×.
        let mut vals = vals.clone();
        vals[0] = 3.0; // peaking
        vals[2] = 6.0; // +6 dB
        let inst = reg.create_instance("parametric_eq", 48000.0, &vals).unwrap();
        let mut sum = 0.0f64;
        let mut n = 0.0f64;
        for i in 0..48000u64 {
            let x = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin();
            let y = inst.process_private(x) as f64;
            assert!(y.is_finite(), "peaking band produced {y}");
            if i > 9600 {
                sum += y * y;
                n += 1.0;
            }
        }
        let rms = (sum / n).sqrt() as f32;
        let want = 0.707 * 10.0f32.powf(6.0 / 20.0); // sine RMS × gain
        assert!((rms / want - 1.0).abs() < 0.04, "peaking +6 dB: rms {rms}, expected ~{want}");
        // 3) high-Q/high-gain shelves: impulse must DIE AWAY, not ring forever.
        let mut vals = vals.clone();
        vals[0] = 1.0; // low shelf
        vals[2] = 24.0;
        vals[3] = 8.0; // Q 8 — the shelf-S formula went imaginary here
        vals[20] = 2.0; // high shelf
        vals[22] = 24.0;
        vals[23] = 8.0;
        let inst = reg.create_instance("parametric_eq", 48000.0, &vals).unwrap();
        // A resonant shelf peaks ~tens of ms after the impulse (band build-up
        // + coefficient glide); the OLD clamped-alpha shelf kept ringing
        // forever instead. Head = first 100 ms, tail = the next 2.5 s.
        let head: f32 = (0..4800).map(|_| inst.process_private(1.0).abs()).fold(0.0, f32::max);
        let tail: f32 = (0..120000).map(|_| inst.process_private(0.0).abs()).fold(0.0, f32::max);
        assert!(head > 1.0, "resonant shelf produced no response: {head}");
        assert!(tail < head * 0.01, "shelf ringing forever: tail {tail} vs head {head}");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_keeps_crate_names_legal() {
        assert_eq!(sanitize_crate_name("my-effect 2"), "fx_my_effect_2");
        assert_eq!(sanitize_crate_name("api"), "fx_api");
        assert!(sanitize_crate_name("").chars().all(|c| c == '_' || c.is_alphanumeric()));
    }

    #[test]
    fn merge_and_resort_rules() {
        let stems: Vec<String> = ["clean", "fuzz", "trem"].iter().map(|s| s.to_string()).collect();
        // Startup: saved order wins for the enabled block.
        let saved = vec![("trem".to_string(), vec![0.5]), ("fuzz".to_string(), vec![])];
        let mut v = merge_entries(&[], &stems, &saved);
        assert_eq!(
            v.iter().map(|e| (e.stem.as_str(), e.enabled)).collect::<Vec<_>>(),
            vec![("trem", true), ("fuzz", true), ("clean", false)]
        );
        // Rescan keeps the live order and drops dead scripts.
        v.push(UiEntry { stem: "gone".into(), enabled: true, params: vec![] });
        let mut v = merge_entries(&v, &stems, &[]);
        assert_eq!(
            v.iter().map(|e| (e.stem.as_str(), e.enabled)).collect::<Vec<_>>(),
            vec![("trem", true), ("fuzz", true), ("clean", false)]
        );
        // Toggle a disabled one on: snaps to the end of the enabled block.
        v.iter_mut().find(|e| e.stem == "clean").unwrap().enabled = true;
        resort_entries(&mut v, &stems);
        assert_eq!(
            v.iter().map(|e| (e.stem.as_str(), e.enabled)).collect::<Vec<_>>(),
            vec![("trem", true), ("fuzz", true), ("clean", true)]
        );
        // Disable the head of the chain: it drops to the disabled block,
        // the rest keeps its order.
        v[0].enabled = false;
        resort_entries(&mut v, &stems);
        assert_eq!(
            v.iter().map(|e| (e.stem.as_str(), e.enabled)).collect::<Vec<_>>(),
            vec![("fuzz", true), ("clean", true), ("trem", false)]
        );
    }

    #[test]
    fn move_enabled_walks_the_chain_order() {
        let mut v: Vec<UiEntry> = ["a", "b", "c", "d"]
            .iter()
            .map(|s| UiEntry { stem: s.to_string(), enabled: *s != "c", params: vec![] })
            .collect();
        // v order: a(on) b(on) c(off) d(on) — enabled chain is a,b,d.
        assert!(move_enabled(&mut v, 3, true), "d moves up past b");
        assert_eq!(
            v.iter().map(|e| (e.stem.as_str(), e.enabled)).collect::<Vec<_>>(),
            vec![("a", true), ("d", true), ("c", false), ("b", true)]
        );
        assert!(!move_enabled(&mut v, 0, true), "head cannot go up");
        assert!(!move_enabled(&mut v, 2, true), "disabled entries never move");
    }

    #[test]
    fn seed_values_pads_with_defaults() {
        let d = EffectDesc {
            name: "x".into(),
            author: None,
            description: None,
            params: vec![
                ParamDesc { label: "a".into(), min: 0.0, max: 1.0, default: 0.25 },
                ParamDesc { label: "b".into(), min: 0.0, max: 2.0, default: 1.0 },
            ],
        };
        assert_eq!(seed_values(&[0.9, 1.5, 9.0], &d), vec![0.9, 1.5]);
        assert_eq!(seed_values(&[], &d), vec![0.25, 1.0]);
    }

    #[test]
    fn template_compiles_and_runs() {
        // The whole pipeline the GUI relies on, on a real effect: write the
        // embedded API contract + template to a temp folder, compile, load,
        // validate, create, process, destroy.
        if !rustc_available() {
            eprintln!("skipping: rustc not on PATH");
            return;
        }
        let dir = temp_dir("template");
        std::fs::write(dir.join(API_FILE), API_SRC).unwrap();
        std::fs::write(dir.join(TEMPLATE_FILE), TEMPLATE_SRC).unwrap();
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        assert_eq!(reg.stems(), vec!["template".to_string()], "errors: {:?}", reg.items().iter().map(|i| &i.error).collect::<Vec<_>>());
        let desc = reg.desc("template").unwrap();
        assert_eq!(desc.name, "Template Tremolo");
        assert_eq!(desc.params.len(), 4);
        let inst = reg.create_instance("template", 48000.0, &[0.0, 4.0, 1.0, 1.0]).expect("create");
        let mut p = [0.0f32; MAX_PARAMS];
        inst.read_params(&mut p);
        assert_eq!(p[0], 0.0);
        // depth 0 => exact pass-through.
        unsafe {
            assert_eq!(inst.call_sample(0.5, &p), 0.5);
            assert_eq!(inst.call_sample(-0.25, &p), -0.25);
        }
        // Ride the depth slider: the generation bumps, a snapshot picks it
        // up, and a deep fast wobble actually wobbles.
        inst.set_param(0, 1.0);
        inst.set_param(1, 1000.0);
        inst.read_params(&mut p);
        let g = inst.param_gen();
        unsafe {
            let a = inst.call_sample(1.0, &p);
            let b = inst.call_sample(1.0, &p);
            assert!((a - b).abs() > 1e-6, "LFO must advance between samples");
            assert!((0.0..=1.0001).contains(&a) && (0.0..=1.0001).contains(&b));
        }
        assert!(g > 1);
        // Offline-render clone is independent but carries the values.
        let fresh = inst.spawn_fresh(desc).expect("spawn_fresh");
        fresh.read_params(&mut p);
        assert_eq!(p[0], 1.0);
        drop(fresh);
        drop(inst);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gui_symbol_optional_and_validated() {
        // Optional child GUI: a script exporting it loads with a handle;
        // one exporting a WRONG GUI ABI is rejected; scripts without the
        // symbol (template) load with gui() == None.
        if !rustc_available() {
            eprintln!("skipping: rustc not on PATH");
            return;
        }
        let minimal_audio = r#"
include!("effect_api.rs");
extern "C" fn create(_sr: f64) -> *mut core::ffi::c_void { core::ptr::null_mut() }
extern "C" fn destroy(_s: *mut core::ffi::c_void) {}
extern "C" fn process(_s: *mut core::ffi::c_void, i: f32, _p: *const f32) -> f32 { i }
pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SIMPLOOPER_EFFECT_ABI, name: c"t".as_ptr(), author: core::ptr::null(),
    description: core::ptr::null(), n_params: 0, params: core::ptr::null(),
    create, destroy, process,
};
#[no_mangle]
pub extern "C" fn simplooper_effect_info() -> *const EffectInfoV1 { &DESC }
"#;
        let gui_fns = r#"
extern "C" fn gcreate(_sr: f64) -> *mut core::ffi::c_void { core::ptr::null_mut() }
extern "C" fn gdraw(_g: *mut core::ffi::c_void, _s: *mut core::ffi::c_void,
    _f: *const GuiFrameV1, _a: *const GuiApiV1) {}
extern "C" fn gdestroy(_g: *mut core::ffi::c_void) {}
pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: ABI_CONST, want_width: 100.0, want_height: 100.0,
    create: gcreate, draw: gdraw, destroy: gdestroy,
};
#[no_mangle]
pub extern "C" fn simplooper_effect_gui() -> *const EffectGuiV1 { &GUI }
"#;
        let dir = temp_dir("gui");
        std::fs::write(dir.join(API_FILE), API_SRC).unwrap();
        std::fs::write(dir.join("plain.rs"), minimal_audio).unwrap();
        std::fs::write(
            dir.join("guied.rs"),
            format!("const ABI_CONST: u32 = SIMPLOOPER_EFFECT_GUI_ABI;\n{minimal_audio}{gui_fns}"),
        )
        .unwrap();
        std::fs::write(
            dir.join("badgui.rs"),
            format!("const ABI_CONST: u32 = 99;\n{minimal_audio}{gui_fns}"),
        )
        .unwrap();
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let stems = reg.stems();
        assert!(stems.contains(&"plain".to_string()) && stems.contains(&"guied".to_string()), "{:?}", reg.items().iter().map(|i| (&i.stem, &i.error)).collect::<Vec<_>>());
        assert!(reg.gui("plain").is_none(), "no GUI symbol must mean auto window");
        assert!(reg.gui("guied").is_some(), "valid GUI symbol must produce a handle");
        let err = reg.items().iter().find(|i| i.stem == "badgui").and_then(|i| i.error.clone()).unwrap();
        assert!(err.contains("GUI ABI"), "{err}");
        // Size hint falls back when the effect leaves it at zero.
        drop(reg);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_scripts_are_reported_not_fatal() {
        let dir = temp_dir("broken");
        std::fs::write(dir.join("ok.rs"), TEMPLATE_SRC).unwrap();
        std::fs::write(dir.join("broken.rs"), "fn main() { this is not rust }").unwrap();
        std::fs::write(dir.join("noinfo.rs"), "// compiles, but exports nothing\n").unwrap();
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // A missing compiler is an environment issue, not a code bug.
        if !rustc_available() {
            assert!(reg.items().iter().all(|i| i.error.as_deref().unwrap_or("").contains("rustc")));
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        assert_eq!(reg.stems(), vec!["ok".to_string()]);
        let errs: HashMap<_, _> = reg.items().iter().map(|i| (i.stem.clone(), i.error.clone().unwrap_or_default())).collect();
        assert!(errs["broken"].contains("compile failed"), "{}", errs["broken"]);
        assert!(errs["noinfo"].contains("simplooper_effect_info"), "{}", errs["noinfo"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn user_presets_folder_prefix_and_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sp-userpresets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = ensure_user_presets_dir(&dir).unwrap();
        assert!(d.is_dir());
        assert_eq!(d.file_name().unwrap(), std::ffi::OsStr::new(USER_PRESET_DIR));
        // Ownership by prefix: only `{stem}-*.json` files belong to a plugin.
        std::fs::write(d.join("space_echo-Warm Room.json"), "{}").unwrap();
        std::fs::write(d.join("space_echo-Cathedral Big.json"), "{}").unwrap();
        std::fs::write(d.join("parametric_eq-Warm Room.json"), "{}").unwrap();
        std::fs::write(d.join("space_echo-notes.txt"), "").unwrap();
        std::fs::write(d.join("unowned.json"), "").unwrap();
        std::fs::write(d.join("space_echo-X.json"), "{}").unwrap();
        assert_eq!(list_user_presets(&dir, "space_echo"), vec!["Cathedral Big", "Warm Room", "X"]);
        assert_eq!(list_user_presets(&dir, "parametric_eq"), vec!["Warm Room"]);
        assert_eq!(list_user_presets(&dir, "nope"), Vec::<String>::new());

        // JSON round trip: numbers out and back, plugin/name never leak as params.
        let params = vec![
            ("time".to_string(), 0.32f32),
            ("damp".to_string(), 1500.0),
            ("tiny".to_string(), -2.5e-3),
        ];
        let j = preset_to_json("space_echo", "Warm Room", &params).unwrap();
        let path = user_preset_path(&dir, "space_echo", "Warm Room");
        std::fs::write(&path, &j).unwrap();
        let back = preset_from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 3, "{j}");
        assert!((back[0].1 - 0.32).abs() < 1e-6);
        assert!((back[1].1 - 1500.0).abs() < 1e-3);
        assert!((back[2].1 + 0.0025).abs() < 1e-9);
        assert!(!back.iter().any(|(k, _)| k == "plugin" || k == "name"));

        // Non-finite values must not be written at all.
        assert!(preset_to_json("x", "y", &[("bad".to_string(), f32::NAN)]).is_err());

        // Name sanitizing: no traversal, no hidden files, unicode-safe.
        assert_eq!(sanitize_preset_name("../../evil"), "evil");
        assert_eq!(sanitize_preset_name("  My/Room 2  "), "My-Room 2");
        assert_eq!(sanitize_preset_name("///"), "preset");
        assert_eq!(sanitize_preset_name("Warm Room ★"), "Warm Room");

        // Delete, and idempotent delete of a gone file.
        delete_user_preset(&dir, "space_echo", "Warm Room").unwrap();
        assert_eq!(list_user_presets(&dir, "space_echo"), vec!["Cathedral Big", "X"]);
        delete_user_preset(&dir, "space_echo", "Warm Room").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
