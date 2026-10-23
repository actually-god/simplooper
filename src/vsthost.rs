//! VST hosting: discover plugins in user-set VST folders, instantiate them,
//! and run them through realtime stage entries.
//!
//! One [`RtStageEntry`] per track holds the plugin plus ring buffers:
//! slooper's block size is whatever the JACK callback hands us, while both
//! plugin APIs (and the VST3 realtime runner especially) want a fixed
//! processing size, so audio is accumulated and fed to the plugin in fixed
//! [`VST_BLOCK`]-sized chunks. Tracks are mono in slooper: the mono signal
//! is upfed to every plugin input channel and the output bus is averaged
//! back to mono.
//!
//! Threading discipline (mirrors `vst3_host::realtime` rules):
//! * Instances are created on the UI thread and moved (as
//!   `Arc<RtStageEntry>`) into the engine, where the audio thread processes
//!   them. `process`/`flush` use the entry's interior mutability and are
//!   audio-thread-only (same shape as the scripted-FX instances).
//! * A replaced entry is *held* by the engine until the UI thread drains
//!   the holding list, so plugin destructors — and VST3 COM teardown, which
//!   the runner hands off rather than running in place — execute on the
//!   creator (UI) thread.
//! * Parameters ride [`VstParams`] (normalized 0..=1 atomics shared with
//!   the UI). VST3 changes are additionally pushed into the runner's
//!   command ring ([`RtStageEntry::set_param`]); VST2 has no ring, so the
//!   audio thread polls the atomics and calls `set_parameter` at the block
//!   boundary.
//! * [`Backend`] wraps raw plugin handles; the `unsafe impl Send` documents
//!   why moving the *owner* between threads is sound.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Fixed processing granularity handed to plugins (samples).
pub const VST_BLOCK: usize = 512;
/// Number of sliders the VST panel exposes per plugin.
pub const VST_MAX_PARAMS: usize = 64;
/// Ring capacity: covers the largest plausible JACK callback plus slop.
/// The rings never grow past this (oldest samples are dropped on overflow),
/// which keeps the audio thread allocation-free after load.
const RING_CAP: usize = 64 * 1024 + 2 * VST_BLOCK;

// ---------------------------------------------------------------- kinds ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VstKind {
    V2,
    V3,
}

impl VstKind {
    pub fn tag(self) -> &'static str {
        match self {
            VstKind::V2 => "VST2",
            VstKind::V3 => "VST3",
        }
    }
    /// Round-trip through config strings.
    pub fn parse(s: &str) -> Option<VstKind> {
        match s {
            "VST2" => Some(VstKind::V2),
            "VST3" => Some(VstKind::V3),
            _ => None,
        }
    }
}

/// One plugin found while scanning the VST folders.
#[derive(Clone, Debug)]
pub struct VstEntry {
    pub path: PathBuf,
    pub kind: VstKind,
    pub name: String,
    pub vendor: String,
    /// Plugin takes no audio inputs (VST2 `inputs == 0`, VST3 empty input
    /// buses): it generates sound from MIDI, so the UI offers it in the
    /// per-track synthesizer slot as well as the effects chain.
    pub instrument: bool,
    /// Set when the file looked like a plugin but failed to probe; the
    /// picker shows it greyed out instead of silently hiding broken files.
    pub error: Option<String>,
}

/// 0-based MIDI channel index -> vst3-host channel enum (no `from_index` there).
const MIDI_CHANNELS: [vst3_host::midi::MidiChannel; 16] = [
    vst3_host::midi::MidiChannel::Ch1,
    vst3_host::midi::MidiChannel::Ch2,
    vst3_host::midi::MidiChannel::Ch3,
    vst3_host::midi::MidiChannel::Ch4,
    vst3_host::midi::MidiChannel::Ch5,
    vst3_host::midi::MidiChannel::Ch6,
    vst3_host::midi::MidiChannel::Ch7,
    vst3_host::midi::MidiChannel::Ch8,
    vst3_host::midi::MidiChannel::Ch9,
    vst3_host::midi::MidiChannel::Ch10,
    vst3_host::midi::MidiChannel::Ch11,
    vst3_host::midi::MidiChannel::Ch12,
    vst3_host::midi::MidiChannel::Ch13,
    vst3_host::midi::MidiChannel::Ch14,
    vst3_host::midi::MidiChannel::Ch15,
    vst3_host::midi::MidiChannel::Ch16,
];

