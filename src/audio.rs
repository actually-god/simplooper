//! JACK/PipeWire-JACK audio backend for simplooper.
//!
//! A dedicated non-realtime "audio manager" thread owns the `jack::Client`.
//! The GUI talks to it over an mpsc command channel and receives state
//! updates (port lists, MIDI events, status) on a second channel.
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

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;

use jack::jack_sys as j;
use jack::{
    AudioIn, AudioOut, Client, ClientOptions, ClientStatus, Control, MidiIn, NotificationHandler,
    Port, PortFlags, PortId, ProcessHandler, ProcessScope,
};

pub const CLIENT_NAME: &str = "simplooper";
const MIDI_PORT_SHORT: &str = "midi_in";
/// Fixed pool of mix slots; `Apply` maps the selected ports onto slots 0..n-1.
pub const MAX_SLOTS: usize = 16;

/// Number of simultaneous loops (tracks).
pub const MAX_LOOPS: usize = 16;
/// Per-track recording capacity: ~60 s @ 48 kHz (~30 s @ 96 kHz).
pub const LOOP_CAP: u32 = 2_880_000;

// Track states (AtomicU8 in the engine, mirrored to the GUI via Evt::LoopState).
pub const TR_IDLE: u8 = 0;
pub const TR_REC: u8 = 1;
pub const TR_PLAY: u8 = 2;
pub const TR_OD: u8 = 3; // overdub = playing + writing live input

/// A remote audio port the user can select (full name is `client:port`).
#[derive(Clone, Debug, PartialEq)]
pub struct PortInfo {
    pub name: String,
    pub client: String,
}

/// Raw MIDI message copied out of the realtime thread.
#[derive(Debug)]
pub struct RawMidiMsg {
    pub bytes: Vec<u8>,
}

/// Commands from the GUI to the audio manager thread.
pub enum Cmd {
    /// Re-enumerate remote audio ports and send them back.
    Refresh,
    /// Connect the selected external ports to mix slots 0..n-1 (disconnecting
    /// whatever was there before). Empty lists stop all audio; MIDI monitoring
    /// keeps running. Selections beyond `MAX_SLOTS` are ignored (with an error).
    Apply { ins: Vec<String>, outs: Vec<String> },
    /// Toggle recording on a loop track. From IDLE/PLAY this starts a fresh
    /// capture (previous content is discarded); from REC it stops and keeps
    /// the captured loop playing.
    ToggleRecord { i: usize },
    /// Toggle overdubbing on a loop track (playing + writing live input).
    ToggleOverdub { i: usize },
    /// Start/stop playback of a loop track.
    TogglePlay { i: usize },
    /// Undo the most recent overdub (across all tracks): the track it was
    /// dubbed over reverts to the content it had before that overdub started.
    /// The undone state goes on the redo stack.
    UndoOverdub,
    /// Re-apply the most recently undone overdub.
    RedoOverdub,
    /// Stop and erase a loop track.
    ClearTrack { i: usize },
    /// Replace track i's recording with `data` (decoded, engine-rate mono
    /// samples; anything beyond the capacity is truncated). The track stops
    /// first — importing replaces whatever was recorded before.
    ImportTrack { i: usize, data: Vec<f32> },
    /// Write one loop's recording to `path` in format `fmt`. Runs on the
    /// manager thread (the GUI stays responsive; a Notice/Error event reports
    /// the outcome).
    ExportTrack {
        i: usize,
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
    },
    /// Render and write the whole dependent-mode ensemble (every loop at
    /// position 0 for `root_loops` periods of the longest loop) to `path`.
    /// Dependent mode is forced on before rendering — that is the only mode
    /// in which a coherent whole-project export exists.
    ExportProject {
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
    },
    /// true = dependent looping (finite ensemble clock), false = independent players.
    SetDependent(bool),
    /// Dependent-mode ensemble length in units of the longest ("root") loop,
    /// clamped to 1..=64. The ensemble period is N root loops: every track
    /// repeats within the period, and the clock wraps at the period end so
    /// the loops keep playing forever (changing N while playing restarts the
    /// ensemble from there).
    SetRootLoops(u32),
    /// Delay the whole output mix (monitoring + loops + metronome) by `ms`
    /// milliseconds to compensate for input/output latency. Clamped to
    /// 0..=1000 ms.
    SetLatency { ms: f32 },
    /// Metronome: on/off, tempo in BPM, subdivision in clicks per beat
    /// (1 = quarter, 2 = eighth, 3 = triplet, 4 = sixteenth, ...), and beats
    /// per measure (the time-signature numerator; beat 1 of every measure
    /// gets the downbeat tone). Three distinct tones: downbeat, quarter note
    /// and subdivision. The click goes to every selected output but is never
    /// recorded into a loop.
    SetMetro { on: bool, bpm: u32, sub: u32, num: u32 },
    /// Easy-quantize every loop against the chosen root loop's recorded length
    /// (or, with `metro = true`, against the metronome grid: beat = 60/bpm
    /// seconds, bar = beat x `num`; `bars` is then unused).
    Quantize {
        num: u32,
        den: u32,
        bars: u32,
        unit: crate::quantize::Unit,
        /// Which loop's recorded length defines the grid (0 = Loop 1).
        root: usize,
        /// true = quantize to the metronome tempo instead of the root loop.
        metro: bool,
        /// Metronome tempo in BPM (used only when `metro` is set).
        bpm: u32,
    },
    Quit,
}

/// Events from the audio manager thread to the GUI.
#[derive(Debug)]
pub enum Evt {
    /// No JACK/PipeWire-JACK server available.
    Unavailable(String),
    Ready { client_name: String, sample_rate: u32 },
    Ports { ins: Vec<PortInfo>, outs: Vec<PortInfo> },
    Applied { ins: usize, outs: usize },
    /// Snapshot of all loop tracks (state + recorded length in samples).
    LoopState { states: [u8; MAX_LOOPS], lens: [u32; MAX_LOOPS] },
    /// A quantize pass finished; `summary` describes the grid and results.
    Quantized { summary: String },
    /// Overdub undo/redo availability changed (an overdub started, an
    /// undo/redo ran, or a content-discarding command invalidated history).
    UndoState { can_undo: bool, can_redo: bool },
    Midi(RawMidiMsg),
    /// Remote MIDI source ports currently wired into `simplooper:midi_in`
    /// (auto-connected; refreshed on port/client registration events).
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
    /// Shared loop engine (GUI reads buffers for waveform rendering).
    pub engine: Arc<LoopEngine>,
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
        let engine = LoopEngine::new();
        let (t_gain, t_peak, t_xruns, t_midi_drops, t_engine) = (
            gain.clone(),
            peak.clone(),
            xruns.clone(),
            midi_drops.clone(),
            engine.clone(),
        );

        let thread = std::thread::Builder::new()
            .name("simplooper-audio".into())
            .spawn(move || {
                run(cmd_rx, evt_tx, t_gain, t_peak, t_xruns, t_midi_drops, t_engine);
            })
            .expect("failed to spawn audio thread");

