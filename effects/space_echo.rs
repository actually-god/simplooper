// ============================================================ slooper ==
// space_echo.rs — echo AND reverb in one effect, with a custom child GUI.
//
// A mono effects chain: a modulated feedback delay (tone-controlled damping
// in the loop) running in parallel with a Schroeder FDN reverb (damped comb
// banks + all-pass diffusers) fed from the echo path, mixed by independent
// echo/reverb levels, then dry/wet mix and output trim. Turning "echo" down
// and "verb" up turns the same circuit into a pure room simulator: the
// factory presets ship that way (practice hall .. cathedral).
//
//   dry ───────────────────────┐
//   in → delay ──┬─ ×echo ─────┤
//        └→ reverb ─ ×verb ────┤ mix → out
//
// (The file stem stays `space_echo` because saved chains and persisted
// param rows reference it; only the display name says "Echo + Reverb".)
//
// Params are ordinary effect params, so the auto slider window works too;
// the child GUI below is a scripted canvas showing the echo pattern
// (tick spacing = time, fade = feedback, jitter = wobble) over a reverb
// tail wedge, plus an input level history trace (bottom-up: silence at the
// floor), a factory row of presets (Echo, Practice Hall, Auditorium,
// Opera House, Cathedral) and the knob row.
// ---------------------------------------------------------------------------

include!("effect_api.rs");

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------
const P_TIME: usize = 0; // delay seconds (log range via slider range)
const P_FEED: usize = 1; // delay feedback 0..0.95
const P_TONE: usize = 2; // wet-path brightness, Hz (log): echo-loop AND reverb-feed damping
const P_WOBBLE: usize = 3; // modulation depth 0..1 (= ±1 % of time)
const P_RATE: usize = 4; // modulation rate Hz
const P_DECAY: usize = 5; // reverb feedback 0.5..0.95
const P_DAMP: usize = 6; // reverb damping, Hz (log)
const P_MIX: usize = 7; // wet mix 0..1
const P_OUT: usize = 8; // output trim 0..2
const P_ECHO: usize = 9; // echo-path level 0..1.5
const P_VERB: usize = 10; // reverb-path level 0..1.5
const NPARAMS: u32 = 11;

/// Factory presets — name + all params (same order as PARAMS).
/// "Echo" is the plain dub-style effect (the old default); the rooms run
/// echo LOW and reverb FULL so the FDN tail, not the slap, dominates —
/// bright tight hall → auditorium → opera house → vast dark cathedral.
/// (Time/feedbk shape the early slap, decay/damp the tail length/colour.)
const PRESETS: [(&str, [f32; NPARAMS as usize]); 5] = [
    ("Echo", [0.38, 0.62, 3600.0, 0.0, 0.6, 0.84, 2800.0, 0.4, 1.0, 1.0, 0.5]),
    ("Practice Hall", [0.07, 0.3, 8000.0, 0.05, 0.5, 0.72, 7500.0, 0.5, 0.95, 0.3, 1.0]),
    ("Auditorium", [0.14, 0.38, 5200.0, 0.1, 0.35, 0.84, 4500.0, 0.55, 0.95, 0.25, 1.0]),
    ("Opera House", [0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0]),
    ("Cathedral", [0.32, 0.5, 2200.0, 0.25, 0.14, 0.95, 1500.0, 0.7, 0.9, 0.18, 1.0]),
];

// ---------------------------------------------------------------------------
// Delay line with cubic (Hermite) fractional read.
// ---------------------------------------------------------------------------
struct Delay {
    buf: Vec<f64>,
    wp: usize,
}

