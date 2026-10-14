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
pub const MAX_LOOPS: usize = 4;
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
    /// Stop and erase a loop track.
    ClearTrack { i: usize },
    /// true = dependent looping (finite ensemble clock), false = independent players.
    SetDependent(bool),
    /// Dependent-mode ensemble length in units of the longest ("root") loop,
    /// clamped to 1..=64. The ensemble plays exactly N root loops: every track
    /// repeats within the period and all tracks stop together at its end
    /// (changing N while playing restarts the ensemble).
    SetRootLoops(u32),
    /// Easy-quantize every loop against Loop 1's recorded length.
    Quantize { num: u32, den: u32, bars: u32, unit: crate::quantize::Unit },
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
    Midi(RawMidiMsg),
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
        let engine = LoopEngine::new();
        let (t_gain, t_peak, t_xruns, t_engine) =
            (gain.clone(), peak.clone(), xruns.clone(), engine.clone());

        let thread = std::thread::Builder::new()
            .name("simplooper-audio".into())
            .spawn(move || {
                run(cmd_rx, evt_tx, t_gain, t_peak, t_xruns, t_engine);
            })
            .expect("failed to spawn audio thread");

        Backend { cmd_tx, evt_rx, gain, peak, xruns, engine, thread: Some(thread) }
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
}

