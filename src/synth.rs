//! Synthesizer plugin framework — the MIDI counterpart of [`crate::fx`].
//!
//! Every script `*.rs` in the synths folder (`~/.local/share/slooper/synths`)
//! is compiled with rustc into a cdylib and loaded behind the small C ABI in
//! [`crate::synth_api`]. A *track* owns exactly one synth instance (its
//! `Instance` is created on the GUI thread and pushed into the engine, which
//! calls it on the audio thread); a track with no binding falls back to the
//! bundled `piano`. The bundled `piano` and `sampler` scripts live in the
//! repo and are seeded into the folder on first start (updated in place when
//! slooper ships a newer bundled version — same rule as effect scripts).
//!
//! Sliders ride atomics exactly like effect params, and samplers fetch rack
//! audio through the host table ([`set_host_assets`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use crate::synth_api::{
    SynthAssetV1, SynthHostV1, SynthInfoV1, SynthParamV1, SLOOPER_MAX_SYNTH_PARAMS,
    SLOOPER_SYNTH_ABI,
};

pub const MAX_PARAMS: usize = SLOOPER_MAX_SYNTH_PARAMS;

/// Bundled scripts. `synth_api.rs` is rewritten (and version-pinned) on
/// every start; the synths themselves are seeded when missing or stale.
pub const API_SRC: &str = include_str!("synth_api.rs");
pub const BUNDLED: &[(&str, &str)] = &[
    ("piano.rs", include_str!("../synths/piano.rs")),
    ("sampler.rs", include_str!("../synths/sampler.rs")),
    // Attribution for the piano's modal model (MIT, fan455); the registry
    // only compiles *.rs, so this rides along as documentation.
    (
        "THIRD-PARTY-NOTICES.md",
        include_str!("../synths/THIRD-PARTY-NOTICES.md"),
    ),
];
/// Bump when a bundled script changes, so shipped fixes replace the user
/// folder's copy (with a `.bak` backup) instead of freezing at first seed.
const BUNDLED_VERSION: &str = "3";

/// Default synths folder (overridable with `SLOOPER_SYNTHS_DIR` for tests).
pub fn default_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SLOOPER_SYNTHS_DIR") {
        return PathBuf::from(d);
    }
    let base = dirs_like_data();
    base.join("slooper").join("synths")
}

fn dirs_like_data() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
}

fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".cache")
        });
    base.join("slooper-synth")
}