        Backend { cmd_tx, evt_rx, gain, peak, xruns, midi_drops, engine, thread: Some(thread) }
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

/// The loop engine: four pre-allocated recording buffers plus atomic
/// per-track state/length, shared between the manager thread (commands) and
/// the realtime handler (capture/playback).
///
/// Concurrency discipline:
/// - The audio thread is the only writer of `len` while a track records
///   (`fetch_add`), and reads/writes sample data only for tracks whose state
///   it snapshotted as non-IDLE.
/// - The manager thread mutates buffers (clear/quantize) only after setting
///   the track IDLE and letting one or two audio cycles elapse; `start_rec`
///   zeroes the buffer *before* arming REC so RT never captures stale data.
pub struct LoopEngine {
    bufs: [*mut f32; MAX_LOOPS],
    state: [AtomicU8; MAX_LOOPS],
    len: [AtomicU32; MAX_LOOPS],
    /// 0 = independent players, 1 = dependent (ensemble) mode.
    mode: AtomicU8,
    /// Bumped whenever the manager rewrites buffer contents (clear/quantize)
    /// so the GUI can invalidate its waveform caches.
    gen: AtomicU64,
    /// Per-track read offset within [0, len) — published by the RT thread for
    /// the GUI playback bars (independent mode).
    pos: [AtomicU64; MAX_LOOPS],
    /// Dependent-mode ensemble clock (samples) — shared playback-bar position.
    master_pos: AtomicU64,
    /// Dependent-mode ensemble length in units of the longest loop (>= 1).
    root_loops: AtomicU32,
    /// Output delay in samples (latency compensation; 0 = off).
    latency: AtomicU32,
    /// Metronome: 0 = off, 1 = on; tempo (BPM); clicks per beat; beats per
    /// measure (time-signature numerator, default 4).
    metro_on: AtomicU8,
    metro_bpm: AtomicU32,
    metro_sub: AtomicU32,
    metro_num: AtomicU32,
}

impl LoopEngine {
    pub fn new() -> Arc<Self> {
        let mut bufs = [std::ptr::null_mut::<f32>(); MAX_LOOPS];
        for buf in bufs.iter_mut() {
            let mut v = vec![0f32; LOOP_CAP as usize];
            *buf = v.as_mut_ptr();
            std::mem::forget(v); // ownership transfers to the engine
        }
        Arc::new(Self {
            bufs,
            state: core::array::from_fn(|_| AtomicU8::new(TR_IDLE)),
            len: core::array::from_fn(|_| AtomicU32::new(0)),
            mode: AtomicU8::new(0),
            gen: AtomicU64::new(0),
            pos: core::array::from_fn(|_| AtomicU64::new(0)),
            master_pos: AtomicU64::new(0),
            root_loops: AtomicU32::new(1),
            latency: AtomicU32::new(0),
            metro_on: AtomicU8::new(0),
            metro_bpm: AtomicU32::new(120),
            metro_sub: AtomicU32::new(1),
            metro_num: AtomicU32::new(4),
        })
    }

    // ---- realtime access (audio thread) ---------------------------------

    #[inline]
    pub fn read(&self, i: usize, off: u32) -> f32 {
        unsafe { *self.bufs[i].add(off as usize) }
    }

    #[inline]
    pub fn write(&self, i: usize, off: u32, v: f32) {
        unsafe { self.bufs[i].add(off as usize).write(v) };
    }

    /// Capture one dry-mixed sample into track i while recording. Returns
    /// true once the buffer is full (caller should auto-stop the take). The
    /// length counter never exceeds LOOP_CAP: the check happens before
    /// advancing, so a loop that hits capacity mid-frame can't overshoot —
    /// every consumer (UI waveform, playback reads) assumes len <= CAP.
    #[inline]
    pub fn rec_sample(&self, i: usize, v: f32) -> bool {
        let off = self.len[i].load(Ordering::Relaxed) as u64;
        if off < LOOP_CAP as u64 {
            self.write(i, off as u32, v);
            self.len[i].store((off + 1) as u32, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    // ---- manager access (track must be quiescent — see command docs) -----

    pub fn state_of(&self, i: usize) -> u8 {
        self.state[i].load(Ordering::Relaxed)
    }

    pub fn len_of(&self, i: usize) -> u32 {
        self.len[i].load(Ordering::Relaxed)
    }

    /// Stop a track and erase its buffer. Safe to call from the manager
    /// thread: IDLE is stored first so RT stops touching the track before the
    /// zero-fill begins (worst case: a few glitch samples at the boundary).
    pub fn clear(&self, i: usize) {
        self.state[i].store(TR_IDLE, Ordering::Relaxed);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.bufs[i], LOOP_CAP as usize) };
        buf.fill(0.0f32);
        self.len[i].store(0, Ordering::Relaxed);
        self.gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Stop track i and replace its recording with `data` (engine-rate mono
    /// samples, truncated to capacity). Safe from the manager thread: IDLE is
    /// stored first so RT stops reading before the buffer is rewritten.
    pub fn import_track(&self, i: usize, data: &[f32]) {
        self.state[i].store(TR_IDLE, Ordering::Relaxed);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.bufs[i], LOOP_CAP as usize) };
        let n = data.len().min(LOOP_CAP as usize);
        buf[..n].copy_from_slice(&data[..n]);
        buf[n..].fill(0.0f32);
        self.pos[i].store(0, Ordering::Relaxed);
        self.len[i].store(n as u32, Ordering::Relaxed);
        self.gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the full dependent-mode ensemble — every track at position 0,
    /// for `root_loops` periods of the longest loop — to mono f32 samples.
    /// Used for whole-project export; None when no track has content.
    pub fn render_ensemble(&self, root_loops: usize) -> Option<Vec<f32>> {
        let lens = self.lens();
        let max_len = lens.iter().max().copied().unwrap_or(0);
        if max_len == 0 {
            return None;
        }
        let periods = (root_loops.max(1) as u32 * max_len) as u64;
        let mut out: Vec<f32> = vec![0.0f32; periods as usize];
        for (t, &l32) in lens.iter().enumerate() {
            let l = l32 as u64;
            if l == 0 {
                continue;
            }
            let buf = unsafe { std::slice::from_raw_parts(self.bufs[t], LOOP_CAP as usize) };
            for p in 0..(periods / max_len as u64) {
                let base = p * max_len as u64;
                let start = (base % l) as usize;
                let end = ((base + max_len as u64) % l) as usize;
                if end > start {
                    for k in 0..(end - start) {
                        out[(base + k as u64) as usize] += buf[start + k];
                    }
                } else {
                    let first = (l - start as u64) as usize;
                    for k in 0..first {
                        out[(base + k as u64) as usize] += buf[start + k];
                    }
                    for k in 0..end {
                        out[(base + first as u64 + k as u64) as usize] += buf[k];
                    }
                }
            }
        }
        Some(out)
    }

    /// Snapshot of all track lengths (samples).
    pub fn lens(&self) -> [u32; MAX_LOOPS] {
        core::array::from_fn(|i| self.len[i].load(Ordering::Relaxed))
    }

    /// Clone of track `i`'s recorded content (length + samples). Safe from
    /// the manager thread while the track is not REC/OD: the RT thread only
    /// reads buffer data during PLAY.
    pub fn snapshot(&self, i: usize) -> (u32, Vec<f32>) {
        let l = self.len_of(i);
        (l, self.read_range(i, l).to_vec())
    }

    /// Zero the buffer and arm a fresh capture on track i.
    pub fn start_rec(&self, i: usize) {
        self.state[i].store(TR_IDLE, Ordering::Relaxed);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.bufs[i], LOOP_CAP as usize) };
        buf.fill(0.0f32);
        self.len[i].store(0, Ordering::Relaxed);
        self.state[i].store(TR_REC, Ordering::Relaxed);
    }

