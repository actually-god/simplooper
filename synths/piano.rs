// ============================================================ slooper ==
// slooper-synth-version: 1
// piano.rs — the default slooper synthesizer.
//
// A warm two-operator FM piano (carrier : modulator 1:3) with per-pitch
// decay: short bright attack, long low-frequency body, voices that free up
// round-robin. This is what every track sounds like until you bind another
// synth — the "basic piano" of the MIDI default. Channel pressure (0xD,
// captured into MIDI clips) rides the FM modulator index for bite; CC7
// rides per-channel volume.
//
// Pure std, allocation-free process, 16 voices. No external crates.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 16;

struct Voice {
    on: bool,
    rel: bool,
    ch: u8,
    note: u8,
    vel: u8,
    ph_c: f64,
    ph_m: f64,
    f: f64,
    // envelope
    env: f64,
    decay: f64,
    rel_rate: f64,
}

struct Piano {
    sr: f64,
    voices: [Voice; VOICES],
    next: usize,
    press: [u8; 16],
    t: f64,
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let mut p = Box::new(Piano {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices: core::array::from_fn(|_| Voice {
            on: false, rel: false, ch: 0, note: 0, vel: 0,
            ph_c: 0.0, ph_m: 0.0, f: 0.0, env: 0.0, decay: 0.0, rel_rate: 0.0,
        }),
        next: 0,
        press: [0; 16],
        t: 0.0,
    });
    // Precompute happens per note; nothing else to size.
    p.t = 0.0;
    Box::into_raw(p) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Piano) });
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    // Pick a free voice, else the oldest (round-robin cursor).
    let mut idx = None;
    for k in 0..VOICES {
        if !p.voices[k].on {
            idx = Some(k);
            break;
        }
    }
    let idx = idx.unwrap_or_else(|| {
        let k = p.next;
        p.next = (p.next + 1) % VOICES;
        k
    });
    let v = &mut p.voices[idx];
    let f = 440.0f64 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    let sr = p.sr;
    v.on = true;
    v.rel = false;
    v.ch = ch & 15;
    v.note = note;
    v.vel = vel.max(1);
    v.ph_c = 0.0;
    v.ph_m = 0.0;
    v.f = f;
    v.env = 0.0;
    // Higher notes decay faster (like a real string set).
    let secs = (6.5 - (f / 12000.0).min(5.4)).clamp(0.45, 6.5);
    v.decay = (secs * sr).recip();
    v.rel_rate = (0.06 * sr).recip();
}

extern "C" fn note_off(s: *mut core::ffi::c_void, ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    for v in &mut p.voices {
        if v.on && !v.rel && v.ch == (ch & 15) && v.note == note {
            v.rel = true;
        }
    }
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    for v in &mut p.voices {
        v.on = false;
        v.env = 0.0;
    }
}

extern "C" fn cc(_s: *mut core::ffi::c_void, _ch: u8, _num: u8, _val: u8) {
    // Volume comes through the track's mixer instead; kept for the ABI.
}

extern "C" fn press(s: *mut core::ffi::c_void, ch: u8, val: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    p.press[(ch & 15) as usize] = val;
}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    let bright = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64; // tone
    let level = unsafe { *params.add(1) }.clamp(0.0, 1.0) as f64; // level
    let sr = p.sr;
    for k in 0..n {
        let mut acc = 0.0f64;
        for v in &mut p.voices {
            if !v.on {
                continue;
            }
            let bite = 1.0 + p.press[v.ch as usize] as f64 / 127.0;
            let vel = v.vel as f64 / 127.0;
            // 2-op FM, ratio 1:3, index rides velocity + pressure + tone.
            let mod_ = {
                let d = (2.0 + 9.0 * vel * bite * (0.35 + 0.65 * bright))
                    * v.f
                    / (0.001 + v.f / 200.0).max(0.2);
                d
            };
            v.ph_m += std::f64::consts::TAU * v.f * 3.0 / sr;
            if v.ph_m > std::f64::consts::TAU {
                v.ph_m -= std::f64::consts::TAU;
            }
            v.ph_c += std::f64::consts::TAU * v.f / sr;
            if v.ph_c > std::f64::consts::TAU {
                v.ph_c -= std::f64::consts::TAU;
            }
            let fm = (v.ph_m.sin() * mod_).sin();
            // Fast attack then exponential body.
            if v.rel {
                v.env -= v.rel_rate * (1.0 + v.env);
                if v.env <= 0.0 {
                    v.on = false;
                    v.env = 0.0;
                    continue;
                }
            } else {
                let at = 0.0008 * sr;
                v.env += (1.0 - v.env / vel.max(0.001)).min(1.0) / at;
                if v.env > vel {
                    v.env = vel;
                }
                v.env *= 1.0 - v.decay;
            }
            acc += fm * v.env;
        }
        let g = level as f64 * 0.22;
        let y = acc.clamp(-1.5, 1.5) * g;
        unsafe {
            *out.add(k) += y as f32;
        }
    }
}

static PARAMS: [SynthParamV1; 2] = [
    SynthParamV1 { label: c"tone".as_ptr(), min: 0.0, max: 1.0, default: 0.55 },
    SynthParamV1 { label: c"level".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"piano".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"FM grand — the default synth every track falls back to".as_ptr(),
    n_params: 2,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    note_on,
    note_off,
    all_off,
    cc,
    press,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 {
    &DESC
}