impl Delay {
    fn new(n: usize) -> Self {
        Self { buf: vec![0.0; n.max(4)], wp: 0 }
    }
    #[inline]
    fn write(&mut self, x: f64) {
        self.buf[self.wp] = x;
        self.wp = (self.wp + 1) % self.buf.len();
    }
    /// Read `d` samples in the past (d >= 1.0), cubic-interpolated.
    #[inline]
    fn read(&self, d: f64) -> f64 {
        let n = self.buf.len() as f64;
        let d = d.clamp(1.0, n - 2.0);
        let df = d.floor();
        let frac = d - df;
        let base = (self.wp as f64 - df) as i64;
        let idx = |k: i64| -> usize {
            let mut i = k % self.buf.len() as i64;
            if i < 0 {
                i += self.buf.len() as i64;
            }
            i as usize
        };
        let xm1 = self.buf[idx(base - 1)];
        let x0 = self.buf[idx(base)];
        let x1 = self.buf[idx(base + 1)];
        let x2 = self.buf[idx(base + 2)];
        // cubic Hermite (Catmull-Rom)
        let a = -0.5 * xm1 + 1.5 * x0 - 1.5 * x1 + 0.5 * x2;
        let b = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
        let c = -0.5 * xm1 + 0.5 * x1;
        ((a * frac + b) * frac + c) * frac + x0
    }
}

// ---------------------------------------------------------------------------
// Schroeder comb with damping (Freeverb topology) and all-pass diffusers.
// ---------------------------------------------------------------------------
/// Feedback comb with damped loop and a SLOWLY MODULATED delay time.
/// A fixed-length comb is a row of infinitely thin resonances; after a
/// transient they ring as individual high-pitched partials — the metallic
/// "ping" (worse with few, closely-spaced combs, and untreatable by the
/// damping knob). Sweeping the length a fraction of a sample at <1 Hz
/// (the EMT-250 / Bricasti trick) Doppler-spreads each mode into a dense
/// band, so late tails wash instead of ring. Read is linearly
/// interpolated at the fractional delay.
struct Comb {
    buf: Vec<f64>,
    idx: usize,
    lp: f64,
    /// second stage of the TWO-POLE loop damping (12 dB/oct rolloff —
    /// one-pole left an icy 4–8 kHz residue that read as "tinny")
    lp2: f64,
    /// base delay in samples and modulation depth/phase offset
    base: f64,
    depth: f64,
    off: f64,
}

impl Comb {
    fn new(base: f64, off: f64) -> Self {
        // Headroom for the room stretch (decay drives delay up to ~2×)
        // plus modulation depth.
        let n = (base as usize * 9 / 4 + 8).max(8);
        // EMT plates modulate ~±0.5 % of the delay length. The old
        // ±0.18 %-max clamp left every comb mode pinned within ~±0.06 % —
        // the eight tuned banks then stacked into a sustained, high-pitched
        // WHINE (user report, 2026-10). At this depth the modes smear into
        // bands and the row of resonators becomes a wash.
        let depth = (base * 0.006).clamp(2.0, 12.0);
        Self { buf: vec![0.0; n], idx: 0, lp: 0.0, lp2: 0.0, base, depth, off }
    }
    #[inline]
    fn process(&mut self, x: f64, fb: f64, damp_c: f64, phase: f64, room: f64) -> f64 {
        // `room` stretches the delay with the decay knob: bigger room =
        // longer combs = lower, denser modes; a fixed bank heard as a boxy
        // midrange "cathedral-prison" ring no matter how long the decay.
        let d = self.base * room + self.depth * (phase + self.off).sin();
        let n = self.buf.len();
        let di = d as usize;
        let fr = d - di as f64;
        let i0 = (self.idx + n - 1 - (di % n)) % n;
        let i1 = (i0 + n - 1) % n;
        let out = self.buf[i0] + (self.buf[i1] - self.buf[i0]) * fr;
        self.lp = out * (1.0 - damp_c) + self.lp * damp_c;
        self.lp2 = self.lp * (1.0 - damp_c) + self.lp2 * damp_c;
        if !self.lp2.is_finite() {
            self.lp = 0.0;
            self.lp2 = 0.0;
        }
        self.buf[self.idx] = x + self.lp2 * fb;
        self.idx = (self.idx + 1) % n;
        if !out.is_finite() {
            return 0.0;
        }
        out
    }
}

#[inline]
fn allpass(prev: &mut Allpass, x: f64, g: f64) -> f64 {
    let bufout = prev.buf[prev.idx];
    prev.buf[prev.idx] = x + g * bufout;
    prev.idx = (prev.idx + 1) % prev.buf.len();
    let out = bufout - g * prev.buf[prev.idx];
    if out.is_finite() {
        out
    } else {
        0.0
    }
}

struct Allpass {
    buf: Vec<f64>,
    idx: usize,
}

