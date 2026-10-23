//! End-to-end tests for the slooper DAW backend.
//!
//! These drive the real `Backend` (manager thread + realtime handler) against
//! a live JACK/PipeWire-JACK server, using a second JACK client as test
//! source:
//! - audio: two constant sources are summed into one sink through the applied
//!   port layout (`out = Σ in × gain`);
//! - transport: the global play/stop/seek + loop wrap/park behaviour, track
//!   mute flags, and the record-take pipeline (`Evt::Take`) are verified
//!   acoustically at a sink;
//! - metronome: click grid, three-tone pattern and immediate downbeat start
//!   are captured at a sink;
//! - export: a placed project renders through `Cmd::Export{Track,Project}`
//!   to a decodable WAV;
//! - midi: messages are sent to `slooper:midi_in` and must come back parsed.
//!
//! If no JACK/PipeWire server is running, the tests skip themselves.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, MidiOut, Port, ProcessHandler, ProcessScope,
    RawMidi,
};

use slooper::arrange::{Arrangement, Clip};
use slooper::audio::{Backend, Cmd, Evt, FxSlot, TrackFx};
use slooper::midi::MidiMsg;

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
        Some(Evt::Ready {
            client_name,
            sample_rate,
        }) => Some((b, client_name, sample_rate)),
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
    frames: Arc<AtomicU64>,      // process cycles handled
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
        self.last_sample
            .store(sink[n - 1].to_bits(), Ordering::Relaxed);
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
    let (client, _status) =
        Client::new("mixer_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let out_b = client.register_port("out_b", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();

    let last_sample = Arc::new(AtomicU32::new(f32::NAN.to_bits()));
    let frames = Arc::new(AtomicU64::new(0));
    let client_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            MixProc {
                a: out_a,
                b: out_b,
                sink,
                last_sample: last_sample.clone(),
                frames: frames.clone(),
            },
        )
        .unwrap();

    // Apply the layout through the backend's command channel.
    // Route 3 = mono downmix (the historical output behaviour).
    b.cmd(Cmd::Apply {
        ins: vec![
            format!("{client_name}:out_a"),
            format!("{client_name}:out_b"),
        ],
        outs: vec![(format!("{client_name}:sink"), 3)],
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
        assert!(
            Instant::now() < deadline,
            "sink never reached 0.125 (last: {observed}, frames: {})",
            frames.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(frames.load(Ordering::Relaxed) > 0);

    // Teardown: stop the backend's audio, drop clients.
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
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
        let _ = self.out.writer(ps).write(&RawMidi {
            time: 0,
            bytes: msg,
        });
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

    // Source client with a MIDI out wired to slooper:midi_in.
    let (client, _status) =
        Client::new("midi_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let midi_out = client
        .register_port("midi_out", MidiOut::default())
        .unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(
            &format!("{src_name}:midi_out"),
            &format!("{client_name}:midi_in"),
        )
        .expect("connect to slooper:midi_in");
    let active = client
        .activate_async(
            (),
            MidiSrcProc {
                out: midi_out,
                step: 0,
            },
        )
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
        Some(&MidiMsg::NoteOn {
            channel: 0,
            note: 60,
            velocity: 100
        })
    );
    let cc = seen
        .iter()
        .find(|m| matches!(m, MidiMsg::ControlChange { .. }));
    assert_eq!(
        cc,
        Some(&MidiMsg::ControlChange {
            channel: 1,
            number: 7,
            value: 128
        })
    );
    let pc = seen
        .iter()
        .find(|m| matches!(m, MidiMsg::ProgramChange { .. }));
    assert_eq!(
        pc,
        Some(&MidiMsg::ProgramChange {
            channel: 2,
            program: 11
        })
    );

    // Display formatting sanity (what the GUI shows).
    let note_on = note_on.unwrap();
    assert_eq!(note_on.type_name(), "NOTE ON");
    assert!(
        note_on.describe().contains("ch 01"),
        "{}",
        note_on.describe()
    );
    assert!(note_on.describe().contains("C4"), "{}", note_on.describe());

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
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
        let _ = self.out.writer(ps).write(&RawMidi {
            time: 0,
            bytes: msg,
        });
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
    let dir = std::env::temp_dir().join(format!("slooper-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &dir);

    // Arm the "select track 2" slot (Select(1) in the action table) so the
    // fired action is observable via `App::selected_loop`. The next bindable
    // MIDI message is captured.
    let mut app = slooper::ui::App::new(b);
    let slot = slooper::midi::MidiAction::idx(slooper::midi::MidiAction::Select(1));
    app.arm_midi_slot(slot);
    app.drain_events();

    // Source client emulating SINCO MIDI 1, wired to slooper:midi_in.
    let (client, _status) =
        Client::new("sinco_emu", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let midi_out = client
        .register_port("midi_out", MidiOut::default())
        .unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(
            &format!("{src_name}:midi_out"),
            &format!("{client_name}:midi_in"),
        )
        .expect("connect to slooper:midi_in");
    let active = client
        .activate_async(
            (),
            SincoSrcProc {
                out: midi_out,
                step: 0,
            },
        )
        .unwrap();

    // Pump App events until the first PC is captured and a repeated PC fires.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.drain_events();
        if app.midi_bind_at(slot).is_some() && app.selected_loop() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // The armed slot must have captured a message from the device's pattern.
    // Which of the four PCs arrives first depends on JACK graph scheduling, so
    // accept any PC with value 1 or 2 on channels 0–3 — exactly what the SINCO
    // MIDI 1 sends.
    let expected: &[slooper::midi::MidiBind] = &[
        slooper::midi::MidiBind {
            ty: 2,
            ch: 0,
            d1: 1,
        },
        slooper::midi::MidiBind {
            ty: 2,
            ch: 1,
            d1: 2,
        },
        slooper::midi::MidiBind {
            ty: 2,
            ch: 2,
            d1: 1,
        },
        slooper::midi::MidiBind {
            ty: 2,
            ch: 3,
            d1: 2,
        },
    ];
    let captured = app.midi_bind_at(slot);
    assert!(
        expected.contains(&captured.unwrap_or(slooper::midi::MidiBind {
            ty: 0,
            ch: 99,
            d1: 99
        })),
        "armed slot must capture one of the SINCO PC patterns, got {captured:?}"
    );
    assert_eq!(
        app.selected_loop(),
        1,
        "a repeated matching PC must fire the bound action (select loop 2)"
    );

    app.backend().cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ----------------------------------------------- DAW transport & takes --

/// Source client: a controllable sample generator (constant or PRNG ±0.2)
/// plus a lock-free capture ring of the sink (what slooper wrote back).
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
            self.ring[(base + i) % self.ring.len()].store(s.to_bits(), Ordering::Relaxed);
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
                    ring[((end - need as u64 + i as u64) % n) as usize].load(Ordering::Relaxed),
                )
            })
            .collect::<Vec<f32>>(),
    )
}

/// Mean of the most recent `n` captured sink samples.
fn ring_mean(ring: &Arc<Vec<AtomicU32>>, widx: &AtomicU64, n: usize) -> Option<f32> {
    read_ring(ring, widx, n, Duration::from_millis(500)).map(|x| x.iter().sum::<f32>() / n as f32)
}

/// Peak |sample| of the most recent `n` captured sink samples.
fn ring_peak(ring: &Arc<Vec<AtomicU32>>, widx: &AtomicU64, n: usize) -> Option<f32> {
    read_ring(ring, widx, n, Duration::from_millis(500))
        .map(|x| x.iter().fold(0.0f32, |m, s| m.max(s.abs())))
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

#[test]
fn e2e_transport_and_clips() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let mode = Arc::new(AtomicU8::new(0));
    let const_val = Arc::new(AtomicU32::new(0.0f32.to_bits()));

    let (client, _status) =
        Client::new("daw_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
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
        outs: vec![(format!("{src_name}:sink"), 3)],
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

    // ---- a project: one 0.5 asset on track 0 at t = 0 ----------------------
    let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 16_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    let clip_len = 16_000u64;
    let total = 32_000u64; // two clip lengths: a real "project end"
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![a0.clone()]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(arr),
        len: total,
    });
    b.cmd(Cmd::SetLoop(true));

    // The transport is the only thing that makes clip sound: silent before.
    std::thread::sleep(Duration::from_millis(150));
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "silence before the transport starts",
    );

    // ---- play: the clip plays from position 0 -------------------------------
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "transport playing the 0.5 clip",
    );
    assert!(
        b.engine.transport_on(),
        "PlayAll did not set the transport flag"
    );

    // ---- a stopped track contributes nothing (clips stay placed) ------------
    b.cmd(Cmd::ToggleTrackOn { i: 0 });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "muting track 0 silences the mix",
    );
    b.cmd(Cmd::ToggleTrackOn { i: 0 });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "un-muting track 0 restores the clip",
    );

    // ---- stop holds the position; play resumes from it ----------------------
    b.cmd(Cmd::StopAll);
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Transport { on: false }),
            Duration::from_secs(2)
        )
        .is_some(),
        "no Transport(off) event from StopAll"
    );
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "silence after StopAll",
    );
    let held = b.engine.pos();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(b.engine.pos(), held, "position moved while stopped");
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "play again resumes from the held position (still inside the clip)",
    );

    // ---- seek past the clip end: silence, playhead still running ------------
    b.cmd(Cmd::Seek {
        pos: clip_len + 4_000,
    });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "silence after seeking past the clip end",
    );
    // Seek back inside: sound returns immediately (scrub-scan works).
    b.cmd(Cmd::Seek { pos: 2_000 });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "sound after scrubbing back into the clip",
    );

    // ---- loop wrap: the playhead rolls over instead of stopping -------------
    // Leave ~125 ms of runway, give the Seek command time to land, then
    // sample: 250 ms later the playhead MUST have passed the end and
    // reappeared at the top.
    b.cmd(Cmd::Seek {
        pos: total - sr as u64 / 8,
    });
    std::thread::sleep(Duration::from_millis(50));
    let p0 = b.engine.pos();
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        b.engine.transport_on(),
        "looping transport must not stop at the end"
    );
    let p1 = b.engine.pos();
    assert!(p1 < p0, "no wrap: pos went {p0} -> {p1} with loop on");

    // ---- loop off: park at the end and stop ---------------------------------
    b.cmd(Cmd::SetLoop(false));
    b.cmd(Cmd::Seek {
        pos: total - sr as u64 / 4,
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Transport { on: false }),
            Duration::from_secs(3)
        )
        .is_some(),
        "non-looping transport did not stop at the project end"
    );
    let parked = b.engine.pos();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(b.engine.pos(), parked, "parked playhead must hold");
    assert!(
        parked >= total - sr as u64 / 2,
        "parked far before the end: {parked}"
    );
    b.cmd(Cmd::SetLoop(true));

    // ---- recording produces a take (Evt::Take), not loop state --------------
    let at0 = b.engine.pos();
    const_val.store(0.25f32.to_bits(), Ordering::Relaxed);
    b.cmd(Cmd::ToggleRecord { i: 0 });
    std::thread::sleep(Duration::from_millis(300));
    b.cmd(Cmd::ToggleRecord { i: 0 });
    let take = wait_for(
        &b,
        |e| matches!(e, Evt::Take { track: 0, .. }),
        Duration::from_secs(3),
    );
    const_val.store(0.0f32.to_bits(), Ordering::Relaxed);
    let Evt::Take { at, data, .. } = take.expect("no Evt::Take from record toggle") else {
        unreachable!()
    };
    assert!(
        data.len() >= (sr as f32 * 0.2) as usize && data.len() <= (sr as f32 * 0.6) as usize,
        "take length {} samples, expected ~0.3 s",
        data.len()
    );
    let mean = data.iter().sum::<f32>() / data.len() as f32;
    assert!(
        (mean - 0.25).abs() < 0.03,
        "take captured mean {mean}, expected 0.25"
    );
    assert!(
        at.saturating_sub(at0) < sr as u64,
        "take start {at} far from playhead {at0}"
    );

    b.cmd(Cmd::StopAll);
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// --------------------------------------------------------------- metronome --

