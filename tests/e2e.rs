//! End-to-end tests for the simplooper JACK backend.
//!
//! These drive the real `Backend` (manager thread + realtime handler) against a
//! live JACK/PipeWire-JACK server, using a second JACK client as test source:
//! - audio: two constant sources are summed into one sink through the applied
//!   port layout (`out = Σ in × gain`);
//! - metronome: click grid, three-tone pattern and immediate downbeat start
//!   are captured at a sink;
//! - overdub undo/redo: an additive overdub is undone back to the pre-overdub
//!   content and redone, verified acoustically at the sink;
//! - midi: messages are sent to `simplooper:midi_in` and must come back parsed.
//!
//! If no JACK/PipeWire server is running, the tests skip themselves.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, MidiOut, Port, ProcessHandler,
    ProcessScope, RawMidi,
};

use simplooper::audio::{Backend, Cmd, Evt, LOOP_CAP, TR_IDLE, TR_OD, TR_PLAY, TR_REC};
use simplooper::midi::MidiMsg;
use simplooper::quantize::{compute_target, snap_length, Unit};

/// Serialize the e2e tests. Each drives the live audio server in real time
/// and asserts on sample-accurate timing; running them concurrently starves
/// the JACK clients and produces spurious "ensemble stalled" gaps.
static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the e2e lock, surviving a poisoned guard from an earlier failed test.
fn e2e_lock() -> std::sync::MutexGuard<'static, ()> {
    E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poll `b.evt_rx` until an event satisfies `pred` (or the timeout expires).
fn wait_for(b: &Backend, pred: impl Fn(&Evt) -> bool, timeout: Duration) -> Option<Evt> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match b.evt_rx.recv_timeout(remaining) {
            Ok(evt) if pred(&evt) => return Some(evt),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Start the backend and wait until it is ready (or give up -> `None`).
/// Returns the backend, the JACK client name (for building port names) and
/// the sample rate.
fn start_backend() -> Option<(Backend, String, u32)> {
    let b = Backend::start();
    match wait_for(
        &b,
        |e| matches!(e, Evt::Ready { .. } | Evt::Unavailable(_) | Evt::Error(_)),
        Duration::from_secs(5),
    ) {
        Some(Evt::Ready { client_name, sample_rate }) => Some((b, client_name, sample_rate)),
        Some(other) => {
            eprintln!("skipping: backend not ready ({other:?})");
            None
        }
        None => {
            eprintln!("skipping: no JACK/PipeWire server (timed out waiting for backend)");
            None
        }
    }
}

// ---------------------------------------------------------------- audio mix --

struct MixProc {
    a: Port<AudioOut>,
    b: Port<AudioOut>,
    sink: Port<AudioIn>,
    last_sample: Arc<AtomicU32>, // f32 bits of the newest sink sample
    frames: Arc<AtomicU64>,     // process cycles handled
}

impl ProcessHandler for MixProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let a = self.a.as_mut_slice(ps);
        let b = self.b.as_mut_slice(ps);
        for i in 0..n {
            a[i] = 0.25;
            b[i] = -0.125;
        }
        let sink = self.sink.as_slice(ps);
        self.last_sample.store(sink[n - 1].to_bits(), Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
        Control::Continue
    }
}

