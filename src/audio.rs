//! JACK/PipeWire-JACK audio backend for slooper (DAW engine).
//!
//! A dedicated non-realtime "audio manager" thread owns the `jack::Client`.
//! The GUI talks to it over an mpsc command channel and receives state
//! updates (port lists, MIDI events, finished takes, status) on a second
//! channel.
//!
//! Port layout: the client registers a fixed pool of input/output slots
//! (`in0..`, `out0..`) plus a MIDI input once at startup, and stays
//! activated for its whole lifetime. Applying a selection only connects /
//! disconnects external ports to/from the pool — it never deactivates the
//! client. (Deactivating is unsafe here: the jack crate's `deactivate` only
//! clears the thread-init/process callbacks, leaving notification callbacks
//! registered against handler memory that is then freed — a use-after-free
//! on PipeWire-JACK.) The realtime process handler reads two atomic
//! "connected" masks and mixes only enabled slots, so no locking or stale
//! port state ever enters the audio path.
//!
//! Playback model: ONE global transport clock drives the whole project. The
//! musical content is an [`Arrangement`] (tracks → lanes → clips) plus an
//! asset table, both shared as immutable Arc snapshots the GUI swaps in with
//! [`Cmd::SetProject`]; the realtime path walks the timeline and sums every
//! clip the playhead crosses. There are no per-track clocks and no overdub —
//! recording captures raw input into a take buffer and hands the finished
//! take back to the GUI (as an asset + clip placement) when stopped.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;

use jack::jack_sys as j;
use jack::{
    AudioIn, AudioOut, Client, ClientOptions, ClientStatus, Control, MidiIn, NotificationHandler,
    Port, PortFlags, PortId, ProcessHandler, ProcessScope,
};

use crate::arrange::Arrangement;

pub const CLIENT_NAME: &str = "slooper";
const MIDI_PORT_SHORT: &str = "midi_in";
/// Fixed pool of mix slots; `Apply` maps the selected ports onto slots 0..n-1.
pub const MAX_SLOTS: usize = 16;

/// 1/sqrt(2) — the equal-power centre gain and the mono-downmix scale.
const F1S2: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Equal-power pan gains `(left, right)` for a position in [0, 1]
/// (0 = hard left, 0.5 = centre, 1 = hard right). At centre both are
/// 1/sqrt(2), so the mono downmix `(l + r)/sqrt(2)` of a centred source
/// is exactly the source — a single (or L+R-routed) output reproduces the
/// old mono mix unchanged at the default pan.
pub fn pan_gains(p: f32) -> (f32, f32) {
    let th = p.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (th.cos(), th.sin())
}

/// Audition voice list: (voice id, asset id, samples). Starts are additive,
/// so the list is the whole audition state the RT thread mixes.
pub type PreviewVoices = Arc<Vec<(u64, u32, Arc<[f32]>)>>;

/// Number of simultaneous tracks.
/// Track capacity (v0.10: 16 → 128). Fixed-size RT arrays scale with this;
/// the per-sample mix path iterates a compact ACTIVE-track list so an empty
/// 128-track project costs the same CPU as a 16-track one did before.
pub const MAX_LOOPS: usize = 128;

/// Preview voice id for auditions that belong to no rack asset (piano-roll
/// note taps, edit echoes). Stopping by this id never touches asset previews.
pub const PREVIEW_TRANSIENT: u32 = u32::MAX;
/// Per-track recording capacity: ~60 s @ 48 kHz (~30 s @ 96 kHz).
pub const LOOP_CAP: u32 = 2_880_000;

// Per-track recording states (AtomicU8 in the engine).
pub const TR_IDLE: u8 = 0;
pub const TR_REC: u8 = 1;

/// MIDI value at which the per-track gain slider sits at unity (0 dB): below
/// it cuts toward -inf, above it boosts toward [`GAIN_MAX_DB`].
pub const GAIN_UNITY_MIDI: u8 = 80;
/// Maximum boost of the per-track gain slider (MIDI 127), in dB.
pub const GAIN_MAX_DB: f32 = 12.0;
/// Linear amplitude of [`GAIN_MAX_DB`] (10^(12/20)).
const GAIN_MAX_LIN: f32 = 1.584_893_2;

/// Seek sentinel in the engine's seek slot: "no seek pending".
pub const NO_SEEK: u64 = u64::MAX;

/// Per-track gain trim: MIDI 0..=127 → linear amplitude (0 = silence / -inf,
/// [`GAIN_UNITY_MIDI`] = unity / 0 dB, 127 = +12 dB).
#[inline]
pub fn gain_amp(v: u8) -> f32 {
    if v <= GAIN_UNITY_MIDI {
        v as f32 / GAIN_UNITY_MIDI as f32
    } else {
        1.0 + (v - GAIN_UNITY_MIDI) as f32 / (127 - GAIN_UNITY_MIDI) as f32 * (GAIN_MAX_LIN - 1.0)
    }
}

/// A remote audio port the user can select (full name is `client:port`).
#[derive(Clone, Debug, PartialEq)]
pub struct PortInfo {
    pub name: String,
    pub client: String,
}

/// One slot of a track's chain: either a scripted effect or a hosted VST
/// plugin. Both kinds live in the *same* ordered chain, so a plugin can sit
/// anywhere in the effects order (FL Studio's mixer-slot model).
pub enum FxKind {
    /// Bundled/JS-compiled effect script.
    Scripted {
        inst: Arc<crate::fx::Instance>,
        desc: crate::fx::EffectDesc,
    },
    /// In-process hosted plugin (VST2/VST3), loaded by `vsthost`.
    Vst(Arc<crate::vsthost::RtStageEntry>),
}

/// A chain slot; see [`FxKind`].
pub struct FxSlot {
    pub kind: FxKind,
}

impl FxSlot {
    pub fn scripted(inst: Arc<crate::fx::Instance>, desc: crate::fx::EffectDesc) -> Self {
        Self { kind: FxKind::Scripted { inst, desc } }
    }
    pub fn vst(entry: Arc<crate::vsthost::RtStageEntry>) -> Self {
        Self { kind: FxKind::Vst(entry) }
    }
    pub fn is_vst(&self) -> bool {
        matches!(self.kind, FxKind::Vst(_))
    }
    /// Chain-list label (window titles, chips).
    pub fn label(&self) -> String {
        match &self.kind {
            FxKind::Scripted { desc, .. } => desc.name.clone(),
            FxKind::Vst(e) => e.name().to_string(),
        }
    }
}

/// A track's live effect chain (GUI order = audio order). Replaced whole
/// when the GUI edits it; the RT thread only reads slots, and slider moves
/// ride on the instance's atomics — no chain rebuild, no locks per sample.
#[derive(Clone, Default)]
pub struct TrackFx {
    pub slots: Arc<Vec<FxSlot>>,
}

impl TrackFx {
    pub fn slots(&self) -> &[FxSlot] {
        &self.slots
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// True when any slot hosts a plugin: the track's chain must run
    /// block-wise (plugins consume whole buffers), not per sample.
    pub fn has_vst(&self) -> bool {
        self.slots.iter().any(|s| s.is_vst())
    }
}

/// A track's sound source for MIDI clips. Hosted plugins are first-class
/// synthesizers (VSTi), not just effects, so the synth table holds both
/// kinds; every consumer (RT render, offline render, sliders, panic) goes
/// through this enum.
pub enum SynthHandle {
    Script(Arc<crate::synth::Instance>),
    Vst(Arc<crate::vsthost::RtStageEntry>),
}

impl SynthHandle {
    /// Silence every ringing voice (transport seek/stop).
    pub fn all_off(&self) {
        match self {
            SynthHandle::Script(i) => i.all_off(),
            // A plugin's already-produced samples live in its staging
            // ring: drop them FIRST, then stop the notes — the all-notes-
            // off messages ride in `pending_midi`, and flushing after
            // would drop them too, leaving plugin voices ringing forever.
            SynthHandle::Vst(e) => {
                e.flush();
                e.all_notes_off();
            }
        }
    }
}

/// Raw MIDI message copied out of the realtime thread.
#[derive(Debug)]
pub struct RawMidiMsg {
    pub bytes: Vec<u8>,
}

/// A selected output port paired with its stereo routing: bit 0 = left
/// bus, bit 1 = right bus (3 = mono downmix, the default).
pub type OutSel = (String, u8);

/// Immutable asset snapshot: sample buffers indexed by asset id, swapped in
/// as one Arc whenever the rack changes. Clip lengths are the asset lengths.
pub type AssetTable = Vec<Arc<[f32]>>;

/// Commands from the GUI to the audio manager thread.
pub enum Cmd {
    /// Re-enumerate remote audio ports and send them back.
    Refresh,
    /// Connect the selected external ports to mix slots 0..n-1 (disconnecting
    /// whatever was there before). Empty lists stop all audio; MIDI monitoring
    /// keeps running. Selections beyond `MAX_SLOTS` are ignored (with an error).
    /// Each output carries its stereo routing as bits: 1 = left bus,
    /// 2 = right bus, 3 = mono downmix `(L + R)/sqrt2` (the default, and the
    /// bit-exact old mono behaviour for centred mixes). A port routed to both
    /// gets the downmix, never two conflicting writes.
    Apply {
        ins: Vec<String>,
        outs: Vec<OutSel>,
    },
    /// Start/stop a raw input capture on track `i`. Stopping sends the take
    /// back as [`Evt::Take`] (armed at the playhead position it started in);
    /// the GUI turns it into an asset + clip. Takes never touch the engine's
    /// playback state.
    ToggleRecord {
        i: usize,
    },
    /// Set / toggle a track's play/stop: while the transport runs, a *stopped*
    /// track contributes nothing (its clips stay on the timeline untouched).
    SetTrackOn {
        i: usize,
        on: bool,
    },
    ToggleTrackOn {
        i: usize,
    },
    /// Start the global transport from the current playhead position.
    PlayAll,
    /// Stop the global transport; the playhead holds its position.
    StopAll,
    /// Move the playhead (samples). Scrubbing sends these continuously.
    Seek {
        pos: u64,
    },
    /// Timeline loop toggle: wrap the playhead at the loop region's end vs.
    /// park at the project end.
    SetLoop(bool),
    /// The active loop region in samples: wrap from `start` to `end`. The
    /// GUI computes `end` from the loop mode (area handles, current-longest
    /// track content, or whole-song end) on every edit; `end = 0` means
    /// Master switch for live input monitoring (Settings checkbox, default
    /// on). Off mutes the live pass-through of the selected track's chain
    /// only — levels, recording, playback and the meter keep working.
    SetMonitor(bool),
    /// "wrap at the project end". Only consulted while [`Cmd::SetLoop`] is
    /// on — with the loop off the transport still parks at the project end.
    SetLoopRegion {
        start: u64,
        end: u64,
    },
    /// Replace the engine's project snapshot (assets + arrangement + total
    /// length). The RT handler picks the new Arcs up on the next cycle.
    SetProject {
        assets: Arc<AssetTable>,
        arr: Arc<Arrangement>,
        len: u64,
    },
    /// Replace the MIDI pattern table (indexed in parallel with `assets`).
    SetMidiAssets(Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>),
    /// Bind/unbind one track's synthesizer (scripted synth or VSTi).
    SetTrackSynth {
        i: usize,
        synth: Option<Arc<SynthHandle>>,
    },
    /// Set the fallback synth (bundled piano) used by tracks with no
    /// explicit binding.
    SetDefaultSynth(Option<Arc<SynthHandle>>),
    /// Write a track's rendered contribution (all its lanes, engine-rate
    /// mixdown of the current project) to `path` in format `fmt`. Runs on
    /// the manager thread; a Notice/Error event reports the outcome.
    ExportTrack {
        i: usize,
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
    },
    /// Render and write the whole project (every sounding track, all lanes,
    /// mixer trims, pans and effects) to `path`.
    ExportProject {
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
    },
    /// Render and write ONLY the listed clips — `(track, lane, clip index)`
    /// into the current arrangement snapshot — time-aligned so the first
    /// clip starts at sample 0. Selected tracks need not be sounding.
    ExportSelection {
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
        items: Vec<(usize, usize, usize)>,
    },
    /// The GUI replaced track `i`'s effect chain (order = audio order). The
    /// slots hold refcounted effect instances created on the GUI thread; the
    /// audio thread picks the new table up on the next cycle without locking.
    SetTrackEffects {
        i: usize,
        chain: TrackFx,
    },
    /// The GUI selection moved to track `i` (0-based). Input monitoring
    /// follows the selected track: its gain, volume, effect chain and pan
    /// shape the live pass-through.
    SetSelected(usize),
    /// Latency compensation: every fresh take discards its first `ms`
    /// milliseconds of input, trimming the silence gap between pressing the
    /// record button and that button click reaching the mic. Monitoring and
    /// the output mix are never delayed. Clamped to 0..=1000 ms.
    SetLatency {
        ms: f32,
    },
    /// Metronome: on/off, tempo in BPM, click subdivision in clicks per beat
    /// (1 = quarter, 2 = eighth, 3 = triplet, 4 = sixteenth, ...), and beats
    /// per measure (the time-signature numerator; beat 1 of every measure
    /// gets the downbeat tone). Three distinct tones: downbeat, quarter note
    /// and subdivision. Clicks land on the *project timeline grid*: while the
    /// transport runs they tick at the transport tempo, and scrubbing the
    /// playhead ticks the grid under it. With the transport stopped the
    /// click free-runs from the playhead position — it never drives the
    /// playhead itself. Never recorded into a take.
    SetMetro {
        on: bool,
        bpm: u32,
        sub: u32,
        num: u32,
    },
    /// Audition a sample at the master output (asset-preview click). The
    /// buffer plays once from its start through the output mix only (never
    /// recorded, never loops). v0.10: ADDITIVE — every play starts a new
    /// independent voice, and repeated clicks of the same asset stack
    /// instead of cutting each other off. `id` tags the source asset so a
    /// rack row re-click can stop just its own voices.
    PreviewPlay {
        id: u32,
        samples: Arc<[f32]>,
    },
    /// Stop audition voices. `None` stops everything (transport stop);
    /// `Some(id)` stops only the voices started from that asset (the rack
    /// row toggle).
    PreviewStop {
        id: Option<u32>,
    },
    Quit,
}

/// Events from the audio manager thread to the GUI.
#[derive(Debug)]
pub enum Evt {
    /// No JACK/PipeWire-JACK server available.
    Unavailable(String),
    Ready {
        client_name: String,
        sample_rate: u32,
    },
    Ports {
        ins: Vec<PortInfo>,
        outs: Vec<PortInfo>,
    },
    Applied {
        ins: usize,
        outs: usize,
    },
    /// Transport state changed (the engine auto-stops at the project end
    /// when the loop toggle is off; the GUI mirrors the flag).
    Transport {
        on: bool,
    },
    /// A recording take finished: `at` is the playhead position when
    /// recording started, `data` the captured mono samples. The GUI stores
    /// it as an asset and places a clip; the engine keeps nothing.
    Take {
        track: usize,
        at: u64,
        data: Arc<[f32]>,
    },
    /// A remote MIDI source ports currently wired into `slooper:midi_in`
    /// (auto-connected; refreshed on port/client registration events).
    Midi(RawMidiMsg),
    MidiSources(Vec<String>),
    /// A non-error status message worth showing in the top bar (e.g. a
    /// finished export).
    Notice(String),
    Xrun,
    Error(String),
}

/// Handle kept by the GUI.
pub struct Backend {
    pub cmd_tx: Sender<Cmd>,
    pub evt_rx: Receiver<Evt>,
    /// Master output level (f32 as raw bits) — written by GUI, read in RT thread.
    pub gain: Arc<AtomicU32>,
    /// Peak of the mixed output during the last cycle (f32 bits).
    pub peak: Arc<AtomicU32>,
    /// Total xruns since start.
    pub xruns: Arc<AtomicU64>,
    /// MIDI events dropped because the GUI event queue was full.
    pub midi_drops: Arc<AtomicU64>,
    /// Shared engine (GUI reads atomics for transport display).
    pub engine: Arc<Engine>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    /// Spawn the audio manager thread and try to open a JACK client with NO_START.
    pub fn start() -> Backend {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        // Bounded: the realtime thread uses try_send and drops events if the GUI lags.
        let (evt_tx, evt_rx) = mpsc::sync_channel::<Evt>(256);
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let peak = Arc::new(AtomicU32::new(0));
        let xruns = Arc::new(AtomicU64::new(0));
        let midi_drops = Arc::new(AtomicU64::new(0));
        let engine = Engine::new();
        let (t_gain, t_peak, t_xruns, t_midi_drops, t_engine) = (
            gain.clone(),
            peak.clone(),
            xruns.clone(),
            midi_drops.clone(),
            engine.clone(),
        );

        let thread = std::thread::Builder::new()
            .name("slooper-audio".into())
            .spawn(move || {
                run(
                    cmd_rx,
                    evt_tx,
                    t_gain,
                    t_peak,
                    t_xruns,
                    t_midi_drops,
                    t_engine,
                );
            })
            .expect("failed to spawn audio thread");

        Backend {
            cmd_tx,
            evt_rx,
            gain,
            peak,
            xruns,
            midi_drops,
            engine,
            thread: Some(thread),
        }
    }

    /// Send a command, ignoring send errors (thread already gone).
    pub fn cmd(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Ask the audio thread to exit and wait for it. Joining (bounded) keeps
        // JACK client teardown off the process-exit path, where it would race
        // PipeWire's event loop and abort.
        self.cmd(Cmd::Quit);
        if let Some(handle) = self.thread.take() {
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = done_tx.send(());
            });
            // The thread exits on Quit; bound the wait so a stuck thread can't
            // hang shutdown forever.
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

/// The transport engine: recording buffers, atomic transport/mixer state,
/// and immutable Arc snapshots of the project (assets + arrangement), shared
/// between the manager thread (commands, take extraction, renders) and the
/// realtime handler (timeline mixing, capture).
///
/// Concurrency discipline:
/// - The audio thread is the only writer of take lengths while recording
///   (`fetch_add` discipline as before) and of the published playhead.
/// - Project snapshots are swapped wholesale as Arcs (try_lock in RT, never
///   blocks); a swap can never tear a clip mid-sample.
/// - The manager mutates nothing the RT path reads except atomics and the
///   quiesced take buffers (stop → one 20 ms sleep → read).
pub struct Engine {
    /// Per-track raw capture buffers (one take in flight per track).
    rec_bufs: [*mut f32; MAX_LOOPS],
    rec_len: [AtomicU32; MAX_LOOPS],
    rec_state: [AtomicU8; MAX_LOOPS],
    /// Playhead position (samples) when the current take started — reported
    /// back with the finished take so the GUI places the clip there.
    rec_at: [AtomicU64; MAX_LOOPS],
    /// Latency compensation in samples (0 = off): every fresh take discards
    /// its first `latency` input samples (armed in `start_rec`, consumed in
    /// `rec_sample`) so the click of pressing the record button — caught a
    /// few ms late by the mic — becomes the take's FIRST sample instead of
    /// landing a few ms into it. Monitoring and the output mix are never
    /// delayed.
    latency: AtomicU32,
    /// Per-track count of leading input samples still to discard for the
    /// current take; armed from `latency` by `start_rec`.
    rec_skip: [AtomicU32; MAX_LOOPS],
    /// Bitmask of tracks whose take the RT handler auto-stopped at buffer
    /// capacity. The manager polls + clears it (it owns the event channel)
    /// and ships those takes to the GUI like any other finished take.
    take_pending: [AtomicU64; 2],

    // ---- global transport ------------------------------------------------
    /// 1 = running (timeline advances), 0 = stopped (playhead holds).
    transport_on: AtomicU8,
    /// 1 = wrap the playhead at the project end, 0 = park at the end.
    loop_tl: AtomicU8,
    /// Loop region (samples): wrap from `loop_start` back to it at
    /// `loop_end`; `loop_end = 0` means "the project end". Set by the GUI
    /// from the loop mode (`Cmd::SetLoopRegion`).
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    /// Playhead position in timeline samples (published by RT, seeks written
    /// by the manager through `seek`).
    pos: AtomicU64,
    /// Pending seek target; `NO_SEEK` when none. The RT handler swaps this at
    /// block start, which makes seeks race-free against RT position writes.
    seek: AtomicU64,
    /// Total project length in samples (>= 1 bar; the GUI guarantees it).
    proj_len: AtomicU64,