    /// Quantize every non-empty track in place (all tracks must be IDLE).
    /// Returns the new lengths.
    pub fn quantize_all(&self, target: u32, sr: u32) -> [u32; MAX_LOOPS] {
        let mut out = [0u32; MAX_LOOPS];
        for (i, o) in out.iter_mut().enumerate() {
            let l = self.len[i].load(Ordering::Relaxed);
            if l == 0 {
                continue;
            }
            let buf = unsafe { std::slice::from_raw_parts_mut(self.bufs[i], LOOP_CAP as usize) };
            *o = crate::quantize::quantize_track(buf, l, target, LOOP_CAP, sr);
            self.len[i].store(*o, Ordering::Relaxed);
        }
        self.gen.fetch_add(1, Ordering::Relaxed);
        out
    }

    /// Read up to `count` samples of track i (GUI waveform rendering).
    pub fn read_range(&self, i: usize, count: u32) -> &[f32] {
        unsafe { std::slice::from_raw_parts(self.bufs[i], (count as usize).min(LOOP_CAP as usize)) }
    }

    /// Current content-generation counter (GUI cache invalidation).
    pub fn generation(&self) -> u64 {
        self.gen.load(Ordering::Relaxed)
    }

    // ---- GUI / test accessors -------------------------------------------

    /// Per-track read offset within [0, len) — for the playback bar.
    pub fn pos_of(&self, i: usize) -> u64 {
        self.pos[i].load(Ordering::Relaxed)
    }

    /// Dependent-mode ensemble clock (samples).
    pub fn master_pos(&self) -> u64 {
        self.master_pos.load(Ordering::Relaxed)
    }

    /// Dependent-mode ensemble length in units of the longest loop (>= 1).
    pub fn root_loops(&self) -> u32 {
        self.root_loops.load(Ordering::Relaxed)
    }

    /// Output delay in samples (0 = latency compensation off).
    pub fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Metronome state: (on, bpm, clicks per beat, beats per measure).
    pub fn metro(&self) -> (bool, u32, u32, u32) {
        (
            self.metro_on.load(Ordering::Relaxed) != 0,
            self.metro_bpm.load(Ordering::Relaxed),
            self.metro_sub.load(Ordering::Relaxed),
            self.metro_num.load(Ordering::Relaxed),
        )
    }
}

impl Drop for LoopEngine {
    fn drop(&mut self) {
        use std::alloc::{dealloc, Layout};
        let layout = Layout::array::<f32>(LOOP_CAP as usize).expect("valid layout");
        for p in self.bufs.iter() {
            unsafe { dealloc(*p as *mut u8, layout) };
        }
    }
}

// The raw pointers are only ever dereferenced from the audio thread (sample
// R/W) or the manager thread while the owning track is quiescent; the Arc
// keeps the engine alive for as long as either thread exists.
unsafe impl Send for LoopEngine {}
unsafe impl Sync for LoopEngine {}

/// Snapshot of all tracks, for Evt::LoopState.
fn loop_state(engine: &LoopEngine) -> ([u8; MAX_LOOPS], [u32; MAX_LOOPS]) {
    let mut states = [0u8; MAX_LOOPS];
    let mut lens = [0u32; MAX_LOOPS];
    for i in 0..MAX_LOOPS {
        states[i] = engine.state_of(i);
        lens[i] = engine.len_of(i);
    }
    (states, lens)
}

fn send_loop_state(engine: &LoopEngine, tx: &SyncSender<Evt>) {
    let (states, lens) = loop_state(engine);
    let _ = tx.send(Evt::LoopState { states, lens });
}

fn send_undo_state(hist: &OdHistory, tx: &SyncSender<Evt>) {
    let _ = tx.try_send(Evt::UndoState {
        can_undo: hist.can_undo(),
        can_redo: hist.can_redo(),
    });
}

/// Max overdub snapshots kept per stack. Each snapshot is one track buffer
/// (up to LOOP_CAP floats), so 8 deep costs at most ~92 MB per stack in the
/// absolute worst case (sixteen 60 s tracks) and far less in practice.
pub const MAX_OD_HISTORY: usize = 8;

/// One overdub-undo entry: a track's full content plus whether it was
/// playing, captured right before an overdub started (or, for a redo entry,
/// the state captured at undo time).
struct OdSnapshot {
    track: usize,
    data: Vec<f32>,
    playing: bool,
}

/// Global overdub undo/redo history — manager-thread state only, owned by
/// the audio manager loop.
///
/// Every overdub start pushes the dubbed track's pre-overdub buffer onto the
/// undo stack (and clears the redo stack, like any fresh edit). Undo stops
/// the track, lets the RT thread quiesce, swaps the buffer for the snapshot,
/// and pushes the overdubbed state onto the redo stack; redo swaps back.
/// Commands that discard or rewrite a track's content (fresh record, clear,
/// import, quantize) invalidate that track's entries so an undo can never
/// resurrect audio the user deliberately threw away.
#[derive(Default)]
pub struct OdHistory {
    undo: Vec<OdSnapshot>,
    redo: Vec<OdSnapshot>,
}

impl OdHistory {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// An overdub is starting on track `i`: capture its current content.
    /// MUST be called before arming TR_OD — while the track still reads as
    /// PLAY/IDLE the RT thread never writes its buffer.
    pub fn begin(&mut self, engine: &LoopEngine, i: usize) {
        let playing = engine.state_of(i) == TR_PLAY;
        let (_, data) = engine.snapshot(i);
        self.undo.push(OdSnapshot { track: i, data, playing });
        while self.undo.len() > MAX_OD_HISTORY {
            self.undo.remove(0);
        }
        // A fresh edit invalidates the redo branch.
        self.redo.clear();
    }

    /// Undo the most recent overdub. Returns the affected track index, or
    /// None when the undo stack is empty.
    pub fn undo(&mut self, engine: &LoopEngine) -> Option<usize> {
        let s = self.undo.pop()?;
        let t = s.track;
        Self::swap_in(engine, s, &mut self.redo);
        Some(t)
    }

    /// Re-apply the most recently undone overdub.
    pub fn redo(&mut self, engine: &LoopEngine) -> Option<usize> {
        let s = self.redo.pop()?;
        let t = s.track;
        Self::swap_in(engine, s, &mut self.undo);
        Some(t)
    }

    /// Drop every entry for track `i` (its content was discarded/rewritten).
    pub fn invalidate_track(&mut self, i: usize) {
        self.undo.retain(|s| s.track != i);
        self.redo.retain(|s| s.track != i);
    }