/// Sink-only capture client: records everything arriving at the applied
/// slooper output into a ring buffer.
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
            let zc = (i + 1..end)
                .filter(|&k| (x[k] >= 0.0) != (x[k - 1] >= 0.0))
                .count();
            let f = zc as f32 * sr as f32 / (2.0 * (end - i) as f32);
            let tone = if f > 1450.0 {
                0
            } else if f > 1000.0 {
                1
            } else {
                2
            };
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
fn e2e_midi_clip_plays_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Sink client: slooper's mix must appear at our capture ring.
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("midi_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            LoopSrcProc {
                out: out_a,
                sink,
                mode: Arc::new(AtomicU8::new(0)),
                const_val: Arc::new(AtomicU32::new(0.0f32.to_bits())),
                seed: 0x9e37_79b9,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the sink layout"
    );

    // The shared default synth, installed exactly as the app does it.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(slooper::audio::SynthHandle::Script(
        inst,
    )))));

    // A MIDI-only project: one bar of quarter notes at 120 BPM on track 0.
    // No audio asset exists at index 0 — the clip resolves through the MIDI
    // table only (a missing audio asset must skip, not crash the RT mix).
    let mut pat = slooper::midipat::Pattern::new(120);
    for (i, note) in [60u8, 64, 67, 72].iter().enumerate() {
        pat.notes.push(slooper::midipat::Note {
            ch: 0,
            note: *note,
            vel: 110,
            press: 0,
            start: (i as u32) * slooper::midipat::PPQ as u32,
            len: slooper::midipat::PPQ as u32 / 2,
        });
    }
    pat.sort();
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    // Project length = exactly the pattern's bar (2 s), so the looping
    // transport re-enters the clip after every wrap.
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: sr as u64 * 2,
    });
    b.cmd(Cmd::SetLoop(true));

    // Silent while the transport is stopped.
    std::thread::sleep(Duration::from_millis(150));
    wait_until(
        || ring_peak(&ring, &widx, 960).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(3),
        "silence before the transport starts",
    );

    // Play: the notes must sound through the default piano. This is the
    // path that used to abort the RT thread (midi scratch rows were never
    // allocated): the first staged note panicked inside `process`.
    b.cmd(Cmd::PlayAll);
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(5),
        "MIDI clip audible through the default synth",
    );

    // Still alive mid-bar and after a transport wrap: notes land in many
    // different blocks, and the wrapped playhead re-arms the whole event
    // queue — a dead RT thread stops cycling the graph entirely.
    // Windows are 333 ms so they can never fall inside the 250 ms gap
    // between the last note-off and the wrap.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        ring_peak(&ring, &widx, 16000).map(|p| p > 0.002).unwrap_or(false),
        "MIDI render died mid-bar",
    );
    let after = widx.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        widx.load(Ordering::Relaxed) > after,
        "audio graph stopped cycling (RT thread died?)"
    );
    assert!(
        ring_peak(&ring, &widx, 16000).map(|p| p > 0.002).unwrap_or(false),
        "no sound after the transport wrapped through the loop point",
    );

    b.cmd(Cmd::StopAll);
    wait_for(
        &b,
        |e| matches!(e, Evt::Transport { on: false }),
        Duration::from_secs(2),
    );
    std::thread::sleep(Duration::from_millis(700)); // let the tail ring out
    wait_until(
        || ring_peak(&ring, &widx, 2400).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(5),
        "silence after StopAll (transport gate closes the MIDI render)",
    );
    let _ = active;
}