    // ---- mixer -----------------------------------------------------------
    /// Per-track play/stop flag (mirror — the GUI is the source of truth;
    /// MIDI/keybinds fire through the GUI too).
    track_on: [AtomicU8; MAX_LOOPS],
    /// Per-track playback volume (linear amplitude as f32 bits; 1.0 = 0 dB).
    /// Applied to each clip's playback contribution only — recorded takes
    /// always stay at raw input level.
    vol: [AtomicU32; MAX_LOOPS],
    /// Per-track gain trim (linear amplitude as f32 bits; 1.0 = 0 dB).
    trk_gain: [AtomicU32; MAX_LOOPS],
    /// Per-track stereo pan position as f32 in [0, 1] (0 = hard left,
    /// 0.5 = centre). Equal-power split between the L and R mix buses.
    pan: [AtomicU32; MAX_LOOPS],
    /// GUI-selected track (0-based). Live input monitoring rides this
    /// track's gain x volume, effect chain and pan (see `Proc::process`).
    selected: AtomicU8,
    /// Live input monitoring master switch (Settings checkbox; default on).
    /// Off mutes the pass-through only — levels, recording and playback
    /// are untouched. Gate on the RT thread via `monitor_amp`.
    monitor_on: AtomicBool,
    /// Per-track live effect chains. The manager thread swaps the whole
    /// table on `SetTrackEffects`; the RT handler refreshes a cached Arc
    /// clone when `fx_gen` changes (try_lock — never blocks).
    fx: std::sync::Mutex<Arc<[TrackFx; MAX_LOOPS]>>,
    fx_gen: AtomicU64,

    // ---- project snapshots -----------------------------------------------
    assets: std::sync::Mutex<Arc<AssetTable>>,
    assets_gen: AtomicU64,
    arr: std::sync::Mutex<Arc<Arrangement>>,
    arr_gen: AtomicU64,
    // ---- MIDI snapshots ---------------------------------------------------
    /// MIDI pattern table, indexed in parallel with `assets` (audio slots
    /// hold `None`); pushed together with the project by the GUI.
    midi: std::sync::Mutex<Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>>,
    midi_gen: AtomicU64,
    /// Per-track synth bindings (scripted or VSTi) + the fallback instance
    /// (bundled piano) used by unbound tracks.
    synths: std::sync::Mutex<Arc<[Option<Arc<SynthHandle>>; MAX_LOOPS]>>,
    synths_gen: AtomicU64,
    default_synth: std::sync::Mutex<Option<Arc<SynthHandle>>>,
    /// Chain reaper: a chain swap displaces VST slots; the displaced entries
    /// are *held* in `vst_held` instead of dropping where the swap happened:
    /// plugin destructors (and the VST3 COM teardown the runner hands off)
    /// must run on the creating (UI) thread. The UI drains held entries once
    /// the audio chain cache has caught up (`vst_gen` == `vst_refreshed`).
    vsts_gen: AtomicU64,
    vst_refreshed: AtomicU64,
    vst_held: std::sync::Mutex<Vec<Arc<crate::vsthost::RtStageEntry>>>,
    /// Jack rate, published once at connect (drives tick math + samplers).
    sample_rate: AtomicU32,

    // ---- metronome ---------------------------------------------------------
    /// Metronome: 0 = off, 1 = on; tempo (BPM); clicks per beat; beats per
    /// measure (time-signature numerator, default 4).
    metro_on: AtomicU8,
    metro_bpm: AtomicU32,
    metro_sub: AtomicU32,
    metro_num: AtomicU32,

    // ---- asset preview -----------------------------------------------------
    /// Sample buffer being auditioned (swapped wholesale, RT reads an Arc
    /// clone refreshed on `preview_gen`).
    /// Audition voice pool (v0.10). Each voice is (voice id, asset id,
    /// buffer). Starts are additive: every play_preview appends a voice and
    /// the RT thread runs all of them with independent cursors.
    preview_voices: std::sync::Mutex<PreviewVoices>,
    preview_vid: AtomicU64,
    preview_gen: AtomicU64,
    /// 1 = a preview is playing (RT clears it when the buffer runs out).
    preview_on: AtomicU8,
    /// Samples consumed by the YOUNGEST preview voice, republished per RT
    /// block — the piano-roll playhead follows its own preview (transport
    /// does not move while auditioning, so reading `pos` left the bar
    /// permanently static, the v10.2 report).
    preview_pos: AtomicU64,
}

impl Engine {
    pub fn new() -> Arc<Self> {
        let mut rec_bufs = [std::ptr::null_mut::<f32>(); MAX_LOOPS];
        for buf in rec_bufs.iter_mut() {
            let mut v = vec![0f32; LOOP_CAP as usize];
            *buf = v.as_mut_ptr();
            std::mem::forget(v); // ownership transfers to the engine
        }
        Arc::new(Self {
            rec_bufs,
            rec_len: core::array::from_fn(|_| AtomicU32::new(0)),
            rec_state: core::array::from_fn(|_| AtomicU8::new(TR_IDLE)),
            rec_at: core::array::from_fn(|_| AtomicU64::new(0)),
            latency: AtomicU32::new(0),
            rec_skip: core::array::from_fn(|_| AtomicU32::new(0)),
            take_pending: [AtomicU64::new(0), AtomicU64::new(0)],
            transport_on: AtomicU8::new(0),
            loop_tl: AtomicU8::new(1),
            loop_start: AtomicU64::new(0),
            loop_end: AtomicU64::new(0),
            pos: AtomicU64::new(0),
            seek: AtomicU64::new(NO_SEEK),
            // Empty project: an idle timeline as long as a take buffer, so
            // a transport parked on it never wraps/parks on a 1-sample
            // grid. The GUI replaces this with the real length via
            // Cmd::SetProject on every edit.
            proj_len: AtomicU64::new(LOOP_CAP as u64),
            track_on: core::array::from_fn(|_| AtomicU8::new(1)),
            vol: core::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
            trk_gain: core::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
            pan: core::array::from_fn(|_| AtomicU32::new(0.5f32.to_bits())),
            selected: AtomicU8::new(0),
            monitor_on: AtomicBool::new(true),
            fx: std::sync::Mutex::new(Arc::new(core::array::from_fn(|_| TrackFx::default()))),
            fx_gen: AtomicU64::new(0),
            assets: std::sync::Mutex::new(Arc::new(Vec::new())),
            assets_gen: AtomicU64::new(0),
            arr: std::sync::Mutex::new(Arc::new(Arrangement::new())),
            arr_gen: AtomicU64::new(0),
            midi: std::sync::Mutex::new(Arc::new(Vec::new())),
            midi_gen: AtomicU64::new(0),
            synths: std::sync::Mutex::new(Arc::new(core::array::from_fn(|_| None))),
            synths_gen: AtomicU64::new(0),
            default_synth: std::sync::Mutex::new(None),
            vsts_gen: AtomicU64::new(0),
            vst_refreshed: AtomicU64::new(0),
            vst_held: std::sync::Mutex::new(Vec::new()),
            sample_rate: AtomicU32::new(0),
            metro_on: AtomicU8::new(0),
            metro_bpm: AtomicU32::new(120),
            metro_sub: AtomicU32::new(1),
            metro_num: AtomicU32::new(4),
            preview_voices: std::sync::Mutex::new(Arc::new(Vec::new())),
            preview_vid: AtomicU64::new(0),
            preview_gen: AtomicU64::new(0),
            preview_on: AtomicU8::new(0),
            preview_pos: AtomicU64::new(0),
        })
    }

    // ---- take capture (audio thread writes, manager reads while quiesced) --