/// Write the ABI contract + bundled scripts into the folder. Bundled scripts
/// present in the folder but stamped with an older `// slooper-synth-version`
/// are replaced (backup `<name>.rs.bak`) — the same anti-freeze rule that
/// keeps effect-script fixes visible.
pub fn seed_folder(dir: &Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return; // read-only folder: the registry just lists fewer synths
    }
    let api = dir.join("synth_api.rs");
    // The shipped API file already carries the current version marker.
    let want_api = API_SRC.to_string();
    let stale_api = std::fs::read_to_string(&api).map(|s| s != want_api).unwrap_or(true);
    if stale_api && std::fs::write(&api, &want_api).is_ok() {
        // Version note only; scripts recompile on next rescan by mtime.
    }
    for (name, src) in BUNDLED {
        let p = dir.join(name);
        match std::fs::read_to_string(&p) {
            Ok(cur) => {
                if cur == *src {
                    continue;
                }
                // Only auto-update copies still carrying OUR version stamp —
                // a hand-edited piano.rs is the user's file. The stamp lives
                // on line 2 (line 1 is the `==== slooper ==` banner), so
                // scan the header block, not just the first line.
                let stamped = cur
                    .lines()
                    .take(4)
                    .any(|l| l.contains("slooper-synth-version:"))
                    && src.contains(&format!("slooper-synth-version: {BUNDLED_VERSION}"));
                if stamped && cur != *src {
                    let bak = dir.join(format!("{name}.bak"));
                    let _ = std::fs::write(&bak, cur);
                    let _ = std::fs::write(&p, src);
                }
            }
            Err(_) => {
                let _ = std::fs::write(&p, src);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host services (rack audio for samplers)
// ---------------------------------------------------------------------------

/// Rack table exposed to samplers; refreshed by the engine on set_project.
static HOST_TABLE: RwLock<Option<(Arc<Vec<crate::assets::AssetData>>, u32)>> = RwLock::new(None);

// Keeps the Arc alive so the pointer returned to a plugin stays valid at
// least until the next load_asset on the same (audio) thread.
thread_local! {
    static HOST_KEEP: std::cell::RefCell<Option<Arc<Vec<crate::assets::AssetData>>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_host_assets(assets: Arc<Vec<crate::assets::AssetData>>, rate: u32) {
    if let Ok(mut t) = HOST_TABLE.write() {
        *t = Some((assets, rate));
    }
}

extern "C" fn host_load_asset(id: u32, out: *mut SynthAssetV1) -> u8 {
    let Ok(t) = HOST_TABLE.read() else {
        return 0;
    };
    let Some((table, rate)) = t.as_ref() else {
        return 0;
    };
    let Some(data) = table.get(id as usize) else {
        return 0;
    };
    if data.is_empty() {
        return 0;
    }
    HOST_KEEP.with(|k| *k.borrow_mut() = Some(table.clone()));
    unsafe {
        (*out).data = data.as_ptr();
        (*out).len = data.len() as u32;
        (*out).sample_rate = *rate;
    }
    1
}

/// The host vtable handed to every synth `create`.
pub static HOST: SynthHostV1 = SynthHostV1 {
    abi: SLOOPER_SYNTH_ABI,
    load_asset: host_load_asset,
};

// ---------------------------------------------------------------------------
// Descriptors & instances
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SynthParamDesc {
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

#[derive(Clone, Debug)]
pub struct SynthDesc {
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub params: Vec<SynthParamDesc>,
}

#[derive(Clone, Copy)]
struct SynthFns {
    create: extern "C" fn(sample_rate: f64, host: *const SynthHostV1) -> *mut core::ffi::c_void,
    destroy: extern "C" fn(s: *mut core::ffi::c_void),
    note_on: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8, vel: u8),
    note_off: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8),
    all_off: extern "C" fn(s: *mut core::ffi::c_void),
    cc: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, num: u8, val: u8),
    press: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, val: u8),
    process: extern "C" fn(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32),
}

/// Library handle that is NEVER dlclosed — see the identical comment in
/// [`crate::fx`]: closing a Rust-runtime plugin while host teardown
/// destructors still run aborts the process.
struct LibHandle(std::mem::ManuallyDrop<libloading::Library>);
unsafe impl Send for LibHandle {}
unsafe impl Sync for LibHandle {}
impl std::ops::Deref for LibHandle {
    type Target = libloading::Library;
    fn deref(&self) -> &libloading::Library {
        &self.0
    }
}

/// One live synth instance. Param sliders are atomics (the GUI rides them);
/// note traffic is only ever driven by the audio thread.
pub struct Instance {
    fns: SynthFns,
    state: *mut core::ffi::c_void,
    sample_rate: f64,
    n_params: usize,
    params: Box<[AtomicU32; MAX_PARAMS]>,
    param_gen: AtomicU64,
    _lib: Arc<LibHandle>,
}
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    fn new(
        fns: SynthFns,
        lib: Arc<LibHandle>,
        desc: &SynthDesc,
        sample_rate: f64,
        values: &[f32],
    ) -> Option<Arc<Self>> {
        let state = (fns.create)(sample_rate, &HOST);
        if state.is_null() {
            return None;
        }
        let inst = Arc::new(Instance {
            fns,
            state,
            sample_rate,
            n_params: desc.params.len().min(MAX_PARAMS),
            params: Box::new(core::array::from_fn(|k| {
                let d = desc.params.get(k).map(|p| p.default).unwrap_or(0.0);
                AtomicU32::new(values.get(k).copied().unwrap_or(d).to_bits())
            })),
            param_gen: AtomicU64::new(1),
            _lib: lib,
        });
        Some(inst)
    }

    /// A brand-new instance of the SAME script with the SAME slider values
    /// — offline bounces use one per clip so live state never leaks into a
    /// render (mirrors `fx::Instance::spawn_fresh`).
    pub fn spawn_fresh(&self) -> Option<Arc<Self>> {
        let vals = self.values();
        let desc = SynthDesc {
            name: String::new(),
            author: None,
            description: None,
            params: vals
                .iter()
                .map(|v| SynthParamDesc { label: String::new(), min: *v, max: *v, default: *v })
                .collect(),
        };
        Self::new(self.fns, self._lib.clone(), &desc, self.sample_rate, &vals)
    }

    pub fn set_param(&self, k: usize, v: f32) {
        if k < self.n_params {
            self.params[k].store(v.to_bits(), Ordering::Relaxed);
            self.param_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn param_gen(&self) -> u64 {
        self.param_gen.load(Ordering::Relaxed)
    }

    pub fn read_params(&self, out: &mut [f32; MAX_PARAMS]) {
        for (o, p) in out.iter_mut().zip(&self.params[..self.n_params]) {
            *o = f32::from_bits(p.load(Ordering::Relaxed));
        }
    }

    pub fn values(&self) -> Vec<f32> {
        let mut p = [0.0f32; MAX_PARAMS];
        self.read_params(&mut p);
        p[..self.n_params].to_vec()
    }

    pub fn note_on(&self, ch: u8, note: u8, vel: u8) {
        (self.fns.note_on)(self.state, ch, note, vel)
    }
    pub fn note_off(&self, ch: u8, note: u8) {
        (self.fns.note_off)(self.state, ch, note)
    }
    pub fn all_off(&self) {
        (self.fns.all_off)(self.state)
    }
    pub fn cc(&self, ch: u8, num: u8, val: u8) {
        (self.fns.cc)(self.state, ch, num, val)
    }
    pub fn press(&self, ch: u8, val: u8) {
        (self.fns.press)(self.state, ch, val)
    }

    /// Render `out.len()` samples (OUT IS ACCUMULATED INTO — the plugin
    /// adds to whatever is there, matching the ABI). `params` is a snapshot.
    ///
    /// # Safety
    /// Only ever called from one thread at a time for this instance.
    pub unsafe fn process_into(&self, out: &mut [f32], params: &[f32; MAX_PARAMS]) {
        (self.fns.process)(self.state, out.as_mut_ptr(), out.len(), params.as_ptr())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        (self.fns.destroy)(self.state);
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct SynthEntry {
    pub stem: String,
    pub desc: Option<SynthDesc>,
    fns: Option<SynthFns>,
    pub error: Option<String>,
    _lib: Option<Arc<LibHandle>>,
}

/// The synths folder plus everything loaded from it.
pub struct Registry {
    dir: PathBuf,
    items: Vec<SynthEntry>,
}
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

impl Registry {
    pub fn open(dir: PathBuf) -> Self {
        seed_folder(&dir);
        let mut r = Registry { dir, items: Vec::new() };
        r.rescan();
        r
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn stems(&self) -> Vec<String> {
        self.items.iter().filter(|e| e.fns.is_some()).map(|e| e.stem.clone()).collect()
    }

    pub fn desc(&self, stem: &str) -> Option<&SynthDesc> {
        self.items
            .iter()
            .find(|e| e.stem == stem && e.fns.is_some())
            .and_then(|e| e.desc.as_ref())
    }

    pub fn errors(&self) -> Vec<(String, String)> {
        self.items
            .iter()
            .filter_map(|e| e.error.as_ref().map(|r| (e.stem.clone(), r.clone())))
            .collect()
    }

    fn find(&self, stem: &str) -> Option<&SynthEntry> {
        self.items.iter().find(|e| e.stem == stem && e.fns.is_some())
    }

    /// Create a live instance of `stem` (returns None on unknown/stem/reject).
    pub fn create_instance(
        &self,
        stem: &str,
        sample_rate: f64,
        values: &[f32],
    ) -> Option<Arc<Instance>> {
        let e = self.find(stem)?;
        let fns = e.fns?;
        Instance::new(fns, e._lib.clone()?, e.desc.as_ref()?, sample_rate, values)
    }

    /// Recompile what changed and reload. Never panics on bad scripts —
    /// rejections land in `error` for the GUI.
    pub fn rescan(&mut self) {
        seed_folder(&self.dir);
        let build = cache_dir();
        let mut out = Vec::new();
        let mut ok: HashMap<String, SynthEntry> = HashMap::new();
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for f in rd.flatten().map(|e| e.path()) {
                let is_script = f.extension().and_then(|e| e.to_str()) == Some("rs")
                    && f.file_name().map(|n| n != "synth_api.rs").unwrap_or(false);
                if is_script {
                    files.push(f);
                }
            }
        }
        files.sort();
        for src in files {
            let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let lib = build.join(format!("{stem}.so"));
            let entry = if needs_build(&src, &lib) {
                match compile_synth(&src, &lib) {
                    Ok(()) => unsafe { load_one(&stem, &lib) },
                    Err(e) => SynthEntry {
                        stem: stem.clone(),
                        desc: None,
                        fns: None,
                        error: Some(e),
                        _lib: None,
                    },
                }
            } else {
                unsafe { load_one(&stem, &lib) }
            };
            if entry.fns.is_some() {
                ok.insert(stem, entry);
            } else {
                out.push(entry);
            }
        }
        // Valid scripts first, alphabetical, then rejections.
        let mut valid: Vec<SynthEntry> = ok.into_values().collect();
        valid.sort_by_key(|e| e.stem.clone());
        out.sort_by_key(|e| e.stem.clone());
        out.splice(0..0, valid);
        self.items = out;
    }
}

fn needs_build(src: &Path, lib: &Path) -> bool {
    match (std::fs::metadata(src), std::fs::metadata(lib)) {
        (Ok(s), Ok(l)) => {
            let (Ok(sm), Ok(lm)) = (s.modified(), l.modified()) else {
                return true;
            };
            sm > lm
        }
        _ => true,
    }
}

/// Serializes rustc invocations: test threads (and a future rescan during
/// playback) must not write the same `.so.tmp` concurrently.
fn compile_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn compile_synth(src: &Path, out: &Path) -> Result<(), String> {
    let _serial = compile_lock();
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("could not create build dir: {e}"))?;
    }
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("slooper_synth");
    let crate_name = {
        let mut s = String::from("syn_");
        for c in stem.chars() {
            s.push(if c.is_ascii_alphanumeric() || c == '_' { c.to_ascii_lowercase() } else { '_' });
        }
        s
    };
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
        std::fs::write(out.with_extension("so.failed"), err.as_bytes()).ok();
        let mut err: String = err.trim().chars().take(1800).collect();
        err.push('…');
        return Err(format!("compile failed:\n{err}"));
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("could not store {}: {e}", out.display()))
}

unsafe fn load_one(stem: &str, lib_path: &Path) -> SynthEntry {
    let reject = |e: String| SynthEntry {
        stem: stem.to_string(),
        desc: None,
        fns: None,
        error: Some(e),
        _lib: None,
    };
    let lib = match libloading::Library::new(lib_path) {
        Ok(l) => Arc::new(LibHandle(std::mem::ManuallyDrop::new(l))),
        Err(e) => return reject(format!("load failed: {e}")),
    };
    let sym: libloading::Symbol<extern "C" fn() -> *const SynthInfoV1> =
        match lib.get(b"slooper_synth_info\0") {
            Ok(s) => s,
            Err(_) => {
                return reject(
                    "missing entry point slooper_synth_info — see the bundled synths".into(),
                )
            }
        };
    let info = sym();
    if info.is_null() {
        return reject("slooper_synth_info returned null".into());
    }
    let d = &*info;
    if d.abi != SLOOPER_SYNTH_ABI {
        return reject(format!(
            "synth ABI {} is not supported (app speaks ABI {SLOOPER_SYNTH_ABI})",
            d.abi
        ));
    }
    if d.n_params as usize > MAX_PARAMS {
        return reject(format!("declares {} params (max {MAX_PARAMS})", d.n_params));
    }
    let cstr = |p: *const core::ffi::c_char| -> Option<String> {
        if p.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(p).to_str().ok().map(|s| s.to_string())
    };
    let name = match cstr(d.name) {
        Some(n) if !n.is_empty() => n,
        _ => return reject("name missing or not UTF-8".into()),
    };
    let nulls = [
        d.create as usize,
        d.destroy as usize,
        d.note_on as usize,
        d.note_off as usize,
        d.all_off as usize,
        d.cc as usize,
        d.press as usize,
        d.process as usize,
    ]
    .iter()
    .filter(|p| **p == 0)
    .count();
    if nulls > 0 {
        return reject(format!("{nulls} callback pointer(s) are null"));
    }
    let params = (0..d.n_params as usize)
        .filter(|_| !d.params.is_null())
        .map(|k| unsafe { &*d.params.add(k) })
        .map(|p| {
            SynthParamDesc {
                label: cstr(p.label).unwrap_or_else(|| "p?".to_string()),
                min: p.min,
                max: p.max,
                default: p.default,
            }
        })
        .collect();
    let desc = SynthDesc {
        name,
        author: cstr(d.author),
        description: cstr(d.description),
        params,
    };
    let fns = SynthFns {
        create: d.create,
        destroy: d.destroy,
        note_on: d.note_on,
        note_off: d.note_off,
        all_off: d.all_off,
        cc: d.cc,
        press: d.press,
        process: d.process,
    };
    SynthEntry {
        stem: stem.to_string(),
        desc: Some(desc),
        fns: Some(fns),
        error: None,
        _lib: Some(lib),
    }
}

/// Seed-value helper shared with the GUI: clamp saved slider values into
/// the freshly-declared ranges, padding with defaults.
pub fn seed_values(saved: &[f32], desc: &SynthDesc) -> Vec<f32> {
    desc.params
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let v = saved.get(k).copied().unwrap_or(p.default);
            if p.max > p.min {
                v.clamp(p.min, p.max)
            } else {
                p.default
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry(tag: &str) -> Registry {
        let dir = std::env::temp_dir().join(format!("sp-synth-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("SLOOPER_SYNTHS_DIR", &dir);
        Registry::open(dir.clone())
    }

    #[test]
    fn seed_folder_updates_stamped_stale_scripts() {
        // The v0.7 modal-piano swap is invisible to existing installs
        // unless an old STAMPED copy updates in place — and the version
        // stamp sits on line 2 of every bundled script (line 1 is the
        // `==== slooper ==` banner), so the header scan must see it.
        let dir = std::env::temp_dir().join("sp-synth-seed");
        let _ = std::fs::remove_dir_all(&dir);
        seed_folder(&dir);
        let piano = dir.join("piano.rs");
        let cur = std::fs::read_to_string(&piano).unwrap();
        assert!(cur.contains(&format!("slooper-synth-version: {BUNDLED_VERSION}")));
        // An install from a previous app version: old content, still stamped.
        let old = "// ============ slooper ==\n// slooper-synth-version: 0\n// old FM voice\n".to_string();
        std::fs::write(&piano, &old).unwrap();
        // A hand-edited script (no stamp in the header) is the user's file.
        let mine = dir.join("sampler.rs");
        std::fs::write(&mine, "// my custom synth\n").unwrap();
        seed_folder(&dir);
        let upd = std::fs::read_to_string(&piano).unwrap();
        assert!(
            upd.contains("fan455"),
            "stale stamped piano.rs was not updated in place"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("piano.rs.bak")).unwrap(),
            old,
            "old copy not backed up"
        );
        assert_eq!(
            std::fs::read_to_string(&mine).unwrap(),
            "// my custom synth\n"
        );
        assert!(!dir.join("sampler.rs.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_piano_and_sampler_load_and_render() {
        let reg = test_registry("bundled");
        let stems = reg.stems();
        assert!(stems.contains(&"piano".to_string()), "stems: {stems:?}");
        assert!(stems.contains(&"sampler".to_string()), "stems: {stems:?}");
        // Give the host a rack asset (asset id 0) so the sampler has something
        // to play; with an empty table silence is correct behavior.
        let sine: Arc<[f32]> = (0..4800)
            .map(|i| (i as f32 * 0.05).sin() * 0.5)
            .collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        for stem in ["piano", "sampler"] {
            let inst = reg
                .create_instance(stem, 48_000.0, &[])
                .unwrap_or_else(|| panic!("{stem} instance"));
            let mut p = [0.0f32; MAX_PARAMS];
            inst.read_params(&mut p);
            // defaults for unknown params are 0 — pass desc defaults instead
            let desc = reg.desc(stem).unwrap();
            for (k, pd) in desc.params.iter().enumerate() {
                p[k] = pd.default;
            }
            let mut out = vec![0.0f32; 2048];
            // One block first: a sampler syncs its asset buffer here, and
            // notes started before that legitimately do nothing.
            unsafe { inst.process_into(&mut out, &p) };
            inst.note_on(0, 60, 100);
            let mut out = vec![0.0f32; 2048];
            unsafe { inst.process_into(&mut out, &p) };
            let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak > 1e-4, "{stem} must make sound (peak {peak})");
            inst.all_off();
            let mut out2 = vec![0.0f32; 4096];
            unsafe { inst.process_into(&mut out2, &p) };
            let tail = out2.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(tail < peak.max(1e-9), "{stem} all_off must stop new sound");
        }
    }

    /// Goertzel-style squared magnitude of `x` at frequency `f` over
    /// `x[a..b]` — enough to tell a harmonic line from the noise floor.
    fn tone_power(x: &[f32], f: f64, sr: f64, a: usize, b: usize) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = std::f64::consts::TAU * f / sr;
        for (n, s) in x.iter().enumerate().take(b).skip(a) {
            let ph = w * n as f64;
            re += *s as f64 * ph.cos();
            im -= *s as f64 * ph.sin();
        }
        re * re + im * im
    }

    /// Peak power within ±0.6 % of `f` (covers the synth's stiff-string
    /// partial stretch) — so the test measures "a line lives near n·f0",
    /// not the exact coefficient choice.
    fn line_power(x: &[f32], f: f64, sr: f64, a: usize, b: usize) -> f64 {
        let mut m = 0.0f64;
        let mut k = -3;
        while k <= 3 {
            let ff = f * (1.0 + 0.002 * k as f64);
            m = m.max(tone_power(x, ff, sr, a, b));
            k += 1;
        }
        m
    }

    #[test]
    fn piano_is_modal_not_static() {
        // AC2.2: the replacement piano must sound like DECAYING PARTIALS —
        // strong lines at the harmonics, upper partials dying faster than
        // the fundamental, and definitely not a flat (white-noise-like)
        // spectrum nor the old fixed-ratio FM buzz.
        let reg = test_registry("modal");
        let inst = reg.create_instance("piano", 48_000.0, &[]).expect("piano");
        let desc = reg.desc("piano").unwrap().clone();
        let mut p = [0.0f32; MAX_PARAMS];
        for (k, pd) in desc.params.iter().enumerate() {
            p[k] = pd.default;
        }
        let sr = 48_000.0f64;
        let note = 57u8; // A3 = 220 Hz
        let f0 = 220.0f64;
        inst.note_on(0, note, 110);
        let secs = 4.0;
        let n = (secs * sr) as usize;
        let mut out = vec![0.0f32; n];
        let mut at = 0usize;
        while at < n {
            let end = (at + 1024).min(n);
            let mut blk = vec![0.0f32; end - at];
            unsafe { inst.process_into(&mut blk, &p) };
            out[at..end].copy_from_slice(&blk);
            at = end;
        }
        let rms = |a: usize, b: usize| -> f64 {
            out[a..b].iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>()
                / (b - a) as f64
        };
        let head = (0.10 * sr) as usize..(0.60 * sr) as usize;
        let tail = (3.40 * sr) as usize..(3.90 * sr) as usize;
        assert!(rms(head.start, head.end) > 1e-4, "piano must make sound");
        assert!(rms(tail.start, tail.end) > 1e-8, "must still ring at 3.4 s");
        assert!(
            rms(head.start, head.end) > 2.0 * rms(tail.start, tail.end),
            "must DECAY: head rms {} vs tail rms {}",
            rms(head.start, head.end),
            rms(tail.start, tail.end)
        );
        // Harmonic lines dominate the inter-harmonic floor at the head.
        for h in [2.0, 3.0, 5.0] {
            let on = line_power(&out, h * f0, sr, head.start, head.end);
            let off = tone_power(&out, (h + 0.5) * f0, sr, head.start, head.end);
            assert!(
                on > 25.0 * off.max(1e-12),
                "harmonic {h} must stand out: on {on} off {off}"
            );
        }
        // Not white noise: fundamentals-side energy dwarfs the high band.
        let low = line_power(&out, f0, sr, head.start, head.end);
        let high = line_power(&out, 7000.0, sr, head.start, head.end);
        assert!(low > 10.0 * high, "spectrum must fall with frequency: {low} vs {high}");
        // PARTIAL-dependent decay (the modal signature): the 6th partial's
        // level RELATIVE to the fundamental drops strongly by the tail.
        let rel = |a: usize, b: usize| -> f64 {
            line_power(&out, 6.0 * f0, sr, a, b) / line_power(&out, f0, sr, a, b).max(1e-12)
        };
        let (rh, rt) = (rel(head.start, head.end), rel(tail.start, tail.end));
        assert!(rt < 0.7 * rh, "upper partials must decay faster: head {rh} tail {rt}");
        inst.all_off();
    }

    #[test]
    fn invalid_scripts_are_rejected_with_reasons() {
        let reg = test_registry("reject");
        std::fs::write(
            reg.dir().join("broken.rs"),
            "pub static DESC: u8 = 1; // no ABI at all",
        )
        .unwrap();
        let reg = Registry::open(reg.dir().to_path_buf());
        let errs = reg.errors();
        assert!(errs.iter().any(|(s, _)| s == "broken"), "errors: {errs:?}");
        assert!(!reg.stems().contains(&"broken".to_string()));
    }
}