#[test]
fn e2e_audio_mixing() {
    let _g = e2e_lock();
    let Some((b, _client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Test source client: two constant sources + one sink.
    let (client, _status) = Client::new("mixer_test", ClientOptions::NO_START_SERVER)
        .expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let out_b = client.register_port("out_b", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();

    let last_sample = Arc::new(AtomicU32::new(f32::NAN.to_bits()));
    let frames = Arc::new(AtomicU64::new(0));
    let client_name = client.name().to_string();
    let active = client
        .activate_async((), MixProc {
            a: out_a,
            b: out_b,
            sink,
            last_sample: last_sample.clone(),
            frames: frames.clone(),
        })
        .unwrap();

    // Apply the layout through the backend's command channel.
    b.cmd(Cmd::Apply {
        ins: vec![format!("{client_name}:out_a"), format!("{client_name}:out_b")],
        outs: vec![format!("{client_name}:sink")],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 2, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the 2-in/1-out layout"
    );

    // Wait until the summed constant actually shows up at the sink.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let observed = f32::from_bits(last_sample.load(Ordering::Relaxed));
        if (observed - 0.125).abs() < 1e-4 {
            break;
        }
        assert!(Instant::now() < deadline, "sink never reached 0.125 (last: {observed}, frames: {})", frames.load(Ordering::Relaxed));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(frames.load(Ordering::Relaxed) > 0);

    // Teardown: stop the backend's audio, drop clients.
    b.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    let _ = active;
}

// ------------------------------------------------------------------ midi ----

/// One message per process cycle, cycling through a fixed list.
struct MidiSrcProc {
    out: Port<MidiOut>,
    step: u32,
}

const MESSAGES: &[&[u8]] = &[
    &[0x90, 60, 100], // NOTE ON  ch 1  C4  vel 100
    &[0xB1, 7, 128],  // CC       ch 2  volume 128
    &[0xC2, 11],      // PC       ch 3  program 11
];

impl ProcessHandler for MidiSrcProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let msg = MESSAGES[(self.step % MESSAGES.len() as u32) as usize];
        // One message per cycle at the cycle head; advance regardless so a
        // transient full buffer can't wedge us on one message.
        let _ = self.out.writer(ps).write(&RawMidi { time: 0, bytes: msg });
        self.step += 1;
        Control::Continue
    }
}

#[test]
fn e2e_midi_monitor() {
    let _g = e2e_lock();
    let Some((b, client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Source client with a MIDI out wired to simplooper:midi_in.
    let (client, _status) = Client::new("midi_test", ClientOptions::NO_START_SERVER)
        .expect("no JACK server");
    let midi_out = client.register_port("midi_out", MidiOut::default()).unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(&format!("{src_name}:midi_out"), &format!("{client_name}:midi_in"))
        .expect("connect to simplooper:midi_in");
    let active = client
        .activate_async((), MidiSrcProc { out: midi_out, step: 0 })
        .unwrap();

    // Collect events until every expected message has been seen (parsed).
    let mut seen: Vec<MidiMsg> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.len() < MESSAGES.len() && Instant::now() < deadline {
        match b.evt_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Evt::Midi(raw)) => {
                if let Some(msg) = MidiMsg::parse(&raw.bytes) {
                    seen.push(msg);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(
        seen.len() >= MESSAGES.len(),
        "expected at least {} MIDI events, got {}: {:?}",
        MESSAGES.len(),
        seen.len(),
        seen
    );

    let note_on = seen.iter().find(|m| matches!(m, MidiMsg::NoteOn { .. }));
    assert_eq!(
        note_on,
        Some(&MidiMsg::NoteOn { channel: 0, note: 60, velocity: 100 })
    );
    let cc = seen.iter().find(|m| matches!(m, MidiMsg::ControlChange { .. }));
    assert_eq!(
        cc,
        Some(&MidiMsg::ControlChange { channel: 1, number: 7, value: 128 })
    );
    let pc = seen.iter().find(|m| matches!(m, MidiMsg::ProgramChange { .. }));
    assert_eq!(pc, Some(&MidiMsg::ProgramChange { channel: 2, program: 11 }));

    // Display formatting sanity (what the GUI shows).
    let note_on = note_on.unwrap();
    assert_eq!(note_on.type_name(), "NOTE ON");
    assert!(note_on.describe().contains("ch 01"), "{}", note_on.describe());
    assert!(note_on.describe().contains("C4"), "{}", note_on.describe());

    b.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    let _ = active;
}

/// The exact pattern the user's SINCO MIDI 1 controller sends (seen on aseq
/// port 32): Program Changes with values 1 and 2, cycling channels 0–3.
const SINCO_MESSAGES: &[&[u8]] = &[
    &[0xC0, 1], // PC  ch1  program 1
    &[0xC1, 2], // PC  ch2  program 2
    &[0xC2, 1], // PC  ch3  program 1
    &[0xC3, 2], // PC  ch4  program 2
];

struct SincoSrcProc {
    out: Port<MidiOut>,
    step: u32,
}

impl ProcessHandler for SincoSrcProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let msg = SINCO_MESSAGES[(self.step % SINCO_MESSAGES.len() as u32) as usize];
        let _ = self.out.writer(ps).write(&RawMidi { time: 0, bytes: msg });
        self.step += 1;
        Control::Continue
    }
}

// ------------------------------------------------------------------ midi ----

/// Full capture → fire round trip through the real JACK MIDI path, emulating
/// the SINCO MIDI 1 device. The armed binding cell must register the first PC
/// (the "listening" box), and a repeated matching message must then fire the
/// bound action — here "select loop 2", observable via `App::selected_loop`.
#[test]
fn e2e_midi_capture_and_fire_sinco() {
    let _g = e2e_lock();
    let Some((b, client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Isolate the config file so App does not read/write the user's real one.
    let dir = std::env::temp_dir().join(format!("simplooper-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &dir);

    // Arm the "select loop 2" slot (Select(1) = table index 7 in the 87-slot
    // layout: 0-5 globals, 6..=19 are Select(0)..Select(15)). The next
    // bindable MIDI message is captured.
    let mut app = simplooper::ui::App::new(b);
    app.arm_midi_slot(7);
    app.drain_events();

    // Source client emulating SINCO MIDI 1, wired to simplooper:midi_in.
    let (client, _status) = Client::new("sinco_emu", ClientOptions::NO_START_SERVER)
        .expect("no JACK server");
    let midi_out = client.register_port("midi_out", MidiOut::default()).unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(
            &format!("{src_name}:midi_out"),
            &format!("{client_name}:midi_in"),
        )
        .expect("connect to simplooper:midi_in");
    let active = client
        .activate_async((), SincoSrcProc { out: midi_out, step: 0 })
        .unwrap();

    // Pump App events until the first PC is captured and a repeated PC fires.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.drain_events();
        if app.midi_bind_at(7).is_some() && app.selected_loop() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // The armed slot must have captured a message from the device's pattern.
    // Which of the four PCs arrives first depends on JACK graph scheduling, so
    // accept any PC with value 1 or 2 on channels 0–3 — exactly what the SINCO
    // MIDI 1 sends.
    let expected: &[simplooper::midi::MidiBind] = &[
        simplooper::midi::MidiBind { ty: 2, ch: 0, d1: 1 },
        simplooper::midi::MidiBind { ty: 2, ch: 1, d1: 2 },
        simplooper::midi::MidiBind { ty: 2, ch: 2, d1: 1 },
        simplooper::midi::MidiBind { ty: 2, ch: 3, d1: 2 },
    ];
    let captured = app.midi_bind_at(7);
    assert!(
        expected.contains(&captured.unwrap_or(simplooper::midi::MidiBind { ty: 0, ch: 99, d1: 99 })),
        "armed slot must capture one of the SINCO PC patterns, got {captured:?}"
    );
    assert_eq!(
        app.selected_loop(),
        1,
        "a repeated matching PC must fire the bound action (select loop 2)"
    );

    app.backend().cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    let _ = active;
}

// --------------------------------------------------- loop engine (record, --
// playback, overdub, quantize, independent vs dependent looping) --

const CAP_N: usize = 48_000; // sink capture window per phase (~1 s @ 48 kHz)

/// Source client: a controllable sample generator (constant or PRNG ±0.2)
/// plus a lock-free capture ring of the sink (what simplooper wrote back).
struct LoopSrcProc {
    out: Port<AudioOut>,
    sink: Port<AudioIn>,
    /// 0 = constant `const_val`, 1 = xorshift PRNG ±0.2.
    mode: Arc<AtomicU8>,
    const_val: Arc<AtomicU32>, // f32 bits
    seed: u32,
    ring: Arc<Vec<AtomicU32>>, // sink samples (f32 bits)
    widx: Arc<AtomicU64>,      // absolute write position in the ring
}

impl ProcessHandler for LoopSrcProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let out = self.out.as_mut_slice(ps);
        let prng = self.mode.load(Ordering::Relaxed) == 1;
        let c = f32::from_bits(self.const_val.load(Ordering::Relaxed));
        if prng {
            for s in out.iter_mut() {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 17;
                self.seed ^= self.seed << 5;
                *s = if self.seed & 1 == 0 { 0.2 } else { -0.2 };
            }
        } else {
            out.fill(c);
        }
        let sink = self.sink.as_slice(ps);
        let base = self.widx.load(Ordering::Relaxed) as usize;
        for (i, s) in sink.iter().enumerate() {
            self.ring[(base + i) % self.ring.len()]
                .store(s.to_bits(), Ordering::Relaxed);
        }
        self.widx.fetch_add(n as u64, Ordering::Relaxed);
        Control::Continue
    }
}

/// Wait until at least `need` new sink samples have been captured, then
/// return the most recent `need` of them.
fn read_ring(
    ring: &Arc<Vec<AtomicU32>>,
    widx: &AtomicU64,
    need: usize,
    timeout: Duration,
) -> Option<Vec<f32>> {
    let start = widx.load(Ordering::Relaxed);
    let deadline = Instant::now() + timeout;
    loop {
        if widx.load(Ordering::Relaxed).saturating_sub(start) as usize >= need {
            break;
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let end = widx.load(Ordering::Relaxed);
    let n = ring.len() as u64;
    Some(
        (0..need)
            .map(|i| {
                f32::from_bits(
                    ring
                        [((end - need as u64 + i as u64) % n) as usize]
                        .load(Ordering::Relaxed),
                )
            })
            .collect::<Vec<f32>>(),
    )
}

/// Mean of the most recent `n` captured sink samples.
fn ring_mean(ring: &Arc<Vec<AtomicU32>>, widx: &AtomicU64, n: usize) -> Option<f32> {
    read_ring(ring, widx, n, Duration::from_millis(500))
        .map(|x| x.iter().sum::<f32>() / n as f32)
}

/// Poll `pred` until true or the timeout expires (panics on timeout).
fn wait_until(pred: impl Fn() -> bool, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Variance of a capture (mean removed).
fn var(x: &[f32]) -> f64 {
    let n = x.len();
    let mean = x.iter().sum::<f32>() / n as f32;
    x.iter()
        .map(|&v| (v - mean) as f64 * (v - mean) as f64)
        .sum::<f64>()
        / n as f64
}

/// Normalized autocorrelation at `lag` over the whole capture. A signal that
/// repeats with period `lag` scores ~variance; uncorrelated content scores ~0.
fn acf_norm(x: &[f32], lag: usize) -> f64 {
    let n = x.len();
    if lag == 0 || lag >= n {
        return 0.0;
    }
    let mean = x.iter().sum::<f32>() / n as f32;
    let m = n - lag;
    (0..m)
        .map(|i| ((x[i] - mean) as f64) * ((x[i + lag] - mean) as f64))
        .sum::<f64>()
        / m as f64
}

#[test]
fn e2e_loop_engine() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19; // big enough to hold a whole dependent-mode run
    let ring: Arc<Vec<AtomicU32>> =
        Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let mode = Arc::new(AtomicU8::new(0));
    let const_val = Arc::new(AtomicU32::new(0.25f32.to_bits()));

    let (client, _status) = Client::new("loop_test", ClientOptions::NO_START_SERVER)
        .expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            LoopSrcProc {
                out: out_a,
                sink,
                mode: mode.clone(),
                const_val: const_val.clone(),
                seed: 0x1234_5678,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    b.cmd(Cmd::Apply {
        ins: vec![format!("{src_name}:out_a")],
        outs: vec![format!("{src_name}:sink")],
    });
    let mut applied = None;
    for _ in 0..200 {
        match b.evt_rx.try_recv() {
            Ok(Evt::Applied { ins, outs }) => {
                applied = Some((ins, outs));
                break;
            }
            Ok(Evt::Error(msg)) => eprintln!("apply error: {msg}"),
            Ok(Evt::Xrun) => {}
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    assert!(
        matches!(applied, Some((1, 1))),
        "backend did not apply the 1-in/1-out layout ({applied:?})"
    );

    // --- Phase 1: record track 0 (constant +0.25 for ~400 ms) -------------
    b.cmd(Cmd::ToggleRecord { i: 0 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_REC),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not enter REC"
    );
    std::thread::sleep(Duration::from_millis(400));
    b.cmd(Cmd::ToggleRecord { i: 0 }); // stop -> PLAY (has content)
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, lens } if states[0] == TR_PLAY && lens[0] > 12_000),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not stop into PLAY"
    );
    let len0 = b.engine.len_of(0);
    assert!(
        ((sr as u64 * 350 / 1000)..(sr as u64 * 450 / 1000)).contains(&(len0 as u64)),
        "recorded {len0} samples, expected ~{sr}×0.4"
    );

    // --- Phase 2: playback with a silent input -----------------------------
    // The sink must still carry the recorded +0.25 — proving the loop plays
    // back from its buffer rather than passing the (now silent) input through.
    const_val.store(0.0f32.to_bits(), Ordering::Relaxed);
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.25).abs() < 0.05).unwrap_or(false),
        Duration::from_secs(3),
        "playback of the recorded constant with silent input",
    );

    // --- Phase 3: additive overdub -0.1 on top of the +0.25 take -----------
    // Overdub must never replace or re-amp the original content: after a full
    // pass the buffer holds 0.25 + (-0.1) = 0.15 everywhere.
    const_val.store((-0.1f32).to_bits(), Ordering::Relaxed);
    b.cmd(Cmd::ToggleOverdub { i: 0 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_OD),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not enter OD"
    );
    let od_time = Duration::from_millis(len0 as u64 * 1000 / sr as u64 + 800);
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.05).abs() < 0.03).unwrap_or(false),
        od_time,
        "additive overdub: sink -> dry(-0.1) + mixed take(0.15) = 0.05",
    );
    b.cmd(Cmd::ToggleOverdub { i: 0 }); // back to PLAY
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_PLAY),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not leave OD"
    );
    const_val.store(0.0f32.to_bits(), Ordering::Relaxed);
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.15).abs() < 0.03).unwrap_or(false),
        Duration::from_secs(2),
        "overdubbed content (0.25 + -0.1 = 0.15) playing back at original level",
    );

    // --- Phase 4: easy-quantize against track 0's own length ---------------
    let len_before = b.engine.len_of(0);
    let (target, beat, _bar) = compute_target(len_before, 1, 4, Unit::Beat).unwrap();
    assert!(target > 0 && beat > 0.0);
    b.cmd(Cmd::Quantize { num: 4, den: 4, bars: 1, unit: Unit::Beat, root: 0, metro: false, bpm: 120 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Quantized { .. }),
            Duration::from_secs(5)
        )
        .is_some(),
        "no Quantized event"
    );
    let ideal = snap_length(len_before, target, LOOP_CAP);
    let w = (sr / 200).min(ideal / 4) as i64;
    let new_len = b.engine.len_of(0) as i64;
    assert!(
        (new_len - ideal as i64).abs() <= w + 64,
        "quantized len {new_len} vs grid point {ideal}"
    );
    assert!(b.engine.len_of(0) >= target);
    wait_for(
        &b,
        |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_IDLE),
        Duration::from_secs(2),
    );

    // --- Phase 5: independent mode — track 1 loops on its own clock --------
    b.cmd(Cmd::TogglePlay { i: 0 }); // restart the (now quantized) take
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_PLAY),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not resume playback"
    );
    mode.store(1, Ordering::Relaxed); // aperiodic PRNG content
    b.cmd(Cmd::ToggleRecord { i: 1 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[1] == TR_REC),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 1 did not enter REC"
    );
    std::thread::sleep(Duration::from_millis(156)); // ~7500 samples @ 48 kHz
    b.cmd(Cmd::ToggleRecord { i: 1 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, lens } if states[1] == TR_PLAY && lens[1] > 4_000),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 1 did not stop into PLAY"
    );
    let len1 = b.engine.len_of(1);
    assert!(
        ((sr as u64 * 120 / 1000)..(sr as u64 * 200 / 1000)).contains(&(len1 as u64)),
        "recorded {len1} samples, expected ~{sr}×0.156"
    );
    mode.store(0, Ordering::Relaxed); // silence the input before capturing
    std::thread::sleep(Duration::from_millis(300));

    let len0q = b.engine.len_of(0);
    let lmax = len0q.max(len1) as u64;
    let rem = lmax % len1 as u64;
    assert!(
        rem > 800 && len1 as u64 - rem > 800,
        "unlucky lengths for mode discrimination: lmax {lmax}, len1 {len1}"
    );

    let x = read_ring(&ring, &widx, CAP_N, Duration::from_secs(3)).expect("capture");
    let x = &x[2048..]; // drop the input-switching tail
    let v = var(x);
    assert!(v > 0.01, "no signal in capture (var {v})");
    let a = acf_norm(x, len1 as usize);
    let c = acf_norm(x, lmax as usize);
    assert!(a > 0.5 * v, "independent: no period at len1 (acf {a} vs var {v})");
    assert!(
        c.abs() < 0.2 * a,
        "independent: unexpected correlation at lmax (acf {c} vs {a})"
    );

    // --- Phase 6: dependent mode — endless ensemble of N root loops --------
    // Stop both tracks first so the run starts from silence at the sink
    // (the input is silent too), letting us measure the run cleanly.
    b.cmd(Cmd::TogglePlay { i: 0 });
    b.cmd(Cmd::TogglePlay { i: 1 });
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| m.abs() < 0.02).unwrap_or(false),
        Duration::from_secs(2),
        "silence before the dependent run",
    );

    b.cmd(Cmd::SetRootLoops(8)); // ensemble period = 8 root loops
    b.cmd(Cmd::SetDependent(true));
    b.cmd(Cmd::TogglePlay { i: 0 });
    b.cmd(Cmd::TogglePlay { i: 1 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_PLAY && states[1] == TR_PLAY),
            Duration::from_secs(2)
        )
        .is_some(),
        "tracks did not start in dependent mode"
    );

    // Continuous wrap: the ensemble must NOT stop at the end of the N root
    // loops — it loops forever. Sleep well past two full ensemble periods and
    // assert both tracks are still playing.
    let period = lmax * 8;
    std::thread::sleep(Duration::from_millis(period * 2 * 1000 / sr as u64 + 2000));
    assert_eq!(
        b.engine.state_of(0),
        TR_PLAY,
        "ensemble stopped at the end of the root loops (must loop forever)"
    );
    assert_eq!(b.engine.state_of(1), TR_PLAY, "ensemble stopped mid-run");

    // Capture ~2 ensemble periods already sitting in the ring (the sleep above
    // guaranteed they exist) and prove the wrap is seamless: no silent chunk
    // anywhere, the mix repeats exactly with the ensemble period (continuous
    // wrap), and the shorter loop keeps tiling.
    let need = (period as usize * 2 + 20_000).min(ring_n - 1);
    let end = widx.load(Ordering::Relaxed);
    let x: Vec<f32> = (0..need)
        .map(|i| {
            f32::from_bits(
                ring[((end - need as u64 + i as u64) % ring_n as u64) as usize]
                    .load(Ordering::Relaxed),
            )
        })
        .collect();
    let chunk = 480usize;
    let rms = |c: &[f32]| {
        (c.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / c.len() as f64).sqrt()
    };
    for k in 0..x.len() / chunk {
        assert!(
            rms(&x[k * chunk..(k + 1) * chunk]) > 0.05,
            "silent gap inside the dependent run at chunk {k} — ensemble stalled?"
        );
    }
    let v = var(&x);
    assert!(v > 0.01, "no signal in run (var {v})");
    let ap = acf_norm(&x, period as usize);
    assert!(
        ap > 0.5 * v,
        "dependent: mix does not repeat with the ensemble period (acf {ap} vs var {v})"
    );

    // Shorter loops must repeat (tile) across the ensemble — not play once
    // and go silent until the root ends. Track 1's contribution is strictly
    // periodic with its own length under modulo-wrap playback.
    let d1 = acf_norm(&x, len1 as usize);
    assert!(
        d1 > 0.5 * v,
        "dependent: shorter loop not repeating (acf at len1 {d1} vs var {v})"
    );

    // Stop the ensemble before clearing.
    b.cmd(Cmd::TogglePlay { i: 0 });
    b.cmd(Cmd::TogglePlay { i: 1 });
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| m.abs() < 0.02).unwrap_or(false),
        Duration::from_secs(2),
        "silence after the dependent run",
    );

    // --- Phase 7: clear ------------------------------------------------------
    b.cmd(Cmd::ClearTrack { i: 0 });
    b.cmd(Cmd::ClearTrack { i: 1 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, lens } if states[0] == TR_IDLE && states[1] == TR_IDLE && lens[0] == 0 && lens[1] == 0),
            Duration::from_secs(2)
        )
        .is_some(),
        "tracks did not clear"
    );
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| m.abs() < 0.02).unwrap_or(false),
        Duration::from_secs(2),
        "silence after clear",
    );

    b.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    let _ = active;
}

