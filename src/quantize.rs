//! Grid math and audio-editing helpers for the DAW workflow.
//!
//! The metronome (tempo, beats per measure, subdivision) defines the *one*
//! timeline grid every track shares: a beat is `60/bpm` seconds, a measure is
//! `metro_num` beats, and a grid line is `sub` evenly-spaced steps per beat
//! (1 = quarter note, 2 = eighth, 3 = triplet, 4 = sixteenth, ...). Two
//! independent grid levels use these helpers:
//!
//! - **Quantize** snaps a clip's *start position* to the nearest grid line at
//!   the chosen quantize subdivision (and records can snap automatically).
//! - **Snap** constrains where dragging or dropping a clip may place it, using
//!   the same subdivision list at its own level.
//!
//! Positions here are always *timeline samples* at the engine rate; nothing in
//! this module knows about JACK — it is pure math, unit-tested offline.

/// Samples per beat at `bpm` for an engine running at `sr`.
pub fn beat_samples(sr: u32, bpm: u32) -> f64 {
    60.0 * sr as f64 / bpm.max(1) as f64
}

/// Grid-unit length in samples for `sub` evenly-spaced steps per beat.
/// A subdivision of `n` divides the beat into `n` equal grid steps.
pub fn unit_samples(sr: u32, bpm: u32, sub: u32) -> f64 {
    beat_samples(sr, bpm) / sub.max(1) as f64
}

/// Snap a timeline position to the nearest multiple of `unit`.
/// Non-finite or non-positive units pass the position through unchanged.
pub fn snap_start(pos: u64, unit: f64) -> u64 {
    if !unit.is_finite() || unit <= 0.0 {
        return pos;
    }
    ((pos as f64 / unit).round() * unit).max(0.0) as u64
}

/// Floor a timeline position down to the grid (used by snap-dragging, where
/// the grab offset should keep the clip under the cursor's left edge).
pub fn floor_start(pos: u64, unit: f64) -> u64 {
    if !unit.is_finite() || unit <= 0.0 {
        return pos;
    }
    ((pos as f64 / unit).floor() * unit).max(0.0) as u64
}

/// Round `pos` up to the next multiple of `bar` (project-length padding).
pub fn round_up_to(pos: u64, bar: u64) -> u64 {
    if bar == 0 {
        pos
    } else {
        pos.div_ceil(bar).saturating_mul(bar)
    }
}

/// Level floor of the silence detector, in dB (everything below is silence).
pub const TRIM_FLOOR_DB: f32 = -60.0;

