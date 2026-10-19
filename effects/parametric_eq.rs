// ============================================================ simplooper ==
// parametric_eq.rs — a full parametric equalizer with a custom child GUI.
//
// Six fully parametric bands (like ReaEQ / most DAW channel EQs), RBJ
// cookbook biquads: off, low shelf, high shelf, peaking (bell), lowpass,
// highpass, notch, bandpass — per-band type / frequency / gain / Q, plus
// dry/wet mix and output trim. Params are plain effect params (26 sliders),
// so persistence and the audio path need nothing special; what makes this
// one shine is the OPTIONAL child GUI (`simplooper_effect_gui` below):
//
//   * the live |H(f)| response curve of the whole band cascade,
//   * a 240-band live spectrum of the incoming audio behind the curve
//     (vertical bars growing UP from the bottom: silence = no bar,
//     0 dBFS = full height, on a 0 … −90 dB scale),
//   * draggable band handles, per-band controls, hover frequency readout.
//
// The GUI lives in this same script and talks to audio through two things
// only: the shared param array (api.get_param/set_param) and atomics in the
// instance state (spectrum levels, an "editor open" flag that turns the
// analyser on only while someone is watching).
// ---------------------------------------------------------------------------

include!("effect_api.rs");

// ---------------------------------------------------------------------------
// Params — 6 bands × (type, freq, gain, Q) + mix + out = 26.
// ---------------------------------------------------------------------------
const NBANDS: usize = 6;
const P_MIX: usize = NBANDS * 4; // chain-wide dry/wet after the EQ
const P_OUT: usize = NBANDS * 4 + 1;
const NPARAMS: u32 = 26;

// Band types (param value, rounded to an integer). Also the match arms in
// Biquad::set_params use these numbers.
#[allow(dead_code)] // documentation of the numeric layout; matches use literals
mod band_types {
    pub const T_OFF: f32 = 0.0;
    pub const T_LOSHELF: f32 = 1.0;
    pub const T_HISHELF: f32 = 2.0;
    pub const T_PEAK: f32 = 3.0;
    pub const T_LOWPASS: f32 = 4.0;
    pub const T_HIGHPASS: f32 = 5.0;
    pub const T_NOTCH: f32 = 6.0;
    pub const T_BANDPASS: f32 = 7.0;
}
use band_types::*;
const TYPE_NAMES: [&str; 8] = ["off", "low shelf", "hi shelf", "peak", "low pass", "hi pass", "notch", "band pass"];
/// Short names for the strip buttons.
const TYPE_SHORT: [&str; 8] = ["OFF", "LOSH", "HISH", "PEAK", "LOP", "HIP", "NOTC", "BAND"];

/// Defaults per band: type, freq, gain, Q (musical ReaEQ-ish starting point).
const BAND_DEFS: [[f32; 4]; NBANDS] = [
    [T_LOSHELF, 80.0, 0.0, 0.71],
    [T_PEAK, 250.0, 0.0, 1.1],
    [T_PEAK, 800.0, 0.0, 1.1],
    [T_PEAK, 2500.0, 0.0, 1.1],
    [T_PEAK, 6000.0, 0.0, 1.1],
    [T_HISHELF, 11000.0, 0.0, 0.71],
];

/// The spectrum analyser: 240 log-spaced constant-Q band passes with peak
/// followers, published as atomic linear levels the GUI samples. Runs on
/// the audio thread ONLY while the editor window is open.
const NANALY: usize = 240;

// ---------------------------------------------------------------------------
// Biquad (RBJ Audio-EQ-Cookbook), Direct Form II transposed, f64 state.
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64, b1: f64, b2: f64, a1: f64, a2: f64,
    // target coefficients (parameter changes glide to them — no clicks)
    tb0: f64, tb1: f64, tb2: f64, ta1: f64, ta2: f64,
    z1: f64, z2: f64,
    /// last parameter snapshot the coefficients were built from
    cache: [f32; 4],
    cache_valid: bool,
    gliding: bool,
}

