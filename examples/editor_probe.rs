// Debug probe: open a plugin editor and HOLD it open, printing state every
// 500 ms, so an external WM_DELETE injection can be observed (does the
// editor thread notice and exit like a real close click?).
//
//   cargo run --release --example editor_probe [path] [hold_secs]
use std::path::Path;

use slooper::vsthost::{open, VstKind};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/admin/.vst/amsynth_vst.so".to_string());
    let hold: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let load = open(Path::new(&path), VstKind::V2, 48_000, None)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    let entry = load.entry;
    let handle = entry.vst2_handle().expect("vst2 handle");
    let mut win = slooper::vstedit::open_editor(&handle, &path, entry.name())
        .unwrap_or_else(|e| panic!("open_editor: {e}"));
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < hold {
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!(
            "{:>5.1}s painted={} children={} closed={} failed={} size={:?}",
            t0.elapsed().as_secs_f32(),
            win.painted(),
            win.child_count(),
            win.is_closed(),
            win.open_failed(),
            win.editor_size()
        );
        if win.is_closed() {
            println!("EDITOR CLOSED (thread exited)");
            break;
        }
    }
    win.close();
    println!("probe done");
}