/// dB -> linear amplitude.
pub fn db_to_amp(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// First and last non-silent sample window `[start, end)` over `thr`.
/// `None` when the buffer is pure silence.
pub fn trim_bounds(buf: &[f32], thr: f32) -> Option<(usize, usize)> {
    let mut start = None;
    let mut end = 0usize;
    for (i, &v) in buf.iter().enumerate() {
        if v.abs() >= thr {
            if start.is_none() {
                start = Some(i);
            }
            end = i + 1;
        }
    }
    start.map(|s| (s, end))
}

/// Clip the silence off the start and end of `buf`: the content between the
/// first and last sample over the [`TRIM_FLOOR_DB`] floor, with a ~5 ms edge
/// fade on both sides so the new boundaries can never click. `None` when the
/// input is pure silence (the caller keeps the original and says so).
pub fn trim_silence(buf: &[f32], sr: u32) -> Option<Vec<f32>> {
    let (s, e) = trim_bounds(buf, db_to_amp(TRIM_FLOOR_DB))?;
    let keep = e - s;
    let fade = ((sr as usize / 200).max(1)).min(keep / 2).max(1); // ~5 ms, never over half
    let mut out = buf[s..e].to_vec();
    for i in 0..fade.min(out.len()) {
        let w = i as f32 / fade as f32;
        out[i] *= w;
    }
    let n = out.len();
    for i in 0..fade.min(n) {
        let w = i as f32 / fade as f32;
        out[n - 1 - i] *= w;
    }
    Some(out)
}

/// Snap an asset's *content* to the quantize grid.
///
/// The leading silence is located, trimmed, and re-padded so the first
/// transient lands exactly on a grid line (`unit` samples apart, anchored at
/// sample 0 = bar 1 beat 1 of the metronome grid); the total length is then
/// rounded up to a whole number of grid units so the clip ends on a line
/// too. Everything between stays bit-identical. `None` for pure silence
/// (the caller keeps the original and says so).
pub fn quantize_asset(buf: &[f32], sr: u32, unit: f64) -> Option<Vec<f32>> {
    let (s, e) = trim_bounds(buf, db_to_amp(TRIM_FLOOR_DB))?;
    let unit = unit.max(1.0).round() as usize;
    // Nearest grid line to the first transient (same law as `snap_start`).
    let new_start = ((s as f64 / unit as f64).round() as usize) * unit;
    let content = e - s;
    // End on a grid line without cutting the tail.
    let len = (new_start + content).div_ceil(unit) * unit;
    let mut out = vec![0.0f32; len];
    out[new_start..new_start + content].copy_from_slice(&buf[s..e]);
    let _ = sr; // (kept for symmetry with trim_silence; grid is in samples)
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_math() {
        // 120 BPM @ 48 kHz: beat = 24000 samples; eighth = 12000.
        assert!((beat_samples(48_000, 120) - 24_000.0).abs() < 1e-9);
        assert!((unit_samples(48_000, 120, 2) - 12_000.0).abs() < 1e-9);
        // Triplets divide evenly too.
        assert!((unit_samples(48_000, 120, 3) * 3.0 - 24_000.0).abs() < 1e-9);
        // Guard against zero tempo / zero subdivision.
        assert!(beat_samples(48_000, 0).is_finite());
        assert!((unit_samples(48_000, 120, 0) - 24_000.0).abs() < 1e-9);
    }

    #[test]
    fn snap_rounds_to_nearest_line() {
        let u = 12_000.0;
        assert_eq!(snap_start(0, u), 0);
        assert_eq!(snap_start(5_000, u), 0); // below half -> down
        assert_eq!(snap_start(6_000, u), 12_000); // half rounds up
        assert_eq!(snap_start(20_000, u), 24_000); // above half -> up
        assert_eq!(snap_start(36_001, u), 36_000);
        // Degenerate units pass through.
        assert_eq!(snap_start(123, 0.0), 123);
        assert_eq!(snap_start(123, f64::NAN), 123);
    }

    #[test]
    fn floor_never_goes_up() {
        let u = 12_000.0;
        assert_eq!(floor_start(11_999, u), 0);
        assert_eq!(floor_start(12_000, u), 12_000);
        assert_eq!(floor_start(35_999, u), 24_000);
        assert_eq!(floor_start(123, 0.0), 123);
    }

    #[test]
    fn round_up_bars() {
        assert_eq!(round_up_to(0, 96_000), 0);
        assert_eq!(round_up_to(1, 96_000), 96_000);
        assert_eq!(round_up_to(96_000, 96_000), 96_000);
        assert_eq!(round_up_to(96_001, 96_000), 192_000);
        assert_eq!(round_up_to(42, 0), 42, "bar 0 passes through");
    }

    #[test]
    fn silence_trim_bounds() {
        let mut buf = vec![0.0f32; 1000];
        for v in buf.iter_mut().take(1000) {
            *v = 0.0;
        }
        assert_eq!(trim_bounds(&buf, 0.001), None, "pure silence");
        buf[200..500].fill(0.25);
        assert_eq!(trim_bounds(&buf, 0.001), Some((200, 500)));
    }

    #[test]
    fn silence_trim_cuts_edges_and_fades() {
        let sr = 48_000u32;
        let mut buf = vec![0.0f32; 48_000];
        for (i, v) in buf.iter_mut().enumerate().skip(9_600).take(38_400 - 9_600) {
            *v = ((i as f32) * 0.11).sin() * 0.5;
        }
        let out = trim_silence(&buf, sr).expect("content survives");
        // Content length ≈ the non-silent window; edges are faded to (near) zero.
        assert!((out.len() as i64 - 28_800).abs() <= 8, "kept {} samples", out.len());
        assert!(out[0].abs() < 1e-3, "head faded");
        assert!(out[out.len() - 1].abs() < 1e-3, "tail faded");
        // Pure silence yields None so the caller can keep the original.
        let silent = vec![0.0f32; 1000];
        assert!(trim_silence(&silent, sr).is_none());
    }
}
