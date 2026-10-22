// ============================================================ slooper ==
// slooper-synth-version: 3
// piano.rs — the default slooper synthesizer.
//
// A REAL-TIME PHYSICAL-MODELING PIANO, translated from the open-source
// modal string model of `fan455_piano_synthesis` (MIT © 2024 fan455,
// https://github.com/fan455/fan455_piano_synthesis, working paper
// arXiv:2409.03481). Their engine is a full finite-element hammer→string→
// soundboard solver; the parts that actually make a piano NOTE are a clean
// modal synthesis (that repo's `test_02 hammer-string interaction.py` and
// `test_09 damping.py`), reproduced here in the allocation-free form a
// real-time voice needs:
//
//   * String = sum of modal partials. Each partial n is a decaying sinusoid
//     `amp_n · e^(−damp_n·t) · sin(φ_n)`. (Their `exp(-damp*t) * sin(...)`.)
//   * MODAL DAMPING grows with mode number — `damp_n = damp0 · n` (their
//     `damp = damp0 * i_freq`). Higher partials die first, so a struck
//     string starts bright and settles warm — the decay no fixed-τ voice
//     can fake. damp0 itself shrinks with pitch so bass rings, treble dries.
//   * 1/n spectral amplitude (their `ampli = 1 / i_freq`) with a velocity
//     tilt: a HARD hit throws energy into the upper partials, a soft hit
//     stays rounded — the hammer-string contact filter, in one line.
//   * ALTERNATING initial phase (odd partials start at π — their
//     `phase[0::2] += np.pi`): a hammer striking near the string's 1/8
//     point drives alternate modes with opposite sign.
//   * Damper dynamics: note-off drops the damper (fast ~70 ms tail), CC64
//     holds it off so released strings keep ringing over the rest.
//
// Two documented additions for the real keyboard, both standard stiff-
// string physics (the reference's modal stage keeps an ideal string):
//   * INHARMONICITY f_n = n·f0·√(1 + B·n²) with B larger in the bass
//     (thick wound strings) — the slight stretch that makes octaves sing.
//   * Per-register decay times (τ ≈ 13 s A0 → 1.6 s C8), their
//     test_09 damping-vs-frequency idea taken to the string fundamental.
//
// Pure std, allocation-free process, 14 voices. No external crates. The
// reference is MIT-licensed; its copyright + permission notice ships in
// THIRD-PARTY-NOTICES.md next to this file.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 14;
const NP: usize = 16; // partials per voice (stiff-string modes)
const TAU: f64 = 6.283_185_307_179_586;

struct Voice {
    on: bool,
    rel: bool,
    /// note_off arrived while the damper pedal held the string — damp it
    /// the moment the pedal comes up.
    held: bool,
    ch: u8,
    note: u8,
    // Per-partial oscillator state. `ph` integrates each mode's phase,
    // `dph` its per-sample increment; `amp` is the CURRENT partial amplitude
    // (multiplied by its own decay factor every sample), `dk` the per-sample
    // decay multiplier = e^(−damp_n/sr).
    ph: [f64; NP],
    dph: [f64; NP],
    amp: [f64; NP],
    dk: [f64; NP],
    // Attack ramp (0→1 over ~0.5 ms) kills the onset click.
    atk: f64,
    // Release (damper) multiplier: 1.0 = ringing, <1 = damper on the string.
    rel_k: f64,
}

impl Voice {
    const fn silent() -> Voice {
        Voice {
            on: false,
            rel: false,
            held: false,
            ch: 0,
            note: 0,
            ph: [0.0; NP],
            dph: [0.0; NP],
            amp: [0.0; NP],
            dk: [0.0; NP],
            atk: 0.0,
            rel_k: 1.0,
        }
    }
}

