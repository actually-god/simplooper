//! midi_tester — send a repeating burst of MIDI messages into a JACK MIDI port.
//!
//! Handy for exercising simplooper's MIDI monitor (or any JACK client) without
//! hardware:
//!
//! ```sh
//! cargo run --example midi_tester                 # target simplooper:midi_in
//! cargo run --example midi_tester -- myapp:midi  # custom target port
//! ```
//!
//! One message is sent per process cycle (a few per second at typical quantum).
//! Pass `--once` to send each message exactly once and exit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jack::{Client, ClientOptions, Control, MidiOut, ProcessHandler, ProcessScope, RawMidi};

const MESSAGES: &[(&str, &[u8])] = &[
    ("NOTE ON  ch1 C4 vel 100", &[0x90, 60, 100]),
    ("CC       ch2 vol 128   ", &[0xB1, 7, 128]),
    ("PC       ch4 prog 11   ", &[0xC3, 11]),
    ("PITCH    ch1 +4096     ", &[0xE0, 0x60, 0x00]),
    ("NOTE OFF ch1 C4 vel 0  ", &[0x80, 60, 0]),
];

struct Src {
    out: jack::Port<MidiOut>,
    sent: Arc<AtomicU64>,
}

impl ProcessHandler for Src {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let i = self.sent.load(Ordering::Relaxed) as usize;
        if i < MESSAGES.len() * 1000 {
            let (_, bytes) = &MESSAGES[i % MESSAGES.len()];
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
        .unwrap_or_else(|| "simplooper:midi_in".into());
    let once = args.iter().any(|a| a == "--once");

    let (client, _status) = match Client::new("midi_tester", ClientOptions::NO_START_SERVER) {
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
            "could not connect {src_name}:midi_out -> {target}: {:?}",
            last_err
        );
        std::process::exit(1);
    }

    let sent = Arc::new(AtomicU64::new(0));
    let active = client.activate_async((), Src { out, sent: sent.clone() }).unwrap();

    eprintln!(
        "midi_tester: {} -> {} ({} messages per cycle)",
        format!("{src_name}:midi_out"),
        target,
        MESSAGES.len()
    );

    // Printer thread: report each message as it goes out (keeps RT clean).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let printer_sent = sent.clone();
    let printer_stop = stop.clone();
    std::thread::spawn(move || {
        let mut last: u64 = 0;
        while !printer_stop.load(Ordering::Relaxed) {
            let cur = printer_sent.load(Ordering::Relaxed);
            for i in last..cur {
                let (name, _) = &MESSAGES[i as usize % MESSAGES.len()];
                eprintln!("[{ts}] sent: {name}", ts = stamp());
            }
            last = cur;
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    if once {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && sent.load(Ordering::Relaxed) < MESSAGES.len() as u64 {
            std::thread::sleep(Duration::from_millis(50));
        }
    } else {
        // Run until killed (Ctrl-C).
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = active;
}

fn stamp() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        (t.as_secs() % 86400) / 3600,
        (t.as_secs() % 3600) / 60,
        t.as_secs() % 60,
        t.as_millis() % 1000
    )
}