#[test]
fn e2e_metronome() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));

    let (client, _status) =
        Client::new("metro_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
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
    b.cmd(Cmd::SetMetro {
        on: true,
        bpm: 120,
        sub: 2,
        num: 4,
    });
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
            clicks[k].1,
            want,
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

    // Playhead freeze: a bare metronome (transport off) must never drag
    // the playhead along — regression for "the metronome forces the
    // playback bar to advance".
    let p0 = b.engine.pos();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(b.engine.pos(), p0, "bare metronome advanced the playhead");

    // --- Phase 2: subdivision change applies on the next quarter note ------
    let start2 = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::SetMetro {
        on: true,
        bpm: 120,
        sub: 4,
        num: 4,
    });
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
    b.cmd(Cmd::SetMetro {
        on: false,
        bpm: 120,
        sub: 4,
        num: 4,
    });
    std::thread::sleep(Duration::from_millis(400));
    let start3 = widx.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(600));
    let x3 = read_range(start3, widx.load(Ordering::Relaxed));
    assert!(
        metro_clicks(&x3, sr).is_empty(),
        "metronome still clicking after stop"
    );

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ------------------------------------------------------------------ export --

/// The Export button's whole path: placed project -> Cmd::ExportTrack /
/// Cmd::ExportProject -> decodable WAV on disk, extension appended from the
/// format (the picker path never carries one), plus explicit "no clips"
/// errors for empty tracks and empty projects.
#[test]
fn e2e_export_writes_file() {
    use slooper::audio_io::ExportFormat;
    let _g = e2e_lock();
    let Some((b, _cn, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    // A project the engine can render without any live ports: a 0.5 asset on
    // track 0, plus a child-lane clip at half level on the same track.
    let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 8_000]);
    let a1: Arc<[f32]> = Arc::from(vec![0.25f32; 4_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    arr.insert(
        0,
        1,
        Clip::new(1, 4_000),
    );
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![a0, a1]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(arr),
        len: 12_000,
    });

    let dir = std::env::temp_dir().join(format!("slooper-e2e-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let stem = dir.join("mix");

    // Track export: both lanes of track 0, engine-rate mixdown.
    b.cmd(Cmd::ExportTrack {
        i: 0,
        path: stem.clone(),
        fmt: ExportFormat::Wav,
    });
    let got = wait_for(
        &b,
        |e| matches!(e, Evt::Notice(m) if m.contains("exported")),
        Duration::from_secs(5),
    );
    assert!(got.is_some(), "no export Notice for a track with clips");
    let file = stem.with_extension("wav"); // extension from the format
    let samples = slooper::audio_io::decode_to_mono(&file, sr).expect("exported wav decodes");
    assert!(
        samples.len() >= 12_000,
        "export shorter than the project: {}",
        samples.len()
    );
    // [0,4000): only the 0.5 clip; [4000,8000): both lanes summed; then silence.
    let seg = |a: usize, c: usize| samples[a..c].iter().sum::<f32>() / (c - a) as f32;
    assert!((seg(0, 4_000) - 0.5).abs() < 0.02, "lane 0 only");
    assert!(
        (seg(4_000, 8_000) - 0.75).abs() < 0.02,
        "lanes stack additively"
    );
    assert!(
        seg(8_000, 12_000).abs() < 1e-6,
        "silence past the last clip"
    );

    // Whole-project export: nothing is playing, but placement is the truth.
    b.cmd(Cmd::ExportProject {
        path: dir.join("whole"),
        fmt: ExportFormat::Wav,
    });
    let got = wait_for(
        &b,
        |e| matches!(e, Evt::Notice(m) if m.contains("exported project")),
        Duration::from_secs(5),
    );
    assert!(got.is_some(), "no Notice for the whole-project export");
    assert!(dir.join("whole.wav").exists(), "project wav missing");

    // Empty track / empty project must fail loudly, not write silence.
    b.cmd(Cmd::ExportTrack {
        i: 7,
        path: dir.join("none"),
        fmt: ExportFormat::Wav,
    });
    let err = wait_for(
        &b,
        |e| matches!(e, Evt::Error(m) if m.contains("no placed clips")),
        Duration::from_secs(2),
    );
    assert!(
        err.is_some(),
        "empty-track export must report 'no placed clips'"
    );
    assert!(
        !dir.join("none.wav").exists(),
        "no file may be written for an empty track"
    );

    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(Arrangement::new()),
        len: 4_800,
    });
    b.cmd(Cmd::ExportProject {
        path: dir.join("empty"),
        fmt: ExportFormat::Wav,
    });
    let err = wait_for(
        &b,
        |e| matches!(e, Evt::Error(m) if m.contains("no clips placed")),
        Duration::from_secs(2),
    );
    assert!(err.is_some(), "empty-project export must report an error");
    assert!(
        !dir.join("empty.wav").exists(),
        "no file for an empty project"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// -------------------------------------------------------------- monitoring --

/// Live input pass-through follows the SELECTED track: input x (vol x gain)
/// of the selected track, equal-power panned across the routed outputs.
/// Nothing needs to be recording for this to hold — that is the whole point
/// of "monitoring rides the selection".
#[test]
fn e2e_selected_loop_monitoring() {
    let _g = e2e_lock();
    let Some((b, _cn, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 16;
    let ring_l: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_r: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));

    struct MonCapProc {
        out: Port<AudioOut>,
        sink_l: Port<AudioIn>,
        sink_r: Port<AudioIn>,
        rl: Arc<Vec<AtomicU32>>,
        rr: Arc<Vec<AtomicU32>>,
        widx: Arc<AtomicU64>,
    }
    impl ProcessHandler for MonCapProc {
        fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
            self.out.as_mut_slice(ps).fill(0.5);
            let l = self.sink_l.as_slice(ps);
            let r = self.sink_r.as_slice(ps);
            let base = self.widx.load(Ordering::Relaxed) as usize;
            for (i, s) in l.iter().enumerate() {
                self.rl[(base + i) % self.rl.len()].store(s.to_bits(), Ordering::Relaxed);
            }
            for (i, s) in r.iter().enumerate() {
                self.rr[(base + i) % self.rr.len()].store(s.to_bits(), Ordering::Relaxed);
            }
            self.widx.fetch_add(ps.n_frames() as u64, Ordering::Relaxed);
            Control::Continue
        }
    }

    let (client, _status) =
        Client::new("mon_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            MonCapProc {
                out: out_a,
                sink_l,
                sink_r,
                rl: ring_l.clone(),
                rr: ring_r.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    // Selected loop 0 at vol 0.5, gain 1.0, pan hard right (1.0):
    // expect L = 0, R = 0.5 x 0.5 = 0.25 on the monitored signal.
    b.engine.set_loop_vol(0, 0.5);
    b.engine.set_loop_gain(0, 1.0);
    b.engine.set_loop_pan(0, 1.0);
    b.cmd(Cmd::SetSelected(0));
    b.cmd(Cmd::Apply {
        ins: vec![format!("{src_name}:out_a")],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { .. }),
            Duration::from_secs(5)
        )
        .is_some(),
        "layout not applied"
    );

    let expect = |ring: &Arc<Vec<AtomicU32>>, want: f32, what: &str| {
        let mut seen = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(30));
            if let Some(x) = read_ring(ring, &widx, 480, Duration::from_millis(200)) {
                let m = x.iter().sum::<f32>() / x.len() as f32;
                seen = Some(m);
                if (m - want).abs() < 0.02 {
                    return;
                }
            }
        }
        panic!("{what}: want mean {want}, last saw {seen:?}");
    };
    expect(&ring_l, 0.0, "left bus (pan hard right)");
    expect(&ring_r, 0.25, "right bus (input x selected-track vol)");

    // Switching the selection to loop 1 (untouched: vol 1, gain 1, centre)
    // re-shapes monitoring immediately: both buses at 0.5 x 0.7071 ≈ 0.354.
    b.engine.set_loop_vol(1, 1.0);
    b.engine.set_loop_gain(1, 1.0);
    b.engine.set_loop_pan(1, 0.5);
    b.cmd(Cmd::SetSelected(1));
    let want = 0.5f32 * std::f32::consts::FRAC_1_SQRT_2;
    expect(&ring_l, want, "left bus after selection switch");
    expect(&ring_r, want, "right bus after selection switch");

    // Settings → live input monitoring (Cmd::SetMonitor): off mutes the
    // pass-through while every level stays put; on brings it straight back.
    b.cmd(Cmd::SetMonitor(false));
    expect(&ring_l, 0.0, "left bus after monitoring off");
    expect(&ring_r, 0.0, "right bus after monitoring off");
    b.cmd(Cmd::SetMonitor(true));
    let want = 0.5f32 * std::f32::consts::FRAC_1_SQRT_2;
    expect(&ring_l, want, "left bus after monitoring back on");
    expect(&ring_r, want, "right bus after monitoring back on");

    // Silence via volume must silence monitoring too.
    b.engine.set_loop_vol(1, 0.0);
    expect(&ring_l, 0.0, "left bus after vol 0");
    expect(&ring_r, 0.0, "right bus after vol 0");

    b.engine.set_loop_vol(0, 1.0);
    b.engine.set_loop_pan(0, 0.5);
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ------------------------------------------------------------------- vst ---

