//! simplooper — a minimalist live audio looper for Linux (JACK / PipeWire).
//!
//! Sixteen simultaneous loops with record/overdub/playback/clear, waveform
//! dashboard, MIDI action mapping plus keyboard shortcuts, easy quantize,
//! independent/dependent loop modes, live mix passthrough and a MIDI monitor.

/// Bundled application icon (placeholder for now — see assets/icon.png).
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn main() -> eframe::Result {
    // Start the JACK/PipeWire-JACK backend thread (NO_START: never auto-starts a server).
    let backend = simplooper::audio::Backend::start();

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
        "simplooper",
        options,
        Box::new(move |_cc| Ok(Box::new(simplooper::ui::App::new(backend)))),
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