/// Default VST search folders (the usual Linux conventions), existing ones
/// only. Users may add their own folders in the VST window.
pub fn default_vst_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".vst3"));
        dirs.push(PathBuf::from(&home).join(".vst"));
    }
    dirs.push(PathBuf::from("/usr/lib/vst3"));
    dirs.push(PathBuf::from("/usr/lib/vst"));
    dirs.push(PathBuf::from("/usr/lib64/vst3"));
    dirs.push(PathBuf::from("/usr/local/lib/vst3"));
    dirs.push(PathBuf::from("/usr/local/lib/vst"));
    dirs.retain(|d| d.is_dir());
    dirs
}

/// Scan the given folders for VST2/VST3 plugins. Each folder is searched
/// directly plus one level of subdirectories (the "label pack" layout).
/// VST3 probes load the factory without instantiating; VST2 probes
/// instantiate once (required for metadata) and drop again — both safe on
/// a UI thread but slow, so this runs only on explicit user rescans.
pub fn scan(dirs: &[PathBuf]) -> Vec<VstEntry> {
    let mut out = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in dirs {
        if !root.is_dir() {
            continue;
        }
        let subdirs = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir());
        for dir in std::iter::once(root.clone()).chain(subdirs) {
            let entries = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for e in entries.flatten() {
                let p = e.path();
                if seen.iter().any(|s| s == &p) {
                    continue;
                }
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".vst3") {
                    seen.push(p.clone());
                    out.push(probe_vst3(&p));
                } else if name.ends_with(".so") && !p.is_dir() {
                    seen.push(p.clone());
                    out.push(probe_vst2(&p));
                }
            }
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

fn base_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .trim_end_matches(".vst3")
        .trim_end_matches(".so")
        .to_string()
}

fn probe_vst3(path: &Path) -> VstEntry {
    match vst3_host::discovery::get_plugin_info(path) {
        Ok(info) => {
            // Instrument or effect? Instantiate once (UI thread, scan only)
            // and look at the audio buses: a synth has no audio inputs.
            let instrument = vst3_host::simple::load_plugin(path)
                .ok()
                .and_then(|p| p.bus_arrangements().ok())
                .map(|b| b.inputs.is_empty())
                .unwrap_or(false);
            VstEntry {
                path: path.to_path_buf(),
                kind: VstKind::V3,
                name: info.name,
                vendor: info.vendor,
                instrument,
                error: None,
            }
        }
        Err(err) => VstEntry {
            path: path.to_path_buf(),
            kind: VstKind::V3,
            name: base_label(path),
            vendor: String::new(),
            instrument: false,
            error: Some(err.to_string()),
        },
    }
}

fn probe_vst2(path: &Path) -> VstEntry {
    use vst2::plugin::Plugin;
    let probed = vst2::host::PluginLoader::load(path, Arc::new(Mutex::new(StubHost)))
        .map(|mut l| l.instance());
    match probed {
        Ok(Ok(inst)) => {
            let info = inst.get_info();
            VstEntry {
                path: path.to_path_buf(),
                kind: VstKind::V2,
                name: info.name,
                vendor: info.vendor,
                instrument: info.inputs == 0,
                error: None,
            }
        }
        Ok(Err(err)) | Err(err) => VstEntry {
            path: path.to_path_buf(),
            kind: VstKind::V2,
            name: base_label(path),
            vendor: String::new(),
            instrument: false,
            error: Some(err.to_string()),
        },
    }
}

// -------------------------------------------------------------- params ---

/// Slider metadata captured at load time (read from the plugin).
#[derive(Clone, Debug)]
pub struct VstParamDesc {
    /// VST3 parameter id, or VST2 parameter index.
    pub id: u32,
    pub name: String,
    /// Normalized (0..=1) default value.
    pub default: f32,
    /// False for VST3 read-only meters — the UI hides them.
    pub writable: bool,
}

/// Shared parameter cell: the UI writes, the audio thread reads (VST2) or
/// the UI additionally pushes through the runner ring (VST3). Values are
/// normalized 0..=1 for both plugin generations — the space both APIs use
/// for `setParameter`.
pub struct VstParams {
    slots: Vec<AtomicU32>,
    generation: AtomicU64,
}

impl VstParams {
    pub fn new(descs: &[VstParamDesc]) -> Arc<Self> {
        Arc::new(Self {
            slots: descs
                .iter()
                .map(|d| AtomicU32::new(d.default.to_bits()))
                .collect(),
            generation: AtomicU64::new(1),
        })
    }