/// VST inserts on the export path. A constant burst on a track carrying a
/// real reverb plugin must produce a tail after the clip end — exactly where
/// the same render without the insert is silent — proving the private-copy
/// render path opens, processes, and parameterises real VST2/VST3 binaries.
/// Skips itself when no plugin is installed (CI).
#[test]
fn e2e_vst_insert_export() {
    use slooper::audio_io::ExportFormat;
    use slooper::vsthost::{open, VstKind};
    let _g = e2e_lock();
    let home = std::env::var("HOME").unwrap_or_default();
    let plugin = [
        format!("{home}/.vst3/DragonflyHallReverb.vst3"),
        format!("{home}/.vst/DragonflyHallReverb-vst.so"),
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|p| p.exists());
    let Some(plugin) = plugin else { return }; // no plugin installed: skip
    let kind = if plugin.extension().is_some_and(|e| e == "vst3") {
        VstKind::V3
    } else {
        VstKind::V2
    };
    let Some((b, _cn, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // 4k-sample constant burst on track 0; 16k-sample project leaves a 12k
    // tail window that is provably dry-silent without the insert.
    let a0: Arc<[f32]> = Arc::from(vec![0.3f32; 4_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![a0]),
        arr: Arc::new(arr),
        len: 16_000,
    });

    let dir = std::env::temp_dir().join(format!("slooper-e2e-vst-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let export = |name: &str| {
        b.cmd(Cmd::ExportTrack {
            i: 0,
            path: dir.join(name),
            fmt: ExportFormat::Wav,
        });
        let got = wait_for(
            &b,
            |e| matches!(e, Evt::Notice(m) if m.contains("exported")),
            Duration::from_secs(5),
        );
        assert!(got.is_some(), "no export Notice ({name})");
        slooper::audio_io::decode_to_mono(&dir.join(format!("{name}.wav")), sr)
            .expect("exported wav decodes")
    };

    let dry = export("dry");
    let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    assert!(energy(&dry[8_000..]) < 1e-9, "dry tail must be silence");

    // Attach the reverb (same open path the UI uses) and export again.
    let load = open(&plugin, kind, sr, None).expect("open reverb plugin");
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::vst(load.entry.clone())]),
        },
    });
    // Same command queue: the export below necessarily runs after the swap.

    let wet = export("wet");
    assert!(wet.iter().all(|v| v.is_finite()), "non-finite samples");
    assert!(energy(&wet[..4_000]) > 1e-4, "wet body must carry audio");
    assert!(
        energy(&wet[8_000..]) > 1e-8,
        "reverb insert must leave a tail where the dry render is silent"
    );

    // Removing the insert restores the dry export.
    b.cmd(Cmd::SetTrackEffects { i: 0, chain: TrackFx::default() });
    drop(load);
    let back = export("back");
    assert!(energy(&back[8_000..]) < 1e-9, "tail gone after removing insert");

    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(Arrangement::new()),
        len: 4_800,
    });
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- midi sustain --