// ------------------------------------------------------ overdub undo/redo --

#[test]
fn e2e_overdub_undo() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> =
        Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let mode = Arc::new(AtomicU8::new(0));
    let const_val = Arc::new(AtomicU32::new(0.25f32.to_bits()));

    let (client, _status) = Client::new("undo_test", ClientOptions::NO_START_SERVER)
        .expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            LoopSrcProc {
                out: out_a,
                sink,
                mode: mode.clone(),
                const_val: const_val.clone(),
                seed: 0x9e37_79b9,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    b.cmd(Cmd::Apply {
        ins: vec![format!("{src_name}:out_a")],
        outs: vec![format!("{src_name}:sink")],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 1, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the 1-in/1-out layout"
    );

    // --- record +0.25 for ~400 ms, keep it playing -------------------------
    b.cmd(Cmd::ToggleRecord { i: 0 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_REC),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not enter REC"
    );
    std::thread::sleep(Duration::from_millis(400));
    b.cmd(Cmd::ToggleRecord { i: 0 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, lens } if states[0] == TR_PLAY && lens[0] > 12_000),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not stop into PLAY"
    );
    let len0 = b.engine.len_of(0);

    // --- additive overdub of -0.1 (buffer becomes 0.15 everywhere) ---------
    const_val.store((-0.1f32).to_bits(), Ordering::Relaxed);
    b.cmd(Cmd::ToggleOverdub { i: 0 });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, .. } if states[0] == TR_OD),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not enter OD"
    );
    let od_time = Duration::from_millis(len0 as u64 * 1000 / sr as u64 + 800);
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.05).abs() < 0.03).unwrap_or(false),
        od_time,
        "overdub pass: sink -> dry(-0.1) + mixed take(0.15) = 0.05",
    );
    b.cmd(Cmd::ToggleOverdub { i: 0 }); // back to PLAY
    const_val.store(0.0f32.to_bits(), Ordering::Relaxed);
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.15).abs() < 0.03).unwrap_or(false),
        Duration::from_secs(2),
        "overdubbed content (0.15) playing back",
    );

    // --- undo: the loop reverts to its pre-overdub content (0.25) ----------
    b.cmd(Cmd::UndoOverdub);
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Notice(m) if m.contains("undid overdub")),
            Duration::from_secs(2)
        )
        .is_some(),
        "no undo notice"
    );
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { states, lens } if states[0] == TR_PLAY && lens[0] == len0),
            Duration::from_secs(2)
        )
        .is_some(),
        "track 0 did not resume playing at its original length after undo"
    );
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.25).abs() < 0.03).unwrap_or(false),
        Duration::from_secs(2),
        "undone content (0.25) playing back",
    );

    // --- redo: the overdubbed state comes back (0.15) ----------------------
    b.cmd(Cmd::RedoOverdub);
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Notice(m) if m.contains("redid overdub")),
            Duration::from_secs(2)
        )
        .is_some(),
        "no redo notice"
    );
    wait_until(
        || ring_mean(&ring, &widx, 480).map(|m| (m - 0.15).abs() < 0.03).unwrap_or(false),
        Duration::from_secs(2),
        "redone overdub (0.15) playing back",
    );

    // --- cleanup ------------------------------------------------------------
    b.cmd(Cmd::TogglePlay { i: 0 });
    b.cmd(Cmd::ClearTrack { i: 0 });
    b.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    let _ = active;
}