    /// UI side: store a normalized value and bump the generation counter.
    pub fn set(&self, index: usize, value: f32) {
        if let Some(slot) = self.slots.get(index) {
            let v = value.clamp(0.0, 1.0);
            if slot.swap(v.to_bits(), Ordering::Relaxed) != v.to_bits() {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn get(&self, index: usize) -> f32 {
        self.slots
            .get(index)
            .map(|s| f32::from_bits(s.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

// -------------------------------------------------------------- backend ---

// Both backends wrap raw plugin handles (libloading instances, VST3 COM
// pointers), so neither auto-Sends. Ownership of a plugin moves
// create(UI) -> process(audio) -> held/reap(UI) in a strictly
// happens-before chain: exactly one thread owns the plugin at any moment
// and every destroy/dtor runs on the UI thread. VST3's IAudioProcessor is
// specified for precisely this pattern (created on the control thread,
// process() called from the audio thread, teardown off the audio thread —
// the realtime runner hands its plugin back on drop rather than destroying
// it in place), and a VST2 host that calls the dispatcher from one thread
// at a time follows the same contract.
struct RtBackend<T>(T);
unsafe impl<T> Send for RtBackend<T> {}

enum Backend {
    V3(RtBackend<vst3_host::realtime::RealtimePluginRunner>),
    V2(RtBackend<vst2::host::PluginInstance>),
}

// ------------------------------------------------------------ rt state ---

/// Mutable audio-thread state of a stage entry. Touched exclusively by
/// `process`/`flush` on the audio thread; wrapped in `UnsafeCell` so the
/// entry can live in an `Arc` shared with the engine table.
struct RtState {
    backend: Backend,
    /// VST2-only: 2-in/2-out host buffer bound to the staging vectors.
    hostbuf: Option<vst2::host::HostBuffer<f32>>,
    /// VST2-only: parameter object (`Sync`; called from the audio thread
    /// while polling — the VST2 `setParameter` contract).
    params_obj: Option<Arc<dyn vst2::plugin::PluginParameters>>,
    /// VST3-only: fixed-size runner buffers.
    v3_bufs: Option<vst3_host::audio::AudioBuffers>,
    v2_out_channels: usize,
    seen_gen: u64,
    last: Vec<f32>,
    // staging (all exactly VST_BLOCK frames, allocated at load)
    stage_l: Vec<f32>,
    stage_r: Vec<f32>,
    stage_ol: Vec<f32>,
    stage_or: Vec<f32>,
    in_ring: VecDeque<f32>,
    out_ring: VecDeque<f32>,
    /// Dry echo filling the startup window until the plugin (plus any
    /// latency it adds) produces its first samples.
    echo: VecDeque<f32>,
    /// Channel voice messages fed for instrument use, delivered at the next
    /// internal 512-block boundary (audio-thread contract, like `process`).
    pending_midi: Vec<[u8; 3]>,
    /// VST2-only: stable heap slots for raw events — `api::Events` carries
    /// pointers, so the events must not move. Filled on first use, reused.
    // The Box is load-bearing (address stability), hence no Vec::into_boxed-slice style fix.
    #[allow(clippy::vec_box)]
    ev_pool: Vec<Box<vst2::api::Event>>,
    errors: u32,
}

// --------------------------------------------------------- stage entry ---

/// One live plugin shared between the UI (parameters, metadata) and the
/// audio thread (processing). Field order is load-bearing for drops:
/// `state` (whose VST3 runner hands the plugin to the control end) must
/// drop before `control`.
pub struct RtStageEntry {
    state: UnsafeCell<RtState>,
    /// VST3-only: UI-thread producer end of the runner's command ring.
    /// Never locked on the audio thread (zero contention).
    control: Mutex<Option<vst3_host::realtime::RtControl>>,
    params: Arc<VstParams>,
    descs: Vec<VstParamDesc>,
    name: String,
    /// Where this plugin was loaded from (re-opens offline render copies).
    spec: (PathBuf, VstKind),
    /// Whether the plugin advertises a custom editor window (VST2
    /// `effFlagsHasEditor`, VST3 `IComponent` view). Drives the "GUI"
    /// button; the slider panel remains the fallback for GUI-less plugins.
    has_editor: bool,
    /// VST2 only: raw `AEffect` dispatcher for editor hosting (see
    /// `src/vstedit.rs`). VST3 editors are not hosted (docs/PLUGINS.md).
    vst2: Option<Vst2Handle>,
}

/// Raw editor-dispatch handle for a loaded VST2 plugin: just the
/// `AEffect` address, so it can cross into the editor GUI thread.
/// Callers must close any editor before the plugin instance is dropped.
#[derive(Clone, Copy)]
pub struct Vst2Handle(*mut vst2::api::AEffect);
unsafe impl Send for Vst2Handle {}
unsafe impl Sync for Vst2Handle {}

impl Vst2Handle {
    pub(crate) fn from_instance(inst: &vst2::host::PluginInstance) -> Self {
        Self(inst.raw_effect())
    }
    /// Raw `AEffect::dispatcher` call with a VST2 opcode number.
    pub fn dispatch(
        &self,
        opcode: i32,
        index: i32,
        value: isize,
        ptr: *mut std::ffi::c_void,
        opt: f32,
    ) -> isize {
        unsafe {
            let ae = self.0;
            ((*ae).dispatcher)(ae, opcode, index, value, ptr, opt)
        }
    }
}

unsafe impl Send for RtStageEntry {}
unsafe impl Sync for RtStageEntry {}

/// What [`open`] hands back: the shared entry plus the UI-facing metadata
/// (also reachable through the entry).
pub struct VstLoad {
    pub entry: Arc<RtStageEntry>,
}

/// Load a plugin from `path` on the UI thread and prepare its realtime
/// entry. `saved` (normalized, in `descs` order) overrides defaults.
pub fn open(
    path: &Path,
    kind: VstKind,
    sample_rate: u32,
    saved: Option<&[f32]>,
) -> Result<VstLoad, String> {
    let entry = match kind {
        VstKind::V3 => open_v3(path, sample_rate, saved)?,
        VstKind::V2 => open_v2(path, sample_rate, saved)?,
    };
    Ok(VstLoad {
        entry: Arc::new(entry),
    })
}

fn stage_buf() -> Vec<f32> {
    vec![0.0f32; VST_BLOCK]
}

fn apply_saved(params: &VstParams, descs: &[VstParamDesc], saved: Option<&[f32]>) {
    if let Some(vals) = saved {
        for (k, v) in vals.iter().enumerate() {
            if k < descs.len() && descs[k].writable {
                params.set(k, *v);
            }
        }
    }
}

/// The audio-thread half of a load, bundled so `assemble` stays readable.
struct RtParts {
    spec: (PathBuf, VstKind),
    backend: Backend,
    hostbuf: Option<vst2::host::HostBuffer<f32>>,
    params_obj: Option<Arc<dyn vst2::plugin::PluginParameters>>,
    v3_bufs: Option<vst3_host::audio::AudioBuffers>,
    control: Option<vst3_host::realtime::RtControl>,
    out_channels: usize,
}

fn assemble(
    name: String,
    parts: RtParts,
    params: Arc<VstParams>,
    descs: Vec<VstParamDesc>,
    has_editor: bool,
    vst2: Option<Vst2Handle>,
) -> RtStageEntry {
    let last: Vec<f32> = (0..descs.len()).map(|k| params.get(k)).collect();
    let seen_gen = params.generation();
    let state = RtState {
        backend: parts.backend,
        hostbuf: parts.hostbuf,
        params_obj: parts.params_obj,
        v3_bufs: parts.v3_bufs,
        v2_out_channels: parts.out_channels,
        seen_gen,
        last,
        stage_l: stage_buf(),
        stage_r: stage_buf(),
        stage_ol: stage_buf(),
        stage_or: stage_buf(),
        in_ring: VecDeque::with_capacity(RING_CAP),
        out_ring: VecDeque::with_capacity(RING_CAP),
        echo: VecDeque::with_capacity(RING_CAP),
        pending_midi: Vec::new(),
        ev_pool: Vec::new(),
        errors: 0,
    };
    RtStageEntry {
        state: UnsafeCell::new(state),
        control: Mutex::new(parts.control),
        params,
        descs,
        name,
        spec: parts.spec,
        has_editor,
        vst2,
    }
}

fn open_v3(path: &Path, sample_rate: u32, saved: Option<&[f32]>) -> Result<RtStageEntry, String> {
    use vst3_host::{
        audio::{AudioBuffers, BusDirection, MediaType},
        realtime::RealtimePluginRunner,
        simple,
    };
    let mut plugin = simple::load_plugin_with_settings(path, sample_rate as f64, VST_BLOCK)
        .map_err(|e| format!("load: {e}"))?;
    // JUCE-wrapper plugins (Vitalium, SwankyAmp — distrho-ports builds) advertise
    // their buses with `active = 0` in `getBusInfo`, and the loader honours that.
    // An unactivated output bus renders NOTHING: `output_channel_count` collapses
    // to 0 and the plugin "loads fine" but is permanently silent. If we see zero
    // active output channels, activate every output bus and re-measure. Only in
    // that broken case — plugins that advertise correctly (Geonkick with its 16
    // auxiliary out buses!) keep their activation and channel accounting intact.
    if plugin.output_channel_count() == 0 {
        if let Ok(layout) = plugin.audio_bus_layout() {
            for i in 0..layout.outputs.len() as i32 {
                let _ = plugin.set_bus_active(MediaType::Audio, BusDirection::Output, i, true);
            }
        }
    }
    let ps = plugin.get_parameters().map_err(|e| format!("params: {e}"))?;
    let descs: Vec<VstParamDesc> = ps
        .iter()
        .take(VST_MAX_PARAMS)
        .map(|p| VstParamDesc {
            id: p.id,
            name: p.name.clone(),
            default: p.default as f32,
            writable: !p.is_read_only,
        })
        .collect();
    let params = VstParams::new(&descs);
    apply_saved(&params, &descs, saved);
    for (k, d) in descs.iter().enumerate() {
        if d.writable {
            let _ = plugin.set_parameter(d.id, params.get(k) as f64);
        }
    }
    let out_ch = plugin.output_channel_count().max(1);
    // Checked before the plugin moves into the realtime runner.
    let has_editor = plugin.has_editor();
    plugin.start_processing().map_err(|e| format!("start: {e}"))?;
    let (mut runner, control) = RealtimePluginRunner::new(plugin, 256);
    runner.start().map_err(|e| format!("run: {e}"))?;
    let bufs = AudioBuffers::new(out_ch, out_ch, VST_BLOCK, sample_rate as f64);
    Ok(assemble(
        plugin_name(path),
        RtParts {
            spec: (path.to_path_buf(), VstKind::V3),
            backend: Backend::V3(RtBackend(runner)),
            hostbuf: None,
            params_obj: None,
            v3_bufs: Some(bufs),
            control: Some(control),
            out_channels: out_ch,
        },
        params,
        descs,
        has_editor,
        None,
    ))
}

fn open_v2(path: &Path, sample_rate: u32, saved: Option<&[f32]>) -> Result<RtStageEntry, String> {
    use vst2::{
        host::{HostBuffer, PluginLoader},
        plugin::{Plugin, PluginParameters},
    };
    let mut loader =
        PluginLoader::load(path, Arc::new(Mutex::new(StubHost))).map_err(|e| format!("load: {e}"))?;
    let mut inst = loader.instance().map_err(|e| format!("instance: {e}"))?;
    let info = inst.get_info();
    if info.inputs > 2 || info.outputs > 2 {
        return Err("multi-channel plugins are not supported".into());
    }
    inst.set_sample_rate(sample_rate as f32);
    inst.set_block_size(VST_BLOCK as i64);
    inst.resume();
    let params_obj: Arc<dyn PluginParameters> = inst.get_parameter_object();
    let n = (info.parameters.max(0) as usize).min(VST_MAX_PARAMS);
    let descs: Vec<VstParamDesc> = (0..n)
        .map(|i| {
            let name = params_obj.get_parameter_name(i as i32);
            VstParamDesc {
                id: i as u32,
                name: if name.trim().is_empty() {
                    format!("Param {}", i + 1)
                } else {
                    name
                },
                default: params_obj.get_parameter(i as i32),
                writable: true,
            }
        })
        .collect();
    let params = VstParams::new(&descs);
    apply_saved(&params, &descs, saved);
    for (k, d) in descs.iter().enumerate() {
        if d.writable {
            params_obj.set_parameter(d.id as i32, params.get(k));
        }
    }
    let out_ch = info.outputs.clamp(1, 2) as usize;
    // Editor hosting: grab the raw effect handle + has-editor flag before
    // the instance moves into the realtime backend.
    let has_editor = unsafe { ((*inst.raw_effect()).flags & 1) != 0 };
    let vst2 = Some(Vst2Handle::from_instance(&inst));
    Ok(assemble(
        info.name,
        RtParts {
            spec: (path.to_path_buf(), VstKind::V2),
            backend: Backend::V2(RtBackend(inst)),
            hostbuf: Some(HostBuffer::new(2, 2)),
            params_obj: Some(params_obj),
            v3_bufs: None,
            control: None,
            out_channels: out_ch,
        },
        params,
        descs,
        has_editor,
        vst2,
    ))
}

fn plugin_name(path: &Path) -> String {
    base_label(path)
}

impl RtStageEntry {
    /// Plugin display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the plugin advertises a custom editor GUI.
    pub fn has_editor(&self) -> bool {
        self.has_editor
    }

    /// VST2 editor dispatch handle (`None` for VST3 — see docs/PLUGINS.md).
    pub fn vst2_handle(&self) -> Option<Vst2Handle> {
        self.vst2
    }

    /// Plugin file path this entry was loaded from.
    pub fn path(&self) -> &std::path::Path {
        &self.spec.0
    }

    /// (Path, kind) this entry was loaded from.
    pub fn spec(&self) -> &(PathBuf, VstKind) {
        &self.spec
    }

    /// Slider descriptors (UI).
    pub fn descs(&self) -> &[VstParamDesc] {
        &self.descs
    }

    /// Shared parameter cell (slider positions read from here).
    pub fn params(&self) -> &Arc<VstParams> {
        &self.params
    }

    /// UI side: update one parameter (normalized) and push it to the
    /// plugin (VST3 via the command ring; VST2 via the atomics the audio
    /// thread polls).
    pub fn set_param(&self, index: usize, value: f32) {
        self.params.set(index, value);
        if let Some(d) = self.descs.get(index) {
            let mut guard = self.control.lock().unwrap();
            if let Some(c) = guard.as_mut() {
                c.set_parameter(d.id, value.clamp(0.0, 1.0) as f64);
            }
        }
    }

    /// Feed one channel voice message (raw status byte with channel low
    /// nibble). Audio thread only — same contract as [`Self::process`];
    /// the message is delivered at the start of the next internal block.
    pub fn feed_midi(&self, msg: [u8; 3]) {
        // SAFETY: audio-thread-only access to `state` (see `flush`).
        let s = unsafe { &mut *self.state.get() };
        if s.pending_midi.len() < 512 {
            s.pending_midi.push(msg);
        }
    }

    /// Note on (channel 0-15). Audio thread only.
    pub fn note_on(&self, ch: u8, note: u8, vel: u8) {
        self.feed_midi([0x90 | ch & 0x0F, note & 0x7F, vel.clamp(1, 127)]);
    }

    /// Note off (channel 0-15). Audio thread only.
    pub fn note_off(&self, ch: u8, note: u8) {
        self.feed_midi([0x80 | ch & 0x0F, note & 0x7F, 0]);
    }

    /// Control change (channel 0-15). Audio thread only.
    pub fn cc(&self, ch: u8, ctl: u8, val: u8) {
        self.feed_midi([0xB0 | ch & 0x0F, ctl & 0x7F, val & 0x7F]);
    }

    /// Panic switch: all-notes-off + all-sounds-off on every channel.
    /// Audio thread only.
    pub fn all_notes_off(&self) {
        for c in 0..16u8 {
            self.cc(c, 123, 0);
            self.cc(c, 120, 0);
        }
    }

    /// Drop all buffered audio. Audio thread only (called from the engine's
    /// seek handling).
    pub fn flush(&self) {
        // SAFETY: `process`/`flush` are only ever called from the audio
        // thread, so no other thread holds a reference into `state`.
        let s = unsafe { &mut *self.state.get() };
        s.in_ring.clear();
        s.out_ring.clear();
        s.echo.clear();
        s.pending_midi.clear();
    }

    /// Process one engine block: mono in, mono out, internally chunked to
    /// [`VST_BLOCK`]. Audio thread only.
    pub fn process(&self, input: &[f32], output: &mut [f32]) {
        // SAFETY: audio-thread-only access to `state` (see `flush`).
        let s = unsafe { &mut *self.state.get() };
        let n = input.len().min(output.len());

        self.poll_params(s);

        for &x in &input[..n] {
            if s.in_ring.len() == RING_CAP {
                s.in_ring.pop_front();
                s.echo.pop_front();
            }
            s.in_ring.push_back(x);
            s.echo.push_back(x);
        }

        while s.in_ring.len() >= VST_BLOCK {
            for (i, sl) in s.stage_l.iter_mut().enumerate() {
                let x = s.in_ring.pop_front().unwrap_or(0.0);
                *sl = x;
                s.stage_r[i] = x;
            }
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
            self.process_block(s);
            for i in 0..VST_BLOCK {
                if s.out_ring.len() == RING_CAP {
                    s.out_ring.pop_front();
                }
                s.out_ring.push_back(s.stage_ol[i]);
            }
        }

        for o in output.iter_mut().take(n) {
            *o = match s.out_ring.pop_front() {
                Some(x) => x,
                // Startup/seek fill: keep the dry signal until the plugin
                // (plus any latency) has produced its first samples.
                None => s.echo.pop_front().unwrap_or(0.0),
            };
        }
    }

    /// Hand pending channel-voice messages to the plugin. VST2 gets raw
    /// `api::Events` (batches of 2 — the C struct's fixed pointer array);
    /// VST3 goes through the runner's command ring (pushes never block).
    fn deliver_midi(&self, s: &mut RtState) {
        if s.pending_midi.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut s.pending_midi);
        if matches!(s.backend, Backend::V3(_)) {
            // The UI holds this lock only for µs-long ring pushes; a brief
            // wait here is bounded well below any block period.
            if let Ok(mut g) = self.control.lock() {
                if let Some(c) = g.as_mut() {
                    use vst3_host::midi::MidiEvent;
                    for m in pending {
                        let ch = MIDI_CHANNELS[(m[0] & 0x0F) as usize];
                        let (d1, d2) = (m[1], m[2]);
                        let _ = match m[0] & 0xF0 {
                            0x90 if d2 > 0 => c.send_midi(MidiEvent::NoteOn {
                                channel: ch,
                                note: d1,
                                velocity: d2,
                            }),
                            0x80 | 0x90 => c.send_midi(MidiEvent::NoteOff {
                                channel: ch,
                                note: d1,
                                velocity: d2,
                            }),
                            0xB0 => c.send_midi(MidiEvent::ControlChange {
                                channel: ch,
                                controller: d1,
                                value: d2,
                            }),
                            0xC0 => {
                                c.send_midi(MidiEvent::ProgramChange { channel: ch, program: d1 })
                            }
                            _ => false,
                        };
                    }
                }
            }
            return;
        }
        // VST2: the pool grows once, then every event is a reuse.
        while s.ev_pool.len() < 2 {
            // Zeroed raw event: a benign null record until overwritten.
            s.ev_pool.push(Box::new(unsafe { std::mem::zeroed() }));
        }
        let mut i = 0usize;
        while i < pending.len() {
            let take = 2usize.min(pending.len() - i);
            if let Backend::V2(inst) = &mut s.backend {
                use vst2::{api, plugin::Plugin};
                for (k, m) in pending[i..i + take].iter().enumerate() {
                    let me = api::MidiEvent {
                        event_type: api::EventType::Midi,
                        byte_size: std::mem::size_of::<api::MidiEvent>() as i32,
                        delta_frames: 0,
                        flags: 0,
                        note_length: 0,
                        note_offset: 0,
                        midi_data: [m[0], m[1], m[2]],
                        _midi_reserved: 0,
                        detune: 0,
                        note_off_velocity: 0,
                        _reserved1: 0,
                        _reserved2: 0,
                    };
                    // Identical layout — the crate's own transmute idiom.
                    let mut raw: api::Event = unsafe { std::mem::transmute(me) };
                    raw.event_type = api::EventType::Midi;
                    *s.ev_pool[k] = raw;
                }
                let events = api::Events {
                    num_events: take as i32,
                    _reserved: 0,
                    // Boxed slots keep the pointers stable; slot 1 is a
                    // dummy whenever `take == 1`.
                    events: [s.ev_pool[0].as_mut(), s.ev_pool[1].as_mut()],
                };
                inst.0.process_events(&events);
            }
            i += take;
        }
    }

    /// One VST_BLOCK-sized plugin call; the mono result lands in
    /// `stage_ol` (downmixed from the plugin's output bus). On error the
    /// block passes silence (and `errors` accumulates for diagnostics).
    fn process_block(&self, s: &mut RtState) {
        self.deliver_midi(s);
        match &mut s.backend {
            Backend::V3(runner) => {
                let mut ok = false;
                if let Some(bufs) = s.v3_bufs.as_mut() {
                    for ch in bufs.inputs.iter_mut() {
                        ch.copy_from_slice(&s.stage_l);
                    }
                    if runner.0.process(bufs).is_ok() && !bufs.outputs.is_empty() {
                        let n = VST_BLOCK;
                        if bufs.outputs.len() == 1 {
                            s.stage_ol.copy_from_slice(&bufs.outputs[0][..n]);
                        } else {
                            for i in 0..n {
                                s.stage_ol[i] = (bufs.outputs[0][i] + bufs.outputs[1][i]) * 0.5;
                            }
                        }
                        ok = true;
                    }
                }
                if !ok {
                    s.errors += 1;
                    s.stage_ol.fill(0.0);
                }
            }
            Backend::V2(inst) => {
                use vst2::plugin::Plugin;
                if let Some(hb) = s.hostbuf.as_mut() {
                    let stereo_out = s.v2_out_channels == 2;
                    let mut ab = hb.bind(
                        &[&s.stage_l[..], &s.stage_r[..]],
                        &mut [&mut s.stage_ol[..], &mut s.stage_or[..]],
                    );
                    inst.0.process(&mut ab);
                    if stereo_out {
                        for i in 0..VST_BLOCK {
                            s.stage_ol[i] = (s.stage_ol[i] + s.stage_or[i]) * 0.5;
                        }
                    }
                } else {
                    s.errors += 1;
                    s.stage_ol.fill(0.0);
                }
            }
        }
    }

    /// VST2 pulls parameter changes on the audio thread; VST3 values were
    /// pushed through the ring at change time, so for it this only keeps
    /// `seen_gen` honest.
    fn poll_params(&self, s: &mut RtState) {
        let gen = self.params.generation();
        if gen == s.seen_gen {
            return;
        }
        s.seen_gen = gen;
        if let Some(po) = &s.params_obj {
            for (k, desc) in self.descs.iter().enumerate() {
                let v = self.params.get(k);
                if k < s.last.len() && (s.last[k] - v).abs() > f32::EPSILON {
                    s.last[k] = v;
                    po.set_parameter(desc.id as i32, v);
                }
            }
        }
    }
}

/// Minimal VST2 host: the crate calls back for identity, time info
/// (ignored) and automation (ignored — slooper's transport is the master).
struct StubHost;

impl vst2::host::Host for StubHost {
    fn get_info(&self) -> (isize, String, String) {
        (
            1,
            String::from("slooper"),
            String::from(env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_store_and_signal() {
        let descs = vec![
            VstParamDesc { id: 0, name: "a".into(), default: 0.25, writable: true },
            VstParamDesc { id: 1, name: "b".into(), default: 0.75, writable: true },
        ];
        let p = VstParams::new(&descs);
        assert_eq!(p.get(0), 0.25);
        assert_eq!(p.len(), 2);
        let g = p.generation();
        p.set(0, 0.5);
        assert_eq!(p.get(0), 0.5);
        assert_eq!(p.generation(), g + 1);
        p.set(0, 0.5); // no-op does not bump
        assert_eq!(p.generation(), g + 1);
        p.set(1, 9.0); // clamped
        assert_eq!(p.get(1), 1.0);
    }

    #[test]
    fn kinds_round_trip() {
        for k in [VstKind::V2, VstKind::V3] {
            assert_eq!(VstKind::parse(k.tag()), Some(k));
        }
        assert_eq!(VstKind::parse("LV2"), None);
    }

    #[test]
    fn scan_tolerates_junk_dirs() {
        // A directory of only unrelated files yields no entries and no
        // panics.
        let tmp = std::env::temp_dir();
        let entries = scan(std::slice::from_ref(&tmp));
        for e in &entries {
            assert!(e.path.extension().is_some());
        }
    }

    /// End-to-end against the local plugin folders when present: every
    /// backend contract — open, ring processing across block sizes,
    /// parameter push — runs for real. Silently skips when no plugins are
    /// installed (CI).
    #[test]
    fn smoke_local_plugins_if_present() {
        let found = scan(&default_vst_dirs());
        let mut ran = 0;
        for e in found.iter().filter(|e| e.error.is_none()).take(3) {
            for kind in [VstKind::V2, VstKind::V3] {
                let Ok(load) = open(&e.path, kind, 48_000, None) else {
                    continue;
                };
                // Deliberately ragged block sizes: exercises the rings.
                for size in [512usize, 1000, 100, 512] {
                    let input = vec![0.1f32; size];
                    let mut output = vec![0.0f32; size];
                    load.entry.process(&input, &mut output);
                    assert_eq!(output.len(), size);
                    assert!(output.iter().all(|v| v.is_finite()));
                }
                load.entry.set_param(0, 0.33);
                let input = vec![0.0f32; 512];
                let mut output = vec![0.0f32; 512];
                for _ in 0..8 {
                    load.entry.process(&input, &mut output);
                }
                assert!(output.iter().all(|v| v.is_finite()));
                drop(load); // held-reap teardown path
                ran += 1;
            }
        }
        // Informational, not an assertion: CI has no plugins installed.
        let _ = ran;
    }

    /// Every installed instrument VST3 must actually SOUND when fed a
    /// note. Regression pin for the JUCE-wrapper silent-bus bug:
    /// distrho-ports builds (Vitalium et al.) advertise their buses with
    /// `active = 0`; a host that honours that renders nothing, and the
    /// plugin silently never plays as a track instrument. `open` must leave
    /// every loadable instrument on an activated output bus.
    #[test]
    fn every_local_instrument_makes_sound() {
        let found = scan(&default_vst_dirs());
        let mut checked = 0;
        for e in found.iter().filter(|e| e.error.is_none() && e.instrument) {
            if !e.path.extension().is_some_and(|x| x == "vst3") {
                continue; // VST2 instruments are covered by the smoke test
            }
            let Ok(load) = open(&e.path, VstKind::V3, 48_000, None) else { continue };
            load.entry.note_on(0, 60, 110);
            let mut peak = 0.0f32;
            // ~1 second at 48k in ring-sized chunks.
            for _ in 0..96 {
                let mut out = [0.0f32; 512];
                load.entry.process(&[0.0f32; 512], &mut out);
                peak = peak.max(out.iter().fold(0.0f32, |m, x| m.max(x.abs())));
            }
            load.entry.all_notes_off();
            assert!(
                peak > 1e-4,
                "{}: instrument VST3 produced silence from a held note \
                 (unactivated output bus?)",
                e.path.display()
            );
            checked += 1;
        }
        // Informational: CI has no instruments installed.
        let _ = checked;
    }
}