/// Build a one-note pattern (start 0, `len_secs` long) at 120 BPM.
fn one_note_pattern(sr: u32, note: u8, len_secs: f64) -> slooper::midipat::Pattern {
    let mut pat = slooper::midipat::Pattern::new(120);
    let ticks = (len_secs * 2.0 * slooper::midipat::PPQ as f64) as u32;
    pat.notes.push(slooper::midipat::Note {
        ch: 0,
        note,
        vel: 110,
        press: 0,
        start: 0,
        len: ticks,
    });
    pat.len_ticks = ticks;
    pat.sort();
    let _ = sr;
    pat
}

/// RMS of a capture window.
fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// A single LONG note must keep sounding across the blocks where the
/// schedule carries NO events — the v0.7.1 regression that made every MIDI
/// track a short blip: `render_midi_block` skipped advancing the synth
/// whenever a block had no staged events, so only the attack block of each
/// note ever rendered. The body of the note lives INSIDE the instrument.
#[test]
fn e2e_midi_note_sustains_across_blocks() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("midi_sus", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc { sink, ring: ring.clone(), widx: widx.clone() },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Default piano; one whole-note pattern (2 s), NOT a run of quarter
    // notes — attacks alone must not be able to pass this test.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(slooper::audio::SynthHandle::Script(inst)))));
    let pat = one_note_pattern(sr, 57, 2.0);
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: (sr as u64) * 3,
    });
    b.cmd(Cmd::SetLoop(false));

    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(5),
        "note attack audible",
    );
    let head = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("head window");
    let head_rms = rms(&head);
    assert!(head_rms > 1e-3, "attack body must carry audio: {head_rms}");

    // ~1.1 s into the 2 s note: no event has been staged for ~100 blocks.
    // The note must still be sounding. (Before the v0.7.1 fix this window
    // was exact silence: the synth sat frozen between events.)
    std::thread::sleep(Duration::from_millis(950));
    let mid = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("mid-note window");
    let mid_rms = rms(&mid);
    assert!(mid_rms > 2e-3, "sustained note must keep sounding mid-body: {mid_rms}");
    assert!(
        mid_rms > 0.2 * head_rms,
        "note body collapsed to a blip: mid {mid_rms} vs head {head_rms}"
    );

    // 0.5 s after the note ended (damper on): back to quiet.
    std::thread::sleep(Duration::from_millis(1400));
    let tail = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("release window");
    let tail_rms = rms(&tail);
    assert!(
        tail_rms < 0.25 * mid_rms.max(1e-9),
        "released note must decay: tail {tail_rms} vs mid {mid_rms}"
    );

    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(500));
    wait_until(
        || ring_peak(&ring, &widx, 2400).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(5),
        "silence after StopAll (sustain state must reset on stop)",
    );
    let _ = active;
}

