// Raw VST2 effect probe: find the init sequence that makes effects audible.
use std::sync::{Arc, Mutex};
use vst2::host::{HostBuffer, PluginInstance, PluginLoader};
use vst2::plugin::Plugin;

struct H;
impl vst2::host::Host for H {
    fn get_info(&self) -> (isize, String, String) {
        (1, "probe".into(), "0".into())
    }
}

type I = PluginInstance;

fn disp(inst: &mut I, op: i32, index: i32, val: isize, opt: f32) -> isize {
    let eff = inst.raw_effect();
    unsafe { ((*eff).dispatcher)(eff, op, index, val, std::ptr::null_mut(), opt) }
}

fn run(inst: &mut I, hb: &mut HostBuffer<f32>) -> f32 {
    let mut peak = 0.0f32;
    for n in 0..120u32 {
        let l: Vec<f32> = (0..512).map(|i| (((n * 512 + i) as f32) * 0.05).sin() * 0.5).collect();
        let r = l.clone();
        let mut ol = [0.0f32; 512];
        let mut or_ = [0.0f32; 512];
        {
            let mut ab = hb.bind(&[&l, &r], &mut [&mut ol, &mut or_]);
            inst.process(&mut ab);
        }
        let blk = ol.into_iter().fold(0.0f32, |m, x| m.max(x.abs()));
        peak = peak.max(blk);
    }
    peak
}

fn load(path: &str) -> I {
    let mut loader =
        PluginLoader::load(std::path::Path::new(path), Arc::new(Mutex::new(H))).expect("load");
    loader.instance().expect("instance")
}

fn main() {
    let path = std::env::args().nth(1).expect("path");

    // A: slooper's current sequence: setSampleRate, setBlockSize, resume(=12,1). No effOpen.
    let mut a = load(&path);
    disp(&mut a, 10, 0, 0, 48000.0);
    disp(&mut a, 11, 0, 512, 0.0);
    disp(&mut a, 12, 0, 1, 0.0);
    let mut hb = HostBuffer::new(2usize, 2usize);
    let pa = run(&mut a, &mut hb);
    println!("A(sr,bs,mains):           peak = {pa}");

    // B: proper VST2 host order: effOpen, sr, bs, setProgram(0), mains(1).
    let mut b = load(&path);
    disp(&mut b, 0, 0, 0, 0.0); // effOpen
    disp(&mut b, 10, 0, 0, 48000.0);
    disp(&mut b, 11, 0, 512, 0.0);
    disp(&mut b, 3, 0, 0, 0.0); // effSetProgram? (SDK: effSetProgram=3? try)
    disp(&mut b, 2, 0, 0, 0.0); // effSetProgram alt (SDK: =2?)
    disp(&mut b, 12, 0, 1, 0.0);
    let pb = run(&mut b, &mut hb);
    println!("B(open,sr,bs,prog,mains): peak = {pb}");

    // C: like B but setSampleRate/BlockSize via the Plugin trait (typed).
    let mut c = load(&path);
    disp(&mut c, 0, 0, 0, 0.0);
    c.set_sample_rate(48000.0);
    c.set_block_size(512);
    c.resume();
    let pc = run(&mut c, &mut hb);
    println!("C(trait init + effOpen):  peak = {pc}");

    println!(
        "VERDICT: effOpen={}  (A={pa:.4} B={pb:.4} C={pc:.4})",
        if pb.max(pc) > 0.01 && pa <= 0.01 { "YES" } else { "no/unclear" }
    );
}
