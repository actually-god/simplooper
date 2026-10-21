// ============================================================ slooper ==
// slooper-synth-version: 1
// sampler.rs — pitch-following audio-clip synthesizer.
//
// Plays ANY audio asset from the rack as an instrument: pick the asset id,
// set the note whose pitch the sample already has (its "root"), and every
// MIDI note replays the clip transposed — the classic sampler trick. Pitch
// is exact resampling (linear interpolation): a note plays the whole clip
// at that pitch.
//
// Params:
//   asset     — rack asset id to sample (the rack lists ids next to names).
//   root note — MIDI note the sample sounds at UNshifted (default C3 = 48).
//   gain      — output trim.
//
// The buffer is copied out of the host at a block boundary when the id
// changes (the host pointer is valid only briefly), then played with a
// fractional read-head per voice. 12-voice poly.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 12;

struct Voice {
    on: bool,
    pos: f64,
    rate: f64,
    vel: f64,
}

struct Sampler {
    host: *const SynthHostV1,
    asset: i32, // id currently copied into buf (-1 = none)
    buf: std::vec::Vec<f32>,
    buf_rate: f64,
    sr: f64,
    // Root note from the last processed param frame — `note_on` runs
    // between blocks, so this is exact for anything the user can set.
    root: f64,
    voices: [Voice; VOICES],
    next: usize,
}

extern "C" fn create(sr: f64, host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(Sampler {
        host,
        asset: -1,
        buf: Vec::new(),
        buf_rate: 0.0,
        sr: if sr > 1.0 { sr } else { 48000.0 },
        root: 48.0,
        voices: core::array::from_fn(|_| Voice { on: false, pos: 0.0, rate: 1.0, vel: 0.0 }),
        next: 0,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Sampler) });
    }
}

/// Copy the wanted asset out of the host (only when the id changed).
fn sync_asset(s: &mut Sampler, want: i32) {
    if want == s.asset {
        return;
    }
    s.asset = want;
    s.buf.clear();
    s.buf_rate = 0.0;
    if s.host.is_null() || want < 0 {
        return;
    }
    let host = unsafe { &*s.host };
    if host.abi != SLOOPER_SYNTH_ABI {
        return;
    }
    let mut info = SynthAssetV1::default();
    if (host.load_asset)(want as u32, &mut info) == 1 && info.len > 0 && !info.data.is_null() {
        let n = (info.len as usize).min(12 * 1024 * 1024);
        unsafe {
            s.buf.extend_from_slice(std::slice::from_raw_parts(info.data, n));
        }
        s.buf_rate = if info.sample_rate > 1 { info.sample_rate as f64 } else { s.sr };
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let sm = unsafe { &mut *(s as *mut Sampler) };
    if sm.buf.is_empty() {
        return;
    }
    let mut idx = None;
    for k in 0..VOICES {
        if !sm.voices[k].on {
            idx = Some(k);
            break;
        }
    }
    let idx = idx.unwrap_or_else(|| {
        let k = sm.next;
        sm.next = (sm.next + 1) % VOICES;
        k
    });
    // Pitch = semitone distance from the asset's own root note.
    let semis = note as f64 - sm.root;
    let rate = (2.0f64).powf(semis / 12.0) * (sm.buf_rate / sm.sr).clamp(0.25, 4.0);
    let v = &mut sm.voices[idx];
    v.on = true;
    v.pos = 0.0;
    v.rate = rate.clamp(0.0625, 16.0);
    v.vel = vel as f64 / 127.0;
}

extern "C" fn note_off(_s: *mut core::ffi::c_void, _ch: u8, _note: u8) {
    // One-shots ring out; note_off has nothing to release.
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let sm = unsafe { &mut *(s as *mut Sampler) };
    for v in &mut sm.voices {
        v.on = false;
    }
}

extern "C" fn cc(_s: *mut core::ffi::c_void, _ch: u8, _num: u8, _val: u8) {}
extern "C" fn press(_s: *mut core::ffi::c_void, _ch: u8, _val: u8) {}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let sm = unsafe { &mut *(s as *mut Sampler) };
    let asset = unsafe { *params.add(0) }.round() as i32;
    sm.root = unsafe { *params.add(1) }.round() as f64;
    let gain = unsafe { *params.add(2) }.clamp(0.0, 1.0) as f64;
    sync_asset(sm, asset);
    if sm.buf.is_empty() {
        return;
    }
    let last = sm.buf.len() as f64 - 1.0;
    for k in 0..n {
        let mut acc = 0.0f64;
        for v in &mut sm.voices {
            if !v.on {
                continue;
            }
            let i = v.pos as usize;
            if i + 1 >= sm.buf.len() {
                v.on = false;
                continue;
            }
            let frac = v.pos - i as f64;
            let a = sm.buf[i] as f64;
            let b = sm.buf[i + 1] as f64;
            acc += (a + (b - a) * frac) * v.vel;
            v.pos += v.rate;
            if v.pos > last {
                v.on = false;
            }
        }
        unsafe {
            *out.add(k) += (acc * gain) as f32;
        }
    }
}

static PARAMS: [SynthParamV1; 3] = [
    SynthParamV1 { label: c"asset".as_ptr(), min: 0.0, max: 63.0, default: 0.0 },
    SynthParamV1 { label: c"root note".as_ptr(), min: 12.0, max: 108.0, default: 48.0 },
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"sampler".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Play any rack asset pitched across the keyboard".as_ptr(),
    n_params: 3,
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