/// The VSTi path of the same contract: a plugin instrument must keep being
/// PROCESSED through event-free blocks (its output lives in the host staging
/// ring; pausing starves it), StopAll must flush the ring AND kill plugin
/// voices so nothing leaks into the next run. Skips without the plugin.
#[test]
fn e2e_midi_vsti_sustains_live() {
    let _g = e2e_lock();
    let path = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h).join(".vst3/vitalium.vst3"),
        Err(_) => return,
    };
    if !path.exists() {
        eprintln!("skipping: {} not installed", path.display());
        return;
    }
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("vsti_sus", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc { sink, ring: ring.clone(), widx: widx.clone() },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    let load = slooper::vsthost::open(&path, slooper::vsthost::VstKind::V3, sr, None)
        .expect("vitalium must load");
    b.cmd(Cmd::SetTrackSynth {
        i: 0,
        synth: Some(Arc::new(slooper::audio::SynthHandle::Vst(load.entry.clone()))),
    });
    let pat = one_note_pattern(sr, 60, 2.0);
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: (sr as u64) * 3,
    });
    b.cmd(Cmd::SetLoop(false));

    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(6),
        "VSTi note audible",
    );
    let head = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("head window");
    let head_rms = rms(&head);
    assert!(head_rms > 1e-3, "VSTi attack must carry audio: {head_rms}");

    std::thread::sleep(Duration::from_millis(950));
    let mid = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("mid-note window");
    let mid_rms = rms(&mid);
    assert!(mid_rms > 2e-3, "VSTi must keep producing through event-free blocks: {mid_rms}");
    assert!(
        mid_rms > 0.2 * head_rms,
        "VSTi note collapsed to a blip: mid {mid_rms} vs head {head_rms}"
    );

    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(600));
    wait_until(
        || ring_peak(&ring, &widx, 2400).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(5),
        "StopAll must silence the plugin (flush ring + all-notes-off)",
    );
    let _ = active;
}