// --------------------------------------------------------------- metronome --

/// Sink-only capture client: records everything arriving at the applied
/// simplooper output into a ring buffer.
struct CapProc {
    sink: Port<AudioIn>,
    ring: Arc<Vec<AtomicU32>>,
    widx: Arc<AtomicU64>,
}

impl ProcessHandler for CapProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let sink = self.sink.as_slice(ps);
        let nring = self.ring.len() as u64;
        let mut w = self.widx.load(Ordering::Relaxed);
        for s in sink {
            self.ring[(w % nring) as usize].store(s.to_bits(), Ordering::Relaxed);
            w += 1;
        }
        self.widx.store(w, Ordering::Relaxed);
        Control::Continue
    }
}

/// Detect metronome clicks in a capture. Returns `(onset sample, tone)` per
/// click, where the tone is classified from the zero-crossing rate over the
/// first 15 ms of the click: 0 = downbeat (1760 Hz), 1 = quarter (1175 Hz),
/// 2 = subdivision (880 Hz).
fn metro_clicks(x: &[f32], sr: u32) -> Vec<(usize, u8)> {
    let thr = 0.08f32;
    let win = (sr as usize * 15 / 1000).max(16);
    let quiet_need = (sr as usize / 500).max(8); // 2 ms of quiet re-arms
    let mut out: Vec<(usize, u8)> = Vec::new();
    let mut armed = true;
    let mut quiet = 0usize;
    let mut i = 0usize;
    while i < x.len() {
        if armed && x[i].abs() > thr {
            let end = (i + win).min(x.len());
            let zc = (i + 1..end).filter(|&k| (x[k] >= 0.0) != (x[k - 1] >= 0.0)).count();
            let f = zc as f32 * sr as f32 / (2.0 * (end - i) as f32);
            let tone = if f > 1450.0 { 0 } else if f > 1000.0 { 1 } else { 2 };
            out.push((i, tone));
            armed = false;
            quiet = 0;
            i = end; // skip the rest of this click
        } else {
            // Re-arm only after a sustained quiet run, so zero crossings
            // inside one click cannot split it into two onsets.
            if x[i].abs() < thr * 0.3 {
                quiet += 1;
            } else {
                quiet = 0;
            }
            if quiet >= quiet_need {
                armed = true;
            }
            i += 1;
        }
    }
    out
}

