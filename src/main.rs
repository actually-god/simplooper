//! slooper — a minimalist clip-based audio DAW for Linux (JACK / PipeWire).
//!
//! Sixteen tracks on a global, draggable playhead with a global loop region
//! (area / current longest / whole song). Asset-rack drag & drop onto a
//! metronome-defined bar grid, a right-docked Mixer aligned per track,
//! clip copy/paste, additive child lanes, takes that record into rack assets
//! and auto-placed clips, metronome-anchored quantize/trim, a bundled TR-808
//! sample pack, `.slooper` projects, per-track effects, MIDI action
//! mapping plus keyboard shortcuts, live mix passthrough and a MIDI monitor.

/// Bundled application icon (placeholder for now — see assets/icon.png).
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn main() -> eframe::Result {
    // Plugin editors decide the app's windowing world: the VST2 spec has
    // exactly one Linux UI ABI — native X11 embedding — so every hosted
    // plugin opens plain X11 windows. Under a Wayland session winit would
    // take the main window to Wayland while all those plugin windows land
    // on Xwayland, and no host can manage across that seam (the v10.2
    // "plugin GUIs don't even show up as a window" report on KDE). When
    // an X display exists, run the whole app through it (Xwayland under
    // Wayland): one server, one world, editors embed and stack normally.
    // Opt back into Wayland (losing VST2 GUIs) with SLOOPER_FORCE_WAYLAND=1.
    if std::env::var_os("SLOOPER_FORCE_WAYLAND").is_none() && std::env::var_os("DISPLAY").is_some()
    {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("WAYLAND_SOCKET");
    }
    // Start the JACK/PipeWire-JACK backend thread (NO_START: never auto-starts a server).
    let backend = slooper::audio::Backend::start();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1080.0, 640.0])
        .with_min_inner_size([780.0, 480.0]);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "slooper",
        options,
        Box::new(move |_cc| Ok(Box::new(slooper::ui::App::new(backend)))),
    )
}

/// Decode the bundled icon into raw RGBA for the window title bar.
fn load_icon() -> Option<std::sync::Arc<egui::IconData>> {
    let img = image::ImageReader::new(std::io::Cursor::new(ICON_PNG))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .into_rgba8();
    Some(std::sync::Arc::new(egui::IconData {
        rgba: img.as_raw().to_vec(),
        width: img.width(),
        height: img.height(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_decodes() {
        let icon = load_icon().expect("bundled icon must decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