impl Allpass {
    fn new(n: usize) -> Self {
        Self { buf: vec![0.0; n.max(4)], idx: 0 }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

struct SpaceEcho {
    sr: f64,
    delay: Delay,
    /// one-pole damping state in the delay feedback loop
    tone_lp: f64,
    tone_c: f64,
    phase: f64, // LFO phase (radians)
    combs: [Comb; 8],
    /// comb-length modulation clock (radians)
    vphase: f64,
    ap: [Allpass; 6],
    /// reverb-input pre-damp (one-pole at the TONE cutoff): real rooms are
    /// fed AIR-ROLLED sound, and raw dry/echo fizz into the comb bank is
    /// where the "tinny" starts — filter before ringing, not after.
    verb_lp: f64,
    /// comb lengths at sr=44.1 kHz; scaled by sr and by a 0.9..1.1 "room"
    /// spread so the two banks don't share modes.
    mix: f64,
    out: f64,
    gui_open: AtomicU32,
    in_peak: AtomicU32,
}

/// Comb tunings (samples @ 44100) and their spread factors.
// Four combs bunched between 1422..1617 samples leave the mode spacing
// sparse enough for individual modes to ring out after a transient: that is
// the high-pitched metallic "ping" on snaps/claps, and no damping setting
// fixes it (low damp passes the ring, high damp only kills the tail). The
// classic Freeverb eight-comb bank from 1116..1617 samples multiplies the
// mode density and smears the resonance into a wash; the 0.5 sum below
// keeps the bank at the old level.
const COMB_TUNE: [f64; 8] = [1116.0, 1188.0, 1277.0, 1356.0, 1422.0, 1491.0, 1557.0, 1617.0];
/// All-pass tunings (samples @ 44100). Six diffusers instead of the
/// original four: more smear per unit of ring, and the two added lengths
/// (672, 773) sit above the old top so the diffusion stays even up to
/// ~2 kHz where the ear reads "metal".
const AP_TUNE: [f64; 6] = [225.0, 341.0, 441.0, 556.0, 672.0, 773.0];

impl SpaceEcho {
    fn new(sample_rate: f64) -> Self {
        let sr = if sample_rate.is_finite() && sample_rate > 1.0 { sample_rate } else { 48000.0 };
        let dl_n = (sr * 2.5) as usize + 16; // up to 2.5 s delay
        let scale = sr / 44100.0;
        let combs = COMB_TUNE.map(|t| Comb::new(t * scale, t)); // phase offset = tuning
        let ap = AP_TUNE.map(|t| Allpass::new((t * scale) as usize + 1));
        Self {
            sr,
            delay: Delay::new(dl_n),
            tone_lp: 0.0,
            tone_c: 0.5,
            phase: 0.0,
            combs,
            vphase: 0.0,
            ap,
            verb_lp: 0.0,
            mix: 0.4,
            out: 1.0,
            gui_open: AtomicU32::new(0),
            in_peak: AtomicU32::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// Audio entry points.
// ---------------------------------------------------------------------------
extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(SpaceEcho::new(sample_rate))) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut SpaceEcho) });
    }
}