#[test]
fn e2e_metronome() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> =
        Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));

    let (client, _status) = Client::new("metro_test", ClientOptions::NO_START_SERVER)
        .expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async((), CapProc { sink, ring: ring.clone(), widx: widx.clone() })
        .unwrap();

    b.cmd(Cmd::Apply { ins: vec![], outs: vec![format!("{src_name}:sink")] });
    assert!(
        wait_for(&b, |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }), Duration::from_secs(5))
            .is_some(),
        "metronome test: output not applied"
    );
    // Let the sink settle on silence before arming the click.
    std::thread::sleep(Duration::from_millis(150));

    let read_range = |a: u64, c: u64| -> Vec<f32> {
        let n = ring_n as u64;
        (a..c)
            .map(|i| f32::from_bits(ring[(i % n) as usize].load(Ordering::Relaxed)))
            .collect()
    };

    // --- Phase 1: activation starts immediately on the downbeat ------------
    // 120 BPM, eighth-note subdivision, 4 beats per measure.
    let beat = sr as usize / 2; // samples per quarter note
    let start = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::SetMetro { on: true, bpm: 120, sub: 2, num: 4 });
    std::thread::sleep(Duration::from_millis(2600)); // ~5 beats
    let x = read_range(start, widx.load(Ordering::Relaxed));
    let clicks = metro_clicks(&x, sr);
    assert!(!clicks.is_empty(), "metronome produced no clicks");
    assert!(
        clicks[0].0 < sr as usize / 10,
        "first click {:.0} ms after activation — must start directly on the beat",
        clicks[0].0 as f32 * 1000.0 / sr as f32
    );

    // Expected tone pattern for 4/4 with eighth notes:
    // downbeat, sub, quarter, sub, quarter, sub, quarter, sub, downbeat, sub.
    let expect = [0u8, 2, 1, 2, 1, 2, 1, 2, 0, 2];
    assert!(
        clicks.len() >= expect.len(),
        "expected at least {} clicks in 2.6 s, got {}",
        expect.len(),
        clicks.len()
    );
    for (k, &want) in expect.iter().enumerate() {
        assert_eq!(
            clicks[k].1, want,
            "click {k} has tone {} ({} Hz-class), expected {want} at {:.3} s",
            clicks[k].1,
            [1760, 1175, 880][clicks[k].1 as usize],
            clicks[k].0 as f32 / sr as f32
        );
    }
    // Clicks land on the eighth-note grid (every half beat, ±10%).
    for k in 1..expect.len() {
        let d = clicks[k].0 - clicks[k - 1].0;
        let want = beat / 2;
        assert!(
            (d as isize - want as isize).abs() < (want / 10) as isize,
            "click {k} off grid: interval {d}, expected ~{want}"
        );
    }

    // --- Phase 2: subdivision change applies on the next quarter note ------
    let start2 = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::SetMetro { on: true, bpm: 120, sub: 4, num: 4 });
    std::thread::sleep(Duration::from_millis(1500)); // 3 more beats
    let end2 = widx.load(Ordering::Relaxed);
    let x2 = read_range(start2, end2);
    // Analyze only the last ~0.8 s: fully inside the sixteenth-note grid.
    let tail_at = x2.len().saturating_sub(sr as usize * 8 / 10);
    let clicks2 = metro_clicks(&x2[tail_at..], sr);
    assert!(
        clicks2.len() >= 4,
        "sixteenth subdivision not applied after the change: {} clicks in 0.8 s",
        clicks2.len()
    );
    for k in 1..clicks2.len() {
        let d = clicks2[k].0 - clicks2[k - 1].0;
        let want = beat / 4;
        assert!(
            (d as isize - want as isize).abs() < (want / 6) as isize,
            "post-change click {k} off the sixteenth grid: interval {d}, expected ~{want}"
        );
    }

    // --- Phase 3: stop silences the click -----------------------------------
    b.cmd(Cmd::SetMetro { on: false, bpm: 120, sub: 4, num: 4 });
    std::thread::sleep(Duration::from_millis(400));
    let start3 = widx.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(600));
    let x3 = read_range(start3, widx.load(Ordering::Relaxed));
    assert!(
        metro_clicks(&x3, sr).is_empty(),
        "metronome still clicking after stop"
    );

    b.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    let _ = active;
}