    /// Capture one dry-mixed sample into track i while recording. Returns
    /// true once the buffer is full (caller should auto-stop the take). The
    /// length counter never exceeds LOOP_CAP: the check happens before
    /// advancing, so a take that hits capacity mid-frame can't overshoot.
    #[inline]
    pub fn rec_sample(&self, i: usize, v: f32) -> bool {
        // Latency compensation: a fresh take discards its first `latency`
        // input samples (armed in `start_rec`) so the click of the button
        // that started recording — heard a few ms late through the mic —
        // becomes the take's FIRST sample instead of landing ms into it.
        let skip = self.rec_skip[i].load(Ordering::Relaxed);
        if skip > 0 {
            self.rec_skip[i].store(skip - 1, Ordering::Relaxed);
            return false;
        }
        let off = self.rec_len[i].load(Ordering::Relaxed) as u64;
        if off < LOOP_CAP as u64 {
            self.rec_write(i, off as u32, v);
            self.rec_len[i].store((off + 1) as u32, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    #[inline]
    pub fn rec_write(&self, i: usize, off: u32, v: f32) {
        unsafe { self.rec_bufs[i].add(off as usize).write(v) };
    }

    #[inline]
    pub fn rec_read(&self, i: usize, off: u32) -> f32 {
        unsafe { *self.rec_bufs[i].add(off as usize) }
    }

    pub fn rec_state_of(&self, i: usize) -> u8 {
        self.rec_state[i].load(Ordering::Relaxed)
    }

    /// RT: this track hit buffer capacity — flag the take for the manager
    /// to pick up and ship to the GUI.
    #[inline]
    pub fn mark_take_pending(&self, i: usize) {
        self.take_pending[i / 64].fetch_or(1u64 << (i % 64), Ordering::SeqCst);
    }

    /// Manager: claim every flagged take (returns a bitmask; bit `i` =
    /// track `i` has a capacity-stopped take ready to copy).
    pub fn claim_takes(&self) -> u128 {
        // Two swaps are individually atomic; a bit set between them is simply
        // claimed by the following claim (no lost updates, claim order
        // irrelevant for the take queue).
        let lo = self.take_pending[0].swap(0, Ordering::SeqCst) as u128;
        let hi = self.take_pending[1].swap(0, Ordering::SeqCst) as u128;
        lo | (hi << 64)
    }

    pub fn rec_len_of(&self, i: usize) -> u32 {
        self.rec_len[i].load(Ordering::Relaxed)
    }

    /// Playhead position when track `i`'s current take started.
    pub fn rec_at(&self, i: usize) -> u64 {
        self.rec_at[i].load(Ordering::Relaxed)
    }

    /// Zero the take buffer, arm the latency-compensation skip, remember the
    /// playhead, and start capturing on track i.
    pub fn start_rec(&self, i: usize) {
        self.rec_state[i].store(TR_IDLE, Ordering::Relaxed);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.rec_bufs[i], LOOP_CAP as usize) };
        buf.fill(0.0f32);
        self.rec_len[i].store(0, Ordering::Relaxed);
        // Snapshot the compensation at arm time: changing latency mid-take
        // must never retroactively shift a recording in progress.
        self.rec_skip[i].store(self.latency.load(Ordering::Relaxed), Ordering::Relaxed);
        self.rec_at[i].store(self.pos.load(Ordering::Relaxed), Ordering::Relaxed);
        self.rec_state[i].store(TR_REC, Ordering::Relaxed);
    }

    /// Stop capturing on track i (state first, so RT releases the buffer)
    /// and clone the take's content. The caller must let one or two audio
    /// cycles elapse between the stop and this read (same discipline the old
    /// clear/quantize used); `finish_take` on the manager wraps that.
    pub fn copy_take(&self, i: usize) -> (u64, Arc<[f32]>) {
        self.rec_state[i].store(TR_IDLE, Ordering::Relaxed);
        let n = self.rec_len[i].load(Ordering::Relaxed) as usize;
        let data = unsafe { std::slice::from_raw_parts(self.rec_bufs[i], n) };
        (self.rec_at[i].load(Ordering::Relaxed), Arc::from(data))
    }

    // ---- global transport ---------------------------------------------------

    pub fn transport_on(&self) -> bool {
        self.transport_on.load(Ordering::Relaxed) != 0
    }

    pub fn set_transport(&self, on: bool) {
        self.transport_on.store(on as u8, Ordering::Relaxed);
    }

    pub fn loop_tl(&self) -> bool {
        self.loop_tl.load(Ordering::Relaxed) != 0
    }
    pub fn set_loop_region(&self, start: u64, end: u64) {
        self.loop_start.store(start, Ordering::Relaxed);
        self.loop_end.store(end, Ordering::Relaxed);
    }
    pub fn loop_region(&self) -> (u64, u64) {
        (
            self.loop_start.load(Ordering::Relaxed),
            self.loop_end.load(Ordering::Relaxed),
        )
    }

    /// Live input monitoring master switch (Settings checkbox).
    pub fn set_monitor_on(&self, on: bool) {
        self.monitor_on.store(on, Ordering::Relaxed);
    }
    pub fn monitor_on(&self) -> bool {
        self.monitor_on.load(Ordering::Relaxed)
    }
    /// The pass-through gain actually applied to live input: the selected
    /// track's amp when monitoring is on, hard zero when off. The RT mix
    /// gates through the same atomic — pinned by
    /// `monitoring_toggle_gates_live_input`.
    pub fn monitor_amp(&self, sel_amp: f32) -> f32 {
        if self.monitor_on.load(Ordering::Relaxed) {
            sel_amp
        } else {
            0.0
        }
    }

    pub fn set_loop_tl(&self, on: bool) {
        self.loop_tl.store(on as u8, Ordering::Relaxed);
    }

    /// Current playhead position (samples).
    pub fn pos(&self) -> u64 {
        self.pos.load(Ordering::Relaxed)
    }

    /// Request a playhead move. RT swaps `seek` at block start, so this is
    /// safe while the transport runs (scrubbing).
    pub fn seek_to(&self, p: u64) {
        let p = p.min(self.proj_len.load(Ordering::Relaxed));
        self.pos.store(p, Ordering::Relaxed);
        self.seek.store(p, Ordering::Relaxed);
    }

    pub fn take_seek(&self) -> Option<u64> {
        match self.seek.swap(NO_SEEK, Ordering::Relaxed) {
            NO_SEEK => None,
            p => Some(p),
        }
    }

    pub fn proj_len(&self) -> u64 {
        self.proj_len.load(Ordering::Relaxed)
    }

    // ---- mixer --------------------------------------------------------------

    pub fn track_on(&self, i: usize) -> bool {
        self.track_on[i].load(Ordering::Relaxed) != 0
    }

    pub fn set_track_on(&self, i: usize, on: bool) {
        self.track_on[i].store(on as u8, Ordering::Relaxed);
    }

    /// Latency compensation in samples (0 = off). Each fresh take discards
    /// this many leading input samples.
    pub fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Per-track playback volume (linear amplitude; 1.0 = 0 dB).
    pub fn loop_vol(&self, i: usize) -> f32 {
        f32::from_bits(self.vol[i].load(Ordering::Relaxed))
    }

    /// Set track `i`'s playback volume (linear amplitude; 0 = silence).
    pub fn set_loop_vol(&self, i: usize, amp: f32) {
        self.vol[i].store(amp.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Per-track gain trim (linear amplitude; 1.0 = 0 dB).
    pub fn loop_gain(&self, i: usize) -> f32 {
        f32::from_bits(self.trk_gain[i].load(Ordering::Relaxed))
    }

    /// Set track `i`'s gain trim (linear amplitude; 0 = silence).
    pub fn set_loop_gain(&self, i: usize, amp: f32) {
        self.trk_gain[i].store(amp.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Per-track pan position ([0, 1]; 0.5 = centre).
    pub fn loop_pan(&self, i: usize) -> f32 {
        f32::from_bits(self.pan[i].load(Ordering::Relaxed))
    }

    /// Set track `i`'s pan position, clamped to [0, 1].
    pub fn set_loop_pan(&self, i: usize, p: f32) {
        self.pan[i].store(p.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Replace track `i`'s live effect chain (manager thread). Any VST slot
    /// displaced by the swap is held for UI-thread teardown (see
    /// [`Self::drain_vst_held`]) so plugin destructors never run where they
    /// could race the audio cache swap.
    pub fn set_track_fx(&self, i: usize, chain: TrackFx) {
        let mut t = self.fx.lock().unwrap();
        if let Some(old) = t.get(i) {
            let mut displaced = false;
            for s in old.slots() {
                if let FxKind::Vst(e) = &s.kind {
                    self.vst_held.lock().unwrap().push(e.clone());
                    displaced = true;
                }
            }
            if displaced {
                // Arms `drain_vst_held`: releasable only once the RT chain
                // cache has picked up this swap and stamped `vst_refreshed`.
                self.vsts_gen.fetch_add(1, Ordering::Relaxed);
            }
        }
        let fresh = core::array::from_fn(|k| if k == i { chain.clone() } else { t[k].clone() });
        *t = Arc::new(fresh);
        drop(t);
        self.fx_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Current effect-chain table (GUI snapshots; the RT handler picks it up
    /// when `fx_gen` moves — see `Proc::process`).
    pub fn fx_table(&self) -> Arc<[TrackFx; MAX_LOOPS]> {
        self.fx.lock().unwrap().clone()
    }

    pub fn fx_gen(&self) -> u64 {
        self.fx_gen.load(Ordering::Relaxed)
    }

    // ---- project snapshots ----------------------------------------------------

    /// Replace the engine's project snapshot (any thread; RT refreshes when
    /// either generation moves).
    pub fn set_project(&self, assets: Arc<AssetTable>, arr: Arc<Arrangement>, len: u64) {
        *self.assets.lock().unwrap() = assets;
        self.assets_gen.fetch_add(1, Ordering::Relaxed);
        *self.arr.lock().unwrap() = arr;
        self.arr_gen.fetch_add(1, Ordering::Relaxed);
        self.proj_len.store(len.max(1), Ordering::Relaxed);
        let pl = self.proj_len.load(Ordering::Relaxed);
        // A project can shrink under the playhead â keep it in bounds.
        if self.pos.load(Ordering::Relaxed) > pl {
            self.seek_to(pl);
        }
        // Samplers read the rack through the host table; refresh it with
        // every project push (assets can change under the same gen).
        crate::synth::set_host_assets(self.assets(), self.sample_rate.load(Ordering::Relaxed));
    }

    /// Replace the MIDI pattern table (parallel to `assets`).
    pub fn set_midi_assets(&self, midi: Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>) {
        *self.midi.lock().unwrap() = midi;
        self.midi_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn midi(&self) -> Arc<Vec<Option<Arc<crate::midipat::Pattern>>>> {
        self.midi.lock().unwrap().clone()
    }
    pub fn midi_gen(&self) -> u64 {
        self.midi_gen.load(Ordering::Relaxed)
    }

    /// Bind/unbind one track's synthesizer (unbound tracks fall back to the
    /// default synth — the bundled piano).
    pub fn set_track_synth(&self, i: usize, synth: Option<Arc<SynthHandle>>) {
        if let Ok(mut g) = self.synths.try_lock() {
            let mut next = g.clone();
            if let Some(slot) = Arc::make_mut(&mut next).get_mut(i) {
                *slot = synth;
            }
            *g = next;
            self.synths_gen.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn synths(&self) -> Arc<[Option<Arc<SynthHandle>>; MAX_LOOPS]> {
        self.synths.lock().unwrap().clone()
    }
    pub fn synths_gen(&self) -> u64 {
        self.synths_gen.load(Ordering::Relaxed)
    }
    pub fn set_default_synth(&self, synth: Option<Arc<SynthHandle>>) {
        *self.default_synth.lock().unwrap() = synth;
        // The RT handler snapshots `default_synth` inside its `synths_gen`
        // refresh — without a bump here it would keep rendering with a
        // stale (or missing) fallback until some track binding changed.
        self.synths_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn default_synth(&self) -> Option<Arc<SynthHandle>> {
        self.default_synth.lock().unwrap().clone()
    }

    pub fn vst_gen(&self) -> u64 {
        self.vsts_gen.load(Ordering::Relaxed)
    }

    /// Hand chain-displaced plugin entries back to the UI thread — but only
    /// once the audio chain cache has caught up with every swap, so the final
    /// reference of each plugin dies on the caller's (UI) thread rather than
    /// mid-swap on the audio thread.
    pub fn drain_vst_held(&self) -> Vec<Arc<crate::vsthost::RtStageEntry>> {
        if self.vst_gen() == self.vst_refreshed.load(Ordering::Relaxed) {
            core::mem::take(&mut *self.vst_held.lock().unwrap())
        } else {
            Vec::new()
        }
    }

    /// The synth a track actually renders through (binding, else default).
    fn synth_for(&self, t: usize) -> Option<Arc<SynthHandle>> {
        self.synths()
            .get(t)
            .cloned()
            .flatten()
            .or_else(|| self.default_synth())
    }

    pub fn set_sample_rate(&self, sr: u32) {
        self.sample_rate.store(sr, Ordering::Relaxed);
        let assets = self.assets();
        crate::synth::set_host_assets(assets, sr);
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    pub fn assets(&self) -> Arc<AssetTable> {
        self.assets.lock().unwrap().clone()
    }

    pub fn assets_gen(&self) -> u64 {
        self.assets_gen.load(Ordering::Relaxed)
    }

    pub fn arrangement(&self) -> Arc<Arrangement> {
        self.arr.lock().unwrap().clone()
    }

    pub fn arr_gen(&self) -> u64 {
        self.arr_gen.load(Ordering::Relaxed)
    }

    // ---- metronome --------------------------------------------------------------

    /// Metronome state: (on, bpm, clicks per beat, beats per measure).
    pub fn metro(&self) -> (bool, u32, u32, u32) {
        (
            self.metro_on.load(Ordering::Relaxed) != 0,
            self.metro_bpm.load(Ordering::Relaxed),
            self.metro_sub.load(Ordering::Relaxed),
            self.metro_num.load(Ordering::Relaxed),
        )
    }

    // ---- asset preview -----------------------------------------------------------

    /// Add an audition voice at the master output (mono, mixed to both
    /// channels). STRICTLY ADDITIVE (v0.10): every call is a NEW voice with
    /// its own RT cursor, so repeated clicks stack, including repeats of the
    /// same asset, instead of cutting each other off. Bounded pool: the
    /// oldest voice is dropped when the ceiling is reached.
    pub fn play_preview(&self, id: u32, samples: Arc<[f32]>) {
        const MAX_VOICES: usize = 12;
        let mut v = (**self.preview_voices.lock().unwrap()).clone();
        if v.len() >= MAX_VOICES {
            v.remove(0);
        }
        let vid = self.preview_vid.fetch_add(1, Ordering::Relaxed) + 1;
        v.push((vid, id, samples));
        *self.preview_voices.lock().unwrap() = Arc::new(v);
        self.preview_gen.fetch_add(1, Ordering::SeqCst);
        self.preview_on.store(1, Ordering::Relaxed);
    }

    /// Stop every audition voice (transport stop / global stop).
    pub fn stop_preview(&self) {
        *self.preview_voices.lock().unwrap() = Arc::new(Vec::new());
        self.preview_gen.fetch_add(1, Ordering::SeqCst);
        self.preview_on.store(0, Ordering::Relaxed);
    }

    /// Stop only the voices started from asset `id` (rack row toggle): a
    /// re-click silences that clip's audition while previews of other
    /// assets keep playing (v0.10 parallel preview).
    pub fn stop_preview_id(&self, id: u32) {
        let mut v = (**self.preview_voices.lock().unwrap()).clone();
        v.retain(|(_, vid, _)| *vid != id);
        let empty = v.is_empty();
        *self.preview_voices.lock().unwrap() = Arc::new(v);
        self.preview_gen.fetch_add(1, Ordering::SeqCst);
        if empty {
            self.preview_on.store(0, Ordering::Relaxed);
        }
    }

    /// Snapshot of the voice list for the RT handler (cheap Arc clone).
    pub fn preview_snapshot(&self) -> PreviewVoices {
        self.preview_voices.lock().unwrap().clone()
    }

    pub fn preview_active(&self) -> bool {
        self.preview_on.load(Ordering::Relaxed) != 0
    }

    /// Samples elapsed in the youngest preview voice (only meaningful
    /// while `preview_active()`).
    pub fn preview_pos(&self) -> u64 {
        self.preview_pos.load(Ordering::Relaxed)
    }

    pub fn preview_gen(&self) -> u64 {
        self.preview_gen.load(Ordering::Relaxed)
    }

    // ---- offline rendering --------------------------------------------------------

    /// Render the current project to mono f32 samples at the project length:
    /// every clip of every *sounding* track (stopped tracks contribute
    /// nothing — the export is what the mix plays). Each track's contribution
    /// runs through fresh private effect instances (phase 0) after its
    /// vol x gain trim, panned to the mono downmix — the same chain the live
    /// mix applies. `None` when the project holds no clips.
    pub fn render_project(&self) -> Option<Vec<f32>> {
        self.render_tracks(None, self.proj_len() as usize)
    }

    /// Render one track's contribution (all lanes; the track's play/stop
    /// flag is ignored when a track export is explicitly requested).
    pub fn render_track(&self, t: usize) -> Option<Vec<f32>> {
        self.render_tracks(Some(t), self.proj_len() as usize)
    }

    fn render_tracks(&self, only: Option<usize>, len: usize) -> Option<Vec<f32>> {
        let arr = self.arrangement();
        let assets = self.assets();
        if only.is_none() && arr.is_empty() {
            return None;
        }
        let sr = self.sample_rate().max(1);
        let mut out = vec![0.0f32; len.max(1)];
        let mut any = false;
        for t in 0..MAX_LOOPS {
            if let Some(want) = only {
                if t != want {
                    continue;
                }
            } else if !self.track_on(t) {
                continue; // exports mirror the live mix
            }
            let tamp = self.loop_vol(t) * self.loop_gain(t);
            let (pgl, pgr) = pan_gains(self.loop_pan(t));
            let pdown = (pgl + pgr) * F1S2;
            let chain = self.fx.lock().unwrap()[t].clone();
            // A chain that hosts a plugin must run continuously over the
            // track's whole contribution (all clips accumulate dry first),
            // so tails ring past the last clip exactly like the live mix —
            // the live RT path uses the same span rule. Scripted-only chains
            // keep the historical per-clip phase-0 treatment.
            let span = chain.has_vst();
            let mut trk: Vec<f32> = if span { vec![0.0f32; len.max(1)] } else { Vec::new() };
            let midi = self.midi();
            // MIDI clips first: the whole track's patterns through one fresh
            // synth, then through the same track FX + gain as audio clips.
            if arr.tracks[t]
                .lanes
                .iter()
                .any(|l| l.clips.iter().any(|c| matches!(midi.get(c.asset as usize), Some(Some(_)))))
            {
                if let Some(h) = self.synth_for(t) {
                    let mut ev: Vec<(u64, u8, u8, u8, u8)> = Vec::new();
                    for lane in &arr.tracks[t].lanes {
                        for cl in &lane.clips {
                            if let Some(Some(pat)) = midi.get(cl.asset as usize) {
                                stage_midi_events(pat, cl, sr, &mut ev);
                            }
                        }
                    }
                    let mut buf = vec![0.0f32; len.max(1)];
                    render_midi_any(&h, &mut ev, 0, len.max(1), &mut buf, sr);
                    if span {
                        for (k, x) in buf.into_iter().take(out.len()).enumerate() {
                            any = true;
                            trk[k] += x * tamp;
                        }
                    } else if let Some(fx) = scripted_chain(&chain) {
                        let sbuf: Vec<f32> = buf
                            .into_iter()
                            .take(out.len())
                            .map(|x| {
                                let mut s = x * tamp;
                                for f in &fx {
                                    s = f.process_private(s);
                                }
                                s
                            })
                            .collect();
                        for (k, s) in sbuf.into_iter().enumerate() {
                            any = true;
                            out[k] += s * pdown;
                        }
                    } else {
                        for (k, x) in buf.into_iter().take(out.len()).enumerate() {
                            any = true;
                            out[k] += x * tamp * pdown;
                        }
                    }
                }
            }
            for lane in &arr.tracks[t].lanes {
                for cl in &lane.clips {
                    let data = match assets.get(cl.asset as usize) {
                        Some(d) if !d.is_empty() => d,
                        _ => continue, // empty (MIDI) or dangling asset
                    };
                    any = true;
                    // Fresh private instances per clip: every clip starts at
                    // effect phase 0, and the live chain is never advanced.
                    let fx = if span { None } else { scripted_chain(&chain) };
                    let mix = |x: f32| {
                        let mut s = x * tamp;
                        if let Some(fx) = &fx {
                            for f in fx {
                                s = f.process_private(s);
                            }
                        }
                        s
                    };
                    // The clip is pinned at `pos` and the source starts at
                    // `cut` (identical to the live callback); `play_len`
                    // past the source contributes silence (nothing summed).
                    let alen = data.len() as u64;
                    let dstart = cl.pos as usize;
                    let dend = cl
                        .data_end(alen)
                        .min(cl.end(alen))
                        .min(len as u64)
                        .max(cl.pos) as usize;
                    for k in dstart..dend {
                        any = true;
                        let s = data[(cl.cut + (k as u64 - cl.pos)) as usize];
                        let m = mix(s) * cl.gain * cl.fade_at(k as u64, alen);
                        if span {
                            trk[k] += m;
                        } else {
                            out[k] += m * pdown;
                        }
                    }
                }
            }
            if span {
                // Full chain over the whole track span (fresh copies: the
                // live chain is never advanced by an export).
                run_chain_over(&chain, sr, &mut trk);
                for (k, s) in trk.into_iter().enumerate() {
                    out[k] += s * pdown;
                }
            }
        }
        if any {
            Some(out)
        } else {
            None
        }
    }

    /// Render staged MIDI events through a track's instrument AND its full
    /// mixer chain — the piano roll's "preview through this track" voice,
    /// so auditioning hears the same synth + effects the clip will play
    /// through. Mono; the gain law mirrors the live order (track gain into
    /// the chain; fader/pan belong to the mix, not the audition).
    pub fn bounce_track_preview(
        &self,
        t: usize,
        ev: &mut [(u64, u8, u8, u8, u8)],
        len: usize,
    ) -> Option<Vec<f32>> {
        let h = self.synth_for(t)?;
        let sr = self.sample_rate().max(1);
        let n = len.max(1);
        let mut buf = vec![0.0f32; n];
        render_midi_any(&h, ev, 0, n, &mut buf, sr);
        let tamp = self.loop_vol(t) * self.loop_gain(t);
        if (tamp - 1.0).abs() > f32::EPSILON {
            for x in buf.iter_mut() {
                *x *= tamp;
            }
        }
        let chain = self.fx.lock().unwrap()[t].clone();
        run_chain_over(&chain, sr, &mut buf);
        Some(buf)
    }

    /// Render ONLY the listed clips (`(track, lane, clip index)` into the
    /// current snapshot), time-aligned so the earliest clip starts at
    /// sample 0 — the export counterpart of multi-clip selection. Track
    /// play/stop flags are ignored (an explicit selection always sounds).
    /// Returns the timeline position the selection started at plus the
    /// rendered mono mix, or `None` when no listed clip resolves to content.
    pub fn render_selection(&self, items: &[(usize, usize, usize)]) -> Option<(u64, Vec<f32>)> {
        let arr = self.arrangement();
        let assets = self.assets();
        let midi = self.midi();
        let sr = self.sample_rate().max(1);
        let clips: Vec<(usize, crate::arrange::Clip)> = items
            .iter()
            .filter_map(|(t, k, c)| {
                let cl = arr.tracks.get(*t)?.lanes.get(*k)?.clips.get(*c).copied()?;
                if assets.get(cl.asset as usize).is_some() {
                    Some((*t, cl))
                } else {
                    None
                }
            })
            .collect();
        let alen = |c: &crate::arrange::Clip| -> u64 {
            match midi.get(c.asset as usize) {
                Some(Some(pat)) => pat.len_samples(sr),
                _ => assets
                    .get(c.asset as usize)
                    .map(|d| d.len() as u64)
                    .unwrap_or(0),
            }
        };
        let from = clips.iter().map(|(_, c)| c.pos).min()?;
        let to = clips.iter().map(|(_, c)| c.end(alen(c))).max()?;
        if to <= from {
            return None;
        }
        let len = (to - from) as usize;
        let mut out = vec![0.0f32; len];
        let chains = self.fx.lock().unwrap().clone();
        for (t, cl) in &clips {
            let tamp = self.loop_vol(*t) * self.loop_gain(*t);
            let (pgl, pgr) = pan_gains(self.loop_pan(*t));
            let pdown = (pgl + pgr) * F1S2;
            let chain = chains[*t].clone();
            let span = chain.has_vst();
            // MIDI clip: fresh synth copy + fresh chain, same treatment as
            // an audio clip (phase 0, live state untouched).
            if let Some(Some(pat)) = midi.get(cl.asset as usize) {
                if let Some(h) = self.synth_for(*t) {
                    let mut ev: Vec<(u64, u8, u8, u8, u8)> = Vec::new();
                    stage_midi_events(pat, cl, sr, &mut ev);
                    let mut buf = vec![0.0f32; len];
                    render_midi_any(&h, &mut ev, from, len, &mut buf, sr);
                    if span {
                        run_chain_over(&chain, sr, &mut buf);
                        for (k, s2) in buf.into_iter().enumerate() {
                            out[k] += s2 * tamp * pdown;
                        }
                    } else {
                        let fx = scripted_chain(&chain);
                        let mut sbuf: Vec<f32> = buf
                            .into_iter()
                            .map(|x| {
                                let mut s2 = x * tamp;
                                if let Some(fx) = &fx {
                                    for f in fx {
                                        s2 = f.process_private(s2);
                                    }
                                }
                                s2
                            })
                            .collect();
                        for (k, s2) in sbuf.drain(..).enumerate() {
                            out[k] += s2 * pdown;
                        }
                    }
                }
                continue;
            }
            let data = match assets.get(cl.asset as usize) {
                Some(d) if !d.is_empty() => d,
                _ => continue,
            };
            let fx = if span { None } else { scripted_chain(&chain) };
            let mix = |x: f32| {
                let mut s = x * tamp;
                if let Some(fx) = &fx {
                    for f in fx {
                        s = f.process_private(s);
                    }
                }
                s
            };
            let alen_u = data.len() as u64;
            // Same clip-pinned-at-pos mapping as live playback and
            // render_tracks; the selection window slices the output.
            let dstart = cl.pos as usize;
            let dend = cl
                .data_end(alen_u)
                .min(cl.end(alen_u))
                .min(cl.pos + alen_u) as usize;
            let mut chunk: Vec<f32> = (dstart..dend)
                .map(|k| data[(cl.cut + (k as u64 - cl.pos)) as usize])
                .enumerate()
                .map(|(j, x)| {
                    mix(x) * cl.gain * cl.fade_at((dstart + j) as u64, alen_u)
                })
                .collect();
            if span {
                // Fresh full-chain pass over this chunk (selection bounces
                // stay clip-local, mirroring the scripted per-clip rule).
                run_chain_over(&chain, sr, &mut chunk);
            }
            for (j, k) in (dstart..dend).enumerate() {
                if k >= from as usize {
                    let o = k - from as usize;
                    if o < len {
                        out[o] += chunk[j] * pdown;
                    }
                }
            }
        }
        Some((from, out))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        use std::alloc::{dealloc, Layout};
        let layout = Layout::array::<f32>(LOOP_CAP as usize).expect("valid layout");
        for p in self.rec_bufs.iter() {
            unsafe { dealloc(*p as *mut u8, layout) };
        }
    }
}

// The raw pointers are only ever dereferenced from the audio thread (take
// sample writes) or the manager thread while the take is quiesced; the Arc
// keeps the engine alive for as long as either thread exists.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

/// Manager-side record toggle: start a fresh capture, or stop the take and
/// hand it to the GUI. Stopping always leaves the engine ready for the next
/// take; empty stops (button mashing) are silently dropped.
fn toggle_record(engine: &Engine, i: usize, tx: &SyncSender<Evt>) {
    if engine.rec_state_of(i) == TR_REC {
        engine.rec_state[i].store(TR_IDLE, Ordering::Relaxed);
        // The GUI owns this take now; drop any capacity-stop flag so the
        // manager poll doesn't ship the same take twice.
        engine.mark_take_pending(i);
        // Let the RT thread quiesce before cloning the take buffer.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (at, data) = engine.copy_take(i);
        if data.is_empty() {
            let _ = tx.try_send(Evt::Notice(format!(
                "track {}: empty take discarded",
                i + 1
            )));
        } else {
            let _ = tx.try_send(Evt::Take { track: i, at, data });
        }
    } else {
        engine.start_rec(i);
    }
}

fn run(
    cmd_rx: Receiver<Cmd>,
    evt_tx: SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    xruns: Arc<AtomicU64>,
    midi_drops: Arc<AtomicU64>,
    engine: Arc<Engine>,
) {
    // Test-only kill switch: headless GUI tests must not open PipeWire-JACK
    // clients at all. Spawning and closing dozens of clients per suite races
    // libpipewire's own callback threads during teardown (UAF inside the
    // client library, reported as glibc heap corruption), and none of that
    // surface is what a headless UI test exercises. Behaves exactly like the
    // "no JACK server" path below.
    if std::env::var("SLOOPER_NULL_AUDIO").is_ok() {
        let _ = evt_tx.send(Evt::Unavailable("null audio (SLOOPER_NULL_AUDIO)".into()));
        return;
    }
    let (client, status) = match Client::new(CLIENT_NAME, ClientOptions::NO_START_SERVER) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "No JACK/PipeWire-JACK server available: {}",
                describe_err(e)
            );
            eprintln!("[slooper] {}", msg);
            let _ = evt_tx.send(Evt::Unavailable(msg));
            return;
        }
    };
    if status.contains(ClientStatus::NAME_NOT_UNIQUE) {
        eprintln!("[slooper] client renamed to '{}'", client.name());
    }

    // Register the fixed slot pool + MIDI port before activation. Pool ports
    // live for the whole client lifetime, so their raw pointers are stable and
    // can be cached in the realtime handler (no lookups, no allocation in RT).
    let mut ins_raw = [std::ptr::null_mut(); MAX_SLOTS];
    let mut outs_raw = [std::ptr::null_mut(); MAX_SLOTS];
    for i in 0..MAX_SLOTS {
        match client.register_port(&format!("in{i}"), AudioIn::default()) {
            Ok(p) => ins_raw[i] = p.raw(),
            Err(e) => {
                let _ = evt_tx.send(Evt::Error(format!(
                    "failed to register input slot in{i}: {}",
                    describe_err(e)
                )));
                return;
            }
        }
        match client.register_port(&format!("out{i}"), AudioOut::default()) {
            Ok(p) => outs_raw[i] = p.raw(),
            Err(e) => {
                let _ = evt_tx.send(Evt::Error(format!(
                    "failed to register output slot out{i}: {}",
                    describe_err(e)
                )));
                return;
            }
        }
    }

    let midi_port = match client.register_port(MIDI_PORT_SHORT, MidiIn::default()) {
        Ok(p) => p,
        Err(e) => {
            let _ = evt_tx.send(Evt::Error(format!(
                "failed to create MIDI input port: {}",
                describe_err(e)
            )));
            return;
        }
    };

    let mask_in = Arc::new(AtomicU64::new(0));
    // Output slots carry independent L / R bits (see `Cmd::Apply`).
    let mask_out_l = Arc::new(AtomicU64::new(0));
    let mask_out_r = Arc::new(AtomicU64::new(0));
    let notif = Notif {
        tx: evt_tx.clone(),
        xruns: xruns.clone(),
    };
    // Sample rate is queryable before activation; the metronome grid is
    // sized from it.
    let sr = client.sample_rate();
    engine.set_sample_rate(sr);
    let proc = Proc {
        midi_in: midi_port,
        ports: RawPorts {
            ins: ins_raw,
            outs: outs_raw,
        },
        mask_in: mask_in.clone(),
        mask_out_l: mask_out_l.clone(),
        mask_out_r: mask_out_r.clone(),
        tx: evt_tx.clone(),
        midi_drops,
        gain: gain.clone(),
        peak: peak.clone(),
        engine: engine.clone(),
        prev_rec: [TR_IDLE; MAX_LOOPS],
        rec_full: 0,
        pos: 0,
        prev_transport: false,
        prev_metro: false,
        sr,
        metro_idx: 0i64,
        metro_start: -1,
        metro_tone: 0,
        metro_clock: 0.0,
        sample_no: 0,
        fx_cache: engine.fx_table(),
        fx_cache_gen: engine.fx_gen(),
        // Seed from the engine's current chains: a launch that restores
        // chains from config never bumps fx_gen, so waiting for a refresh
        // here left an empty grid beside a non-empty cache → RT panic.
        fx_rt: seed_fx_rt(&engine.fx_table()[..]),
        arr: engine.arrangement(),
        arr_cache_gen: engine.arr_gen(),
        assets: engine.assets(),
        assets_cache_gen: engine.assets_gen(),
        preview: engine.preview_snapshot(),
        preview_cache_gen: engine.preview_gen(),
        preview_curs: Vec::new(),
        preview_live: false,
        clip_buf: Vec::new(),
        synths_cache: engine.synths(),
        synths_cache_gen: engine.synths_gen(),
        mix_l: Vec::new(),
        mix_r: Vec::new(),
        trk_bufs: vec![Vec::new(); MAX_LOOPS],
        mon_buf: Vec::new(),
        vst_tmp: Vec::new(),
        dflt_synth: engine.default_synth(),
        midi_cache: engine.midi(),
        midi_cache_gen: engine.midi_gen(),
        midi_ev: Vec::new(),
        // One row per track, EXACTLY like trk_bufs: `render_midi_block`
        // indexes `midi_scratch[t]` the moment the first note stages, and an
        // empty vector there aborted the RT thread (v0.6/v0.7 MIDI crash).
        midi_scratch: vec![Vec::new(); MAX_LOOPS],
        midi_live: [false; MAX_LOOPS],
        midi_ring_to: [0u64; MAX_LOOPS],
        midi_sustain: [false; MAX_LOOPS],
        midi_quiet: [0u32; MAX_LOOPS],
        synth_p: [0.0f32; crate::synth::MAX_PARAMS],
        synth_pg: [0u64; MAX_LOOPS],
        mono: Vec::new(),
        sil: Vec::new(),
    };

    let active = match client.activate_async(notif, proc) {
        Ok(a) => a,
        Err(e) => {
            let _ = evt_tx.send(Evt::Error(format!(
                "failed to activate JACK client: {}",
                describe_err(e)
            )));
            return;
        }
    };

    let client_name = active.as_client().name().to_string();
    let sample_rate = active.as_client().sample_rate();
    eprintln!(
        "[slooper] JACK client '{}' active @ {} Hz",
        client_name, sample_rate
    );
    let _ = evt_tx.send(Evt::Ready {
        client_name: client_name.clone(),
        sample_rate,
    });
    send_ports(active.as_client(), &evt_tx);

    // Remote MIDI sources currently wired into our midi_in port. Devices often
    // reappear on a fresh port name every connect, so this set is re-derived
    // periodically and on registration events (see rescan_midi_sources).
    let mut midi_sources: Vec<String> = Vec::new();

    // What each slot is currently connected to (manager-thread state only).
    let mut cur_in: Vec<Option<String>> = vec![None; MAX_SLOTS];
    let mut cur_out: Vec<Option<(String, u8)>> = vec![None; MAX_SLOTS];

    loop {
        match cmd_rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(Cmd::Refresh) => send_ports(active.as_client(), &evt_tx),
            Ok(Cmd::Apply { ins, outs }) => {
                apply_layout(
                    active.as_client(),
                    &ins,
                    &outs,
                    &mut cur_in,
                    &mut cur_out,
                    &mask_in,
                    &mask_out_l,
                    &mask_out_r,
                    &evt_tx,
                );
            }
            Ok(Cmd::ToggleRecord { i }) => {
                toggle_record(&engine, i.min(MAX_LOOPS - 1), &evt_tx);
            }
            Ok(Cmd::SetTrackOn { i, on }) => {
                engine.set_track_on(i.min(MAX_LOOPS - 1), on);
            }
            Ok(Cmd::ToggleTrackOn { i }) => {
                let i = i.min(MAX_LOOPS - 1);
                let on = !engine.track_on(i);
                engine.set_track_on(i, on);
            }
            Ok(Cmd::PlayAll) => {
                // Playing at (or past) the end starts over from the top.
                if engine.pos() >= engine.proj_len() {
                    engine.seek_to(0);
                }
                // v0.10: starting the transport clears pending auditions.
                // A leftover rack preview kept playing OVER the real
                // playback — the "something else is processing my audio"
                // double-voice. Auditions belong to browsing, not mixing.
                engine.stop_preview();
                engine.set_transport(true);
                let _ = evt_tx.try_send(Evt::Transport { on: true });
            }
            Ok(Cmd::StopAll) => {
                engine.stop_preview();
                engine.set_transport(false);
                let _ = evt_tx.try_send(Evt::Transport { on: false });
            }
            Ok(Cmd::Seek { pos }) => engine.seek_to(pos),
            Ok(Cmd::SetLoop(on)) => engine.set_loop_tl(on),
            Ok(Cmd::SetMonitor(on)) => engine.set_monitor_on(on),
            Ok(Cmd::SetLoopRegion { start, end }) => engine.set_loop_region(start, end),
            Ok(Cmd::SetProject { assets, arr, len }) => {
                engine.set_project(assets, arr, len);
            }
            Ok(Cmd::SetMidiAssets(midi)) => engine.set_midi_assets(midi),
            Ok(Cmd::SetTrackSynth { i, synth }) => {
                engine.set_track_synth(i.min(MAX_LOOPS - 1), synth)
            }
            Ok(Cmd::SetDefaultSynth(s)) => engine.set_default_synth(s),
            Ok(Cmd::ExportTrack { i, path, fmt }) => {
                let i = i.min(MAX_LOOPS - 1);
                match engine.render_track(i) {
                    Some(samples) => {
                        match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                            Ok(()) => {
                                let _ = evt_tx.send(Evt::Notice(format!(
                                    "exported track {} ({} samples) → {}",
                                    i + 1,
                                    samples.len(),
                                    path.display()
                                )));
                            }
                            Err(e) => {
                                let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                            }
                        }
                    }
                    None => {
                        let _ = evt_tx.send(Evt::Error(format!(
                            "export: track {} has no placed clips",
                            i + 1
                        )));
                    }
                }
            }
            Ok(Cmd::ExportProject { path, fmt }) => match engine.render_project() {
                Some(samples) => match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                    Ok(()) => {
                        let _ = evt_tx.send(Evt::Notice(format!(
                            "exported project ({} samples) → {}",
                            samples.len(),
                            path.display()
                        )));
                    }
                    Err(e) => {
                        let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                    }
                },
                None => {
                    let _ = evt_tx.send(Evt::Error("export: no clips placed in any track".into()));
                }
            },
            Ok(Cmd::ExportSelection { path, fmt, items }) => {
                match engine.render_selection(&items) {
                    Some((_from, samples)) => {
                        match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                            Ok(()) => {
                                let _ = evt_tx.send(Evt::Notice(format!(
                                    "exported selection of {} clip(s) ({} samples) → {}",
                                    items.len(),
                                    samples.len(),
                                    path.display()
                                )));
                            }
                            Err(e) => {
                                let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                            }
                        }
                    }
                    None => {
                        let _ = evt_tx.send(Evt::Error(
                            "export: nothing to export (empty selection?)".into(),
                        ));
                    }
                }
            }
            Ok(Cmd::SetTrackEffects { i, chain }) => {
                engine.set_track_fx(i.min(MAX_LOOPS - 1), chain);
            }
            Ok(Cmd::SetSelected(i)) => {
                engine
                    .selected
                    .store(i.min(MAX_LOOPS - 1) as u8, Ordering::Relaxed);
            }
            Ok(Cmd::SetLatency { ms }) => {
                let ms = ms.clamp(0.0, 1000.0);
                engine.latency.store(
                    (ms * sample_rate as f32 / 1000.0).round() as u32,
                    Ordering::Relaxed,
                );
            }
            Ok(Cmd::SetMetro { on, bpm, sub, num }) => {
                engine.metro_on.store(on as u8, Ordering::Relaxed);
                engine
                    .metro_bpm
                    .store(bpm.clamp(20, 300), Ordering::Relaxed);
                engine.metro_sub.store(sub.clamp(1, 16), Ordering::Relaxed);
                engine.metro_num.store(num.clamp(1, 16), Ordering::Relaxed);
            }
            Ok(Cmd::PreviewPlay { id, samples }) => engine.play_preview(id, samples),
            Ok(Cmd::PreviewStop { id: None }) => engine.stop_preview(),
            Ok(Cmd::PreviewStop { id: Some(id) }) => engine.stop_preview_id(id),
            Ok(Cmd::Quit) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Takes the RT handler auto-stopped at buffer capacity never
                // saw a stop command, so they wait in the engine until the
                // manager claims them here.
                let mut pend = engine.claim_takes();
                while pend != 0 {
                    let i = pend.trailing_zeros() as usize;
                    pend &= !(1u128 << i);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    let (at, data) = engine.copy_take(i);
                    if !data.is_empty() {
                        let _ = evt_tx.try_send(Evt::Take { track: i, at, data });
                    }
                }
                // No command pending: re-derive the MIDI source set. This is
                // what makes bindings work when a device (re)appears on a new
                // port name — e.g. hardware that gets a fresh ALSA/JACK port
                // number on every connect instead of a fixed one.
                rescan_midi_sources(active.as_client(), &evt_tx, &mut midi_sources);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Teardown: null out every callback before the handler context is freed.
    // The jack crate's deactivate only clears thread-init/process, so without
    // this a late notification would call into freed memory on PipeWire-JACK.
    unsafe { clear_all_callbacks(active.as_client().raw()) };
    // Nulling the pointers does NOT cancel an invocation PipeWire-JACK has
    // already dispatched: its thread may have loaded the callback + context
    // pointer microseconds ago. Closing the client right now frees the jack
    // crate's Arc'd context underneath that in-flight call — a use-after-free
    // that glibc reports (in whichever thread frees first) as "corrupted size
    // vs. prev_size" and aborts. Let a dozen audio cycles elapse so every
    // dispatched callback runs against still-live state, then close.
    std::thread::sleep(std::time::Duration::from_millis(250));
    drop(active); // deactivates + closes the client; no live callbacks remain.
}

/// Connect with retries: PipeWire-JACK does not accept connections on a
/// freshly-activated client's ports until its activation has settled into the
/// graph, so a just-started remote client needs a few cycles before its ports
/// can be wired up. Runs on the manager thread (sleeping is fine).
fn connect_retry(client: &Client, src: &str, dst: &str) -> Result<(), jack::Error> {
    let mut last_err = None;
    for _ in 0..10 {
        match client.connect_ports_by_name(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// Connect / disconnect external ports against the fixed slot pool, updating
/// the RT-visible masks. Runs on the manager thread (may block/allocate).
#[allow(clippy::too_many_arguments)] // one arg per slot-pool state piece; a wrapper struct is pure ceremony
fn apply_layout(
    client: &Client,
    ins: &[String],
    outs: &[(String, u8)],
    cur_in: &mut [Option<String>],
    cur_out: &mut [Option<(String, u8)>],
    mask_in: &AtomicU64,
    mask_out_l: &AtomicU64,
    mask_out_r: &AtomicU64,
    tx: &SyncSender<Evt>,
) {
    if ins.len() > MAX_SLOTS || outs.len() > MAX_SLOTS {
        let _ = tx.try_send(Evt::Error(format!(
            "too many ports selected: max {MAX_SLOTS} inputs and {MAX_SLOTS} outputs"
        )));
    }

    let mut applied_in = 0usize;
    for (i, slot) in cur_in.iter_mut().enumerate() {
        let new_in = ins.get(i).cloned();
        if *slot == new_in {
            continue; // unchanged: keep the existing connection
        }
        let pool_name = format!("{}:in{i}", client.name());
        if slot.take().is_some() {
            // Stop reading before disconnecting, so RT never reads a slot that
            // is mid-teardown (unconnected input buffers hold stale data).
            mask_in.fetch_and(!(1u64 << i), Ordering::Relaxed);
            if let Some(pool) = client.port_by_name(&pool_name) {
                if let Err(e) = client.disconnect(&pool) {
                    let _ = tx.try_send(Evt::Error(format!(
                        "could not disconnect input slot in{i}: {}",
                        describe_err(e)
                    )));
                }
            }
        }
        if let Some(src) = new_in {
            match connect_retry(client, &src, &pool_name) {
                Ok(()) => {
                    // Only start reading once the connection exists.
                    mask_in.fetch_or(1u64 << i, Ordering::Relaxed);
                    *slot = Some(src);
                    applied_in += 1;
                }
                Err(e) => {
                    let msg = format!(
                        "could not connect input {src} -> {pool_name}: {}",
                        describe_err(e)
                    );
                    eprintln!("[slooper] {msg}");
                    let _ = tx.try_send(Evt::Error(msg));
                }
            }
        }
    }

    let mut applied_out = 0usize;
    for (i, slot) in cur_out.iter_mut().enumerate() {
        // Route bits: 1 = L bus, 2 = R bus; 0 counts as both (mono downmix
        // — the historical behaviour, and the default for fresh selections).
        let new_out = outs
            .get(i)
            .map(|(n, lr)| (n.clone(), if lr & 3 == 0 { 3 } else { lr & 3 }));
        let same_name = slot.as_ref().map(|s| &s.0) == new_out.as_ref().map(|s| &s.0);
        if same_name && slot.is_some() {
            // Still connected to the same port: only the L/R routing can
            // have moved — retarget the masks without touching the link.
            if let Some((_, lr)) = &new_out {
                if slot.as_ref().map(|s| s.1) != Some(*lr) {
                    mask_out_l.fetch_and(!(1u64 << i), Ordering::Relaxed);
                    mask_out_r.fetch_and(!(1u64 << i), Ordering::Relaxed);
                    if lr & 1 != 0 {
                        mask_out_l.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    if lr & 2 != 0 {
                        mask_out_r.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    *slot = new_out;
                }
            }
            continue;
        }
        if *slot == new_out {
            continue;
        }
        let pool_name = format!("{}:out{i}", client.name());
        if slot.take().is_some() {
            // Keep writing until the disconnect completes, then stop.
            if let Some(pool) = client.port_by_name(&pool_name) {
                if let Err(e) = client.disconnect(&pool) {
                    let _ = tx.try_send(Evt::Error(format!(
                        "could not disconnect output slot out{i}: {}",
                        describe_err(e)
                    )));
                }
            }
            mask_out_l.fetch_and(!(1u64 << i), Ordering::Relaxed);
            mask_out_r.fetch_and(!(1u64 << i), Ordering::Relaxed);
        }
        if let Some((dst, lr)) = new_out {
            match connect_retry(client, &pool_name, &dst) {
                Ok(()) => {
                    if lr & 1 != 0 {
                        mask_out_l.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    if lr & 2 != 0 {
                        mask_out_r.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    *slot = Some((dst, lr));
                    applied_out += 1;
                }
                Err(e) => {
                    let _ = tx.try_send(Evt::Error(format!(
                        "could not connect output {pool_name} -> {dst}: {}",
                        describe_err(e)
                    )));
                }
            }
        }
    }

    eprintln!("[slooper] applied {applied_in} input(s), {applied_out} output(s)");
    let _ = tx.send(Evt::Applied {
        ins: applied_in,
        outs: applied_out,
    });
}

/// Enumerate remote audio ports, hiding those whose client belongs to a
/// disabled device (see [`crate::ports`]), and push them to the GUI.
fn send_ports(client: &Client, tx: &SyncSender<Evt>) {
    let (ins, outs) = enumerate_visible_ports(client);
    let _ = tx.send(Evt::Ports { ins, outs });
}

/// Like `send_ports`, but non-blocking (for JACK's notification thread).
fn send_ports_try(client: &Client, tx: &SyncSender<Evt>) {
    let (ins, outs) = enumerate_visible_ports(client);
    let _ = tx.try_send(Evt::Ports { ins, outs });
}

/// Re-derive which remote MIDI source ports feed `slooper:midi_in` and
/// make the actual connections. Devices often get a brand-new port name on
/// every connect (and may use any channel), so a single manually-made link
/// is not enough: we wire *every* remote MIDI source port into our input,
/// drop links whose source vanished, and report the live set to the GUI.
/// Runs on the manager thread only (connects may block). Returns true if the
/// connected set changed.
fn rescan_midi_sources(client: &Client, tx: &SyncSender<Evt>, known: &mut Vec<String>) -> bool {
    let mine = client.name().to_string();
    let prefix = format!("{mine}:");
    let dst = format!("{prefix}midi_in");

    // Remote MIDI source ports (any client, any channel — channels are
    // irrelevant for transport-level binding; the device picks its own).
    let found: Vec<String> = client
        .ports(None, Some("midi"), PortFlags::IS_OUTPUT)
        .into_iter()
        .filter(|n| !n.starts_with(&prefix))
        .collect();

    // Nothing to do if the set is unchanged (connect_retry would still work,
    // but skipping avoids pointless connect calls every 250 ms).
    if found == *known {
        return false;
    }

    // Drop links whose source port no longer exists.
    for name in known.iter() {
        if !found.contains(name) {
            let _ = client.disconnect_ports_by_name(name, &dst);
        }
    }
    // (Re)connect everything currently present. connect_retry absorbs the
    // PipeWire-JACK "freshly activated" delay for ports that just appeared.
    for name in found.iter() {
        if let Err(e) = connect_retry(client, name, &dst) {
            eprintln!("[slooper] MIDI link {name} → {dst}: {e}");
        }
    }
    *known = found.clone();
    eprintln!(
        "[slooper] MIDI sources: {}",
        if known.is_empty() {
            "none".into()
        } else {
            known.join(", ")
        }
    );
    let _ = tx.try_send(Evt::MidiSources(known.clone()));
    true
}

/// Shared port enumeration: candidates = other clients' audio ports, minus
/// any client that belongs to a disabled device.
fn enumerate_visible_ports(client: &Client) -> (Vec<PortInfo>, Vec<PortInfo>) {
    let mine = client.name().to_string();
    let prefix = format!("{mine}:");
    let hidden = hidden_clients_cached();

    // Candidate inputs = other clients' OUTPUT (source) audio ports.
    let ins: Vec<PortInfo> = client
        .ports(None, Some("audio"), PortFlags::IS_OUTPUT)
        .into_iter()
        .filter(|n| !n.starts_with(&prefix))
        .filter(|n| port_visible(n, hidden.as_ref()))
        .map(|name| port_info(&name))
        .collect();

    // Candidate outputs = other clients' INPUT (sink) audio ports.
    let outs: Vec<PortInfo> = client
        .ports(None, Some("audio"), PortFlags::IS_INPUT)
        .into_iter()
        .filter(|n| !n.starts_with(&prefix))
        .filter(|n| port_visible(n, hidden.as_ref()))
        .map(|name| port_info(&name))
        .collect();

    (ins, outs)
}

/// True when a full port name (`client:port`) should be shown.
fn port_visible(full_name: &str, hidden: Option<&std::collections::HashSet<String>>) -> bool {
    match hidden {
        Some(set) => !set.contains(full_name.split(':').next().unwrap_or("")),
        None => true, // can't determine device state → show everything
    }
}

/// Cached "hidden clients" set. The pactl/pw-cli lookups cost a few process
/// spawns, so the result is cached for two seconds (device state rarely
/// changes, and port lists refresh on registration events anyway).
fn hidden_clients_cached() -> Option<std::collections::HashSet<String>> {
    use std::time::{Duration, Instant};
    static CACHE: std::sync::Mutex<Option<(Instant, Option<std::collections::HashSet<String>>)>> =
        std::sync::Mutex::new(None);

    // Fresh enough? (The lock is released at the end of this statement.)
    if let Some((t, set)) = CACHE.lock().unwrap().as_ref() {
        if t.elapsed() < Duration::from_secs(2) {
            return set.clone();
        }
    }

    // Compute outside the lock (it spawns processes), then store.
    let fresh = crate::ports::hidden_clients();
    *CACHE.lock().unwrap() = Some((Instant::now(), fresh.clone()));
    fresh
}

fn port_info(full_name: &str) -> PortInfo {
    let client = full_name.split(':').next().unwrap_or("").to_string();
    PortInfo {
        name: full_name.to_string(),
        client,
    }
}

/// Clamp a loop-region request to the live project: `ls` never past the
/// end, and a degenerate `le` (at/behind `ls`, past the end, or 0 = "whole
/// song") falls back to the project end. Pure so it is unit-testable.
fn clamp_loop_region(ls: u64, le: u64, proj_len: u64) -> (u64, u64) {
    let ls = ls.min(proj_len.saturating_sub(1));
    let le = if le <= ls || le > proj_len {
        proj_len
    } else {
        le
    };
    (ls, le)
}

/// Region wrap: `Some(ls)` when `pos` reached the region end (bounce back
/// to the start), `None` to keep rolling. Pure so it is unit-testable.
fn wrap_pos(pos: u64, ls: u64, le: u64) -> Option<u64> {
    (pos >= le).then_some(ls)
}

/// Notification handler: keeps the GUI port list live and counts xruns.
struct Notif {
    tx: SyncSender<Evt>,
    xruns: Arc<AtomicU64>,
}

impl NotificationHandler for Notif {
    fn port_registration(&mut self, client: &Client, _port_id: PortId, _is_registered: bool) {
        // Remote ports appeared/disappeared — refresh the GUI list.
        // try_send: this runs on JACK's client thread; a full list is carried in
        // every Ports event, so dropping stale ones while the GUI lags is safe.
        send_ports_try(client, &self.tx);
    }

    fn client_registration(&mut self, client: &Client, _name: &str, _is_registered: bool) {
        send_ports_try(client, &self.tx);
    }

    fn xrun(&mut self, _client: &Client) -> Control {
        self.xruns.fetch_add(1, Ordering::Relaxed);
        // try_send: never block JACK's client thread on a full channel.
        let _ = self.tx.try_send(Evt::Xrun);
        Control::Continue
    }
}

/// Raw JACK port pointers of the fixed slot pool. The pool ports are never
/// unregistered, so the pointers stay valid for the whole client lifetime;
/// `Send` is safe to assert (they are only ever read from the RT thread).
struct RawPorts {
    ins: [*mut j::jack_port_t; MAX_SLOTS],
    outs: [*mut j::jack_port_t; MAX_SLOTS],
}

unsafe impl Send for RawPorts {}

/// One block's worth of live clips: a raw pointer to the asset data offset to
/// the window start, and the window `[i0, i1)` of block-relative sample
/// indices the clip covers. Rebuilt per block (cheap — tens of clips); the
/// sample loop then does zero arithmetic per clip beyond `ptr.add(i - i0)`.
struct ClipEntry {
    track: u8,
    ptr: *const f32,
    i0: usize,
    i1: usize,
    /// Per-clip gain (linear) and a piecewise fade envelope evaluated at
    /// ABSOLUTE timeline samples. No fade: `fi_end = 0` and
    /// `fo_start = u64::MAX` so both window tests miss for every sample.
    gain: f32,
    pos: u64,
    fi_end: u64,
    fi_step: f32,
    fo_start: u64,
    fo_end: u64,
    fo_step: f32,
}

unsafe impl Send for ClipEntry {}

/// Stage one MIDI pattern clip's events into a shared list, keyed by
/// ABSOLUTE timeline sample (kind: 0=note-off 1=CC 2=pressure 3=note-on).
/// Shared by the offline renders; the RT path stages per block inline.
/// Pass a render chunk through a track's private VST copy (offline export);
/// a no-op when the track has no insert. Fresh per clip, mirroring the
/// scripted-FX export rule (every clip starts at phase 0; the live chain is
/// never advanced).
fn vst_chunk(e: Option<&crate::vsthost::RtStageEntry>, chunk: &mut [f32]) {
    if let Some(e) = e {
        let src = chunk.to_vec();
        e.process(&src, chunk);
    }
}

/// Reopen a *private* copy of a loaded plugin for offline rendering: same
/// path/kind, current slider values. The live entry is never touched (the
/// audio thread owns it), so exports and the running mix cannot interfere.
fn spawn_entry_private(
    e: &crate::vsthost::RtStageEntry,
    sr: u32,
) -> Option<Arc<crate::vsthost::RtStageEntry>> {
    let (path, kind) = e.spec().clone();
    let params: Vec<f32> = (0..e.params().len()).map(|k| e.params().get(k)).collect();
    crate::vsthost::open(&path, kind, sr.max(1), Some(&params))
        .ok()
        .map(|l| l.entry)
}

/// Fresh scripted instances for a chain (offline renders), or `None` when
/// the chain has no scripted slot at all (cheap passthrough for callers).
fn scripted_chain(chain: &TrackFx) -> Option<Vec<Arc<crate::fx::Instance>>> {
    let v: Vec<Arc<crate::fx::Instance>> = chain
        .slots()
        .iter()
        .filter_map(|s| match &s.kind {
            FxKind::Scripted { inst, desc } => inst.spawn_fresh(desc),
            FxKind::Vst(_) => None,
        })
        .collect();
    (!v.is_empty()).then_some(v)
}

/// Run the whole chain (scripted + plugins, in order) over a full render
/// buffer using fresh private copies. Plugin slots consume the buffer
/// block-wise; scripted slots run sample-wise like the live path.
fn run_chain_over(chain: &TrackFx, sr: u32, buf: &mut [f32]) {
    for slot in chain.slots() {
        match &slot.kind {
            FxKind::Scripted { inst, desc } => {
                if let Some(f) = inst.spawn_fresh(desc) {
                    for x in buf.iter_mut() {
                        *x = f.process_private(*x);
                    }
                }
            }
            FxKind::Vst(e) => {
                if let Some(pv) = spawn_entry_private(e, sr) {
                    vst_chunk(Some(pv.as_ref()), buf);
                }
            }
        }
    }
}

/// Render staged MIDI events through either synth kind. VSTi timing lands
/// at internal-block boundaries (≤ `VST_BLOCK` samples late, uniformly —
/// the same contract as the live path).
fn render_midi_any(
    h: &SynthHandle,
    ev: &mut [(u64, u8, u8, u8, u8)],
    from: u64,
    len: usize,
    out: &mut [f32],
    sr: u32,
) {
    match h {
        SynthHandle::Script(inst) => {
            if let Some(fresh) = inst.spawn_fresh() {
                render_midi_events(&fresh, ev, from, len, out);
            }
        }
        SynthHandle::Vst(e) => {
            if let Some(pv) = spawn_entry_private(e, sr) {
                render_midi_vst(&pv, ev, from, len, out);
            }
        }
    }
}

fn render_midi_vst(
    e: &crate::vsthost::RtStageEntry,
    ev: &[(u64, u8, u8, u8, u8)],
    from: u64,
    len: usize,
    out: &mut [f32],
) {
    let mut ev: Vec<(usize, u8, u8, u8, u8)> = ev
        .iter()
        .filter(|(at, ..)| *at >= from && *at - from < len as u64)
        .map(|(at, k, c, d1, d2)| ((*at - from) as usize, *k, *c, *d1, *d2))
        .collect();
    ev.sort_unstable();
    let sil = vec![0.0f32; crate::vsthost::VST_BLOCK];
    let process_seg = |a: usize, b: usize, out: &mut [f32]| {
        let mut a = a;
        while a < b {
            let n = (b - a).min(crate::vsthost::VST_BLOCK).min(out.len() - a);
            if n == 0 {
                break;
            }
            e.process(&sil[..n], &mut out[a..a + n]);
            a += n;
        }
    };
    let mut cur = 0usize;
    let mut i = 0usize;
    while i < ev.len() {
        let off = ev[i].0.min(len).min(out.len());
        if off > cur {
            process_seg(cur, off, out);
            cur = off;
        }
        while i < ev.len() && ev[i].0.min(len).min(out.len()) == off {
            let (_, kind, ch, d1, d2) = ev[i];
            match kind {
                0 => e.note_off(ch, d1),
                1 => e.cc(ch, d1, d2),
                _ => e.note_on(ch, d1, d2),
            }
            i += 1;
        }
    }
    if cur < out.len() {
        process_seg(cur, out.len(), out);
    }
    e.all_notes_off();
}

fn stage_midi_events(
    pat: &crate::midipat::Pattern,
    cl: &crate::arrange::Clip,
    sr: u32,
    ev: &mut Vec<(u64, u8, u8, u8, u8)>,
) {
    let avail = pat.len_samples(sr);
    let span = cl.span(avail);
    let base = cl.pos.saturating_sub(cl.cut);
    for note in pat.notes.iter() {
        let s0 = pat.ticks_to_samples(note.start, sr);
        if s0 < cl.cut || s0 - cl.cut >= span {
            continue;
        }
        let on = base + s0;
        let vel = (note.vel as f32 * cl.gain * cl.fade_at(on, avail))
            .clamp(0.0, 127.0)
            .round() as u8;
        if vel == 0 {
            continue;
        }
        ev.push((on, 3, note.ch, note.note, vel));
        let len_s = pat.ticks_to_samples(note.len, sr).max(1);
        ev.push((on + len_s, 0, note.ch, note.note, 0));
    }
    for m in pat.ccs.iter() {
        let s = pat.ticks_to_samples(m.at, sr);
        if s < cl.cut || s - cl.cut >= span {
            continue;
        }
        ev.push((base + s, 1, m.ch, m.num, m.val));
    }
    for m in pat.press.iter() {
        let s = pat.ticks_to_samples(m.at, sr);
        if s < cl.cut || s - cl.cut >= span {
            continue;
        }
        ev.push((base + s, 2, m.ch, 0, m.val));
    }
}

/// Render staged absolute-sample events through `inst` into `len` samples
/// of `out` (starting at timeline `from`). Events outside the window are
/// ignored; notes ringing at `from` simply start ringing at sample 0 of
/// this bounce (offline bounces are stateless).
pub fn render_midi_events(
    inst: &crate::synth::Instance,
    ev: &mut [(u64, u8, u8, u8, u8)],
    from: u64,
    len: usize,
    out: &mut [f32],
) {
    let mut p: [f32; crate::synth::MAX_PARAMS] = [0.0; crate::synth::MAX_PARAMS];
    inst.read_params(&mut p);
    // Prime before events (see render_midi_block): a note at offset 0 must
    // not be delivered onto a script instrument that has never processed.
    unsafe { inst.process_into(&mut out[..0], &p) };
    let mut ev: Vec<(usize, u8, u8, u8, u8)> = ev
        .iter()
        .filter(|(at, ..)| *at >= from && *at - from < len as u64)
        .map(|(at, k, c, d1, d2)| ((*at - from) as usize, *k, *c, *d1, *d2))
        .collect();
    // Note-offs (kind 0) before note-ons (kind 3) at one sample.
    ev.sort_unstable();
    let mut cur = 0usize;
    let mut i = 0usize;
    while i < ev.len() {
        let off = ev[i].0.min(len);
        if off > cur {
            let end = off.min(out.len());
            if end > cur {
                unsafe { inst.process_into(&mut out[cur..end], &p) };
                cur = end;
            }
        }
        while i < ev.len() && ev[i].0 == off {
            let (_, kind, ch, d1, d2) = ev[i];
            match kind {
                0 => inst.note_off(ch, d1),
                1 => inst.cc(ch, d1, d2),
                2 => inst.press(ch, d2),
                _ => inst.note_on(ch, d1, d2),
            }
            i += 1;
        }
    }
    if cur < out.len() {
        unsafe { inst.process_into(&mut out[cur..], &p) };
    }
    inst.all_off();
}

/// Peak level below which a rendered MIDI block counts as silent
/// (~ -80 dBFS): only such blocks advance the synth's stop-the-clock timer.
const MIDI_SILENCE_FLOOR: f32 = 1e-4;
/// Consecutive silent blocks (nothing scheduled) after which a track's
/// synth stops advancing — a few tens of ms, far below any real tail but
/// long enough for the piano's ~70 ms damper release.
const MIDI_QUIET_BLOCKS: u32 = 6;

impl Proc {
    /// Render every MIDI pattern clip sounding this block through each
    /// track's synth into `midi_scratch[t]` (mono). Notes are triggered by
    /// the pattern; a note whose head was trimmed away (`cut`) stays silent,
    /// one already ringing keeps ringing inside the synth until its off
    /// event — contiguous blocks make that seamless, and a transport seek
    /// `all_off`s everything. A track whose schedule is still ringing (or
    /// whose tail has not decayed yet) keeps ADVANCING its synth even on
    /// event-free blocks: the sound lives inside the instrument, and
    /// pausing the synth between events turned every sustained note into a
    /// one-block blip. Returns whether any track produced sound.
    fn render_midi_block(&mut self, p0: u64, block_n: usize) -> bool {
        // Clear last block's scratch first (only rows that actually sounded).
        for (t, live) in self.midi_live.iter_mut().enumerate() {
            if *live {
                self.midi_scratch[t].iter_mut().for_each(|x| *x = 0.0);
                *live = false;
            }
        }
        if !self.engine.transport_on() || self.midi_cache.is_empty() || block_n == 0 {
            // Transport stopped (or the pattern table went away) while
            // something was sounding: silence every voice once so nothing
            // leaks into the next run, and drop all sustain bookkeeping.
            if self.midi_sustain.iter().any(|s| *s) || self.midi_ring_to.iter().any(|r| *r != 0)
            {
                for h in self.synths_cache.iter().flatten() {
                    h.all_off();
                }
                if let Some(d) = &self.dflt_synth {
                    d.all_off();
                }
            }
            for t in 0..MAX_LOOPS {
                self.midi_sustain[t] = false;
                self.midi_quiet[t] = 0;
                self.midi_ring_to[t] = 0;
            }
            return false;
        }
        let sr = self.sr.max(1);
        let span_end = p0 + block_n as u64;
        let arr = self.arr.clone();
        let dflt = self.dflt_synth.clone();
        let engine = &self.engine;
        let mut any = false;
        let Proc {
            midi_ev,
            mono,
            midi_scratch,
            midi_live,
            midi_ring_to,
            midi_sustain,
            midi_quiet,
            synth_p,
            synth_pg,
            synths_cache,
            midi_cache,
            sil,
            ..
        } = self;
        for t in 0..MAX_LOOPS {
            if !engine.track_on(t) {
                continue;
            }
            let handle = match synths_cache.get(t).cloned().flatten().or_else(|| dflt.clone()) {
                Some(i) => i,
                None => continue, // no synth loaded at all: silence
            };
            midi_ev.clear();
            // Ring horizon: the latest sample this track must keep the
            // synth advancing to (updated from every note staged here).
            let mut ring = midi_ring_to[t];
            for l in arr.tracks[t].lanes.iter() {
                for cl in l.clips.iter() {
                    let Some(Some(pat)) = midi_cache.get(cl.asset as usize) else {
                        continue;
                    };
                    let avail = pat.len_samples(sr);
                    let span = cl.span(avail);
                    // Clip timeline window vs this block.
                    if cl.end(avail) <= p0 || cl.pos >= span_end {
                        continue;
                    }
                    let base = cl.pos.saturating_sub(cl.cut);
                    for note in pat.notes.iter() {
                        let s0 = pat.ticks_to_samples(note.start, sr);
                        if s0 < cl.cut || s0 - cl.cut >= span {
                            continue; // head trimmed away or past clip end
                        }
                        let on = base + s0;
                        let len_s = pat.ticks_to_samples(note.len, sr).max(1);
                        if on >= p0 && on < span_end {
                            let vel = (note.vel as f32 * cl.gain * cl.fade_at(on, avail))
                                .clamp(0.0, 127.0)
                                .round() as u8;
                            if vel > 0 {
                                midi_ev.push(((on - p0) as usize, 3, note.ch, note.note, vel));
                            }
                            let off_tl = on + len_s;
                            if off_tl >= p0 && off_tl < span_end {
                                midi_ev.push(((off_tl - p0) as usize, 0, note.ch, note.note, 0));
                            }
                            ring = ring.max(off_tl);
                        } else if on < p0 && on + len_s > p0 && on + len_s <= span_end {
                            // Rang in earlier; its off lands in this block.
                            midi_ev.push(((on + len_s - p0) as usize, 0, note.ch, note.note, 0));
                            ring = ring.max(on + len_s);
                        }
                    }
                    for m in pat.ccs.iter() {
                        let s = pat.ticks_to_samples(m.at, sr);
                        if s < cl.cut || s - cl.cut >= span {
                            continue;
                        }
                        let at = base + s;
                        if at >= p0 && at < span_end {
                            midi_ev.push(((at - p0) as usize, 1, m.ch, m.num, m.val));
                        }
                    }
                    for m in pat.press.iter() {
                        let s = pat.ticks_to_samples(m.at, sr);
                        if s < cl.cut || s - cl.cut >= span {
                            continue;
                        }
                        let at = base + s;
                        if at >= p0 && at < span_end {
                            midi_ev.push(((at - p0) as usize, 2, m.ch, 0, m.val));
                        }
                    }
                }
            }
            // Render this block when it carries staged events OR when the
            // track is still sustaining (a scheduled note rings past this
            // block, or the synth's tail has not decayed yet). Pausing the
            // instrument on event-free blocks is what made every note a
            // one-block blip — the sound between events lives INSIDE the
            // synth (script voices / the plugin's own ring).
            let has_ev = !midi_ev.is_empty();
            if !has_ev && !midi_sustain[t] {
                continue;
            }
            let row = &mut midi_scratch[t];
            if row.len() < block_n {
                row.clear();
                row.resize(block_n, 0.0);
            }
            // (offset, kind) ordering puts offs (kind 0) before ons (kind 3)
            // at the same sample, so a retrigger never kills its own note.
            midi_ev.sort_unstable();
            match handle.as_ref() {
                SynthHandle::Script(inst) => {
                    // Sliders: only refetch when the GUI actually moved one.
                    let gen = inst.param_gen();
                    if gen != synth_pg[t] {
                        inst.read_params(synth_p);
                        synth_pg[t] = gen;
                    }
                    // Prime the instrument with a zero-length block before
                    // delivering any event. A sampler syncs its asset buffer
                    // (and root param) inside process(), so a note staged at
                    // sample 0 delivered FIRST would find an empty buffer
                    // and vanish — the sampler "needs a synth re-selection
                    // to work" bug. One call, no audio, exact params.
                    if has_ev {
                        unsafe { inst.process_into(&mut [], synth_p) };
                    }
                    mono.clear();
                    mono.resize(block_n, 0.0);
                    let mut cur = 0usize;
                    let mut i = 0usize;
                    while i < midi_ev.len() {
                        let off = midi_ev[i].0.min(block_n);
                        if off > cur {
                            // SAFETY: single-threaded here; one instance, one call.
                            unsafe { inst.process_into(&mut mono[cur..off], synth_p) };
                            cur = off;
                        }
                        while i < midi_ev.len() && midi_ev[i].0 == off {
                            let (_, kind, ch, d1, d2) = midi_ev[i];
                            match kind {
                                0 => inst.note_off(ch, d1),
                                1 => inst.cc(ch, d1, d2),
                                2 => inst.press(ch, d2),
                                _ => inst.note_on(ch, d1, d2),
                            }
                            i += 1;
                        }
                    }
                    if cur < block_n {
                        unsafe { inst.process_into(&mut mono[cur..block_n], synth_p) };
                    }
                    for (r, m) in row[..block_n].iter_mut().zip(mono[..block_n].iter()) {
                        *r += *m;
                    }
                }
                SynthHandle::Vst(e) => {
                    // A VSTi generates from events: feed each event, drive
                    // the plugin over silence in between, straight into the
                    // row. Events land at internal-block boundaries (the
                    // host's uniform ≤ VST_BLOCK staging delay).
                    sil.clear();
                    sil.resize(block_n, 0.0);
                    let mut cur = 0usize;
                    let mut i = 0usize;
                    while i < midi_ev.len() {
                        let off = midi_ev[i].0.min(block_n);
                        if off > cur {
                            e.process(&sil[cur..off], &mut row[cur..off]);
                            cur = off;
                        }
                        while i < midi_ev.len() && midi_ev[i].0.min(block_n) == off {
                            let (_, kind, ch, d1, d2) = midi_ev[i];
                            match kind {
                                0 => e.note_off(ch, d1),
                                1 => e.cc(ch, d1, d2),
                                2 => e.press(ch, d2),
                                _ => e.note_on(ch, d1, d2),
                            }
                            i += 1;
                        }
                    }
                    if cur < block_n {
                        e.process(&sil[cur..block_n], &mut row[cur..block_n]);
                    }
                }
            }
            midi_live[t] = true;
            any = true;
            // Sustain bookkeeping. Keep advancing while the schedule rings
            // past this block; once nothing is scheduled, stop only after a
            // few near-silent rendered blocks, so the instrument's release
            // tail plays out fully but a dead track costs no CPU.
            midi_ring_to[t] = ring;
            if has_ev {
                midi_sustain[t] = true;
                midi_quiet[t] = 0;
            }
            if ring <= span_end {
                let peak = row[..block_n].iter().fold(0.0f32, |m, x| m.max(x.abs()));
                if peak < MIDI_SILENCE_FLOOR {
                    midi_quiet[t] = midi_quiet[t].saturating_add(1);
                    if midi_quiet[t] >= MIDI_QUIET_BLOCKS {
                        midi_sustain[t] = false;
                        midi_quiet[t] = 0;
                    }
                } else {
                    midi_quiet[t] = 0;
                }
            }
        }
        any
    }
}

/// Realtime process handler. No allocation, no locks, no I/O: the slot pool
/// is fixed at startup and its raw port pointers are cached here; per-cycle
/// work is a handful of atomic loads plus one pass over the arrangement's
/// clips to build this block's entry list. The global transport advances the
/// playhead (or follows seeks), clips are summed where the playhead crosses
/// them, and takes capture raw input inline while armed.
struct Proc {
    midi_in: Port<MidiIn>,
    ports: RawPorts,
    mask_in: Arc<AtomicU64>,
    mask_out_l: Arc<AtomicU64>,
    mask_out_r: Arc<AtomicU64>,
    tx: SyncSender<Evt>,
    midi_drops: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    engine: Arc<Engine>,
    // RT-local take-capture state (audio thread only).
    prev_rec: [u8; MAX_LOOPS],
    /// Tracks that hit LOOP_CAP while recording: suppresses the auto-stop
    /// state store from re-firing on every remaining sample of the block.
    /// Cleared on the track's next state edge.
    rec_full: u128,
    /// RT-local playhead (the engine atomic is the published copy).
    pos: u64,
    /// Last transport flag; the falling edge publishes an auto-stop event.
    prev_transport: bool,
    /// Metronome flag of the previous block (activation edge, below).
    prev_metro: bool,
    /// Engine sample rate (for metronome math).
    sr: u32,
    /// Metronome clock: the grid index (click number on the timeline) last
    /// emitted (-1 = nothing yet, so a transport start on a grid line clicks
    /// immediately), the absolute sample the current click started at
    /// (-1 = idle) and its tone (0 = downbeat, 1 = quarter, 2 = subdivision).
    metro_idx: i64,
    metro_start: i64,
    metro_tone: u8,
    /// Free-running metronome position (samples). While the transport runs
    /// the click ticks off the transport position; with the transport
    /// stopped the click must keep ticking WITHOUT moving the playhead, so
    /// it counts on this clock instead. Re-anchored to the playhead on
    /// seeks/activation, and synced to the playhead on every transport
    /// block, so switching states never bursts or jumps the beat.
    metro_clock: f64,
    /// Absolute sample counter (click envelope clock).
    sample_no: u64,
    /// Cached effect-chain table; refreshed from the engine when fx_gen
    /// moves (rare, user-driven — the only allocation left on this thread).
    fx_cache: Arc<[TrackFx; MAX_LOOPS]>,
    fx_cache_gen: u64,
    /// Per-track, per-slot cached slider snapshots (+ the instance
    /// generation they were read at), refreshed per block on change.
    fx_rt: Vec<Vec<FxRt>>,
    /// Cached project snapshots; refreshed when either generation moves.
    arr: Arc<Arrangement>,
    arr_cache_gen: u64,
    assets: Arc<AssetTable>,
    assets_cache_gen: u64,
    /// Asset preview: cached audition buffer (refreshed on preview_gen),
    /// its RT read cursor, and whether one plays this block.
    preview: PreviewVoices,
    preview_cache_gen: u64,
    /// RT read cursors, (voice id, sample index), kept across generation
    /// bumps so surviving voices never restart when a new one joins.
    preview_curs: Vec<(u64, usize)>,
    preview_live: bool,
    /// Reusable per-block clip window list (taken out of self during the
    /// block so the sample loop can call `&mut self` effect code freely).
    clip_buf: Vec<ClipEntry>,
    // ---- MIDI playback ------------------------------------------------------
    synths_cache: Arc<[Option<Arc<SynthHandle>>; MAX_LOOPS]>,
    synths_cache_gen: u64,
    dflt_synth: Option<Arc<SynthHandle>>,
    /// Per-block pre-master mix. Tracks whose chain hosts a plugin cannot
    /// use the per-sample path (a chain runs block-wise), so the block mix
    /// lands here and the port writes move to a post-loop pass — for every
    /// block, uniform path.
    mix_l: Vec<f32>,
    mix_r: Vec<f32>,
    /// Held-back track sums (tracks with a plugin in the chain) and monitor
    /// tap; plus the VST scratch buffer for block-wise chain passes.
    trk_bufs: Vec<Vec<f32>>,
    mon_buf: Vec<f32>,
    vst_tmp: Vec<f32>,
    /// MIDI pattern table (parallel to `assets`; refreshed on `midi_gen`).
    midi_cache: Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>,
    midi_cache_gen: u64,
    /// Events staged for the current block, sorted by (offset, kind):
    /// (sample offset, kind 0=note-off 1=CC 2=pressure 3=note-on, ch, d1, d2).
    midi_ev: Vec<(usize, u8, u8, u8, u8)>,
    /// Per-track synth render output for this block (mixed into `acc`).
    midi_scratch: Vec<Vec<f32>>,
    midi_live: [bool; MAX_LOOPS],
    /// Latest staged note-off per track (timeline samples): the synth keeps
    /// rendering while the schedule is still ringing. The note sounds
    /// INSIDE the instrument between events, so event-free blocks must not
    /// pause it — skipping them rendered every sustained note as a blip.
    midi_ring_to: [u64; MAX_LOOPS],
    /// Track's synth is still advancing (until its tail falls silent).
    midi_sustain: [bool; MAX_LOOPS],
    /// Consecutive rendered near-silent blocks with nothing scheduled:
    /// this many and the synth stops advancing until the next note.
    midi_quiet: [u32; MAX_LOOPS],
    /// Per-track cached synth param snapshot + change counter.
    synth_p: [f32; crate::synth::MAX_PARAMS],
    synth_pg: [u64; MAX_LOOPS],
    /// Reusable mono render buffer for one track's synth block.
    mono: Vec<f32>,
    /// Reusable zero input for driving a VSTi with no audio side-chain.
    sil: Vec<f32>,
}

/// RT-side snapshot of one effect instance's sliders.
struct FxRt {
    gen: u64,
    p: [f32; crate::fx::MAX_PARAMS],
}

/// Build the per-slot RT slider grid matching `table` (one row per track,
/// one `FxRt` per chain slot). Proc seeding AND the `process` refresh must
/// both use this: an empty grid next to a non-empty `fx_cache` makes
/// `fx_rt[t]` index out of bounds the first time such a track sounds.
fn seed_fx_rt(table: &[TrackFx]) -> Vec<Vec<FxRt>> {
    table
        .iter()
        .map(|tf| {
            tf.slots()
                .iter()
                .map(|_| FxRt {
                    gen: 0,
                    p: [0.0; crate::fx::MAX_PARAMS],
                })
                .collect()
        })
        .collect()
}

impl Proc {
    /// Run one track-output sample through the track's effect chain (the
    /// volume x gain trim has already been multiplied into `x`). Only for
    /// scripted-only chains; plugin chains go through `run_chain_block`.
    #[inline]
    fn fx_mix(&mut self, t: usize, x: f32) -> f32 {
        let chain = &self.fx_cache[t];
        if chain.is_empty() {
            return x;
        }
        let mut s = x;
        for (slot, rt) in chain.slots().iter().zip(self.fx_rt[t].iter_mut()) {
            let FxKind::Scripted { inst, .. } = &slot.kind else { continue };
            let g = inst.param_gen();
            if g != rt.gen {
                inst.read_params(&mut rt.p);
                rt.gen = g;
            }
            // SAFETY: the live chain is advanced by this thread only, and
            // `rt.p` holds the instance's current slider snapshot.
            s = unsafe { inst.call_sample(s, &rt.p) }
        }
        s
    }

    /// Run a plugin-bearing chain over a whole block (mono in, mono out,
    /// in place — `tmp` is scratch). Scripted slots keep their per-slot
    /// slider snapshots (`fx_rt`); plugins consume/replenish their rings.
    /// The dry input is already trim-scaled; pan/master are folded later.
    fn run_chain_block(&mut self, t: usize, buf: &mut [f32], tmp: &mut [f32]) {
        let n = buf.len().min(tmp.len());
        let Self { fx_cache, fx_rt, .. } = self;
        for (k, slot) in fx_cache[t].slots().iter().enumerate() {
            match &slot.kind {
                FxKind::Scripted { inst, .. } => {
                    let rt = &mut fx_rt[t][k];
                    let g = inst.param_gen();
                    if g != rt.gen {
                        inst.read_params(&mut rt.p);
                        rt.gen = g;
                    }
                    // SAFETY: audio thread owns the live chain (as fx_mix).
                    for x in buf[..n].iter_mut() {
                        *x = unsafe { inst.call_sample(*x, &rt.p) };
                    }
                }
                FxKind::Vst(e) => {
                    e.process(&buf[..n], &mut tmp[..n]);
                    buf[..n].copy_from_slice(&tmp[..n]);
                }
            }
        }
    }

    /// Build this block's clip windows for the timeline span `[p0, p0 + n)`.
    /// `entries` is reused across blocks (cleared, never re-allocated).
    fn collect_clips(&self, p0: u64, n: usize, entries: &mut Vec<ClipEntry>) {
        entries.clear();
        let span_end = p0.saturating_add(n as u64);
        for (t, track) in self.arr.tracks.iter().enumerate() {
            if !self.engine.track_on(t) {
                continue;
            }
            for lane in &track.lanes {
                for cl in &lane.clips {
                    let data = match self.assets.get(cl.asset as usize) {
                        Some(d) if !d.is_empty() => d,
                        _ => continue,
                    };
                    let alen = data.len() as u64;
                    // Clip window on the timeline vs. this block.
                    let c0 = cl.pos;
                    let c1 = cl.end(alen);
                    let w0 = c0.max(p0);
                    let w1 = c1.min(span_end);
                    // Only the part backed by SOURCE data contributes; a
                    // trimmed start (`cut`) shifts the read offset, and a
                    // `play_len` past the data adds nothing (silence).
                    let dstart = cl.pos;
                    let dend = cl.data_end(alen).min(c1);
                    let wd0 = w0.max(dstart);
                    let wd1 = w1.min(dend);
                    if wd0 < wd1 {
                        // The entry's pointer already includes the offset from
                        // the clip start to the window start, so the sample
                        // loop reads `ptr.add(i - i0)` with no per-sample math.
                        let off = (cl.cut + (wd0 - dstart)) as usize;
                        let off = off.min(data.len().saturating_sub(1));
                        let base = unsafe { data.as_ptr().add(off) };
                        let span = cl.span(alen);
                        let fi = cl.fade_in.min(span / 2);
                        let fo = cl.fade_out.min(span / 2);
                        entries.push(ClipEntry {
                            track: t as u8,
                            ptr: base,
                            i0: (wd0 - p0) as usize,
                            i1: (wd1 - p0) as usize,
                            gain: cl.gain,
                            pos: cl.pos,
                            fi_end: if fi > 0 { cl.pos + fi } else { 0 },
                            fi_step: if fi > 0 { 1.0 / fi as f32 } else { 0.0 },
                            fo_start: if fo > 0 { cl.end(alen) - fo } else { u64::MAX },
                            fo_end: cl.end(alen),
                            fo_step: if fo > 0 { 1.0 / fo as f32 } else { 0.0 },
                        });
                    }
                }
            }
        }
    }
}

/// Per-block compact track lists for the mix loop (v0.10 / 128 tracks).
///
/// `entry_mask` marks tracks holding clip windows, `has_midi` + `midi_live`
/// the tracks whose synth is sounding, `chain` the tracks with a non-empty
/// FX chain (their tails must keep decaying), and `vst_mask` the tracks
/// whose chain hosts a plugin. Only these tracks touch the per-sample loop,
/// so CPU scales with ACTIVE tracks, not with the 128-track capacity.
#[derive(Clone, Copy)]
struct MixLists {
    mix: [u16; MAX_LOOPS],
    n_mix: usize,
    acc: [u16; MAX_LOOPS],
    n_acc: usize,
}

fn build_mix_lists(
    entry_mask: u128,
    has_midi: bool,
    midi_live: &[bool],
    audible: &[bool],
    vst_mask: &[bool],
    chain: &[bool],
) -> MixLists {
    let mut out = MixLists {
        mix: [0u16; MAX_LOOPS],
        n_mix: 0,
        acc: [0u16; MAX_LOOPS],
        n_acc: 0,
    };
    for t in 0..MAX_LOOPS {
        let is_src = ((entry_mask >> t) & 1) != 0 || (has_midi && midi_live[t]);
        if is_src {
            out.acc[out.n_acc] = t as u16;
            out.n_acc += 1;
        }
        if audible[t] && (vst_mask[t] || chain[t] || is_src) {
            out.mix[out.n_mix] = t as u16;
            out.n_mix += 1;
        }
    }
    out
}

impl ProcessHandler for Proc {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;

        // MIDI: copy events out (bounded channel; count drops if the GUI is slow).
        for e in self.midi_in.iter(ps) {
            let bytes = e.bytes.to_vec();
            if self.tx.try_send(Evt::Midi(RawMidiMsg { bytes })).is_err() {
                self.midi_drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        if n == 0 {
            return Control::Continue;
        }

        let in_mask = self.mask_in.load(Ordering::Relaxed);
        let out_mask_l = self.mask_out_l.load(Ordering::Relaxed);
        let out_mask_r = self.mask_out_r.load(Ordering::Relaxed);

        // --- engine snapshot for this cycle (atomics read once per block) ---
        let transport = self.engine.transport_on();
        let seek = self.engine.take_seek();
        let proj_len = self.engine.proj_len().max(1);
        let looping = self.engine.loop_tl();

        // Project snapshots: refresh Arcs when a generation moved (try_lock —
        // never blocks on the manager thread). Swapping the Arrangement also
        // re-derives nothing else; clip windows are rebuilt per block anyway.
        let ag = self.engine.arr_gen();
        if ag != self.arr_cache_gen {
            if let Ok(a) = self.engine.arr.try_lock() {
                self.arr = a.clone();
                self.arr_cache_gen = ag;
            }
        }
        let asg = self.engine.assets_gen();
        if asg != self.assets_cache_gen {
            if let Ok(a) = self.engine.assets.try_lock() {
                self.assets = a.clone();
                self.assets_cache_gen = asg;
            }
        }
        let sg = self.engine.synths_gen();
        if sg != self.synths_cache_gen {
            if let Ok(s) = self.engine.synths.try_lock() {
                self.synths_cache = s.clone();
                self.synths_cache_gen = sg;
                self.dflt_synth = self
                    .engine
                    .default_synth
                    .try_lock()
                    .ok()
                    .and_then(|d| d.clone());
            }
        }
        let mg = self.engine.midi_gen();
        if mg != self.midi_cache_gen {
            if let Ok(m) = self.engine.midi.try_lock() {
                self.midi_cache = m.clone();
                self.midi_cache_gen = mg;
            }
        }
        // Pick up edited effect chains (user action only; try_lock so the
        // audio path can never block on the manager thread).
        let fxg = self.engine.fx_gen();
        if fxg != self.fx_cache_gen {
            if let Ok(tb) = self.engine.fx.try_lock() {
                self.fx_cache = tb.clone();
                self.fx_cache_gen = fxg;
                self.fx_rt = seed_fx_rt(&self.fx_cache[..]);
                // Stamp the chain reaper (see `Engine::drain_vst_held`): the
                // displaced chain died right here, but every VST entry it
                // held also lives in `vst_held`, so no plugin destructor ran
                // on this thread — the UI may now collect them.
                let vg = self.engine.vst_gen();
                self.engine.vst_refreshed.store(vg, Ordering::Relaxed);
            }
        }
        // Asset preview: refresh the audition buffer on a generation change
        // (restarts its read cursor) and mirror the play flag every block —
        // the RT side clears the flag itself when a buffer runs out.
        let pvg = self.engine.preview_gen();
        if pvg != self.preview_cache_gen {
            let snap = self.engine.preview_snapshot();
            // Cursors are keyed by voice id: a new voice joins without
            // rewinding the ones already playing, and stopped voices lose
            // their cursor.
            let mut curs = std::mem::take(&mut self.preview_curs);
            curs.retain(|(vid, _)| snap.iter().any(|(v, _, _)| v == vid));
            for (vid, _, _) in snap.iter() {
                if !curs.iter().any(|(v, _)| v == vid) {
                    curs.push((*vid, 0));
                }
            }
            self.preview_curs = curs;
            self.preview = snap;
            self.preview_cache_gen = pvg;
        }
        self.preview_live = self.engine.preview_on.load(Ordering::Relaxed) != 0
            && !self.preview_curs.is_empty();

        // Per-track output trim (volume x gain, linear) snapshotted once per
        // cycle; applied to each clip's PLAYBACK contribution only, so the
        // mix level rides the slider while recorded takes stay at raw input.
        let mut amp = [1.0f32; MAX_LOOPS];
        // Equal-power pan gains per track, snapshotted with the trim.
        let mut pgl = [F1S2; MAX_LOOPS];
        let mut pgr = [F1S2; MAX_LOOPS];
        // Tracks whose clips can sound at all this cycle: sounding (play)
        // AND not recording... (takes are raw — capture ignores the mix).
        let mut audible = [false; MAX_LOOPS];
        for (t, a) in amp.iter_mut().enumerate() {
            *a = f32::from_bits(self.engine.vol[t].load(Ordering::Relaxed))
                * f32::from_bits(self.engine.trk_gain[t].load(Ordering::Relaxed));
            let (l, r) = pan_gains(f32::from_bits(self.engine.pan[t].load(Ordering::Relaxed)));
            pgl[t] = l;
            pgr[t] = r;
            audible[t] = transport && self.engine.track_on(t);
        }

        // Input monitoring follows the selected track: its gain x volume,
        // effect chain and pan shape the live pass-through (takes capture
        // the raw, unprocessed `src_in`).
        let sel = (self.engine.selected.load(Ordering::Relaxed) as usize).min(MAX_LOOPS - 1);
        let (mut mon_amp, mon_l, mon_r) = (amp[sel], pgl[sel], pgr[sel]);
        if !self.engine.monitor_on.load(Ordering::Relaxed) {
            mon_amp = 0.0; // Settings → live input monitoring off
        }

        // Compact list of recording tracks: the sample loop iterates only
        // these instead of all MAX_LOOPS slots per sample.
        let st_now: [u8; MAX_LOOPS] =
            core::array::from_fn(|i| self.engine.rec_state[i].load(Ordering::Relaxed));
        let mut rec_tracks: [u8; MAX_LOOPS] = [0u8; MAX_LOOPS];
        let mut nrec = 0usize;
        for (t, &s) in st_now.iter().enumerate() {
            if s != self.prev_rec[t] {
                self.rec_full &= !(1u128 << t);
                self.prev_rec[t] = s;
            }
            if s == TR_REC {
                rec_tracks[nrec] = t as u8;
                nrec += 1;
            }
        }
        let any_rec = nrec > 0;

        let clip_sound = transport
            && self.arr.tracks.iter().enumerate().any(|(t, tr)| {
                self.engine.track_on(t) && tr.lanes.iter().any(|l| !l.clips.is_empty())
            });
        let metro_on = self.engine.metro().0;
        if in_mask == 0
            && (out_mask_l | out_mask_r) == 0
            && !any_rec
            && !clip_sound
            && !metro_on
            && !self.preview_live
        {
            return Control::Continue; // nothing connected, nothing playing: idle
        }

        // --- transport position for this block ------------------------------
        let prev_pos = self.pos;
        if let Some(p) = seek {
            self.pos = p.min(proj_len);
        }
        if seek.is_some() {
            // A jump in the timeline: nothing that was ringing still applies.
            for inst in self.synths_cache.iter().flatten() {
                inst.all_off();
            }
            if let Some(d) = &self.dflt_synth {
                d.all_off();
            }
            // Sustain bookkeeping dies with the old timeline position.
            for t in 0..MAX_LOOPS {
                self.midi_sustain[t] = false;
                self.midi_quiet[t] = 0;
                self.midi_ring_to[t] = 0;
            }
            // Plugin buffers/rings in every chain: drop stale audio.
            for tf in self.fx_cache.iter() {
                for s in tf.slots() {
                    if let FxKind::Vst(e) = &s.kind {
                        e.flush();
                    }
                }
            }
        }
        let mut p0 = self.pos;
        // Does the clock advance this block? ONLY the transport moves the
        // playhead — a bare metronome run ticks on its own free-running
        // clock (`metro_clock`) and must leave the (always-visible)
        // playhead exactly where it is. Stop means stop.
        let advance = transport;
        // The active loop region (samples): a looping transport wraps from
        // `ls` back at `le`. Degenerate requests fall back to the whole
        // project (`le = 0` means "the project end").
        let (ls, le) = clamp_loop_region(
            self.engine.loop_region().0,
            self.engine.loop_region().1,
            proj_len,
        );
        // End handling: looping clips the block at the region end (wrapped
        // after the sample loop); non-looping clips at the project end and
        // parks there (auto-stop).
        let mut block_n = n;
        let mut auto_stop = false;
        if advance && looping {
            // Playhead outside the region (a handle dragged under it, or a
            // seek beyond it): re-enter at the region start.
            if p0 < ls || p0 >= le {
                p0 = ls;
                self.pos = ls;
            }
            if p0.saturating_add(n as u64) > le {
                block_n = (le - p0) as usize;
            }
        } else if advance && p0.saturating_add(n as u64) > proj_len {
            block_n = (proj_len - p0.min(proj_len)) as usize;
            auto_stop = true;
        }

        // Build this block's clip windows over [p0, p0 + block_n). The list
        // lives in a local (taken out of self) so the sample loop below can
        // call `&mut self` effect code while iterating it.
        let mut entries = std::mem::take(&mut self.clip_buf);
        entries.clear();
        if clip_sound {
            self.collect_clips(p0, block_n, &mut entries);
        }
        // MIDI pattern clips of this block, rendered per track through the
        // track's synth into `midi_scratch` (mixed in the sample loop).
        let has_midi = if clip_sound {
            self.render_midi_block(p0, block_n)
        } else {
            false
        };

        // Cache this cycle's input buffers (raw pointers are stable). Only
        // the connected slots are kept, compacted to the front, so the sample
        // loop iterates the live ones with no per-sample null checks.
        let mut in_bufs: [*const f32; MAX_SLOTS] = [std::ptr::null(); MAX_SLOTS];
        let mut nin = 0usize;
        for s in 0..MAX_SLOTS {
            if in_mask & (1u64 << s) != 0 {
                let p = self.ports.ins[s];
                if !p.is_null() {
                    in_bufs[nin] =
                        unsafe { j::jack_port_get_buffer(p, ps.n_frames()) } as *const f32;
                    nin += 1;
                }
            }
        }
        // Cache output buffers too: the stereo mix is computed once per
        // sample and written to every enabled slot according to its routing
        // — left bus, right bus, or the mono downmix (both bits).
        let mut out_bufs_l: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        let mut out_bufs_r: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        let mut out_bufs_m: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        let (mut nl, mut nr, mut nm) = (0usize, 0usize, 0usize);
        for s in 0..MAX_SLOTS {
            let l = out_mask_l & (1u64 << s) != 0;
            let r = out_mask_r & (1u64 << s) != 0;
            if !(l || r) {
                continue;
            }
            let p = self.ports.outs[s];
            if p.is_null() {
                continue;
            }
            let b = unsafe { j::jack_port_get_buffer(p, ps.n_frames()) as *mut f32 };
            match (l, r) {
                (true, false) => {
                    out_bufs_l[nl] = b;
                    nl += 1;
                }
                (false, true) => {
                    out_bufs_r[nr] = b;
                    nr += 1;
                }
                _ => {
                    out_bufs_m[nm] = b;
                    nm += 1;
                }
            }
        }
        let ins = &in_bufs[..nin];

        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));

        // Metronome grid on the timeline: click k at timeline sample
        // `k * sub_len` (sub = clicks per beat). Track the grid index last
        // emitted so playhead crossings — while playing OR while scrubbing —
        // fire exactly one click each. Re-anchored on transport starts and
        // seeks (the position jump itself must not burst-fire).
        let metro_bpm = self
            .engine
            .metro_bpm
            .load(Ordering::Relaxed)
            .clamp(20, 300)
            .max(1);
        let metro_sub = self.engine.metro_sub.load(Ordering::Relaxed).clamp(1, 16);
        let metro_num = self.engine.metro_num.load(Ordering::Relaxed).clamp(1, 16);
        let sub_len = (60.0 * self.sr as f64 / metro_bpm as f64) / metro_sub as f64;
        // The next click is the first grid line at or after the playhead.
        // Re-anchor on transport starts and backward seeks; a forward seek
        // keeps the old anchor so crossing grid lines while scrubbing still
        // fires one click each. Any seek or start also re-anchors the
        // free-running click clock to the (now stationary) playhead, so a
        // bare metronome run ticks from where the playhead sits.
        if seek.is_some_and(|p| p <= prev_pos)
            || (transport && !self.prev_transport)
            || (metro_on && !self.prev_metro)
        {
            // Activating the click (or starting/seeking back into a run)
            // drops the anchor one grid line behind the playhead, so the
            // very first block fires the click at the current line.
            self.metro_idx = (self.pos as f64 / sub_len).ceil() as i64 - 1;
            self.metro_clock = self.pos as f64;
        } else if seek.is_some() {
            // Forward seek: keep the grid anchor (scrub crossings still
            // tick), but jump the click clock to the new position.
            self.metro_clock = self.pos as f64;
        }
        self.prev_transport = transport;
        self.prev_metro = metro_on;
        let metro_base = self.metro_clock;
        let metro_dur = (self.sr / 40).max(2); // 25 ms click
        let metro_decay = self.sr as f32 * 0.012;

        // Mixer gate: while the transport runs, every sounding track's chain
        // processes each sample (so delay/reverb tails keep decaying across
        // silent gaps instead of freezing). With no clips out and no effect
        // chains there is nothing to run at all. `has_entries` is
        // block-constant: the accumulator only needs refilling when this
        // block actually holds clip windows.
        let mut mixer_live = clip_sound;
        let mut chain_mask = [false; MAX_LOOPS];
        let mut any_chain = false;
        for (t, m) in chain_mask.iter_mut().enumerate() {
            *m = !self.fx_cache[t].is_empty();
            any_chain |= *m;
        }
        if transport && !mixer_live {
            mixer_live = any_chain;
        }
        // Per-track sample accumulators for the clip mix.
        let mut acc = [0.0f32; MAX_LOOPS];

        // Tracks whose chain hosts a plugin: their per-track sums (and the
        // monitor tap) run the chain block-wise after the sample loop.
        let mut vst_mask = [false; MAX_LOOPS];
        for t in 0..MAX_LOOPS {
            vst_mask[t] = audible[t] && self.fx_cache[t].has_vst();
        }
        let sel_vst = audible[sel] && self.fx_cache[sel].has_vst();
        let has_vst = vst_mask.iter().any(|v| *v) || sel_vst;

        // v0.10 (128 tracks): compact per-block lists of the tracks that can
        // actually contribute — clip windows, live MIDI, or a non-empty FX
        // chain (whose tail must keep decaying). The per-sample loop below
        // touches ONLY these, so an empty 128-track project costs exactly
        // what an empty 16-track one did before.
        let mut entry_mask = 0u128;
        for e in entries.iter() {
            entry_mask |= 1u128 << e.track;
        }
        let lists = build_mix_lists(
            entry_mask,
            has_midi,
            &self.midi_live,
            &audible,
            &vst_mask,
            &chain_mask,
        );
        let MixLists { mix: mix_list, n_mix: nmix, acc: acc_list, n_acc: nacc } = lists;

        // Per-block mix buffers (taken out of self so the sample loop can
        // call `&mut self` effect code freely; `clip_buf` uses the same trick).
        let mut mix_l = std::mem::take(&mut self.mix_l);
        let mut mix_r = std::mem::take(&mut self.mix_r);
        let mut trk_bufs = std::mem::take(&mut self.trk_bufs);
        let mut mon_buf = std::mem::take(&mut self.mon_buf);
        let mut vst_tmp = std::mem::take(&mut self.vst_tmp);
        mix_l.clear();
        mix_l.resize(n, 0.0);
        mix_r.clear();
        mix_r.resize(n, 0.0);
        vst_tmp.clear();
        vst_tmp.resize(n, 0.0);
        for (t, buf) in trk_bufs.iter_mut().enumerate() {
            buf.clear();
            if vst_mask[t] {
                buf.resize(n, 0.0);
            }
        }
        if sel_vst {
            mon_buf.clear();
            mon_buf.resize(n, 0.0);
        }

        let mut peak_val: u32 = 0;
        let mut pos_now = self.pos;
        for i in 0..n {
            // Output buffer writes beyond `block_n` (the auto-stop tail of a
            // non-looping block) still need to emit silence.
            let live = i < block_n;

            // Dry mix of the connected inputs.
            let mut dry = 0.0f32;
            for b in ins {
                dry += unsafe { *b.add(i) };
            }
            // Live input for this sample. Latency compensation is NOT a
            // running input delay: it only discards the first `latency`
            // samples of a fresh take (engine `rec_skip`), so monitoring
            // stays exactly aligned with the timeline grid while the
            // button-press silence at a take's start gets trimmed away.
            let src_in = dry;

            // Take capture (raw input; independent of the mix and of the
            // transport — an armed track records whenever there is input,
            // even while the transport is parked at the project end).
            if any_rec {
                for &ti in &rec_tracks[..nrec] {
                    let t = ti as usize;
                    if self.engine.rec_sample(t, src_in) && self.rec_full & (1u128 << t) == 0 {
                        // Capacity reached: stop capturing into this track;
                        // the manager still hasn't been told, so finish the
                        // take here and hand it over asynchronously.
                        self.rec_full |= 1u128 << t;
                        self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                        self.engine.mark_take_pending(t);
                    }
                }
            }

            // Clip mix: sum the entries covering this sample into their
            // track accumulators, then run the mixer over every sounding
            // track (chains keep decaying through silent gaps).
            let mut play_l = 0.0f32;
            let mut play_r = 0.0f32;
            if live && mixer_live {
                if nacc != 0 {
                    for k in 0..nacc {
                        acc[acc_list[k] as usize] = 0.0;
                    }
                    for e in entries.iter() {
                        if i >= e.i0 && i < e.i1 {
                            let mut v = unsafe { *e.ptr.add(i - e.i0) } * e.gain;
                            let s = p0 + i as u64;
                            if s < e.fi_end {
                                v *= (s - e.pos) as f32 * e.fi_step;
                            } else if s >= e.fo_start {
                                v *= (e.fo_end - s) as f32 * e.fo_step;
                            }
                            acc[e.track as usize] += v;
                        }
                    }
                    if has_midi {
                        for &tt in &acc_list[..nacc] {
                            let t = tt as usize;
                            if self.midi_live[t] && i < block_n {
                                acc[t] += self.midi_scratch[t][i];
                            }
                        }
                    }
                }
                for &tt in &mix_list[..nmix] {
                    let t = tt as usize;
                    let a = acc[t];
                    if vst_mask[t] {
                        // Held back: this track's chain (scripts and
                        // plugins, in chain order) runs block-wise over
                        // the pre-pan sum after the sample loop.
                        trk_bufs[t][i] = a * amp[t];
                    } else {
                        let m = self.fx_mix(t, a * amp[t]);
                        play_l += m * pgl[t];
                        play_r += m * pgr[t];
                    }
                }
            }

            // Metronome click (added to the output mix only — `dry` never
            // sees it, so it can never be recorded into a take). While the
            // transport runs the clicks tick off the transport position;
            // with it stopped they tick off the free-running click clock,
            // which leaves the playhead untouched.
            let mut click = 0.0f32;
            if live && metro_on {
                let p = if advance {
                    (p0 + i as u64) as f64
                } else {
                    metro_base + i as f64
                };
                let idx = (p / sub_len) as i64;
                if idx > self.metro_idx {
                    self.metro_idx = idx;
                    self.metro_start = self.sample_no as i64;
                    // Tone of this click: the first quarter note of the
                    // measure is the downbeat, the other quarter notes get
                    // the plain quarter tone, everything in between gets the
                    // subdivision tone.
                    let beat = (idx / metro_sub as i64) % metro_num as i64;
                    let k = idx % metro_sub as i64;
                    self.metro_tone = if k == 0 {
                        if beat == 0 {
                            0
                        } else {
                            1
                        }
                    } else {
                        2
                    };
                }
            }
            if self.metro_start >= 0 {
                let k = self.sample_no as i64 - self.metro_start;
                if k >= 0 && (k as u32) < metro_dur {
                    let kf = k as f32;
                    // Three distinct tones: downbeat (high, loudest),
                    // quarter note (mid), subdivision (low, softest).
                    let (f, a) = match self.metro_tone {
                        0 => (1760.0f32, 0.5f32),
                        1 => (1175.0, 0.35),
                        _ => (880.0, 0.25),
                    };
                    click = (kf * std::f32::consts::TAU * f / self.sr as f32).sin()
                        * (-kf / metro_decay).exp()
                        * a;
                } else {
                    self.metro_start = -1;
                }
            }
            self.sample_no += 1;

            // Asset audition: next sample of the preview buffer, dead
            // centre like the click, output mix only (never recorded).
            // Auto-stops when the buffer runs out.
            let mut pvc = 0.0f32;
            if self.preview_live {
                let mut any = false;
                for (k, (_, idx)) in self.preview_curs.iter_mut().enumerate() {
                    let buf = &self.preview[k].2;
                    if *idx < buf.len() {
                        pvc += buf[*idx] * F1S2;
                        *idx += 1;
                        any = true;
                    }
                }
                if !any {
                    // All voices finished: auto-stop like a single one-shot.
                    self.preview_live = false;
                    self.engine.preview_on.store(0, Ordering::Relaxed);
                }
            }

            // Live input rides the selected track's chain/trim/pan; the click
            // always sits dead centre. At unity trim and centre pan the L/R
            // pair still sums to the historical unity mono mix.
            // The monitored chain still runs every sample (tails decay);
            // with a VST insert on the monitored track its output is held
            // back for the plugin instead of feeding the mix directly.
            let mut mon = 0.0f32;
            if sel_vst {
                mon_buf[i] = src_in * mon_amp;
            } else {
                mon = self.fx_mix(sel, src_in * mon_amp);
            }
            let clickc = click * F1S2;
            // Pre-master, pre-gain mix; ports get written post-loop (once any
            // VST inserts have run). The output mix is NEVER delayed: what
            // the user hears stays exactly on the timeline grid (latency
            // compensation only trims the start of a fresh take — see
            // `Engine::rec_sample`).
            mix_l[i] = mon * mon_l + clickc + play_l + pvc;
            mix_r[i] = mon * mon_r + clickc + play_r + pvc;

            // Advance the transport clock (only while running).
            if live && advance {
                pos_now += 1;
            }
        }

        // Plugin chains: run the held-back track sums (pre-pan) and the
        // monitor tap through their full chains block-wise, folding each
        // result into the mix at the track's pan / the monitor level.
        if has_vst {
            for t in 0..MAX_LOOPS {
                if !vst_mask[t] {
                    continue;
                }
                let (buf, tmp) = (&mut trk_bufs[t], &mut vst_tmp);
                self.run_chain_block(t, buf, tmp);
                for i in 0..n {
                    mix_l[i] += buf[i] * pgl[t];
                    mix_r[i] += buf[i] * pgr[t];
                }
            }
            if sel_vst {
                let (buf, tmp) = (&mut mon_buf, &mut vst_tmp);
                self.run_chain_block(sel, buf, tmp);
                for i in 0..n {
                    mix_l[i] += buf[i] * mon_l;
                    mix_r[i] += buf[i] * mon_r;
                }
            }
        }

        // Master gain + port writes (moved out of the sample loop so VST
        // inserts can run before the mix leaves the callback).
        for i in 0..n {
            let v_l = mix_l[i] * g;
            let v_r = mix_r[i] * g;
            let av = v_l.abs().max(v_r.abs());
            if av.to_bits() > peak_val {
                peak_val = av.to_bits();
            }
            if nm > 0 {
                let v_m = (v_l + v_r) * F1S2;
                for b in &out_bufs_m[..nm] {
                    unsafe { *b.add(i) = v_m };
                }
            }
            for b in &out_bufs_l[..nl] {
                unsafe { *b.add(i) = v_l };
            }
            for b in &out_bufs_r[..nr] {
                unsafe { *b.add(i) = v_r };
            }
        }

        // Give the per-block buffers back for the next block.
        self.mix_l = mix_l;
        self.mix_r = mix_r;
        self.trk_bufs = trk_bufs;
        self.mon_buf = mon_buf;
        self.vst_tmp = vst_tmp;

        // Give the clip window list back for the next block.
        self.clip_buf = entries;

        // Continuous loop: at the loop region end the playhead wraps back to
        // the region start; a non-looping transport parks at the project end
        // and auto-stops (an event wakes the GUI so its button mirrors
        // reality). The block was clipped at `le`, so one wrap per block.
        if advance {
            if looping {
                if let Some(wrapped) = wrap_pos(pos_now, ls, le) {
                    pos_now = wrapped;
                    // Wrap re-anchors the metronome grid without a burst.
                    self.metro_idx = (pos_now as f64 / sub_len).ceil() as i64 - 1;
                }
                self.pos = pos_now;
            } else {
                self.pos = pos_now.min(proj_len);
            }
        }
        // Keep the free-running click clock glued to the playhead while the
        // transport advances; with the transport stopped and the click on,
        // it counts on through the live samples on its own (the playhead
        // itself stays put).
        if advance {
            self.metro_clock = self.pos as f64;
        } else if metro_on {
            self.metro_clock = metro_base + block_n as f64;
        }
        if auto_stop && transport {
            self.engine.set_transport(false);
            self.prev_transport = false;
            let _ = self.tx.try_send(Evt::Transport { on: false });
        }
        // Publish the playhead for the GUI (display resolution is far
        // coarser than the block rate).
        self.engine.pos.store(self.pos, Ordering::Relaxed);
        self.engine.preview_pos.store(
            self.preview_curs.last().map(|(_, i)| *i as u64).unwrap_or(0),
            Ordering::Relaxed,
        );
        self.peak.store(peak_val, Ordering::Relaxed);

        Control::Continue
    }
}

/// Null out every callback that `activate_async` registered (the non-slow-sync
/// set), so no notification can reach freed handler memory after the context
/// is dropped. The jack crate's own `clear_callbacks` only covers
/// thread-init/process — not enough on PipeWire-JACK.
unsafe fn clear_all_callbacks(client: *mut j::jack_client_t) {
    let null = std::ptr::null_mut();
    j::jack_set_thread_init_callback(client, None, null);
    j::jack_on_info_shutdown(client, None, null);
    j::jack_set_process_callback(client, None, null);
    j::jack_set_freewheel_callback(client, None, null);
    j::jack_set_buffer_size_callback(client, None, null);
    j::jack_set_sample_rate_callback(client, None, null);
    j::jack_set_client_registration_callback(client, None, null);
    j::jack_set_port_registration_callback(client, None, null);
    j::jack_set_port_connect_callback(client, None, null);
    j::jack_set_graph_order_callback(client, None, null);
    j::jack_set_xrun_callback(client, None, null);
}

/// Format a JACK error for display.
fn describe_err(e: jack::Error) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrange::Clip;

    /// The project render must run each track's effect chain (on private
    /// instances, leaving the live ones untouched) after the vol x gain trim,
    /// and honour per-track play/stop, panning and clip placement.
    #[test]
    fn pan_gains_are_equal_power_and_mono_safe() {
        // Hard positions.
        let (l, r) = pan_gains(0.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6, "0 = hard left");
        let (l, r) = pan_gains(1.0);
        assert!(l.abs() < 1e-6 && (r - 1.0).abs() < 1e-6, "1 = hard right");
        // Centre: 1/sqrt2 on both buses, so the (L + R)/sqrt2 downmix of a
        // centred source is exactly unity — the historical mono mix intact.
        let (l, r) = pan_gains(0.5);
        assert!((l - F1S2).abs() < 1e-6 && (r - F1S2).abs() < 1e-6);
        assert!(((l + r) * F1S2 - 1.0).abs() < 1e-4);
        // Constant power across the whole field; clamped outside [0, 1].
        for k in 0..=16 {
            let p = k as f32 / 16.0;
            let (l, r) = pan_gains(p);
            assert!((l * l + r * r - 1.0).abs() < 1e-3, "power at p={p}");
        }
        assert_eq!(pan_gains(-2.0), pan_gains(0.0));
        assert_eq!(pan_gains(9.0), pan_gains(1.0));
    }

    #[test]
    fn engine_fresh() {
        let e = Engine::new();
        for i in 0..MAX_LOOPS {
            assert_eq!(e.rec_state_of(i), TR_IDLE);
            assert_eq!(e.rec_len_of(i), 0);
            assert!(e.track_on(i), "tracks start sounding");
        }
        assert!(!e.transport_on());
        assert_eq!(e.pos(), 0);
        assert!(e.loop_tl(), "the timeline loops by default");
        assert_eq!(e.take_seek(), None);
    }

    #[test]
    fn loop_pan_defaults_centre_and_clamps() {
        let e = Engine::new();
        assert!((e.loop_pan(3) - 0.5).abs() < 1e-9, "default is centre");
        e.set_loop_pan(3, 0.25);
        assert!((e.loop_pan(3) - 0.25).abs() < 1e-6);
        e.set_loop_pan(3, 4.0);
        assert!((e.loop_pan(3) - 1.0).abs() < 1e-6, "clamped to full right");
        e.set_loop_pan(3, -1.0);
        assert_eq!(e.loop_pan(3), 0.0, "clamped to full left");
    }

    #[test]
    fn seek_clamps_and_publishes() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1000);
        e.seek_to(400);
        assert_eq!(e.pos(), 400);
        assert_eq!(e.take_seek(), Some(400));
        assert_eq!(e.take_seek(), None);
        // Seeks clamp to the project end.
        e.seek_to(5000);
        assert_eq!(e.pos(), 1000);
    }

    #[test]
    fn take_capture_and_finish_roundtrip() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 10_000);
        e.seek_to(500);
        e.start_rec(2);
        assert_eq!(e.rec_state_of(2), TR_REC);
        assert_eq!(
            e.rec_at(2),
            500,
            "the take remembers where the playhead was"
        );
        // Simulate RT capture.
        for k in 0..100u32 {
            e.rec_write(2, k, k as f32 * 0.01);
        }
        e.rec_len[2].store(100, Ordering::Relaxed);
        let (at, data) = e.copy_take(2);
        assert_eq!(at, 500);
        assert_eq!(data.len(), 100);
        assert!((data[50] - 0.5).abs() < 1e-6);
        assert_eq!(e.rec_state_of(2), TR_IDLE);
    }

    /// Latency compensation is a START-of-take skip: a fresh recording
    /// discards its first `latency` input samples (so the button click that
    /// started it — a few ms late through the mic — lands at content[0]),
    /// while everything after the skip is captured live.
    /// Live input monitoring defaults on and mutes ONLY the pass-through
    /// gain when switched off (regression pin for the Settings toggle).
    #[test]
    fn monitoring_toggle_gates_live_input() {
        let e = Engine::new();
        assert!(e.monitor_on(), "monitoring defaults on (current behavior)");
        assert_eq!(e.monitor_amp(0.75), 0.75);
        e.set_monitor_on(false);
        assert_eq!(e.monitor_amp(0.75), 0.0, "off mutes the pass-through");
        e.set_monitor_on(true);
        assert_eq!(e.monitor_amp(0.75), 0.75, "back on restores it");
    }

    #[test]
    fn latency_skips_silence_at_recording_start() {
        let e = Engine::new();
        e.latency.store(5, Ordering::Relaxed);
        e.start_rec(0);
        assert_eq!(e.rec_state_of(0), TR_REC);
        for k in 1..=10 {
            assert!(!e.rec_sample(0, k as f32));
        }
        // The first 5 samples were discarded; content starts at sample #6 —
        // exactly the latency-compensated moment.
        let (_at, data) = e.copy_take(0);
        assert_eq!(&data[..], &[6.0, 7.0, 8.0, 9.0, 10.0]);

        // A second take re-arms from the current latency, and a latency of 0
        // records immediately.
        e.latency.store(0, Ordering::Relaxed);
        e.start_rec(0);
        assert!(!e.rec_sample(0, 42.0));
        assert_eq!(e.rec_len_of(0), 1);
        assert_eq!(e.rec_read(0, 0), 42.0);
    }

    /// Changing the latency mid-take must not retroactively shift the
    /// recording in progress: the skip is armed at start_rec time.
    #[test]
    fn latency_change_mid_take_does_not_shift_recording() {
        let e = Engine::new();
        e.latency.store(3, Ordering::Relaxed);
        e.start_rec(0);
        assert!(!e.rec_sample(0, 1.0)); // consumes one armed skip
        e.latency.store(5000, Ordering::Relaxed); // huge change mid-take
        assert!(!e.rec_sample(0, 2.0)); // armed skip has 2 left
        assert!(!e.rec_sample(0, 3.0)); // ...and captures nothing more
        assert!(!e.rec_sample(0, 4.0)); // skip exhausted: capture resumes
        let (_at, data) = e.copy_take(0);
        assert_eq!(
            &data[..],
            &[4.0],
            "only the samples after the armed 3-sample skip"
        );
    }

    #[test]
    fn rec_sample_never_overshoots_capacity() {
        let e = Engine::new();
        // Fill exactly to capacity, one sample at a time (RT path).
        for _ in 0..LOOP_CAP {
            assert!(!e.rec_sample(0, 1.0));
        }
        assert_eq!(e.rec_len_of(0), LOOP_CAP);
        // Once full, further samples must report "full" and must not push the
        // counter past LOOP_CAP.
        for _ in 0..1024 {
            assert!(e.rec_sample(0, 0.0));
        }
        assert_eq!(e.rec_len_of(0), LOOP_CAP);
    }

    /// v0.9: per-clip gain + fades must be sample-accurate in the mixdown
    /// (this exercises the export path; the live path shares `fade_at`).
    #[test]
    fn v09_clip_gain_and_fade_shape_render() {
        let a: Arc<[f32]> = Arc::from(vec![1.0f32; 4000]);
        let mut arr = Arrangement::new();
        let mut cl = Clip::new(0, 0);
        cl.gain = 0.5;
        cl.fade_in = 1000;
        cl.fade_out = 1000;
        arr.insert(0, 0, cl);
        let e = Engine::new();
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 4000);
        let out = e.render_project().expect("clips exist");
        assert!(out[0].abs() < 1e-4, "fade-in starts silent, got {}", out[0]);
        assert!((out[500] - 0.25).abs() < 1e-3, "mid fade-in ~ half gain");
        assert!((out[2000] - 0.5).abs() < 1e-3, "body carries the gain");
        assert!((out[3500] - 0.25).abs() < 1e-3, "mid fade-out ~ half gain");
        assert!(out[3999].abs() < 1e-3, "fade-out lands on silence");
    }

    /// v0.9: splitting a clip must not change a single sample — the right
    /// half continues the source at the matching offset.
    #[test]
    fn v09_split_renders_identically() {
        let a: Arc<[f32]> = Arc::from(
            (0..4000)
                .map(|k| if k < 2000 { 0.25f32 } else { 0.75 })
                .collect::<Vec<f32>>(),
        );
        let whole = Clip::new(0, 0);
        let mut arr1 = Arrangement::new();
        arr1.insert(0, 0, whole);
        let e1 = Engine::new();
        e1.set_project(Arc::new(vec![a.clone()]), Arc::new(arr1), 4000);
        let before = e1.render_project().unwrap();

        let (l, r) = whole.split_at(2000, a.len() as u64).unwrap();
        let mut arr2 = Arrangement::new();
        arr2.insert(0, 0, l);
        arr2.insert(0, 0, r);
        let e2 = Engine::new();
        e2.set_project(Arc::new(vec![a]), Arc::new(arr2), 4000);
        let after = e2.render_project().unwrap();
        let diff = before
            .iter()
            .zip(after.iter())
            .fold(0f32, |m, (x, y)| m.max((x - y).abs()));
        assert!(diff < 1e-9, "split changed the render by {diff}");
    }

    /// v0.9: MIDI clips carry the same clip properties — gain and the fade
    /// ramp scale NOTE VELOCITY (reaper-style item volume), silent parts
    /// drop the note entirely.
    #[test]
    fn v09_fade_scales_midi_velocity() {
        use crate::midipat::{Pattern, PPQ};
        let mut pat = Pattern::new(120);
        // beat 0 (fade-in, sample 0), beat 1 (24000 samples: past the fade)
        for start in [0u32, PPQ as u32] {
            pat.notes.push(crate::midipat::Note {
                ch: 0,
                note: 60,
                vel: 100,
                press: 0,
                start,
                len: (PPQ / 2) as u32,
            });
        }
        let mut cl = Clip::new(0, 0);
        cl.fade_in = 4800; // 100 ms
        let mut ev = Vec::new();
        stage_midi_events(&pat, &cl, 48_000, &mut ev);
        let ons: Vec<_> = ev.iter().filter(|e| e.1 == 3).cloned().collect();
        assert_eq!(ons.len(), 1, "note inside the silent fade start drops");
        assert_eq!(ons[0].0, 24000);
        assert_eq!(ons[0].4, 100);
        // Pure gain halves the velocity; 4x clamps at 127.
        let mut cl2 = Clip::new(0, 0);
        cl2.gain = 0.5;
        ev.clear();
        stage_midi_events(&pat, &cl2, 48_000, &mut ev);
        let ons: Vec<_> = ev.iter().filter(|e| e.1 == 3).cloned().collect();
        assert_eq!(ons.len(), 2);
        assert!(ons.iter().all(|e| e.4 == 50), "gain 0.5: {ons:?}");
        let mut cl3 = Clip::new(0, 0);
        cl3.gain = 4.0;
        ev.clear();
        stage_midi_events(&pat, &cl3, 48_000, &mut ev);
        let ons: Vec<_> = ev.iter().filter(|e| e.1 == 3).cloned().collect();
        assert!(ons.iter().all(|e| e.4 == 127), "gain clamps: {ons:?}");
    }

    /// Helper: two assets, track 0 clip at 0, track 1 clip offset by 1000.
    fn two_track_project() -> (Arc<Engine>, Arc<[f32]>, Arc<[f32]>) {
        let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 2000]);
        let a1: Arc<[f32]> = Arc::from(vec![0.25f32; 2000]);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        arr.insert(
            1,
            0,
            Clip::new(1, 1000),
        );
        let e = Engine::new();
        e.set_project(Arc::new(vec![a0.clone(), a1.clone()]), Arc::new(arr), 4000);
        (e, a0, a1)
    }

    #[test]
    fn render_project_places_mutes_and_mixes() {
        let (e, _a0, _a1) = two_track_project();
        let out = e.render_project().expect("clips exist");
        assert_eq!(out.len(), 4000);
        // [0,1000): only track 0's asset (unity trims, centre pan).
        assert!((out[500] - 0.5).abs() < 1e-6);
        // [1000,2000): both assets summed additively.
        assert!((out[1500] - 0.75).abs() < 1e-6);
        // [2000,3000): track 1 only.
        assert!((out[2500] - 0.25).abs() < 1e-6);
        assert!((out[3999]).abs() < 1e-9, "past the last clip is silence");

        // Stopping a track mutes it in the render (the mix truth).
        e.set_track_on(0, false);
        let out = e.render_project().unwrap();
        assert!(out[500].abs() < 1e-9);
        assert!((out[2500] - 0.25).abs() < 1e-6);

        // Volume trim rides the render.
        e.set_track_on(0, true);
        e.set_loop_vol(1, 0.5);
        let out = e.render_project().unwrap();
        assert!((out[2500] - 0.125).abs() < 1e-5, "half volume");

        // Single-track export ignores the mute flag (explicit request).
        e.set_loop_vol(1, 1.0); // reset the trim so the level reads pure
        e.set_track_on(1, false);
        let one = e.render_track(1).unwrap();
        assert!((one[2500] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn render_child_lanes_stack() {
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 1000]);
        let mut arr = Arrangement::new();
        // Two clips of the same asset overlapping on different lanes.
        arr.insert(4, 0, Clip::new(0, 0));
        arr.insert(4, 1, Clip::new(0, 500));
        let e = Engine::new();
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 2000);
        let out = e.render_project().unwrap();
        assert!((out[250] - 0.5).abs() < 1e-6);
        assert!((out[750] - 1.0).abs() < 1e-6, "lanes sum additively");
    }

    #[test]
    fn render_empty_project_is_none() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1000);
        assert!(e.render_project().is_none());
        assert!(e.render_track(0).is_none());
    }

    #[test]
    fn render_applies_effect_chain() {
        if !crate::fx::rustc_available() {
            return; // environments without a compiler skip the loader
        }
        let dir = std::env::temp_dir().join(format!("slooper-fxrender-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::fx::TEMPLATE_FILE), crate::fx::TEMPLATE_SRC).unwrap();
        let mut reg = crate::fx::Registry::open(dir.clone());
        reg.rescan();
        assert!(reg.desc("template").is_some(), "template must load");

        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 4000]);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 4000);
        let desc = reg.desc("template").unwrap().clone();

        // No chain: the render is the content itself (trim = 1).
        let plain = e.render_project().unwrap();
        assert!(plain.iter().take(4000).all(|&v| (v - 0.5).abs() < 1e-6));

        // Template at depth 0: exact pass-through, per the ABI contract.
        let inst0 = reg
            .create_instance("template", 48000.0, &[0.0, 4.0, 1.0, 1.0])
            .unwrap();
        e.set_track_fx(
            0,
            TrackFx {
                slots: std::sync::Arc::new(vec![FxSlot::scripted(inst0, desc.clone())]),
            },
        );
        assert_eq!(e.fx_gen(), 1);
        let thru = e.render_project().unwrap();
        assert!(
            thru.iter().take(4000).all(|&v| (v - 0.5).abs() < 1e-3),
            "depth 0 must pass through"
        );

        // depth 1 / mix 1: the tremolo must dip the render (a full LFO
        // cycle lands inside the 4000-sample window at 4 Hz).
        let inst1 = reg
            .create_instance("template", 48000.0, &[1.0, 4.0, 1.0, 1.0])
            .unwrap();
        e.set_track_fx(
            0,
            TrackFx {
                slots: std::sync::Arc::new(vec![FxSlot::scripted(inst1.clone(), desc)]),
            },
        );
        assert_eq!(e.fx_gen(), 2);
        let wet = e.render_project().unwrap();
        let lo = wet.iter().take(4000).copied().fold(f32::MAX, f32::min);
        assert!(lo < 0.45, "tremolo must modulate the render (min was {lo})");
        // Rendering ran on private clones: the live instance is untouched.
        assert_eq!(inst1.param(0), 1.0);
        assert_eq!(e.fx_table()[0].slots().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_project_bounds_the_playhead() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 10_000);
        e.seek_to(9_000);
        // A project can shrink under the playhead — it clamps to the new end.
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1_000);
        assert_eq!(e.pos(), 1_000);
        assert_eq!(e.proj_len(), 1_000);
    }

    #[test]
    fn metro_defaults_roundtrip() {
        let e = Engine::new();
        assert_eq!(e.metro(), (false, 120, 1, 4));
    }

    // ===================== v0.10 acceptance pins =====================

    /// AC8.1/8.2: the audition pool is additive — every start is a new
    /// voice, and the same asset twice is two voices with distinct ids.
    #[test]
    fn v10_preview_voices_play_in_parallel() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 100]);
        let b: Arc<[f32]> = Arc::from(vec![0.25f32; 80]);
        e.play_preview(1, a.clone());
        e.play_preview(2, b.clone());
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 2, "two starts = two simultaneous voices");
        assert_eq!(snap[0].1, 1);
        assert_eq!(snap[1].1, 2);
        assert_ne!(snap[0].0, snap[1].0, "voice ids are unique");
        assert!(e.preview_active());
    }