struct Piano {
    sr: f64,
    voices: [Voice; VOICES],
    next: usize,
    press: [u8; 16],
    // Damper-pedal (CC64) held per channel: released notes keep ringing.
    sustain: [bool; 16],
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let p = Box::new(Piano {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices: core::array::from_fn(|_| Voice::silent()),
        next: 0,
        press: [0; 16],
        sustain: [false; 16],
    });
    Box::into_raw(p) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Piano) });
    }
}

/// Stiff-string inharmonicity coefficient by MIDI note. Thick, short bass
/// strings are the most inharmonic; the treble is close to ideal. Inside
/// the published range for real pianos (B ~ 3e-4 bass, ~2e-5 treble).
fn inharmonicity_b(note: u8) -> f64 {
    let n = note.clamp(21, 108) as f64;
    let t = (n - 21.0) / (108.0 - 21.0); // 0 = bass, 1 = treble
    3.0e-4 * (1.0 - t) + 1.8e-5 * t
}

/// Fundamental modal decay rate (1/s, one-sided 1/e time). Bass rings long,
/// treble dries fast: τ_fund ≈ 13 s (A0) .. 1.6 s (C8). Partial n decays at
/// `damp0 · n` per second — the reference's `damp = damp0 * i_freq`.
fn fundamental_damp(note: u8) -> f64 {
    let n = note.clamp(21, 108) as f64;
    let t = (n - 21.0) / (108.0 - 21.0);
    1.0 / (13.0 * (1.0 - t) + 1.6 * t)
}

extern "C" fn note_on(s: *mut core::ffi::c_void, ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    // Free voice, else steal the round-robin cursor's oldest slot.
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
    let sr = p.sr;
    let vel = vel.clamp(1, 127);
    let f0 = 440.0f64 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    let b = inharmonicity_b(note);
    let damp0 = fundamental_damp(note);

    let nv = vel as f64 / 127.0;
    // Hammer hardness: harder hits brighten (shorter contact). Squared so
    // soft playing stays mellow and fortissimo bites.
    let hardness = nv * nv;
    let nyq = sr * 0.5;

    v.on = true;
    v.rel = false;
    v.held = false;
    v.ch = ch & 15;
    v.note = note;
    v.atk = 0.0;
    v.rel_k = 1.0;
    let mut n_sum = 0.0f64; // running sum of |amp| to normalize the peak
    for i in 0..NP {
        let n = (i + 1) as f64;
        // Stiff-string dispersion: partial n sits a hair above n·f0.
        let f_n = n * f0 * (1.0 + b * n * n).sqrt();
        if f_n >= nyq * 0.96 {
            v.ph[i] = 0.0;
            v.dph[i] = 0.0;
            v.amp[i] = 0.0;
            v.dk[i] = 1.0;
            continue;
        }
        // Alternating initial phase (their phase[0::2] += π).
        v.ph[i] = if i % 2 == 0 { 0.0 } else { std::f64::consts::PI };
        v.dph[i] = TAU * f_n / sr;
        // 1/n^(1.15) roll-off (struck string) × hardness tilt upward.
        let base = 1.0 / n.powf(1.15);
        let tilt = 1.0 + hardness * (n.sqrt() * 0.9);
        let a = base * tilt;
        v.amp[i] = a;
        n_sum += a;
        // damp_n = damp0 · n — higher partials decay proportionally faster.
        v.dk[i] = (-damp0 * n / sr).exp();
    }
    // Normalize the summed partials near unity, then velocity: a real piano
    // is both LOUDER and BRIGHTER when struck harder (tilt already added).
    let gain = if n_sum > 0.0 { 0.62 / n_sum } else { 0.0 } * (0.18 + 0.82 * nv);
    for a in v.amp.iter_mut() {
        *a *= gain;
    }
}