/// The audition/preview chain end to end: `bounce_track_preview` must
/// render a SUSTAINED tone (not an attack blip) through the track's synth,
/// and the rendered buffer must actually reach the master output via
/// `Cmd::PreviewPlay` while the transport is stopped, then auto-stop.
#[test]
fn e2e_preview_renders_and_plays() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("preview_e2e", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc { sink, ring: ring.clone(), widx: widx.clone() },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Track 0 gets the bundled piano; audition one staged note (on @0,
    // off @1.5 s) through the exact UI preview path.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg.create_instance("piano", sr as f64, &[]).expect("piano");
    b.cmd(Cmd::SetTrackSynth {
        i: 0,
        synth: Some(Arc::new(slooper::audio::SynthHandle::Script(inst))),
    });
    std::thread::sleep(Duration::from_millis(200));

    let n = (sr as usize) * 3;
    let mut ev = vec![
        (0u64, 3u8, 0u8, 57u8, 110u8),
        ((sr as u64) * 3 / 2, 0, 0, 57, 0),
    ];
    let buf = b
        .engine
        .bounce_track_preview(0, &mut ev, n)
        .expect("preview bounce (track 0)");
    let win = (sr as usize) / 10;
    let head_rms = rms(&buf[(sr as usize) / 20..(sr as usize) / 20 + win]);
    let mid_rms = rms(&buf[(sr as usize)..(sr as usize) + win]);
    assert!(head_rms > 1e-3, "preview head must carry audio: {head_rms}");
    // "Not a blip": the note lasts 1.5 s, so 0.5–1.0 s must still carry it.
    // The recorded piano DECAYS (a real grand loses roughly half its level
    // in the first second), so the bar is a blip-proof floor, not the flat
    // sustain the old oscillator piano made easy: a 30 ms blip leaves
    // mid_rms ~1e-4 here.
    assert!(
        mid_rms > 0.05 * head_rms && mid_rms > 2e-3,
        "preview must sustain, not blip: mid {mid_rms} vs head {head_rms}"
    );

    // Play it: audible at the sink with the transport STOPPED, then silent
    // once the buffer ran out (auto-stop).
    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PreviewPlay { samples: Arc::from(buf) });
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(5),
        "preview audible at the master out",
    );
    std::thread::sleep(Duration::from_millis(3100));
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(5),
        "preview auto-stops at the end of its buffer",
    );

    // The user's real setting: audition THROUGH A TRACK whose instrument is
    // a VSTi (`preview_track` points at it from the piano roll). The
    // offline plugin render must also produce a sustained tone, not a blip.
    let vst_path = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h).join(".vst3/vitalium.vst3"),
        Err(_) => std::path::PathBuf::from("/nonexistent"),
    };
    if vst_path.exists() {
        let load = slooper::vsthost::open(&vst_path, slooper::vsthost::VstKind::V3, sr, None)
            .expect("vitalium must load");
        b.cmd(Cmd::SetTrackSynth {
            i: 0,
            synth: Some(Arc::new(slooper::audio::SynthHandle::Vst(load.entry.clone()))),
        });
        std::thread::sleep(Duration::from_millis(200));
        let mut ev = vec![
            (0u64, 3u8, 0u8, 60u8, 110u8),
            ((sr as u64) * 3 / 2, 0, 0, 60, 0),
        ];
        let buf = b
            .engine
            .bounce_track_preview(0, &mut ev, n)
            .expect("VSTi preview bounce");
        let head_rms = rms(&buf[(sr as usize) / 20..(sr as usize) / 20 + win]);
        let mid_rms = rms(&buf[(sr as usize)..(sr as usize) + win]);
        assert!(head_rms > 1e-3, "VSTi preview head: {head_rms}");
        assert!(
            mid_rms > 0.2 * head_rms && mid_rms > 2e-3,
            "VSTi preview must sustain through the whole window: mid {mid_rms} vs head {head_rms}"
        );
    }
    let _ = active;
}