impl Biquad {
    fn new() -> Self {
        let b = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, tb0: 1.0, tb1: 0.0, tb2: 0.0, ta1: 0.0, ta2: 0.0, z1: 0.0, z2: 0.0, cache: [0.0; 4], cache_valid: false, gliding: false };
        b
    }

    /// Rebuild target coefficients from (type, freq, gain dB, Q) at sr.
    fn set_params(&mut self, typ: f32, freq: f32, gain: f32, q: f32, sr: f64) {
        let typ = typ as i32;
        let sr = if sr > 1.0 { sr } else { 48000.0 };
        let hi = (sr * 0.45).max(20.0);
        let (freq, gain, q) = ((freq as f64).clamp(10.0, hi), gain as f64, (q as f64).clamp(0.05, 40.0));
        let w0 = core::f64::consts::TAU * freq / sr;
        let (sw, cw) = w0.sin_cos();
        let alpha = sw / (2.0 * q);
        let a = (10.0_f64).powf(gain / 40.0); // peaking/shelf amplitude
        let sab = a.sqrt();
        // Shelf slope: the plain-Q alpha (same alpha as every other type).
        // The cookbook's shelf-SLOPE form alpha = sin·√((A+1/A)(1/S−1)+2)
        // goes imaginary for S > ~2 at high gain — clamped to 0 it parked
        // the shelf poles EXACTLY on the unit circle (perpetual ringing).
        // sin/(2Q) is stable for every Q in the slider range.
        let alpha_s = alpha;
        let (b0, b1, b2, a0, a1, a2) = match typ {
            1 => (
                a * ((a + 1.0) - (a - 1.0) * cw + 2.0 * sab * alpha_s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                a * ((a + 1.0) - (a - 1.0) * cw - 2.0 * sab * alpha_s),
                (a + 1.0) + (a - 1.0) * cw + 2.0 * sab * alpha_s,
                -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                (a + 1.0) + (a - 1.0) * cw - 2.0 * sab * alpha_s,
            ),
            2 => (
                a * ((a + 1.0) + (a - 1.0) * cw + 2.0 * sab * alpha_s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                a * ((a + 1.0) + (a - 1.0) * cw - 2.0 * sab * alpha_s),
                (a + 1.0) - (a - 1.0) * cw + 2.0 * sab * alpha_s,
                2.0 * ((a - 1.0) - (a + 1.0) * cw),
                (a + 1.0) - (a - 1.0) * cw - 2.0 * sab * alpha_s,
            ),
            3 => (
                // RBJ peaking: b = (1 + αA, −2cos, 1 − αA),
                //          a = (1 + α/A, −2cos, 1 − α/A) — the SAME
                // (1 ± α/A) shape as the other types' denominators.
                // Swapping the signs to (1 − α/A … 1 + α/A) puts the poles
                // at radius √((1+α/A)/(1−α/A)) > 1: the band self-oscillates
                // to ±inf for ANY nonzero alpha — even at 0 dB gain — which
                // is what made the whole EQ peg the output at inf.
                1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a,
                1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a,
            ),
            4 => ((1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            5 => ((1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            6 => (1.0, -2.0 * cw, 1.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            7 => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            _ => (1.0, 0.0, 0.0, 1.0, 0.0, 0.0), // off = pass-through
        };
        let (inv, na1, na2) = (1.0 / a0, a1 / a0, a2 / a0);
        let (nb0, nb1, nb2) = (b0 * inv, b1 * inv, b2 * inv);
        if !(nb0.is_finite() && nb1.is_finite() && nb2.is_finite() && na1.is_finite() && na2.is_finite()) {
            self.tb0 = 1.0; self.tb1 = 0.0; self.tb2 = 0.0; self.ta1 = 0.0; self.ta2 = 0.0;
        } else {
            self.tb0 = nb0; self.tb1 = nb1; self.tb2 = nb2; self.ta1 = na1; self.ta2 = na2;
        }
        self.gliding = true;
    }

    #[inline]
    fn step(&mut self, x: f64) -> f64 {
        if self.gliding {
            const K: f64 = 0.006; // ~330 µs time constant at 48 kHz
            self.b0 += (self.tb0 - self.b0) * K;
            self.b1 += (self.tb1 - self.b1) * K;
            self.b2 += (self.tb2 - self.b2) * K;
            self.a1 += (self.ta1 - self.a1) * K;
            self.a2 += (self.ta2 - self.a2) * K;
            let d = (self.b0 - self.tb0).abs() + (self.a1 - self.ta1).abs() + (self.a2 - self.ta2).abs();
            if d < 1e-6 {
                self.gliding = false;
                self.b0 = self.tb0; self.b1 = self.tb1; self.b2 = self.tb2;
                self.a1 = self.ta1; self.a2 = self.ta2;
            }
        }
        // Direct Form II transposed.
        let out = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * out + self.z2;
        self.z2 = self.b2 * x - self.a2 * out;
        if !out.is_finite() {
            self.z1 = 0.0; self.z2 = 0.0;
            return 0.0;
        }
        out
    }

    /// |H(e^jw)| at angular frequency `w` (for the response display).
    fn mag(&self, w: f64) -> f64 {
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        let d = (dr * dr + di * di).max(1e-12);
        ((nr * nr + ni * ni) / d).sqrt()
    }
}

// ---------------------------------------------------------------------------
// Spectrum analyser bands (fixed coefficients — they never move).
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
struct AnBand {
    bq: Biquad,
    env: f64,
}

// ---------------------------------------------------------------------------
// State — one per enabled (loop, slot) instance.
// ---------------------------------------------------------------------------
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

struct ParametricEq {
    sr: f64,
    bands: [Biquad; NBANDS],
    an: [AnBand; NANALY],
    an_release: f64,
    mix: f64,
    out: f64,
    /// 1 while an editor window is drawing (it raises it, destroy lowers it).
    gui_open: AtomicU32,
    /// Per-band linear peak levels (f32 bits) for the spectrum display.
    levels: [AtomicU32; NANALY],
}

impl ParametricEq {
    fn new(sample_rate: f64) -> Self {
        let sr = if sample_rate.is_finite() && sample_rate > 1.0 { sample_rate } else { 48000.0 };
        let an_release = (-(1.0 / (sr * 0.08))).exp(); // ~80 ms peak decay
        // Analyser: constant-Q band passes log-spaced 20 Hz … 20 kHz
        // (the full human range, matching the graph's F_LO..F_HI axis).
        let mut an: [AnBand; NANALY] = core::array::from_fn(|_| AnBand { bq: Biquad::new(), env: 0.0 });
        for (i, a) in an.iter_mut().enumerate() {
            let f = 20.0f32 * (20000.0f32 / 20.0f32).powf(i as f32 / (NANALY - 1) as f32);
            a.bq.cache_valid = false;
            a.bq.set_params(T_BANDPASS, f, 0.0, 2.2, sr);
            a.bq.b0 = a.bq.tb0; a.bq.b1 = a.bq.tb1; a.bq.b2 = a.bq.tb2;
            a.bq.a1 = a.bq.ta1; a.bq.a2 = a.bq.ta2;
            a.bq.gliding = false;
        }
        let mut s = Self {
            sr,
            bands: core::array::from_fn(|_| Biquad::new()),
            an,
            an_release,
            mix: 1.0,
            out: 1.0,
            gui_open: AtomicU32::new(0),
            levels: core::array::from_fn(|_| AtomicU32::new(0)),
        };
        for b in s.bands.iter_mut() {
            // force coefficient build on the first sample
            b.cache_valid = false;
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Audio entry points.
// ---------------------------------------------------------------------------
extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let s = Box::new(ParametricEq::new(sample_rate));
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut ParametricEq) });
    }
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let st = unsafe { &mut *(state as *mut ParametricEq) };
    let sr = if st.sr > 1.0 { st.sr } else { 48000.0 };
    // sample_rate==0 at create means "backend not up yet"; the engine
    // rebuilds instances with the real rate, but keep coefficients sane.
    let mix = (unsafe { *params.add(P_MIX) }).clamp(0.0, 1.0) as f64;
    let out = (unsafe { *params.add(P_OUT) }).clamp(0.0, 2.0) as f64;
    // Slew the two static trims a hair (they are per-block constant anyway).
    st.mix += (mix - st.mix) * 0.01;
    st.out += (out - st.out) * 0.01;

    // Spectrum (only while someone is watching).
    let want_spec = st.gui_open.load(Relaxed) != 0;
    let mut s = input as f64;

    // EQ cascade; each band rebuilds its coefficients only when its four
    // params moved.
    for (bi, b) in st.bands.iter_mut().enumerate() {
        let base = bi * 4;
        let p = [
            unsafe { *params.add(base) },
            unsafe { *params.add(base + 1) },
            unsafe { *params.add(base + 2) },
            unsafe { *params.add(base + 3) },
        ];
        if !b.cache_valid || b.cache != p {
            b.cache = p;
            b.cache_valid = true;
            b.set_params(p[0], p[1], p[2], p[3], sr);
        }
        s = b.step(s);
    }

    if want_spec {
        let x = input as f64;
        let rel = st.an_release;
        for (a, lev) in st.an.iter_mut().zip(st.levels.iter()) {
            let y = a.bq.step(x);
            let m = if y < 0.0 { -y } else { y };
            a.env = if m > a.env { m } else { a.env * rel };
            if !a.env.is_finite() {
                a.env = 0.0;
            }
            lev.store((a.env as f32).to_bits(), Relaxed);
        }
    }

    let dry = input as f64;
    let wet = s * st.out;
    let o = dry * (1.0 - st.mix) + wet * st.mix;
    if !o.is_finite() {
        return 0.0;
    }
    o as f32
}

// ---------------------------------------------------------------------------
// Custom child GUI (see effect_api.rs, "the child GUI contract").
// ---------------------------------------------------------------------------
/// Per-window editor state (GUI thread only — allocation is fine here).
struct Gs {
    /// Band selected by the strip below the graph.
    sel: usize,
    show_spec: bool,
    /// Band whose handle is being dragged (None = not dragging).
    drag: Option<usize>,
    /// Audio instance state, remembered from the first draw so `destroy`
    /// can drop the "editor open" flag.
    inst: Option<*mut core::ffi::c_void>,
}

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs { sel: 0, show_spec: true, drag: None, inst: None })) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if gs.is_null() {
        return;
    }
    let gs = unsafe { Box::from_raw(gs as *mut Gs) };
    if let Some(p) = gs.inst {
        unsafe { (*(p as *mut ParametricEq)).gui_open.store(0, Relaxed) };
    }
}

// ---- graph mapping helpers -------------------------------------------------
const F_LO: f32 = 20.0f32;
const F_HI: f32 = 20000.0f32;
const DB_TOP: f32 = 18.0f32;

fn f2x(f: f32, w: f32) -> f32 {
    ((f.max(F_LO) / F_LO).ln() / (F_HI / F_LO).ln()) * w
}
fn x2f(x: f32, w: f32) -> f32 {
    F_LO * (F_HI / F_LO).powf((x / w).clamp(0.0, 1.0))
}
fn db2y(db: f32, h: f32) -> f32 {
    (0.5 - db / (2.0 * DB_TOP)).clamp(0.0, 1.0) * h
}
fn y2db(y: f32, h: f32) -> f32 {
    (0.5 - y / h) * 2.0 * DB_TOP
}
fn uses_gain(typ: i32) -> bool {
    matches!(typ, 1 | 2 | 3)
}

/// Handle position for band `bi` — THE one geometry used for both drawing
/// and hit-testing. It used to differ (draw at the cascade |H(fc)|, grab at
/// the band's own gain), so after moving one band the other five handles
/// were painted far away from anywhere they could be grabbed: dragging
/// B1 visibly shoved every handle around ("random frequencies affected")
/// and B2..B6 looked dead.
fn handle_pt(bi: usize, p: &[f32; 32], gx: f32, gy: f32, gw: f32, gph: f32) -> (f32, f32) {
    let base = bi * 4;
    let hx = gx + f2x(p[base + 1], gw);
    let hy = if uses_gain(p[base] as i32) {
        gy + db2y(p[base + 2], gph)
    } else {
        gy + gph * 0.5
    };
    (hx, hy)
}

fn fmt_hz(f: f32) -> String {
    if f >= 10000.0 {
        format!("{:.1}k", f / 1000.0)
    } else if f >= 1000.0 {
        format!("{:.2}k", f / 1000.0)
    } else {
        format!("{:.0}", f)
    }
}

/// Curve sample count for the response trace.
const CURVE_PTS: usize = 180;

extern "C" fn gui_draw(
    gs: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
    frame: *const GuiFrameV1,
    api: *const GuiApiV1,
) {
    if gs.is_null() || frame.is_null() || api.is_null() {
        return;
    }
    let gs = unsafe { &mut *(gs as *mut Gs) };
    let fr = unsafe { &*frame };
    let api = unsafe { &*api };
    let c = api.ctx;

    // Remember the instance (for gui_open bookkeeping) and keep the flag up.
    if !state.is_null() {
        if gs.inst.is_none() {
            gs.inst = Some(state);
        }
        unsafe { (*(state as *mut ParametricEq)).gui_open.store(1, Relaxed) };
    }

    // ---- layout ----------------------------------------------------------
    let w = fr.width;
    let gph = (fr.height - 150.0).max(120.0); // graph height
    let gx = 34.0f32; // graph left (dB labels)
    let gy = 8.0f32;
    let gw = (w - gx - 10.0).max(80.0);

    // Param snapshot.
    let np = api.n_params.max(NPARAMS as u32).min(32);
    let gp = |k: u32| unsafe { (api.get_param)(c, k) };
    let sp = |k: u32, v: f32| unsafe { (api.set_param)(c, k, v) };
    let mut p = [0.0f32; 32];
    for k in 0..np as usize {
        p[k] = gp(k as u32);
    }

    // ---- graph backdrop + grid -------------------------------------------
    unsafe {
        (api.rect_filled)(c, gx, gy, gw, gph, 0.09, 0.10, 0.13, 1.0);
        // horizontal dB lines every 6 dB
        let mut db = -12.0f32;
        while db <= DB_TOP {
            let y = gy + db2y(db, gph);
            let zero = db.abs() < 0.5;
            let col = if zero { (0.5, 0.5, 0.55) } else { (0.22, 0.24, 0.28) };
            (api.line)(c, gx, y, gx + gw, y, 1.0, col.0, col.1, col.2, 1.0);
            if !zero {
                let s = std::ffi::CString::new(format!("{:+.0}", db)).unwrap();
                (api.text)(c, gx - 4.0, y, 9.0, 2, 0.55, 0.55, 0.6, 1.0, s.as_ptr());
            }
            db += 6.0;
        }
        // Vertical frequency grid: minor ×2..×9 per decade, majors with
        // labels at 20, 50, 100, 200, 500, 1k, 2k, 5k, 10k, 20k — the full
        // human range on both edges of the axis.
        const MAJORS: [f32; 10] = [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0];
        for f in MAJORS {
            let xm = (gx + f2x(f, gw)).min(gx + gw);
            (api.line)(c, xm, gy, xm, gy + gph, 1.0, 0.27, 0.29, 0.34, 1.0);
            let lab = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{}", f as i32) };
            let s = std::ffi::CString::new(lab).unwrap();
            // Keep the edge labels inside the graph.
            let tx = (xm - 6.0).max(gx + 1.0).min(gx + gw - 18.0);
            (api.text)(c, tx, gy + gph + 9.0, 9.0, 1, 0.55, 0.55, 0.6, 1.0, s.as_ptr());
        }
        let mut decade = 10.0f32;
        while decade <= 100000.0 {
            let mut k = 2.0f32;
            while k <= 9.0 {
                let f = decade * k;
                if f > F_LO && f < F_HI {
                    let x = gx + f2x(f, gw);
                    (api.line)(c, x, gy, x, gy + gph, 1.0, 0.18, 0.20, 0.24, 1.0);
                }
                k += 1.0;
            }
            decade *= 10.0;
        }
    }

    // ---- live spectrum (vertical bars, bottom-up, loud = tall) -----------
    // One bar per analyser band spanning its slice of the x axis. Bars grow
    // UP from the bottom on a 0 dBFS .. −90 dB scale: silence = NO bar,
    // 0 dBFS = full height (top of the graph). The old mapping inverted this
    // (silence painted a full-height bar and signal "subtracted" from it).
    if gs.show_spec && !state.is_null() {
        let st = unsafe { &*(state as *mut ParametricEq) };
        let freq = |i: usize| F_LO * (F_HI / F_LO).powf(i as f32 / (NANALY - 1) as f32);
        for i in 0..NANALY {
            let lin = f32::from_bits(st.levels[i].load(Relaxed)) as f64;
            let db = 20.0 * lin.max(1e-7).log10(); // −∞..0 dBFS-ish
            // −90 dB -> 0 (no bar), 0 dB -> 1 (full height).
            let up = ((db + 90.0) / 90.0).clamp(0.0, 1.0) as f32;
            if up <= 0.004 {
                continue;
            }
            // Bar spans from the midpoint to the previous band to the
            // midpoint to the next band (edge bands clamp to the graph).
            let f_lo = if i == 0 { F_LO } else { (freq(i - 1) * freq(i)).sqrt() };
            let f_hi = if i + 1 == NANALY { F_HI } else { (freq(i) * freq(i + 1)).sqrt() };
            let x0 = gx + f2x(f_lo, gw);
            let x1 = gx + f2x(f_hi, gw);
            let h = up * gph;
            unsafe {
                (api.rect_filled)(c, x0 + 0.5, gy + gph - h, (x1 - x0 - 1.0).max(1.0), h, 0.30, 0.85, 0.55, 0.55);
            }
        }
        // Right-hand scale ticks for the analyser's own dB axis (0 dBFS at
        // the TOP — full-height bars reach it — down to −90 at the bottom).
        for (dbf, lab) in [(0.0f32, "0"), (-45.0, "-45"), (-90.0, "-90")] {
            let y = gy + ((-dbf) / 90.0) * gph;
            let s = std::ffi::CString::new(lab.to_string()).unwrap();
            unsafe { (api.text)(c, gx + gw - 16.0, (y - 4.0).max(gy + 1.0), 8.0, 1, 0.30, 0.85, 0.55, 0.8, s.as_ptr()) };
        }
    }

    // ---- response curve (cascade of the six biquads at current params) ----
    {
        // Build display filters: same maths as audio, evaluated on a grid.
        let sr = if fr.sample_rate > 1.0 { fr.sample_rate } else { 48000.0 };
        let mut disp: Vec<Biquad> = Vec::with_capacity(NBANDS);
        for bi in 0..NBANDS {
            let mut b = Biquad::new();
            let base = bi * 4;
            b.set_params(p[base], p[base + 1], p[base + 2], p[base + 3], sr);
            b.b0 = b.tb0; b.b1 = b.tb1; b.b2 = b.tb2; b.a1 = b.ta1; b.a2 = b.ta2;
            disp.push(b);
        }
        let mix = p[P_MIX].clamp(0.0, 1.0) as f64;
        let outg = p[P_OUT].clamp(0.0, 2.0) as f64;
        let mut pts: Vec<f32> = Vec::with_capacity(CURVE_PTS * 2);
        for i in 0..CURVE_PTS {
            let f = F_LO * (F_HI / F_LO).powf(i as f32 / (CURVE_PTS - 1) as f32);
            let wf = core::f64::consts::TAU * f as f64;
            let mut mag = 1.0f64;
            for b in disp.iter() {
                mag *= b.mag(wf);
            }
            // Magnitude approximation of |dry·(1−mix) + wet·mix| and trim.
            let mag = (1.0 - mix) + mix * mag;
            let db = 20.0 * mag.max(1e-6).log10() + 20.0 * outg.max(1e-6).log10();
            let x = gx + (i as f32 / (CURVE_PTS - 1) as f32) * gw;
            let y = gy + db2y(db as f32, gph);
            pts.push(x);
            pts.push(y);
        }
        unsafe { (api.polyline)(c, pts.as_ptr(), pts.len() as u32 / 2, 0, 2.0, 0.42, 0.72, 1.0, 1.0) };

        // ---- band handles + interaction ---------------------------------
        // Drawn at the SAME geometry the grab test below uses (handle_pt).
        for bi in 0..NBANDS {
            let base = bi * 4;
            if p[base] as i32 == 0 {
                continue;
            }
            let (hx, hy) = handle_pt(bi, &p, gx, gy, gw, gph);
            let sel = gs.sel == bi;
            let (r, g, bcol) = if sel { (1.0, 0.75, 0.30) } else { (0.45, 0.62, 0.85) };
            unsafe {
                (api.rect_filled)(c, hx - 4.0, hy - 4.0, 8.0, 8.0, r, g, bcol, 1.0);
                if sel {
                    (api.rect_stroke)(c, hx - 6.0, hy - 6.0, 12.0, 12.0, 1.0, 1.0, 0.85, 0.45, 0.9);
                }
            }
        }
    }

    // ---- hover readout / drag ---------------------------------------------
    let inside = fr.mouse_inside != 0;
    let mx = fr.mouse_x;
    let my = fr.mouse_y;
    if inside && mx >= gx && mx <= gx + gw && my >= gy && my <= gy + gph {
        let hf = x2f(mx - gx, gw);
        unsafe {
            (api.line)(c, mx, gy, mx, gy + gph, 1.0, 0.4, 0.4, 0.45, 0.7);
            let s = std::ffi::CString::new(fmt_hz(hf)).unwrap();
            (api.text)(c, mx, gy + 7.0, 10.0, 1, 0.9, 0.9, 0.95, 1.0, s.as_ptr());
        }
        // Press near a handle grabs it (same geometry that drew it).
        if fr.mouse_pressed != 0 {
            let mut best = None;
            let mut bestd = 16.0f32;
            for bi in 0..NBANDS {
                if p[bi * 4] as i32 == 0 {
                    continue;
                }
                let (hx, hy) = handle_pt(bi, &p, gx, gy, gw, gph);
                let d = ((hx - mx).abs()).max((hy - my).abs());
                if d < bestd {
                    bestd = d;
                    best = Some(bi);
                }
            }
            if let Some(bi) = best {
                gs.drag = Some(bi);
                gs.sel = bi;
            }
        }
    }
    if let Some(bi) = gs.drag {
        if fr.mouse_down != 0 {
            let base = (bi * 4) as u32;
            let nf = x2f((mx - gx).clamp(0.0, gw), gw);
            sp(base + 1, nf);
            let typ = p[bi * 4] as i32;
            if matches!(typ, 1 | 2 | 3) {
                let g = y2db(my - gy, gph).clamp(-24.0, 24.0);
                sp(base + 2, (g * 4.0).round() / 4.0);
            } else if typ >= 4 {
                let q = (16.0f32).powf((1.0 - (my - gy) / gph).clamp(0.0, 1.0) * 2.0 - 0.5) * 0.5;
                sp(base + 3, q.clamp(0.1, 16.0));
            }
        } else {
            gs.drag = None;
        }
    }

    // ---- control strip -----------------------------------------------------
    let sy = gy + gph + 22.0;
    unsafe {
        // Band buttons.
        let bw = 46.0f32;
        let x0 = 8.0f32;
        for bi in 0..NBANDS {
            let typ = gp((bi * 4) as u32) as i32;
            let on = typ != 0;
            let lab = format!("{} B{}{}", if gs.sel == bi { "▸" } else { " " }, bi + 1, if on { "" } else { "·" });
            let s = std::ffi::CString::new(lab).unwrap();
            if (api.button)(c, x0 + bi as f32 * bw, sy, bw - 4.0, s.as_ptr()) != 0 {
                gs.sel = bi;
            }
        }
        // Spectrum toggle + reset.
        let bx = x0 + NBANDS as f32 * bw;
        let mut spec = if gs.show_spec { 1.0f32 } else { 0.0f32 };
        let s = std::ffi::CString::new("spectrum").unwrap();
        if (api.checkbox)(c, bx, sy + 4.0, s.as_ptr(), &mut spec) != 0 {
            gs.show_spec = spec != 0.0;
        }
        let s = std::ffi::CString::new("reset").unwrap();
        if (api.button)(c, bx + 86.0, sy, 56.0, s.as_ptr()) != 0 {
            for bi in 0..NBANDS {
                let d = BAND_DEFS[bi];
                sp((bi * 4) as u32, d[0]);
                sp((bi * 4 + 1) as u32, d[1]);
                sp((bi * 4 + 2) as u32, d[2]);
                sp((bi * 4 + 3) as u32, d[3]);
            }
            sp(P_MIX as u32, 1.0);
            sp(P_OUT as u32, 1.0);
        }
        // Edge-filter quick toggles: one click drops a lowpass on B1 /
        // highpass on B6 (the usual LP/HP positions); clicking again puts
        // the band's default shelf back.
        let b6 = (NBANDS - 1) as u32 * 4;
        let lp_on = gp(0) as i32 == 4;
        let s = std::ffi::CString::new(format!("B1 lp {}", if lp_on { "on" } else { "off" })).unwrap();
        if (api.button)(c, bx + 152.0, sy, 78.0, s.as_ptr()) != 0 {
            sp(0, if lp_on { BAND_DEFS[0][0] } else { 4.0 });
        }
        let hp_on = gp(b6) as i32 == 5;
        let s = std::ffi::CString::new(format!("B6 hp {}", if hp_on { "on" } else { "off" })).unwrap();
        if (api.button)(c, bx + 234.0, sy, 78.0, s.as_ptr()) != 0 {
            sp(b6, if hp_on { BAND_DEFS[NBANDS - 1][0] } else { 5.0 });
        }

        // Selected-band controls.
        let by = sy + 30.0;
        let sel = gs.sel.min(NBANDS - 1);
        let base = sel * 4;
        let typ = gp(base as u32) as i32;
        let uses_gain = uses_gain(typ);
        let uses_q = true;
        let s = std::ffi::CString::new(format!("B{}: {}", sel + 1, TYPE_NAMES[typ.clamp(0, 7) as usize])).unwrap();
        (api.text)(c, 12.0, by + 10.0, 12.0, 0, 0.85, 0.85, 0.9, 1.0, s.as_ptr());

        let mut cx = 130.0f32;
        // Type button: cycles on click.
        let s = std::ffi::CString::new(format!("type ▸ {}", TYPE_SHORT[typ.clamp(0, 7) as usize])).unwrap();
        if (api.button)(c, cx, by, 96.0, s.as_ptr()) != 0 {
            sp(base as u32, ((typ + 1) % 8) as f32);
        }
        cx += 106.0;
        let mut on = if typ == 0 { 0.0f32 } else { 1.0f32 };
        let s = std::ffi::CString::new("on").unwrap();
        if (api.checkbox)(c, cx, by + 4.0, s.as_ptr(), &mut on) != 0 {
            sp(base as u32, if on != 0.0 { 3.0 } else { 0.0 }); // "on" defaults to peaking
        }
        cx += 60.0;
        // Knobs: freq / gain / Q + global mix / out.
        let mut v = p[base + 1];
        let s = std::ffi::CString::new(format!("freq {}", fmt_hz(v))).unwrap();
        if (api.knob)(c, cx, by + 16.0, 46.0, s.as_ptr(), &mut v, 20.0, 20000.0, 0.0) != 0 {
            sp(base as u32 + 1, v);
        }
        cx += 62.0;
        if uses_gain {
            let mut v = p[base + 2];
            let s = std::ffi::CString::new(format!("gain {:+.1}", v)).unwrap();
            if (api.knob)(c, cx, by + 16.0, 46.0, s.as_ptr(), &mut v, -24.0, 24.0, 1.0) != 0 {
                sp(base as u32 + 2, v);
            }
            cx += 62.0;
        }
        if uses_q {
            let mut v = p[base + 3];
            let s = std::ffi::CString::new(format!("Q {:.2}", v)).unwrap();
            if (api.knob)(c, cx, by + 16.0, 46.0, s.as_ptr(), &mut v, 0.1, 16.0, 0.0) != 0 {
                sp(base as u32 + 3, v);
            }
            cx += 62.0;
        }
        let _ = cx;
    }
    // Global knobs right-aligned on the strip.
    unsafe {
        let mut v = p[P_MIX];
        let s = std::ffi::CString::new(format!("mix {:.0}%", v * 100.0)).unwrap();
        if (api.knob)(c, w - 150.0, gy + gph + 52.0, 46.0, s.as_ptr(), &mut v, 0.0, 1.0, 1.0) != 0 {
            sp(P_MIX as u32, v);
        }
        let mut v = p[P_OUT];
        let s = std::ffi::CString::new(format!("out {:+.1}dB", 20.0f32 * (v.max(1e-4)).log10())).unwrap();
        if (api.knob)(c, w - 88.0, gy + gph + 52.0, 46.0, s.as_ptr(), &mut v, 0.0, 2.0, 1.0) != 0 {
            sp(P_OUT as u32, v);
        }
    }
}

/// The 26 auto-window sliders (also the persistence layout): per band
/// type/freq/gain/Q, then mix and output trim.
static PARAMS: [EffectParamV1; 26] = [
    EffectParamV1 { label: c"B1 type".as_ptr(), min: 0.0, max: 7.0, default: 1.0 },
    EffectParamV1 { label: c"B1 freq".as_ptr(), min: 20.0, max: 20000.0, default: 80.0 },
    EffectParamV1 { label: c"B1 gain".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"B1 Q".as_ptr(), min: 0.1, max: 16.0, default: 0.71 },
    EffectParamV1 { label: c"B2 type".as_ptr(), min: 0.0, max: 7.0, default: 3.0 },
    EffectParamV1 { label: c"B2 freq".as_ptr(), min: 20.0, max: 20000.0, default: 250.0 },
    EffectParamV1 { label: c"B2 gain".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"B2 Q".as_ptr(), min: 0.1, max: 16.0, default: 1.1 },
    EffectParamV1 { label: c"B3 type".as_ptr(), min: 0.0, max: 7.0, default: 3.0 },
    EffectParamV1 { label: c"B3 freq".as_ptr(), min: 20.0, max: 20000.0, default: 800.0 },
    EffectParamV1 { label: c"B3 gain".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"B3 Q".as_ptr(), min: 0.1, max: 16.0, default: 1.1 },
    EffectParamV1 { label: c"B4 type".as_ptr(), min: 0.0, max: 7.0, default: 3.0 },
    EffectParamV1 { label: c"B4 freq".as_ptr(), min: 20.0, max: 20000.0, default: 2500.0 },
    EffectParamV1 { label: c"B4 gain".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"B4 Q".as_ptr(), min: 0.1, max: 16.0, default: 1.1 },
    EffectParamV1 { label: c"B5 type".as_ptr(), min: 0.0, max: 7.0, default: 3.0 },
    EffectParamV1 { label: c"B5 freq".as_ptr(), min: 20.0, max: 20000.0, default: 6000.0 },
    EffectParamV1 { label: c"B5 gain".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"B5 Q".as_ptr(), min: 0.1, max: 16.0, default: 1.1 },
    EffectParamV1 { label: c"B6 type".as_ptr(), min: 0.0, max: 7.0, default: 2.0 },
    EffectParamV1 { label: c"B6 freq".as_ptr(), min: 20.0, max: 20000.0, default: 11000.0 },
    EffectParamV1 { label: c"B6 gain".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"B6 Q".as_ptr(), min: 0.1, max: 16.0, default: 0.71 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SIMPLOOPER_EFFECT_ABI,
    name: c"Parametric EQ".as_ptr(),
    author: c"simplooper".as_ptr(),
    description:
        c"6-band parametric EQ (RBJ biquads): shelf/peak/notch/bandpass per band, response curve + live spectrum in its own window.".as_ptr(),
    n_params: NPARAMS,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SIMPLOOPER_EFFECT_GUI_ABI,
    want_width: 700.0,
    want_height: 470.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

#[no_mangle]
pub extern "C" fn simplooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

#[no_mangle]
pub extern "C" fn simplooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
