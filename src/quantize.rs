//! "Easy quantization" — snap loop lengths onto a beat/bar grid.
//!
//! The tempo is derived from Loop 1's actual recorded length: the user says
//! "Loop 1 is N bars of M/D time", so one bar = len1 / N samples and one beat
//! = bar / M samples. Every loop's length is then snapped to the nearest
//! multiple of the chosen unit (beat or bar). Following the standard
//! loop-editing practice (DAW snap-to-grid + seamless-loop tools), a raw
//! grid snap alone would cut mid-waveform and click, so each new boundary is
//! refined to the quietest sample (nearest zero crossing) within ±5 ms and a
//! short wrap-around crossfade blends the tail into the head across the seam.

/// The grid unit lengths are snapped to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    Beat,
    Bar,
}

/// Compute the grid from Loop 1's length and the user's time signature.
///
/// Returns `(target_samples, beat_samples, bar_samples)` where `target` is
/// the (rounded) unit the loops will be snapped to.
pub fn compute_target(
    len1: u32,
    bars: u32,
    num: u32,
    unit: Unit,
) -> Result<(u32, f64, f64), String> {
    if len1 == 0 {
        return Err("Loop 1 has no recording yet — record it first".into());
    }
    if bars == 0 || num == 0 {
        return Err("bars and numerator must be at least 1".into());
    }
    let bar = len1 as f64 / bars as f64;
    let beat = bar / num as f64;
    let target_f = match unit {
        Unit::Beat => beat,
        Unit::Bar => bar,
    };
    if target_f < 0.5 {
        return Err(
            "grid too fine: fewer than half a sample per unit — use more bars or a smaller numerator"
                .into(),
        );
    }
    let target = (target_f + 0.5) as u32; // round to nearest whole sample
    Ok((target, beat, bar))
}

/// Snap `len` to the nearest multiple of `target`, clamped to `[target, cap]`.
pub fn snap_length(len: u32, target: u32, cap: u32) -> u32 {
    let mult = (len as f64 / target as f64).round().max(1.0);
    let snapped = (mult * target as f64) as u32;
    snapped.clamp(target.min(cap), cap)
}

/// Pick the quietest sample within `±window` of `ideal` (nearest zero
/// crossing) as the actual cut point.
pub fn refine_zero_crossing(buf: &[f32], ideal: u32, window: u32, cap: u32) -> u32 {
    if buf.is_empty() {
        return ideal;
    }
    let lo = ideal.saturating_sub(window).max(1);
    let hi = (ideal + window).min(cap).min(buf.len() as u32 - 1);
    if lo > hi {
        return ideal.min(hi).max(lo);
    }
    let mut best = ideal.clamp(lo, hi);
    let mut best_abs = f32::MAX;
    for k in lo..=hi {
        let a = buf[k as usize].abs();
        if a < best_abs {
            best_abs = a;
            best = k;
        }
    }
    best
}

/// Wrap-around linear crossfade: blend the last `f` samples (the tail) into
/// the first `f` samples (the head) with complementary gains, so playback is
/// continuous when the loop wraps from index `len-1` back to 0. In place.
pub fn seam_crossfade(buf: &mut [f32], len: u32, f: u32) {
    if f == 0 || len <= f {
        return;
    }
    let f = (f as usize).min((len / 4) as usize); // never more than a quarter of the loop
    let l = len as usize;
    for i in 0..f {
        let w = (i + 1) as f32 / f as f32; // 0 → 1 across the window
        let head = buf[i];
        let tail = buf[l - f + i];
        buf[i] = head * w + tail * (1.0 - w);
    }
}