/// Seeking away from a LONG held note must silence it: the seek handler
/// all-`off`s every synth AND drops the sustain bookkeeping, so a held
/// note cannot keep rendering after its timeline window is gone.
#[test]
fn e2e_midi_seek_silences_held_note() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("midi_seek", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc { sink, ring: ring.clone(), widx: widx.clone() },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg.create_instance("piano", sr as f64, &[]).expect("piano");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(slooper::audio::SynthHandle::Script(inst)))));
    let pat = one_note_pattern(sr, 57, 4.0);
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: (sr as u64) * 10,
    });
    b.cmd(Cmd::SetLoop(false));

    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(5),
        "note audible before the seek",
    );
    // Jump to 6 s — past the clip end, mid-note. Nothing may keep ringing.
    b.cmd(Cmd::Seek { pos: (sr as u64) * 6 });
    wait_until(
        || ring_peak(&ring, &widx, 2400).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(4),
        "seek must silence the held note (sustain state resets with the position)",
    );
    // And stay silent while the transport keeps running over empty bars.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        ring_peak(&ring, &widx, 4800).map(|p| p < 0.002).unwrap_or(false),
        "held note resurrected after the seek (sustain kept advancing)"
    );
    b.cmd(Cmd::StopAll);
    let _ = active;
}





