//! End-to-end tests for the simplooper JACK backend.
//!
//! These drive the real `Backend` (manager thread + realtime handler) against a
//! live JACK/PipeWire-JACK server, using a second JACK client as test source:
//! - audio: two constant sources are summed into one sink through the applied
//!   port layout (`out = Σ in × gain`);
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
            for i in 0..n {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 17;
                self.seed ^= self.seed << 5;
                out[i] = if self.seed & 1 == 0 { 0.2 } else { -0.2 };
            }
        } else {
            for i in 0..n {
                out[i] = c;
            }
        }
        let sink = self.sink.as_slice(ps);
        let base = self.widx.load(Ordering::Relaxed) as usize;
        for i in 0..n {
            self.ring[(base + i) % self.ring.len()]
                .store(sink[i].to_bits(), Ordering::Relaxed);
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
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 16;
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

    // --- Phase 3: overdub -0.1 until the buffer is fully overwritten -------
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
        || ring_mean(&ring, &widx, 480).map(|m| (m + 0.2).abs() < 0.03).unwrap_or(false),
        od_time,
        "overdub to fully overwrite the take (sink -> -0.2)",
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
        || ring_mean(&ring, &widx, 480).map(|m| (m + 0.1).abs() < 0.03).unwrap_or(false),
        Duration::from_secs(2),
        "overdubbed content (-0.1) playing back",
    );

    // --- Phase 4: easy-quantize against track 0's own length ---------------
    let len_before = b.engine.len_of(0);
    let (target, beat, _bar) = compute_target(len_before, 1, 4, Unit::Beat).unwrap();
    assert!(target > 0 && beat > 0.0);
    b.cmd(Cmd::Quantize { num: 4, den: 4, bars: 1, unit: Unit::Beat });
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

    // --- Phase 6: dependent mode — the ensemble repeats with period lmax ---
    b.cmd(Cmd::SetDependent(true));
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::LoopState { .. }),
            Duration::from_secs(2)
        )
        .is_some(),
        "no LoopState after SetDependent"
    );
    std::thread::sleep(Duration::from_millis(400));
    let x = read_ring(&ring, &widx, CAP_N, Duration::from_secs(3)).expect("capture");
    let x = &x[2048..];
    let v = var(x);
    assert!(v > 0.01, "no signal in capture (var {v})");
    let d = acf_norm(x, lmax as usize);
    assert!(d > 0.5 * v, "dependent: no period at lmax (acf {d} vs var {v})");
    // Shorter loops must repeat (tile) across the ensemble — not play once
    // and go silent until the root ends. Track 1's contribution is strictly
    // periodic with its own length under modulo-wrap playback.
    let d1 = acf_norm(x, len1 as usize);
    assert!(
        d1 > 0.5 * v,
        "dependent: shorter loop not repeating (acf at len1 {d1} vs var {v})"
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