impl LoopEngine {
    pub fn new() -> Arc<Self> {
        let mut bufs = [std::ptr::null_mut::<f32>(); MAX_LOOPS];
        for i in 0..MAX_LOOPS {
            let mut v = vec![0f32; LOOP_CAP as usize];
            bufs[i] = v.as_mut_ptr();
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
        for i in 0..MAX_LOOPS {
            let l = self.len[i].load(Ordering::Relaxed);
            if l == 0 {
                continue;
            }
            let buf = unsafe { std::slice::from_raw_parts_mut(self.bufs[i], LOOP_CAP as usize) };
            out[i] = crate::quantize::quantize_track(buf, l, target, LOOP_CAP, sr);
            self.len[i].store(out[i], Ordering::Relaxed);
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
    let proc = Proc {
        midi_in: midi_port,
        ports: RawPorts { ins: ins_raw, outs: outs_raw },
        mask_in: mask_in.clone(),
        mask_out: mask_out.clone(),
        tx: evt_tx.clone(),
        gain: gain.clone(),
        peak: peak.clone(),
        engine: engine.clone(),
        prev_state: [TR_IDLE; MAX_LOOPS],
        pos: [0u64; MAX_LOOPS],
        master_pos: 0,
        prev_mode: 0,
        prev_root: 1,
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

    // What each slot is currently connected to (manager-thread state only).
    let mut cur_in: Vec<Option<String>> = vec![None; MAX_SLOTS];
    let mut cur_out: Vec<Option<String>> = vec![None; MAX_SLOTS];

    loop {
        match cmd_rx.recv() {
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
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::ToggleOverdub { i }) => {
                match engine.state_of(i) {
                    TR_OD => engine.state[i].store(TR_PLAY, Ordering::Relaxed),
                    TR_PLAY => engine.state[i].store(TR_OD, Ordering::Relaxed),
                    TR_IDLE if engine.len_of(i) > 0 => {
                        engine.state[i].store(TR_OD, Ordering::Relaxed)
                    }
                    _ => {} // IDLE with no content, or REC: ignore
                }
                send_loop_state(&engine, &evt_tx);
            }
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
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::SetDependent(dep)) => {
                engine.mode.store(if dep { 1 } else { 0 }, Ordering::Relaxed);
                send_loop_state(&engine, &evt_tx);
            }
            Ok(Cmd::SetRootLoops(n)) => {
                engine.root_loops.store(n.clamp(1, 64), Ordering::Relaxed);
            }
            Ok(Cmd::Quantize { num, den, bars, unit }) => {
                if den == 0 || num == 0 || bars == 0 {
                    let _ = evt_tx.send(Evt::Error(
                        "quantize: numerator, denominator and bars must be at least 1".into(),
                    ));
                } else {
                    let len1 = engine.len_of(0);
                    match crate::quantize::compute_target(len1, bars, num, unit) {
                        Ok((target, beat, bar)) => {
                            // Stop everything and let the audio thread quiesce
                            // before buffers are rewritten in place.
                            for i in 0..MAX_LOOPS {
                                engine.state[i].store(TR_IDLE, Ordering::Relaxed);
                            }
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            let lens = engine.quantize_all(target, sample_rate);
                            let bpm = 60.0 / (beat / sample_rate as f64);
                            let unit_name = match unit {
                                crate::quantize::Unit::Beat => "beat",
                                crate::quantize::Unit::Bar => "bar",
                            };
                            let mut parts: Vec<String> = Vec::new();
                            for i in 0..MAX_LOOPS {
                                if lens[i] > 0 {
                                    parts.push(format!(
                                        "L{} → {} smp ({}×{unit_name})",
                                        i + 1,
                                        lens[i],
                                        (lens[i] as f64 / target as f64).round()
                                    ));
                                }
                            }
                            let summary = format!(
                                "{num}/{den}, {bars} bar(s): beat={} smp, bar={} smp, ≈{bpm:.1} BPM → {}",
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
            }
            Ok(Cmd::Quit) | Err(_) => break,
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
    for i in 0..MAX_SLOTS {
        let new_in = ins.get(i).cloned();
        if cur_in[i] == new_in {
            continue; // unchanged: keep the existing connection
        }
        let pool_name = format!("{}:in{i}", client.name());
        if cur_in[i].take().is_some() {
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
                    cur_in[i] = Some(src);
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
    for i in 0..MAX_SLOTS {
        let new_out = outs.get(i).cloned();
        if cur_out[i] == new_out {
            continue;
        }
        let pool_name = format!("{}:out{i}", client.name());
        if cur_out[i].take().is_some() {
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
                    cur_out[i] = Some(dst);
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
}

impl ProcessHandler for Proc {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;

        // MIDI: copy events out (bounded channel; drop if the GUI is slow).
        for e in self.midi_in.iter(ps) {
            let bytes = e.bytes.to_vec();
            let _ = self.tx.try_send(Evt::Midi(RawMidiMsg { bytes }));
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
        for i in 0..MAX_LOOPS {
            let (prev, now) = (self.prev_state[i], st_now[i]);
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
        for s in 0..MAX_SLOTS {
            if in_mask & (1u64 << s) != 0 {
                let p = self.ports.ins[s];
                if !p.is_null() {
                    in_bufs[s] = unsafe { j::jack_port_get_buffer(p, ps.n_frames()) } as *const f32;
                }
            }
        }
        // Cache output buffers too: the mix is computed once per sample and
        // written to every enabled slot.
        let mut out_bufs: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        for s in 0..MAX_SLOTS {
            if out_mask & (1u64 << s) != 0 {
                let p = self.ports.outs[s];
                if !p.is_null() {
                    out_bufs[s] = unsafe {
                        j::jack_port_get_buffer(p, ps.n_frames()) as *mut f32
                    };
                }
            }
        }

        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let dependent = mode == 1;
        // Dependent-mode ensemble: the longest PLAYING/overdubbing track is
        // the "root" loop; the ensemble period is `root_loops` copies of it.
        // Every track repeats continuously (p % len) across the period, so
        // shorter recordings tile horizontally until everything stops in sync
        // together at the period boundary — no gaps, ever.
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
                        let off = self.engine.len[t].fetch_add(1, Ordering::Relaxed) as u64;
                        if off < LOOP_CAP as u64 {
                            self.engine.write(t, off as u32, dry);
                        } else {
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
                                    self.engine.write(t, off as u32, dry);
                                }
                            } else {
                                let off = self.pos[t] % l;
                                play += self.engine.read(t, off as u32);
                                if st == TR_OD {
                                    self.engine.write(t, off as u32, dry);
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
                for t in 0..MAX_LOOPS {
                    self.engine.pos[t].store(pos_now[t], Ordering::Relaxed);
                }
                self.engine.master_pos.store(self.master_pos, Ordering::Relaxed);
            }

            let v = (dry + play) * g;
            let av = v.abs();
            if av.to_bits() > peak_val {
                peak_val = av.to_bits();
            }
            for s in 0..MAX_SLOTS {
                if !out_bufs[s].is_null() {
                    unsafe { *out_bufs[s].add(i) = v };
                }
            }
        }
        // Finite ensemble: at the period end, stop every playing track in
        // sync and restart the clock. (At most one block of samples can
        // overshoot the boundary — content is continuous, so no glitch.)
        if dependent && period > 0 && self.master_pos >= period {
            for t in 0..MAX_LOOPS {
                if st_now[t] == TR_PLAY || st_now[t] == TR_OD {
                    self.engine.state[t].store(TR_IDLE, Ordering::Relaxed);
                }
            }
            self.master_pos = 0;
            let (states, lens_snap) = loop_state(&self.engine);
            let _ = self.tx.try_send(Evt::LoopState { states, lens: lens_snap });
        }
        // Publish final positions for this block.
        for t in 0..MAX_LOOPS {
            self.engine.pos[t].store(pos_now[t], Ordering::Relaxed);
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
}
