//! sinco_emu — emulate the "SINCO MIDI 1" controller for testing slooper's
//! MIDI binding capture without touching the hardware.
//!
//! The real device is a four-channel switch/footswitch: each channel (MIDI
//! channels 0..3) sends Program Change 1 when its switch is down and
//! Program Change 2 when it is up (verified with `aseqdump` on port 32).
//! This client reproduces exactly that traffic: it cycles through all eight
//! messages — PC 1/2 on channels 0, 1, 2, 3 — one per process cycle.
//!
//! ```sh
//! cargo run --example sinco_emu                # target slooper:midi_in
//! cargo run --example sinco_emu -- myapp:midi  # custom target port
//! cargo run --example sinco_emu -- --once      # send each message once, exit
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jack::{Client, ClientOptions, Control, MidiOut, ProcessHandler, ProcessScope, RawMidi};

/// The SINCO MIDI 1 pattern: PC 1 and PC 2 on each of channels 0..3.
const MESSAGES: &[&[u8]] = &[
    &[0xC0, 1], // PC ch 0 prog 1 (switch 1 down)
    &[0xC0, 2], // PC ch 0 prog 2 (switch 1 up)
    &[0xC1, 1], // PC ch 1 prog 1 (switch 2 down)
    &[0xC1, 2], // PC ch 1 prog 2 (switch 2 up)
    &[0xC2, 1], // PC ch 2 prog 1 (switch 3 down)
    &[0xC2, 2], // PC ch 2 prog 2 (switch 3 up)
    &[0xC3, 1], // PC ch 3 prog 1 (switch 4 down)
    &[0xC3, 2], // PC ch 3 prog 2 (switch 4 up)
];

struct Src {
    out: jack::Port<jack::MidiOut>,
    sent: Arc<AtomicU64>,
}

impl ProcessHandler for Src {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let i = self.sent.load(Ordering::Relaxed) as usize;
        if i < MESSAGES.len() * 1000 {
            let bytes = MESSAGES[i % MESSAGES.len()];
            let _ = self.out.writer(ps).write(&RawMidi { time: 0, bytes });
            self.sent.fetch_add(1, Ordering::Relaxed);
        }
        Control::Continue
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "slooper:midi_in".into());
    let once = args.iter().any(|a| a == "--once");

    let (client, _status) = match Client::new("sinco_emu", ClientOptions::NO_START_SERVER) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("no JACK/PipeWire server: {e:?}");
            std::process::exit(1);
        }
    };
    let out = client
        .register_port("midi_out", MidiOut::default())
        .expect("register midi_out");
    let src_name = client.name().to_string();
    // A freshly-activated client's ports can't be connected immediately on
    // PipeWire-JACK, so retry briefly.
    let mut connected = false;
    let mut last_err: Option<jack::Error> = None;
    for _ in 0..10 {
        match client.connect_ports_by_name(&format!("{src_name}:midi_out"), &target) {
            Ok(()) => {
                connected = true;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    if !connected {
        eprintln!(
            "could not connect {src_name}:midi_out -> {target}: {last_err:?}"
        );
        std::process::exit(1);
    }

    let sent = Arc::new(AtomicU64::new(0));
    let active = client.activate_async((), Src { out, sent: sent.clone() }).unwrap();
    eprintln!(
        "sinco_emu: {src_name}:midi_out -> {} (PC 1/2 on ch 0-3, {} messages per cycle)",
        target,
        MESSAGES.len()
    );

    if once {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && sent.load(Ordering::Relaxed) < MESSAGES.len() as u64 {
            std::thread::sleep(Duration::from_millis(50));
        }
    } else {
        // Run until killed.
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    let _ = active;
}