#[inline]
fn one_c(fc_hz: f64, sr: f64) -> f64 {
    // one-pole lowpass coefficient
    (-(core::f64::consts::TAU * fc_hz.max(20.0) / sr)).exp()
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let st = unsafe { &mut *(state as *mut SpaceEcho) };
    let sr = st.sr;
    let g = |k: usize| unsafe { *params.add(k) };
    let x = input as f64;
    let x = if x.is_finite() { x } else { 0.0 };

    // Input level for the GUI meter (free when the editor is closed).
    if st.gui_open.load(Relaxed) != 0 {
        let m = if x < 0.0 { -x } else { x };
        let prev = f32::from_bits(st.in_peak.load(Relaxed)) as f64;
        let env = if m > prev { m } else { prev * 0.999 };
        st.in_peak.store((env as f32).to_bits(), Relaxed);
    }

    // ---- delay with LFO-modulated time and damped feedback ----
    let time_s = (g(P_TIME) as f64).clamp(0.02, 2.4);
    let fb = (g(P_FEED) as f64).clamp(0.0, 0.95);
    let wobble = (g(P_WOBBLE) as f64).clamp(0.0, 1.0);
    let rate = (g(P_RATE) as f64).clamp(0.01, 20.0);
    let decay = (g(P_DECAY) as f64).clamp(0.5, 0.95);
    let echo_lvl = (g(P_ECHO) as f64).clamp(0.0, 1.5);
    let verb_lvl = (g(P_VERB) as f64).clamp(0.0, 1.5);
    st.mix += ((g(P_MIX) as f64).clamp(0.0, 1.0) - st.mix) * 0.01;
    st.out += ((g(P_OUT) as f64).clamp(0.0, 2.0) - st.out) * 0.01;

    st.tone_c = one_c(g(P_TONE) as f64, sr);
    st.phase += core::f64::consts::TAU * rate / sr;
    if st.phase > core::f64::consts::TAU {
        st.phase -= core::f64::consts::TAU;
    }
    let w = 1.0 + wobble * 0.01 * st.phase.sin();
    let d = (time_s * sr * w).clamp(1.0, (st.delay.buf.len() - 4) as f64);

    let echoed = st.delay.read(d);
    // damped feedback loop
    st.tone_lp = echoed * (1.0 - st.tone_c) + st.tone_lp * st.tone_c;
    if !st.tone_lp.is_finite() {
        st.tone_lp = 0.0;
    }
    st.delay.write(x + st.tone_lp * fb);

    // ---- FDN reverb in parallel with the echo (fed from echo + dry) ----
    // 0.45 feed: rooms need the reverb to read as the ROOM, not as a faint
    // halo around the slap — the preset row drives echo down / verb up.
    // The feed is damped at the TONE cutoff (user report: "the tone knob
    // does seemingly nothing" — at low feedback the echo loop alone made
    // tone inaudible; reverb brightness responds at any setting, so tone
    // now voices the entire wet path). Filtering before the comb bank is
    // what separates "air" from "tin can" — filters after the ring can
    // only dull everything together.
    st.verb_lp = (echoed * 0.6 + x * 0.4) * 0.45 * (1.0 - st.tone_c)
        + st.verb_lp * st.tone_c;
    if !st.verb_lp.is_finite() {
        st.verb_lp = 0.0;
    }
    let verb_in = st.verb_lp;
    let damp_c = 1.0 - one_c(g(P_DAMP) as f64, sr);
    // Room stretch: decay 0.5 → 1.0× comb length, decay 0.95 → ~1.95×.
    // Long tails through SHORT combs are the boxy, metallic "cathedral
    // prison" ring — the modes have to actually descend for the room.
    let room = 1.0 + ((decay - 0.5) / 0.45).clamp(0.0, 1.0) * 0.95;
    // comb-length LFO: ~0.85 Hz, one clock, per-comb offsets from the tunings.
    st.vphase += core::f64::consts::TAU * 0.85 / sr;
    if st.vphase > core::f64::consts::TAU {
        st.vphase -= core::f64::consts::TAU;
    }
    let mut sum = 0.0f64;
    for c in st.combs.iter_mut() {
        sum += c.process(verb_in, decay, damp_c, st.vphase, room);
    }
    let mut v = sum * 0.5;
    for a in st.ap.iter_mut() {
        v = allpass(a, v, 0.5);
    }

    let wet = echoed * echo_lvl + v * verb_lvl;
    let o = x * (1.0 - st.mix) + wet * st.mix * st.out;
    if !o.is_finite() {
        return 0.0;
    }
    o as f32
}

// ---------------------------------------------------------------------------
// Custom child GUI.
// ---------------------------------------------------------------------------
const HIST: usize = 160;