    /// Drop everything (a quantize rewrote every buffer in place).
    pub fn clear_all(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Restore snapshot `s` into its track and push the state being replaced
    /// onto `stack` (the opposite direction's stack).
    fn swap_in(engine: &LoopEngine, s: OdSnapshot, stack: &mut Vec<OdSnapshot>) {
        // Was the track audible before we stop it? (Undo during a live
        // overdub counts as playing.)
        let cur_playing = matches!(engine.state_of(s.track), TR_PLAY | TR_OD);
        // Stop first, then let the RT thread quiesce before rewriting the
        // buffer (same discipline as clear/quantize).
        engine.state[s.track].store(TR_IDLE, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (_, cur_data) = engine.snapshot(s.track);
        stack.push(OdSnapshot { track: s.track, data: cur_data, playing: cur_playing });
        while stack.len() > MAX_OD_HISTORY {
            stack.remove(0);
        }
        engine.import_track(s.track, &s.data);
        engine.state[s.track].store(
            if s.playing && !s.data.is_empty() { TR_PLAY } else { TR_IDLE },
            Ordering::Relaxed,
        );
    }
}

/// Manager-side record toggle: start a fresh capture, or stop and keep the
/// captured loop playing.
fn toggle_record(engine: &LoopEngine, i: usize) {
    match engine.state_of(i) {
        TR_REC => {
            let st = if engine.len_of(i) > 0 { TR_PLAY } else { TR_IDLE };
            engine.state[i].store(st, Ordering::Relaxed);
        }
        TR_OD => engine.state[i].store(TR_PLAY, Ordering::Relaxed),
        _ => {
            // IDLE or PLAY: fresh capture (previous content is discarded).
            engine.start_rec(i);
        }
    }
}

fn run(
    cmd_rx: Receiver<Cmd>,
    evt_tx: SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    xruns: Arc<AtomicU64>,
    midi_drops: Arc<AtomicU64>,
    engine: Arc<LoopEngine>,
) {
    let (client, status) = match Client::new(CLIENT_NAME, ClientOptions::NO_START_SERVER) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("No JACK/PipeWire-JACK server available: {}", describe_err(e));
            eprintln!("[simplooper] {}", msg);
            let _ = evt_tx.send(Evt::Unavailable(msg));
            return;
        }
    };
    if status.contains(ClientStatus::NAME_NOT_UNIQUE) {
        eprintln!("[simplooper] client renamed to '{}'", client.name());
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
    let mask_out = Arc::new(AtomicU64::new(0));
    let notif = Notif { tx: evt_tx.clone(), xruns: xruns.clone() };
    // Sample rate is queryable before activation; the metronome and the
    // latency delay line are sized from it.
    let sr = client.sample_rate();
    let delay_n = (sr.max(1) as usize).next_power_of_two();
    let proc = Proc {
        midi_in: midi_port,
        ports: RawPorts { ins: ins_raw, outs: outs_raw },
        mask_in: mask_in.clone(),
        mask_out: mask_out.clone(),
        tx: evt_tx.clone(),
        midi_drops,
        gain: gain.clone(),
        peak: peak.clone(),
        engine: engine.clone(),
        prev_state: [TR_IDLE; MAX_LOOPS],
        pos: [0u64; MAX_LOOPS],
        master_pos: 0,
        prev_mode: 0,
        prev_root: 1,
        sr,
        delay: vec![0.0f32; delay_n],
        didx: 0,
        metro_next: 0,
        metro_start: -1,
        metro_beat_start: 0,
        metro_beat_len: 0,
        metro_beat: 0,
        metro_k: 0,
        metro_sub_eff: 1,
        metro_tone: 0,
        prev_metro_on: false,
        sample_no: 0,
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
        "[simplooper] JACK client '{}' active @ {} Hz",
        client_name, sample_rate
    );
    let _ = evt_tx.send(Evt::Ready { client_name: client_name.clone(), sample_rate });
    send_ports(active.as_client(), &evt_tx);

    // Remote MIDI sources currently wired into our midi_in port. Devices often
    // reappear on a fresh port name every connect, so this set is re-derived
    // periodically and on registration events (see rescan_midi_sources).
    let mut midi_sources: Vec<String> = Vec::new();

    // What each slot is currently connected to (manager-thread state only).
    let mut cur_in: Vec<Option<String>> = vec![None; MAX_SLOTS];
    let mut cur_out: Vec<Option<String>> = vec![None; MAX_SLOTS];

    // Overdub undo/redo history (manager-thread state only).
    let mut hist = OdHistory::default();

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
                    &mask_out,
                    &evt_tx,
                );
            }
            Ok(Cmd::ToggleRecord { i }) => {
                toggle_record(&engine, i);
                // A fresh capture discards the old content — any history
                // entry for this track would resurrect discarded audio.
                if engine.state_of(i) == TR_REC {
                    hist.invalidate_track(i);
                    send_undo_state(&hist, &evt_tx);
                }
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::ToggleOverdub { i }) => {
                match engine.state_of(i) {
                    TR_OD => engine.state[i].store(TR_PLAY, Ordering::Relaxed),
                    TR_PLAY => {
                        // Snapshot BEFORE arming OD (RT must not be writing).
                        hist.begin(&engine, i);
                        engine.state[i].store(TR_OD, Ordering::Relaxed);
                    }
                    TR_IDLE if engine.len_of(i) > 0 => {
                        hist.begin(&engine, i);
                        engine.state[i].store(TR_OD, Ordering::Relaxed);
                    }
                    _ => {} // IDLE with no content, or REC: ignore
                }
                send_undo_state(&hist, &evt_tx);
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::UndoOverdub) => match hist.undo(&engine) {
                Some(t) => {
                    let _ = evt_tx.send(Evt::Notice(format!("undid overdub on loop {}", t + 1)));
                    send_undo_state(&hist, &evt_tx);
                    send_loop_state(&engine, &evt_tx);
                }
                None => {
                    let _ = evt_tx.send(Evt::Notice("nothing to undo".into()));
                }
            },
            Ok(Cmd::RedoOverdub) => match hist.redo(&engine) {
                Some(t) => {
                    let _ = evt_tx.send(Evt::Notice(format!("redid overdub on loop {}", t + 1)));
                    send_undo_state(&hist, &evt_tx);
                    send_loop_state(&engine, &evt_tx);
                }
                None => {
                    let _ = evt_tx.send(Evt::Notice("nothing to redo".into()));
                }
            },
            Ok(Cmd::TogglePlay { i }) => {
                match engine.state_of(i) {
                    TR_IDLE if engine.len_of(i) > 0 => {
                        engine.state[i].store(TR_PLAY, Ordering::Relaxed)
                    }
                    TR_PLAY | TR_OD => engine.state[i].store(TR_IDLE, Ordering::Relaxed),
                    TR_REC => {
                        // Committing the take via play: keep it playing if non-empty.
                        let st = if engine.len_of(i) > 0 { TR_PLAY } else { TR_IDLE };
                        engine.state[i].store(st, Ordering::Relaxed)
                    }
                    _ => {}
                }
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::ClearTrack { i }) => {
                engine.clear(i);
                hist.invalidate_track(i);
                send_undo_state(&hist, &evt_tx);
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::ImportTrack { i, data }) => {
                // The import dialog + decode already ran on the GUI thread;
                // this is just the buffer swap (track is quiesced by import).
                let i = i.min(MAX_LOOPS - 1);
                engine.import_track(i, &data);
                hist.invalidate_track(i);
                send_undo_state(&hist, &evt_tx);
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::ExportTrack { i, path, fmt }) => {
                let i = i.min(MAX_LOOPS - 1);
                let n = engine.len_of(i);
                if n == 0 {
                    let _ = evt_tx.send(Evt::Error(format!(
                        "export: loop {} has no recording",
                        i + 1
                    )));
                } else {
                    let samples = engine.read_range(i, n).to_vec();
                    match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                        Ok(()) => {
                            let _ = evt_tx.send(Evt::Notice(format!(
                                "exported loop {} → {}",
                                i + 1,
                                path.display()
                            )));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                        }
                    }
                }
            }
            Ok(Cmd::ExportProject { path, fmt }) => {
                // Whole-project export only exists in dependent mode — force
                // it on before rendering (the GUI checkbox mirrors this).
                engine.mode.store(1, Ordering::Relaxed);
                match engine.render_ensemble(engine.root_loops() as usize) {
                    Some(samples) => match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                        Ok(()) => {
                            let _ = evt_tx.send(Evt::Notice(format!(
                                "exported ensemble ({} samples) → {}",
                                samples.len(),
                                path.display()
                            )));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                        }
                    },
                    None => {
                        let _ = evt_tx.send(Evt::Error(
                            "export: no recorded audio in any loop".into(),
                        ));
                    }
                }
            }
            Ok(Cmd::SetDependent(dep)) => {
                engine.mode.store(if dep { 1 } else { 0 }, Ordering::Relaxed);
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::SetRootLoops(n)) => {
                engine.root_loops.store(n.clamp(1, 64), Ordering::Relaxed);
            }
            Ok(Cmd::SetLatency { ms }) => {
                let ms = ms.clamp(0.0, 1000.0);
                engine
                    .latency
                    .store((ms * sample_rate as f32 / 1000.0).round() as u32, Ordering::Relaxed);
            }
            Ok(Cmd::SetMetro { on, bpm, sub, num }) => {
                engine.metro_on.store(on as u8, Ordering::Relaxed);
                engine.metro_bpm.store(bpm.clamp(20, 300), Ordering::Relaxed);
                engine.metro_sub.store(sub.clamp(1, 16), Ordering::Relaxed);
                engine.metro_num.store(num.clamp(1, 16), Ordering::Relaxed);
            }
            Ok(Cmd::Quantize { num, den, bars, unit, root, metro, bpm }) => {
                // Grid source: the metronome tempo, or the root loop's
                // recorded length (`bars` is unused in metronome mode).
                let grid = if metro {
                    if bpm == 0 || num == 0 {
                        Err("quantize to metronome: tempo and numerator must be at least 1".into())
                    } else {
                        let beat = 60.0 * sample_rate as f64 / bpm as f64;
                        let bar = beat * num as f64;
                        let target_f = match unit {
                            crate::quantize::Unit::Beat => beat,
                            crate::quantize::Unit::Bar => bar,
                        };
                        if target_f < 0.5 {
                            Err("grid too fine: raise the tempo or the numerator".into())
                        } else {
                            Ok(((target_f + 0.5) as u32, beat, bar))
                        }
                    }
                } else if den == 0 || num == 0 || bars == 0 {
                    Err("quantize: numerator, denominator and bars must be at least 1".into())
                } else {
                    let len1 = engine.len_of(root.min(MAX_LOOPS - 1));
                    crate::quantize::compute_target(len1, bars, num, unit)
                };
                match grid {
                    Ok((target, beat, bar)) => {
                        // Stop everything and let the audio thread quiesce
                        // before buffers are rewritten in place.
                        for i in 0..MAX_LOOPS {
                            engine.state[i].store(TR_IDLE, Ordering::Relaxed);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        let lens = engine.quantize_all(target, sample_rate);
                        // Every buffer was rewritten in place — no overdub
                        // snapshot still describes its track.
                        hist.clear_all();
                        send_undo_state(&hist, &evt_tx);
                        let eff_bpm = 60.0 / (beat / sample_rate as f64);
                        let unit_name = match unit {
                            crate::quantize::Unit::Beat => "beat",
                            crate::quantize::Unit::Bar => "bar",
                        };
                        let mut parts: Vec<String> = Vec::new();
                        for (i, &l) in lens.iter().enumerate() {
                            if l > 0 {
                                parts.push(format!(
                                    "L{} → {} smp ({}×{unit_name})",
                                    i + 1,
                                    l,
                                    (l as f64 / target as f64).round()
                                ));
                            }
                        }
                        let grid_desc = if metro {
                            format!("metronome {bpm} BPM, {num}/{den}")
                        } else {
                            format!(
                                "{num}/{den}, {bars} bar(s), root loop {}, ≈{eff_bpm:.1} BPM",
                                root + 1
                            )
                        };
                        let summary = format!(
                            "{grid_desc}: beat={} smp, bar={} smp → {}",
                            beat.round(),
                            bar.round(),
                            if parts.is_empty() { "nothing to quantize".into() } else { parts.join(", ") }
                        );
                        eprintln!("[simplooper] quantize: {summary}");
                        let _ = evt_tx.send(Evt::Quantized { summary });
                        send_loop_state(&engine, &evt_tx);
                    }
                    Err(e) => {
                        let _ = evt_tx.send(Evt::Error(format!("quantize: {e}")));
                    }
                }
            }
            Ok(Cmd::Quit) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
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
    outs: &[String],
    cur_in: &mut [Option<String>],
    cur_out: &mut [Option<String>],
    mask_in: &AtomicU64,
    mask_out: &AtomicU64,
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
                    eprintln!("[simplooper] {msg}");
                    let _ = tx.try_send(Evt::Error(msg));
                }
            }
        }
    }

    let mut applied_out = 0usize;
    for (i, slot) in cur_out.iter_mut().enumerate() {
        let new_out = outs.get(i).cloned();
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
            mask_out.fetch_and(!(1u64 << i), Ordering::Relaxed);
        }
        if let Some(dst) = new_out {
            match connect_retry(client, &pool_name, &dst) {
                Ok(()) => {
                    mask_out.fetch_or(1u64 << i, Ordering::Relaxed);
                    *slot = Some(dst);
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

    eprintln!("[simplooper] applied {applied_in} input(s), {applied_out} output(s)");
    let _ = tx.send(Evt::Applied { ins: applied_in, outs: applied_out });
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

/// Re-derive which remote MIDI source ports feed `simplooper:midi_in` and
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
            eprintln!("[simplooper] MIDI link {name} → {dst}: {e}");
        }
    }
    *known = found.clone();
    eprintln!("[simplooper] MIDI sources: {}", if known.is_empty() { "none".into() } else { known.join(", ") });
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
    PortInfo { name: full_name.to_string(), client }
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

/// Realtime process handler. No allocation, no locks, no I/O: the slot pool
/// is fixed at startup and its raw port pointers are cached here; per-cycle
/// work is a handful of atomic loads plus direct buffer access. Loop capture
/// and playback happen inline in the sample loop (the engine buffers are
/// pre-allocated; `len` is written with `fetch_add` while recording).
struct Proc {
    midi_in: Port<MidiIn>,
    ports: RawPorts,
    mask_in: Arc<AtomicU64>,
    mask_out: Arc<AtomicU64>,
    tx: SyncSender<Evt>,
    midi_drops: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    engine: Arc<LoopEngine>,
    // RT-local loop state (audio thread only).
    prev_state: [u8; MAX_LOOPS],
    /// Independent-mode play position per track.
    pos: [u64; MAX_LOOPS],
    /// Dependent-mode ensemble clock (samples since the ensemble started).
    master_pos: u64,
    prev_mode: u8,
    /// Last root-loops value seen; a change restarts the ensemble.
    prev_root: u32,
    /// Engine sample rate (for metronome / latency math).
    sr: u32,
    /// Latency-compensation delay line (power-of-two size, ring index).
    delay: Vec<f32>,
    didx: usize,
    /// Metronome clock: absolute sample of the next click, start sample of
    /// the click currently sounding (-1 = idle), and the beat-grid state:
    /// start sample and length of the current beat, the index of the next
    /// quarter note within the measure (0 = downbeat), the index of the next
    /// click within the beat (0 = quarter note), the subdivision in force
    /// for the current beat, and the tone of the click currently sounding
    /// (0 = downbeat, 1 = quarter note, 2 = subdivision).
    metro_next: u64,
    metro_start: i64,
    metro_beat_start: u64,
    metro_beat_len: u64,
    metro_beat: u32,
    metro_k: u32,
    metro_sub_eff: u32,
    metro_tone: u8,
    /// Last metronome on/off flag; the rising edge re-arms the grid so the
    /// first click is always a downbeat, immediately on activation.
    prev_metro_on: bool,
    /// Absolute sample counter (metronome timeline).
    sample_no: u64,
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
        let out_mask = self.mask_out.load(Ordering::Relaxed);

        // Per-cycle snapshot of the loop engine (atomics are only read here).
        let st_now: [u8; MAX_LOOPS] = core::array::from_fn(|i| {
            self.engine.state[i].load(Ordering::Relaxed)
        });
        let lens: [u32; MAX_LOOPS] = core::array::from_fn(|i| {
            self.engine.len[i].load(Ordering::Relaxed)
        });
        let mode = self.engine.mode.load(Ordering::Relaxed);
        let any_loop_active = st_now.iter().any(|&s| s != TR_IDLE);

        if in_mask == 0 && out_mask == 0 && !any_loop_active {
            return Control::Continue; // nothing connected: idle
        }

        // --- state edges (the manager changed something between cycles) ----
        if mode != self.prev_mode {
            // Mode switch: restart the ensemble clock and all positions.
            self.master_pos = 0;
            for i in 0..MAX_LOOPS {
                self.pos[i] = 0;
            }
            self.prev_mode = mode;
        }
        for (i, &now) in st_now.iter().enumerate() {
            let prev = self.prev_state[i];
            if prev == now {
                continue;
            }
            // A fresh start from IDLE/REC restarts the position; OD entered
            // from PLAY keeps playing where it left off.
            if (now == TR_PLAY || now == TR_OD) && prev != TR_PLAY && prev != TR_OD {
                self.pos[i] = 0;
            }
            self.prev_state[i] = now;
        }

        // Cache this cycle's input buffers (raw pointers are stable).
        let mut in_bufs: [*const f32; MAX_SLOTS] = [std::ptr::null(); MAX_SLOTS];
        for (s, b) in in_bufs.iter_mut().enumerate() {
            if in_mask & (1u64 << s) != 0 {
                let p = self.ports.ins[s];
                if !p.is_null() {
                    *b = unsafe { j::jack_port_get_buffer(p, ps.n_frames()) } as *const f32;
                }
            }
        }
        // Cache output buffers too: the mix is computed once per sample and
        // written to every enabled slot.
        let mut out_bufs: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        for (s, b) in out_bufs.iter_mut().enumerate() {
            if out_mask & (1u64 << s) != 0 {
                let p = self.ports.outs[s];
                if !p.is_null() {
                    *b = unsafe { j::jack_port_get_buffer(p, ps.n_frames()) as *mut f32 };
                }
            }
        }

        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
        // Metronome parameters for this cycle (click is added to the output
        // mix only — never to `dry`, so it can never be recorded into a loop).
        let (metro_on, metro_bpm, metro_sub, metro_num) = self.engine.metro();
        let metro_bpm = metro_bpm.clamp(20, 300).max(1);
        let metro_sub = metro_sub.clamp(1, 16);
        let metro_num = metro_num.clamp(1, 16);
        if metro_on && !self.prev_metro_on {
            // Rising edge: re-arm the grid so the very first click is a
            // downbeat, right now — the metronome always starts directly on
            // the first beat of a measure.
            self.metro_beat = 0;
            self.metro_k = 0;
            self.metro_sub_eff = metro_sub;
            self.metro_beat_len = ((self.sr as u64 * 60) / metro_bpm as u64).max(1);
            self.metro_beat_start = self.sample_no;
            self.metro_next = self.sample_no;
            self.metro_start = -1;
        }
        self.prev_metro_on = metro_on;
        let metro_dur = (self.sr / 40).max(2); // 25 ms click
        let metro_decay = self.sr as f32 * 0.012;
        // Latency-compensation delay (samples, clamped to the line size).
        let dn = self.delay.len();
        let dly = (self.engine.latency.load(Ordering::Relaxed) as usize).min(dn - 1);
        let dependent = mode == 1;
        // Dependent-mode ensemble: the longest PLAYING/overdubbing track is
        // the "root" loop; the ensemble period is `root_loops` copies of it.
        // Every track repeats continuously (p % len) across the period, so
        // shorter recordings tile horizontally, and at the period end the
        // clock wraps — the ensemble loops forever, no gaps, ever.
        let root = self.engine.root_loops.load(Ordering::Relaxed).max(1);
        if root != self.prev_root {
            // Ensemble length changed: restart so it plays exactly N fresh
            // root loops from here.
            self.prev_root = root;
            self.master_pos = 0;
        }
        let mut lmax = 0u64;
        if dependent {
            for i in 0..MAX_LOOPS {
                if st_now[i] == TR_PLAY || st_now[i] == TR_OD {
                    lmax = lmax.max(lens[i] as u64);
                }
            }
        }
        let period = lmax * root as u64;

        // Per-track read offsets for the GUI playback bars, republished every
        // 8 samples (RT-cheap; display resolution is far coarser than that).
        let mut pos_now: [u64; MAX_LOOPS] = core::array::from_fn(|t| self.pos[t]);

        let mut peak_val: u32 = 0;
        for i in 0..n {
            // Dry mix of the connected inputs.
            let mut dry = 0.0f32;
            for b in &in_bufs {
                if !b.is_null() {
                    dry += unsafe { *b.add(i) };
                }
            }

            // Loop engine step, shared by all output slots.
            let mut play = 0.0f32;
            if any_loop_active {
                // Ensemble position in dependent mode (before advancing).
                let p = if period > 0 { self.master_pos % period } else { 0 };
                for t in 0..MAX_LOOPS {
                    let st = st_now[t];
                    if st == TR_REC {
                        // Contiguous capture from the start of the buffer.
                        if self.engine.rec_sample(t, dry) {
                            // Capacity reached: stop capturing into this loop,
                            // keep playing (the buffer is full of content).
                            self.engine.state[t].store(TR_PLAY, Ordering::Relaxed);
                            let (states, lens_snap) = loop_state(&self.engine);
                            let _ = self.tx.try_send(Evt::LoopState { states, lens: lens_snap });
                        }
                    } else if st == TR_PLAY || st == TR_OD {
                        let l = lens[t] as u64;
                        if l > 0 {
                            if dependent {
                                // Repeat semantics: the recording tiles across
                                // the whole ensemble period.
                                let off = p % l;
                                play += self.engine.read(t, off as u32);
                                pos_now[t] = off;
                                if st == TR_OD {
                                    // Additive overdub: layer the live input
                                    // on top of the existing recording at its
                                    // original level — never replace it.
                                    let old = self.engine.read(t, off as u32);
                                    self.engine.write(t, off as u32, old + dry);
                                }
                            } else {
                                let off = self.pos[t] % l;
                                play += self.engine.read(t, off as u32);
                                if st == TR_OD {
                                    let old = self.engine.read(t, off as u32);
                                    self.engine.write(t, off as u32, old + dry);
                                }
                                pos_now[t] = off;
                                self.pos[t] += 1;
                            }
                        } else {
                            pos_now[t] = 0;
                        }
                    }
                }
                // Advance the ensemble clock only while something is actually
                // playing: a stopped ensemble stays put, so REC-only cycles
                // can't drift the phase of a later playback.
                if dependent && period > 0 {
                    self.master_pos += 1;
                }
            }

            // Publish playback positions ~every 8 samples.
            if i & 7 == 7 {
                for (t, &p) in pos_now.iter().enumerate() {
                    self.engine.pos[t].store(p, Ordering::Relaxed);
                }
                self.engine.master_pos.store(self.master_pos, Ordering::Relaxed);
            }

            // Metronome click (added to the output mix only — `dry` never
            // sees it, so it can never be recorded into a loop).
            let mut click = 0.0f32;
            if metro_on {
                if self.sample_no >= self.metro_next {
                    self.metro_start = self.sample_no as i64;
                    // Tone of this click: the first quarter note of the
                    // measure is the downbeat, the other quarter notes get
                    // the plain quarter tone, everything in between gets the
                    // subdivision tone.
                    self.metro_tone = if self.metro_k == 0 {
                        if self.metro_beat == 0 {
                            0
                        } else {
                            1
                        }
                    } else {
                        2
                    };
                    // Schedule the next click: subdivision clicks land at
                    // even fractions of the current beat; after the last one
                    // the next click is the next quarter note.
                    self.metro_k += 1;
                    if self.metro_k < self.metro_sub_eff.max(1) {
                        let s = self.metro_sub_eff.max(1) as u64;
                        self.metro_next = self.metro_beat_start
                            + self.metro_k as u64 * self.metro_beat_len / s;
                    } else {
                        // Next quarter note: rescan tempo, beats-per-measure
                        // and subdivision here, so a change made mid-beat
                        // takes effect exactly on that next quarter note.
                        self.metro_beat = (self.metro_beat + 1) % metro_num.max(1);
                        self.metro_beat_len =
                            ((self.sr as u64 * 60) / metro_bpm as u64).max(1);
                        self.metro_sub_eff = metro_sub;
                        self.metro_k = 0;
                        self.metro_beat_start += self.metro_beat_len;
                        self.metro_next = self.metro_beat_start;
                        if self.metro_next <= self.sample_no {
                            // Tempo change outran the schedule: resync.
                            self.metro_beat_start = self.sample_no;
                            self.metro_next = self.sample_no + self.metro_beat_len;
                        }
                    }
                }
                if self.metro_start >= 0 {
                    let k = self.sample_no as i64 - self.metro_start;
                    if k >= 0 && (k as u32) < metro_dur {
                        let kf = k as f32;
                        // Three distinct tones: downbeat (high, loudest),
                        // quarter note (mid), subdivision (low, softest).
                        let (f, amp) = match self.metro_tone {
                            0 => (1760.0f32, 0.5f32),
                            1 => (1175.0, 0.35),
                            _ => (880.0, 0.25),
                        };
                        click = (kf * std::f32::consts::TAU * f / self.sr as f32).sin()
                            * (-kf / metro_decay).exp()
                            * amp;
                    } else {
                        self.metro_start = -1;
                    }
                }
            }
            self.sample_no += 1;

            let v = (dry + play + click) * g;
            // Latency compensation: delay the whole output mix by `dly`
            // samples (the line is always fed so re-enabling never gaps).
            let out = if dly > 0 {
                let s = self.delay[(self.didx + dn - dly) & (dn - 1)];
                self.delay[self.didx] = v;
                self.didx = (self.didx + 1) & (dn - 1);
                s
            } else {
                self.delay[self.didx] = v;
                self.didx = (self.didx + 1) & (dn - 1);
                v
            };
            let av = out.abs();
            if av.to_bits() > peak_val {
                peak_val = av.to_bits();
            }
            for b in out_bufs.iter() {
                if !b.is_null() {
                    unsafe { *b.add(i) = out };
                }
            }
        }
        // Continuous ensemble: at the period end the clock simply wraps and
        // every track keeps looping. (At most one block of samples can
        // overshoot the boundary — content is continuous, so no glitch.)
        if dependent && period > 0 && self.master_pos >= period {
            self.master_pos = 0;
        }
        // Publish final positions for this block.
        for (t, &p) in pos_now.iter().enumerate() {
            self.engine.pos[t].store(p, Ordering::Relaxed);
        }
        self.engine.master_pos.store(self.master_pos, Ordering::Relaxed);
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

    #[test]
    fn engine_fresh() {
        let e = LoopEngine::new();
        for i in 0..MAX_LOOPS {
            assert_eq!(e.state_of(i), TR_IDLE);
            assert_eq!(e.len_of(i), 0);
            assert!(e.read_range(i, 64).iter().all(|&s| s == 0.0));
        }
        assert_eq!(e.generation(), 0);
    }

    #[test]
    fn engine_rec_clear_roundtrip() {
        let e = LoopEngine::new();
        let g0 = e.generation();
        e.start_rec(1);
        assert_eq!(e.state_of(1), TR_REC);
        assert_eq!(e.len_of(1), 0);
        // Simulate RT capture.
        for k in 0..100u32 {
            e.write(1, k, k as f32 * 0.01);
        }
        e.len[1].store(100, Ordering::Relaxed);
        assert!((e.read_range(1, 100)[50] - 0.5).abs() < 1e-6);
        e.clear(1);
        assert_eq!(e.state_of(1), TR_IDLE);
        assert_eq!(e.len_of(1), 0);
        assert!(e.read_range(1, 100).iter().all(|&s| s == 0.0));
        assert!(e.generation() > g0);
    }

    #[test]
    fn engine_quantize_all_snap() {
        let e = LoopEngine::new();
        let sr = 48_000u32;
        // Track 0: a sine, length slightly off the grid.
        let len = 144_100u32;
        for k in 0..len {
            e.write(0, k, (k as f64 * std::f64::consts::TAU / 109.0).sin() as f32);
        }
        e.len[0].store(len, Ordering::Relaxed);
        let target = 12_000u32;
        let lens = e.quantize_all(target, sr);
        // Empty track stays untouched.
        assert_eq!(lens[1], 0);
        let ideal = crate::quantize::snap_length(len, target, LOOP_CAP);
        let w = (sr / 200).min(ideal / 4) as i64;
        let nl = lens[0] as i64;
        assert!((nl - ideal as i64).abs() <= w + 64, "new len {nl} vs ideal {ideal}");
        assert!(lens[0] >= target);
        // Content far from the seam is preserved.
        let mid = (ideal / 2) as usize;
        let expect = (mid as f64 * std::f64::consts::TAU / 109.0).sin() as f32;
        assert!((e.read(0, mid as u32) - expect).abs() < 1e-5);
    }

    #[test]
    fn toggle_record_state_machine() {
        let e = LoopEngine::new();
        toggle_record(&e, 2); // IDLE -> REC (fresh capture)
        assert_eq!(e.state_of(2), TR_REC);
        e.len[2].store(500, Ordering::Relaxed); // simulate RT capture
        toggle_record(&e, 2); // REC with content -> PLAY
        assert_eq!(e.state_of(2), TR_PLAY);
        assert_eq!(e.len_of(2), 500);
        toggle_record(&e, 2); // PLAY -> fresh REC (content discarded)
        assert_eq!(e.state_of(2), TR_REC);
        assert_eq!(e.len_of(2), 0);
        toggle_record(&e, 2); // REC with no content -> IDLE
        assert_eq!(e.state_of(2), TR_IDLE);
    }

    #[test]
    fn rec_sample_never_overshoots_capacity() {
        let e = LoopEngine::new();
        // Fill exactly to capacity, one sample at a time (RT path).
        for _ in 0..LOOP_CAP {
            assert!(!e.rec_sample(0, 1.0));
        }
        assert_eq!(e.len_of(0), LOOP_CAP);
        // Once full, further samples must report "full" and must not push the
        // counter past LOOP_CAP (the old fetch_add could overshoot by up to a
        // frame, which made the dependent tiled waveform index out of bounds).
        for _ in 0..1024 {
            assert!(e.rec_sample(0, 0.0));
        }
        assert_eq!(e.len_of(0), LOOP_CAP);
    }

    /// Fill track `i` with a constant and mark it playing (manager-side).
    fn fill_playing(e: &LoopEngine, i: usize, n: u32, v: f32) {
        for k in 0..n {
            e.write(i, k, v);
        }
        e.len[i].store(n, Ordering::Relaxed);
        e.state[i].store(TR_PLAY, Ordering::Relaxed);
    }

    #[test]
    fn overdub_undo_redo_roundtrip() {
        let e = LoopEngine::new();
        fill_playing(&e, 2, 1000, 0.25);
        let mut h = OdHistory::default();
        assert!(!h.can_undo() && !h.can_redo());
        assert!(h.undo(&e).is_none(), "undo on empty history is a no-op");

        // Overdub starts: snapshot, then simulate the additive write (0.25-0.1).
        h.begin(&e, 2);
        assert!(h.can_undo() && !h.can_redo());
        for k in 0..1000 {
            e.write(2, k, 0.15);
        }

        // Undo: buffer back to the pre-overdub content, track resumes playing.
        assert_eq!(h.undo(&e), Some(2));
        assert_eq!(e.state_of(2), TR_PLAY);
        assert_eq!(e.len_of(2), 1000);
        assert!(e.read_range(2, 1000).iter().all(|&v| (v - 0.25).abs() < 1e-9));
        assert!(
            !h.can_undo() && h.can_redo(),
            "the only entry was undone: nothing left to undo, redo available"
        );

        // Redo: the overdubbed content comes back.
        assert_eq!(h.redo(&e), Some(2));
        assert_eq!(e.state_of(2), TR_PLAY);
        assert!(e.read_range(2, 1000).iter().all(|&v| (v - 0.15).abs() < 1e-9));
        assert!(!h.can_redo());

        // Undo again (redo must stay reachable).
        h.undo(&e);
        assert!(e.read_range(2, 1000).iter().all(|&v| (v - 0.25).abs() < 1e-9));
        assert!(h.can_redo());
    }

    #[test]
    fn overdub_undo_stacks_across_tracks_and_fresh_begin_clears_redo() {
        let e = LoopEngine::new();
        fill_playing(&e, 0, 500, 0.1);
        fill_playing(&e, 1, 700, 0.2);
        let mut h = OdHistory::default();

        // Two overdubs on two tracks; undo pops the most recent first.
        h.begin(&e, 0);
        for k in 0..500 {
            e.write(0, k, 0.9);
        }
        h.begin(&e, 1);
        for k in 0..700 {
            e.write(1, k, 0.8);
        }
        assert_eq!(h.undo(&e), Some(1), "undo must take the LAST overdub");
        assert!(e.read_range(1, 700).iter().all(|&v| (v - 0.2).abs() < 1e-9));
        // Track 0 keeps its overdubbed content until its own undo.
        assert!(e.read_range(0, 500).iter().all(|&v| (v - 0.9).abs() < 1e-9));
        assert_eq!(h.undo(&e), Some(0));
        assert!(e.read_range(0, 500).iter().all(|&v| (v - 0.1).abs() < 1e-9));

        // A fresh overdub invalidates the redo branch.
        assert!(h.can_redo());
        h.begin(&e, 0);
        assert!(!h.can_redo(), "a new edit clears redo");
    }

    #[test]
    fn overdub_history_invalidation_and_cap() {
        let e = LoopEngine::new();
        fill_playing(&e, 3, 100, 0.5);
        let mut h = OdHistory::default();
        h.begin(&e, 3);
        h.begin(&e, 3);
        assert_eq!(h.undo(&e), Some(3));
        // Clearing the track drops its entries in BOTH directions.
        h.invalidate_track(3);
        assert!(!h.can_undo() && !h.can_redo());

        // Depth cap: only the newest MAX_OD_HISTORY snapshots survive.
        for _ in 0..(MAX_OD_HISTORY + 5) {
            h.begin(&e, 3);
        }
        assert_eq!(h.undo.len(), MAX_OD_HISTORY);
        h.clear_all();
        assert!(!h.can_undo() && !h.can_redo());
    }

    #[test]
    fn overdub_undo_of_paused_track_stays_paused() {
        let e = LoopEngine::new();
        fill_playing(&e, 1, 100, 0.5);
        e.state[1].store(TR_IDLE, Ordering::Relaxed); // paused with content
        let mut h = OdHistory::default();
        h.begin(&e, 1);
        assert!(!h.undo(&e).is_none());
        assert_eq!(
            e.state_of(1),
            TR_IDLE,
            "undo restores playback state, not forced playback"
        );
    }
}
