// Temp probe: does a VST2 effect pass audio? Does a VST2 instrument make sound?
use slooper::vsthost::{open, VstKind};

fn render(path: &str, instrument: bool) -> f32 {
    let load = open(path.as_ref(), VstKind::V2, 48_000, None)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    println!("{path}: name={} params={}", load.entry.name(), load.entry.descs().len());
    if instrument {
        load.entry.note_on(0, 60, 110);
    }
    let mut peak = 0.0f32;
    let probe = |peak: &mut f32, load: &slooper::vsthost::VstLoad| {
        for i in 0..200 {
            let inp: Vec<f32> = (0..512)
                .map(|k| (((i * 512 + k) as f32) * 0.05).sin() * 0.5)
                .collect();
            let mut out = vec![0.0f32; 512];
            load.entry.process(&inp, &mut out);
            *peak = peak.max(out.iter().fold(0.0f32, |m, x| m.max(x.abs())));
        }
    };
    // First pass without any program selection:
    probe(&mut peak, &load);
    println!("  pass1 (no setProgram): peak = {peak}");
    // effSetProgram(0) = VST2 opcode 50 — the classic effect wake-up call.
    if let Some(h) = load.entry.vst2_handle() {
        let r = h.dispatch(50, 0, 0, std::ptr::null_mut(), 0.0);
        println!("  effSetProgram(0) -> {r}");
        peak = 0.0;
        probe(&mut peak, &load);
    }
    for i in 0..200 {
        let inp: Vec<f32> = (0..512)
            .map(|k| (((i * 512 + k) as f32) * 0.05).sin() * 0.5)
            .collect();
        let mut out = vec![0.0f32; 512];
        load.entry.process(&inp, &mut out);
        peak = peak.max(out.iter().fold(0.0f32, |m, x| m.max(x.abs())));
    }
    peak
}

fn main() {
    let home = std::env::var("HOME").unwrap();
    println!("=== EFFECTS (in→out passthrough expected) ===");
    for p in [
        format!("{home}/.vst/wolf-shaper-vst.so"),
        format!("{home}/.vst/DragonflyHallReverb-vst.so"),
    ] {
        let peak = render(&p, false);
        println!("  EFFECT {p}: peak = {peak} {}", if peak > 0.01 { "OK" } else { "SILENT!!" });
    }
    println!("=== INSTRUMENTS ===");
    let p = format!("{home}/.vst/amsynth_vst.so");
    let peak = render(&p, true);
    println!("  INSTR {p}: peak = {peak} {}", if peak > 0.001 { "OK" } else { "SILENT!!" });
}