    #[test]
    fn v10_preview_same_asset_twice_is_two_voices() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 100]);
        e.play_preview(7, a.clone());
        e.play_preview(7, a);
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 2, "a repeat of the SAME asset stacks, never cuts off");
        assert_ne!(snap[0].0, snap[1].0);
    }

    #[test]
    fn v10_preview_voices_retire_and_stop_clears() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 40]);
        let b: Arc<[f32]> = Arc::from(vec![0.5f32; 40]);
        e.play_preview(3, a.clone());
        e.play_preview(4, b.clone());
        // Rack toggle stops ONLY that asset's voices (AC8.4 semantics).
        e.stop_preview_id(3);
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 1, "stopping asset 3 leaves asset 4 auditioning");
        assert_eq!(snap[0].1, 4);
        assert!(e.preview_active(), "a voice is still playing");
        e.stop_preview_id(4);
        assert!(e.preview_snapshot().is_empty());
        assert!(!e.preview_active(), "nothing left: auditions fully stopped");
        // Global stop clears everything at once.
        e.play_preview(5, a.clone());
        e.play_preview(6, a);
        e.stop_preview();
        assert!(e.preview_snapshot().is_empty());
        assert!(!e.preview_active());
    }

    #[test]
    fn v10_preview_voice_pool_is_bounded() {
        let e = Engine::new();
        for i in 0..30u32 {
            let a: Arc<[f32]> = Arc::from(vec![0.05f32; 20]);
            e.play_preview(i, a);
        }
        let snap = e.preview_snapshot();
        assert!(snap.len() <= 12, "pool stays bounded: {}", snap.len());
        // Oldest-first eviction: the most recent start survives.
        assert_eq!(snap.last().unwrap().1, 29);
    }

    /// AC7.2: per-block work scales with ACTIVE tracks, not the 128
    /// capacity — the compact lists must hold exactly the sounding ones.
    #[test]
    fn v10_active_track_list_compact() {
        let mut audible = [true; MAX_LOOPS];
        let no = [false; MAX_LOOPS];
        let mut midi_live = [false; MAX_LOOPS];
        let mut chain = [false; MAX_LOOPS];

        // One clip on track 70 in a 128-track project.
        let l = build_mix_lists(1u128 << 70, false, &midi_live, &audible, &no, &chain);
        assert_eq!(l.n_mix, 1, "only the clip track reaches the mix loop");
        assert_eq!(l.mix[0], 70);
        assert_eq!(l.n_acc, 1);

        // Live MIDI on 9 and an FX chain tail on 5 add exactly two more.
        midi_live[9] = true;
        chain[5] = true;
        let l = build_mix_lists(1u128 << 70, true, &midi_live, &audible, &no, &chain);
        assert_eq!(l.n_mix, 3);
        assert_eq!(l.n_acc, 2, "tail track accumulates nothing");

        // Inaudible (track-off) contributes nothing at all.
        audible = [false; MAX_LOOPS];
        let l = build_mix_lists(1u128 << 70, true, &midi_live, &audible, &no, &chain);
        assert_eq!(l.n_mix, 0);
    }

    /// AC7.1: tracks past the old 16 limit are real: track 64 plays, and
    /// its play flag mutes it like any other track.
    #[test]
    fn v10_track_64_plays_and_mutes() {
        let a: Arc<[f32]> = Arc::from((0..1000).map(|k| ((k % 40) as f32) * 0.01 - 0.2).collect::<Vec<f32>>());
        let mut arr = Arrangement::new();
        arr.insert(64, 0, Clip::new(0, 0));
        let e = Engine::new();
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 1000);
        let out = e.render_project().expect("render");
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.05, "clip on track 64 must sound (peak {peak})");

        e.set_track_on(64, false);
        let out = e.render_project().unwrap_or_default();
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak < 1e-9, "stopped track 64 must be silent, got {peak}");
    }

    /// AC6.1 (sample side): a track's audio clip renders EXACTLY its
    /// samples — no synth, no doubling — even when the same engine also
    /// holds a MIDI clip elsewhere.
    #[test]
    fn v10_audio_clip_is_sample_only_midi_is_synth_only() {
        let a: Arc<[f32]> = Arc::from((0..600).map(|k| (k as f32) * 0.001 - 0.3).collect::<Vec<f32>>());
        // Pattern asset (id 1) on track 2: the SAMPLE path must ignore it
        // entirely — pattern audio comes only through the track's synth,
        // which the head render never runs.
        let pat = crate::midipat::Pattern::new(120);
        let mut arr = Arrangement::new();
        arr.insert(1, 0, Clip::new(0, 0));
        arr.insert(2, 0, Clip::new(1, 0));
        let e = Engine::new();
        e.set_project(Arc::new(vec![a.clone(), Arc::from(Vec::<f32>::new())]), Arc::new(arr), 600);
        e.set_midi_assets(Arc::new(vec![None, Some(std::sync::Arc::new(pat))]));
        let out = e.render_project().expect("render");
        assert_eq!(out.len(), 600);
        for (i, want) in a.iter().enumerate() {
            assert!(
                (out[i] - want).abs() < 1e-6,
                "sample {i}: {} vs {want} — audio clip must render exactly its samples",
                out[i]
            );
        }
        // A pattern clip contributes zero samples to the render.
        let mut arr2 = Arrangement::new();
        arr2.insert(2, 0, Clip::new(1, 0));
        let e2 = Engine::new();
        e2.set_project(Arc::new(vec![a]), Arc::new(arr2), 600);
        e2.set_midi_assets(Arc::new(vec![None, Some(std::sync::Arc::new(crate::midipat::Pattern::new(120)))]));
        let out = e2.render_project().unwrap_or_default();
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak < 1e-9, "a MIDI clip never renders raw samples (peak {peak})");
    }


    /// AC1.3: the full user flow with NO re-selection ritual: bind a fresh
    /// sampler to asset id 2, play a note (must sound); then the user drags
    /// a NEW sample in under the same asset id and plays again — the new
    /// content sounds immediately. Both stages use the SAME instance, i.e.
    /// exactly what the track keeps while the user swaps samples.
    #[test]
    fn v10_sampler_binding_needs_no_ritual() {
        let dir = std::env::temp_dir().join(format!("sp-ritual-{}", std::process::id()));
        std::env::set_var("SLOOPER_SYNTHS_DIR", &dir);
        let reg = crate::synth::Registry::open(dir.clone());
        std::env::remove_var("SLOOPER_SYNTHS_DIR");

        let table = |v: f32| {
            let mut t: Vec<crate::assets::AssetData> =
                (0..3).map(|_| std::sync::Arc::from(Vec::<f32>::new())).collect();
            t[2] = std::sync::Arc::from(vec![v; 1200]);
            std::sync::Arc::new(t)
        };
        crate::synth::set_host_assets(table(0.85), 48_000);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[2.0, 48.0, 1.0])
            .expect("fresh sampler binds without ritual");
        let render = |inst: &crate::synth::Instance| {
            let mut ev = vec![(0u64, 3u8, 0u8, 60u8, 110u8), (30_000, 0, 0, 60, 0)];
            let mut out = vec![0f32; 48_000 / 8];
            render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
            out.iter().fold(0f32, |m, x| m.max(x.abs()))
        };
        let first = render(&inst);
        assert!(first > 0.5, "first bound sample must sound (peak {first})");
        // User drags a new sample in (same id, new contents) — play again.
        crate::synth::set_host_assets(table(0.05), 48_000);
        let second = render(&inst);
        assert!(
            second < first / 8.0 && second > 0.01,
            "new sample must sound without re-selecting the synth: {second} vs {first}"
        );
        crate::synth::set_host_assets(std::sync::Arc::new(Vec::new()), 48_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC6.2: audition voices are additive AND isolated per asset: stopping
    /// one asset never touches another voice, and the same asset stacks.
    #[test]
    fn v10_audition_voices_are_additive_and_isolated() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 500]);
        let b: Arc<[f32]> = Arc::from(vec![0.5f32; 500]);
        e.play_preview(1, a.clone());
        e.play_preview(1, a.clone()); // repeat of the same asset stacks
        e.play_preview(2, b);
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 3, "three additive voices live");
        e.stop_preview_id(1);
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 1, "asset 1's voices all stop; asset 2 survives");
        assert_eq!(snap[0].1, 2);
        e.stop_preview_id(99); // unknown id: no-op, never panics
        assert_eq!(e.preview_snapshot().len(), 1);
    }

}
