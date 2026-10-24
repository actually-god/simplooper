// VST2 custom-editor smoke: opens a real plugin's editor window on the
// X11 display and asserts it PAINTS (the user-reported failure was a blank
// window that never drew and could not be closed). Exits non-zero when the
// editor stays blank; always closes the window before exiting.
//
//   cargo run --release --example editor_smoke [path-to-vst2.so]
use std::path::Path;

use slooper::vsthost::{open, VstKind};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/admin/.vst/amsynth_vst.so".to_string());
    let load = open(Path::new(&path), VstKind::V2, 48_000, None)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    let entry = load.entry;
    assert!(entry.has_editor(), "{} exposes no editor", entry.name());
    let handle = entry.vst2_handle().expect("vst2 handle");
    let mut win = slooper::vstedit::open_editor(&handle, &path, entry.name())
        .unwrap_or_else(|e| panic!("open_editor: {e}"));
    let mut outcome = String::from("painted");
    for _ in 0..80 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if win.painted() {
            break;
        }
        if win.open_failed() {
            outcome = "open refused (plugin could not create its editor)".into();
            break;
        }
        if win.is_closed() {
            outcome = "editor exited".into();
            break;
        }
    }
    let painted = win.painted();
    let (w, h) = win.editor_size();
    println!(
        "{}: painted = {painted}, children = {}, size = {w}x{h}, outcome: {outcome}",
        entry.name(),
        win.child_count()
    );
    // If it painted, prove the WINDOW-MANAGER close button works: inject
    // the same WM_DELETE_WINDOW client message a title-bar X sends.
    if painted {
        let xid = win.xid();
        if xid != 0 && slooper::vstedit::request_wm_delete(xid) {
            let mut closed = false;
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if win.is_closed() {
                    closed = true;
                    break;
                }
            }
            println!("WM_DELETE_WINDOW injected: closed = {closed}");
            if !closed {
                win.close();
                std::process::exit(3);
            }
        }
    }
    win.close();
    drop(entry);
    println!("editor closed cleanly (effEditClose done, X window destroyed)");
    if !painted {
        std::process::exit(2);
    }
}