/// Quantize one recorded track in place: snap its length to the grid, refine
/// the cut to a zero crossing, pad with silence or truncate, and smooth the
/// seam. Returns the new length.
pub fn quantize_track(buf: &mut [f32], len: u32, target: u32, cap: u32, sr: u32) -> u32 {
    let ideal = snap_length(len, target, cap);
    // ±5 ms search window for the zero crossing (bounded by the loop itself).
    let window = ((sr / 200).max(1)).min(ideal / 4);
    let cut = refine_zero_crossing(buf, ideal, window, cap).max(target.min(cap));

    if cut > len {
        // Pad with silence up to the new length.
        for k in (len as usize)..(cut as usize) {
            buf[k] = 0.0;
        }
    }
    // Truncation needs no data move: playback only ever reads [0, cut).
    seam_crossfade(buf, cut, (sr / 50).min(cut / 4)); // ~20 ms seam blend
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_math() {
        // 1 s loop @ 48 kHz, "2 bars of 4/4": bar = 24000, beat = 6000.
        let (t, beat, bar) = compute_target(48_000, 2, 4, Unit::Beat).unwrap();
        assert_eq!(t, 6000);
        assert!((beat - 6000.0).abs() < 1e-9);
        assert!((bar - 24_000.0).abs() < 1e-9);
        let (t, _, _) = compute_target(48_000, 2, 4, Unit::Bar).unwrap();
        assert_eq!(t, 24_000);
    }

    #[test]
    fn target_errors() {
        assert!(compute_target(0, 2, 4, Unit::Beat).is_err());
        assert!(compute_target(96_000, 0, 4, Unit::Bar).is_err());
        // 1 sample of loop, 1000 bars: grid finer than half a sample.
        assert!(compute_target(1, 1000, 128, Unit::Beat).is_err());
    }

    #[test]
    fn snap_nearest_multiple() {
        assert_eq!(snap_length(10, 3, 100), 9); // 10/3 = 3.33 → 3×3
        assert_eq!(snap_length(4, 3, 100), 3); // nearest multiple of 3 to 4
        assert_eq!(snap_length(1, 3, 100), 3); // clamps up to one unit
        assert_eq!(snap_length(97, 3, 100), 96); // nearest multiple: 96 (d=1) vs 99 (d=2)
        assert_eq!(snap_length(5000, 3, 100), 100); // clamps to cap
    }

    #[test]
    fn zero_crossing_refine() {
        let mut buf = vec![0.5f32; 100];
        buf[57] = 0.01; // quietest point
        let k = refine_zero_crossing(&buf, 60, 10, 100);
        assert_eq!(k, 57);
    }

    #[test]
    fn crossfade_continuity() {
        let mut buf: Vec<f32> = (0..16).map(|i| i as f32 * 0.1).collect();
        let len = 16u32;
        let f = 4u32;
        let tail_first = buf[(len - f) as usize]; // sample that precedes the wrap region
        let head_first = buf[0];
        let last = buf[len as usize - 1];
        seam_crossfade(&mut buf, len, f);
        // The first sample is a blend dominated by the tail (continuation of
        // what played before the wrap): out[0] = head*w + tail*(1-w), w=1/f.
        let w = 1.0 / f as f32;
        assert!((buf[0] - (head_first * w + tail_first * (1.0 - w))).abs() < 1e-6);
        // Past the window the head is untouched.
        assert!((buf[f as usize] - f as f32 * 0.1).abs() < 1e-6);
        // The wrap jump shrank: |out[0] - out[len-1]| < |head - last|.
        let raw_jump = (head_first - last).abs();
        let new_jump = (buf[0] - buf[len as usize - 1]).abs();
        assert!(new_jump < raw_jump);
    }

    #[test]
    fn quantize_sine_to_grid() {
        let sr = 48_000u32;
        let mut buf = vec![0f32; 200_000];
        // Fill with a sine so the cut point is never trivially silent.
        for i in 0..150_000 {
            buf[i] = (i as f64 * std::f64::consts::PI * 2.0 * 440.0 / sr as f64).sin() as f32;
        }
        let len = 144_100u32; // 3.002 s — just off the 12 kHz grid point (12×)
        let target = 12_000u32; // "beat"
        let ideal = snap_length(len, target, 200_000);
        let new_len = quantize_track(&mut buf, len, target, 200_000, sr);
        // The cut lands on the grid within the ±5 ms zero-crossing window.
        assert!(
            (new_len as i64 - ideal as i64).abs() <= (sr / 200) as i64,
            "cut {new_len} drifted from grid point {ideal}"
        );
        // And it is a plausible loop length.
        assert!(new_len >= target && new_len <= len + sr / 200);
    }
}