extern "C" fn note_off(s: *mut core::ffi::c_void, ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Piano) };
    let ch = ch & 15;
    for v in &mut p.voices {
        if v.on && !v.rel && v.ch == ch && v.note == note {
            if p.sustain[ch as usize] {
                // Damper pedal down: mark held, keep ringing until pedal-up.
                v.held = true;
                continue;
            }
            v.rel = true;
            // Damper touches the string: ~70 ms body tail on top of the
            // partials' own modal decay.
            v.rel_k = (-1.0 / (0.07 * p.sr)).exp();
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
        v.rel = false;
        v.atk = 0.0;
        for a in v.amp.iter_mut() {
            *a = 0.0;
        }
    }
}

extern "C" fn cc(s: *mut core::ffi::c_void, ch: u8, num: u8, val: u8) {
    if s.is_null() {
        return;
    }
    if num == 64 {
        // CC64 damper pedal: held (>63) = released notes keep ringing; on
        // pedal-up every string that was note-offed while the pedal was
        // down gets damped retroactively.
        let p = unsafe { &mut *(s as *mut Piano) };
        let on = val > 63;
        let ch = (ch & 15) as usize;
        p.sustain[ch] = on;
        if !on {
            for v in &mut p.voices {
                if v.on && v.ch as usize == ch && v.held {
                    v.rel = true;
                    v.held = false;
                    v.rel_k = (-1.0 / (0.07 * p.sr)).exp();
                }
            }
        }
    }
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
    let tone = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64;
    let level = unsafe { *params.add(1) }.clamp(0.0, 1.0) as f64;
    let sr = p.sr;
    // `tone` balances body vs top AFTER the per-hit hammer tilt: lo = felt,
    // hi = bright. Split around the 6th partial, gentle ±.
    let body = 0.8 + 0.5 * tone;
    let top = 0.7 + 0.6 * tone;
    // Attack ramp per sample (~0.5 ms linear) kills the onset click.
    let atk_step = 1.0 / (0.0005 * sr).max(1.0);
    let g = level * 0.9;

    for k in 0..n {
        let mut acc = 0.0f64;
        for v in &mut p.voices {
            if !v.on {
                continue;
            }
            if v.rel {
                // Damper on the string: shrink the whole voice per sample.
                for a in v.amp.iter_mut() {
                    *a *= v.rel_k;
                }
            }
            // Channel pressure adds a touch of bite to the upper modes.
            let bite = 1.0 + p.press[v.ch as usize] as f64 / 300.0;
            let mut vsig = 0.0f64;
            for i in 0..NP {
                let a = v.amp[i];
                if a == 0.0 {
                    continue;
                }
                let tilt = if i < 6 { body } else { top * bite };
                vsig += a * tilt * v.ph[i].sin();
                // Advance phase; single subtraction suffices (dph < TAU).
                let mut ph = v.ph[i] + v.dph[i];
                if ph >= TAU {
                    ph -= TAU;
                }
                v.ph[i] = ph;
                // Per-partial modal decay.
                v.amp[i] = if a * v.dk[i] > 1.0e-7 { a * v.dk[i] } else { 0.0 };
            }
            if v.atk < 1.0 {
                v.atk += atk_step;
                if v.atk > 1.0 {
                    v.atk = 1.0;
                }
            }
            acc += vsig * v.atk;

            // Recycle a fully silent released voice.
            if v.rel {
                let mut peak = 0.0f64;
                for &a in v.amp.iter() {
                    if a > peak {
                        peak = a;
                    }
                }
                if peak <= 0.0 {
                    v.on = false;
                    v.rel = false;
                }
            }
        }
        let y = acc.clamp(-4.0, 4.0) * g;
        unsafe {
            *out.add(k) += y as f32;
        }
    }
}

static PARAMS: [SynthParamV1; 2] = [
    SynthParamV1 { label: c"tone".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    SynthParamV1 { label: c"level".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"piano".as_ptr(),
    author: c"slooper (modal model after fan455, MIT)".as_ptr(),
    description:
        c"Modal piano — inharmonic decaying string partials + hammer. The default synth".as_ptr(),
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