struct Gs {
    /// input-level history, newest last.
    hist: Vec<f32>,
    /// animation clock (seconds of GUI time)
    clock: f64,
    /// level history from the audio side, cached across draws
    inst: Option<*mut core::ffi::c_void>,
    last_t: f64,
}

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs { hist: vec![0.0; HIST], clock: 0.0, inst: None, last_t: 0.0 })) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if gs.is_null() {
        return;
    }
    let gs = unsafe { Box::from_raw(gs as *mut Gs) };
    if let Some(p) = gs.inst {
        unsafe { (*(p as *mut SpaceEcho)).gui_open.store(0, Relaxed) };
    }
}

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

    // animation clock from the app-wide time
    if gs.last_t == 0.0 {
        gs.last_t = fr.time;
    }
    gs.clock += (fr.time - gs.last_t).clamp(0.0, 0.1);
    gs.last_t = fr.time;

    if !state.is_null() {
        gs.inst = Some(state);
        unsafe { (*(state as *mut SpaceEcho)).gui_open.store(1, Relaxed) };
    }

    let gp = |k: u32| unsafe { (api.get_param)(c, k) };
    let sp = |k: u32, v: f32| unsafe { (api.set_param)(c, k, v) };

    let w = fr.width;
    let h = fr.height;
    let canvas_h = (h - 132.0).max(100.0);

    // ---- level history ----------------------------------------------------
    let lvl = if !state.is_null() {
        f32::from_bits(unsafe { (*(state as *const SpaceEcho)).in_peak.load(Relaxed) })
    } else {
        0.0
    };
    gs.hist.remove(0);
    gs.hist.push(lvl);

    unsafe {
        (api.rect_filled)(c, 8.0, 8.0, w - 16.0, canvas_h, 0.09, 0.10, 0.13, 1.0);
        // history trace — bottom-up like the EQ's analyser: silence sits at
        // the floor, louder input climbs toward the top (0 dBFS). The old
        // mapping was inverted (silence pinned the trace to the top edge).
        let cw = w - 16.0;
        let mut pts: Vec<f32> = Vec::with_capacity(HIST * 2);
        for (i, v) in gs.hist.iter().enumerate() {
            let db = 20.0f32 * (v.max(1e-6)).log10(); // -120..0
            let up = 1.0 - ((-db) / 60.0).clamp(0.0, 1.0);
            pts.push(8.0 + (i as f32 / (HIST - 1) as f32) * cw);
            pts.push(8.0 + canvas_h - up.max(0.0) * canvas_h * 0.98);
        }
        (api.polyline)(c, pts.as_ptr(), pts.len() as u32 / 2, 0, 1.4, 0.30, 0.85, 0.55, 0.85);

        // ---- echo pattern + reverb tail ---------------------------------
        let time = gp(P_TIME as u32);
        let feed = gp(P_FEED as u32);
        let wobble = gp(P_WOBBLE as u32);
        let rate = gp(P_RATE as u32);
        let decay = gp(P_DECAY as u32);
        let clk = gs.clock as f32;

        // scrolling playhead: 2 s across the canvas
        let scroll = 2.0f32;
        let px_per_s = cw / scroll;
        let midy = 8.0 + canvas_h * 0.62;
        // "now" is the right edge; echoes arrive every `time` seconds.
        let mut et = clk % time; // phase into the next echo
        let mut n = 0usize;
        while n < 24 {
            let x = cw + 8.0 - et * px_per_s;
            if x < 8.0 {
                break;
            }
            let amp = feed.powi(n as i32);
            // wobble jitters the arrival time
            let jitter = (clk * rate * core::f32::consts::TAU + n as f32 * 1.7).sin() * wobble * 0.01 * time;
            let x = x + jitter * px_per_s;
            let hh = (amp * canvas_h * 0.36).clamp(2.0, canvas_h * 0.48);
            (api.rect_filled)(c, x - 2.0, midy - hh / 2.0, 4.0, hh, 1.0, 0.75, 0.35, (0.25 + amp * 0.75).min(1.0));
            et += time;
            n += 1;
        }
        // reverb tail wedge behind each echo, sized by decay
        let tail = (0.05_f32).max((decay.ln() / (0.95_f32).ln()).sqrt()) * time * 4.0;
        et = clk % time;
        for n in 0..6 {
            let amp = feed.powi(n as i32);
            let x = cw + 8.0 - et * px_per_s;
            let tl = (tail * px_per_s * amp).clamp(0.0, cw * 2.0);
            (api.rect_filled)(c, x, midy - 2.0, tl, 4.0, 0.42, 0.60, 0.95, 0.10 + amp * 0.20);
            et += time;
        }

        // input head marker on the left
        (api.text)(c, 12.0, 12.0, 10.0, 0, 0.7, 0.7, 0.75, 1.0, c"in".as_ptr());
        let lv = lvl.clamp(0.0, 1.0) * (canvas_h - 20.0);
        (api.rect_filled)(c, 12.0, 8.0 + canvas_h - 10.0 - lv, 6.0, lv, 0.30, 0.85, 0.55, 0.9);

        // ---- knob row ----------------------------------------------------
        // (ky starts high in the strip: the preset row lives at its bottom.)
        // Pitch 66 fits eleven knobs across the 760 px window.
        let ky = canvas_h + 6.0;
        let row = |i: usize, label: &str, idx: usize, min: f32, max: f32, curve: f32, fmt: &str| {
            let mut v = gp(idx as u32);
            let shown = match fmt {
                "ms" => format!("{label} {:.0}ms", v * 1000.0),
                "hz" => format!("{label} {:.1}Hz", v),
                "khz" => format!("{label} {:.1}kHz", v / 1000.0),
                "pct" => format!("{label} {:.0}%", v * 100.0),
                "db" => format!("{label} {:+.1}dB", 20.0 * (v.max(1e-4)).log10()),
                _ => format!("{label} {v:.2}"),
            };
            let s = std::ffi::CString::new(shown).unwrap();
            if (api.knob)(c, 12.0 + i as f32 * 66.0, ky + 18.0, 44.0, s.as_ptr(), &mut v, min, max, curve) != 0 {
                sp(idx as u32, v);
            }
        };
        row(0, "time", P_TIME, 0.02, 2.4, 0.0, "ms");
        row(1, "feedbk", P_FEED, 0.0, 0.95, 1.0, "pct");
        row(2, "tone", P_TONE, 200.0, 16000.0, 0.0, "khz");
        row(3, "wobble", P_WOBBLE, 0.0, 1.0, 1.0, "pct");
        row(4, "rate", P_RATE, 0.05, 12.0, 0.0, "hz");
        row(5, "echo", P_ECHO, 0.0, 1.5, 1.0, "pct");
        row(6, "verb", P_VERB, 0.0, 1.5, 1.0, "pct");
        row(7, "decay", P_DECAY, 0.5, 0.95, 1.0, "pct");
        row(8, "damp", P_DAMP, 200.0, 16000.0, 0.0, "khz");
        row(9, "mix", P_MIX, 0.0, 1.0, 1.0, "pct");
        row(10, "out", P_OUT, 0.0, 2.0, 1.0, "db");

        // ---- factory presets ------------------------------------------
        // One click applies all eleven params: plain echo, or a room with
        // the reverb carrying the tail (echo low, verb full).
        let py = canvas_h + 96.0;
        let s = std::ffi::CString::new("presets:").unwrap();
        (api.text)(c, 8.0, py + 5.0, 10.0, 0, 0.7, 0.7, 0.75, 1.0, s.as_ptr());
        for (pi, (name, vals)) in PRESETS.iter().enumerate() {
            let s = std::ffi::CString::new(*name).unwrap();
            if (api.button)(c, 62.0 + pi as f32 * 122.0, py, 118.0, s.as_ptr()) != 0 {
                for (k, v) in vals.iter().enumerate() {
                    sp(k as u32, *v);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Descriptor + GUI tables + exports.
// ---------------------------------------------------------------------------
static PARAMS: [EffectParamV1; 11] = [
    EffectParamV1 { label: c"time".as_ptr(), min: 0.02, max: 2.4, default: 0.38 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.95, default: 0.62 },
    EffectParamV1 { label: c"tone".as_ptr(), min: 200.0, max: 16000.0, default: 3600.0 },
    EffectParamV1 { label: c"wobble".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.05, max: 12.0, default: 0.6 },
    EffectParamV1 { label: c"decay".as_ptr(), min: 0.5, max: 0.95, default: 0.84 },
    EffectParamV1 { label: c"damp".as_ptr(), min: 200.0, max: 16000.0, default: 2800.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.4 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"echo".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
    EffectParamV1 { label: c"verb".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Echo + Reverb".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Modulated echo and FDN reverb with independent echo/reverb levels; damped loops, wobble LFO, animated echo-pattern window, factory presets (plain echo + practice hall .. cathedral).".as_ptr(),
    n_params: NPARAMS,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 760.0,
    want_height: 424.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

#[no_mangle]
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
