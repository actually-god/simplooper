//! The track / child-lane / clip arrangement model.
//!
//! This is the project's musical content as a plain data tree:
//!
//! ```text
//! Arrangement ── 16 tracks
//!                 └── lane 0 (the track itself) ── clips…
//!                 └── lane 1.. (child tracks, stacked additively)
//! ```
//!
//! A [`Clip`] is a reference to an asset plus a timeline position in samples;
//! the clip's length is the asset's length (clips play once when the playhead
//! crosses them — the *project* loops, clips never tile).
//!
//! The GUI owns the single mutable copy (every drop, drag, placement and
//! delete happens here first) and pushes an `Arc<Arrangement>` snapshot into
//! the audio engine, which only ever reads it. All length-dependent helpers
//! take an `alen` closure (asset id → asset length in samples) so this module
//! stays decoupled from the asset store and is trivially unit-testable.

use crate::audio::MAX_LOOPS;
use crate::quantize::round_up_to;

/// A placed instance of an asset: start position in timeline samples. The
/// asset id indexes the engine/GUI asset table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    pub asset: u32,
    pub pos: u64,
}

/// One lane: a horizontal strip of clips. Lane 0 is the track itself; lanes
/// 1.. are the track's child tracks. Overlapping clips — within a lane or
/// across lanes — always sum additively.
#[derive(Clone, Debug, Default)]
pub struct Lane {
    pub clips: Vec<Clip>,
}

impl Lane {
    /// True when any clip in this lane overlaps `[pos, pos + len)`.
    pub fn occupied(&self, pos: u64, len: u64) -> bool {
        let end = pos.saturating_add(len);
        self.clips
            .iter()
            .any(|c| pos < c.pos.saturating_add(len.max(1)) && c.pos < end)
    }
}

/// A track: the base lane plus any number of child lanes, index 0 = base.
#[derive(Clone, Debug)]
pub struct Track {
    pub lanes: Vec<Lane>,
}

impl Default for Track {
    fn default() -> Self {
        Track { lanes: vec![Lane::default()] }
    }
}

impl Track {
    /// Add a child lane; returns its index (always >= 1).
    pub fn add_lane(&mut self) -> usize {
        self.lanes.push(Lane::default());
        self.lanes.len() - 1
    }

    /// Remove a child lane. Lane 0 (the track itself) can never be removed.
    pub fn remove_lane(&mut self, k: usize) -> bool {
        if k == 0 || k >= self.lanes.len() {
            return false;
        }
        self.lanes.remove(k);
        true
    }
}

/// The whole project's musical content: exactly `MAX_LOOPS` tracks.
#[derive(Clone, Debug)]
pub struct Arrangement {
    pub tracks: Vec<Track>,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self::new()
    }
}

impl Arrangement {
    pub fn new() -> Self {
        Arrangement { tracks: (0..MAX_LOOPS).map(|_| Track::default()).collect() }
    }

    /// Remove every clip and child lane from a track (back to one empty lane).
    pub fn clear_track(&mut self, t: usize) {
        self.tracks[t] = Track::default();
    }

    pub fn add_lane(&mut self, t: usize) -> usize {
        self.tracks[t].add_lane()
    }

    pub fn remove_lane(&mut self, t: usize, k: usize) -> bool {
        self.tracks[t].remove_lane(k)
    }

    /// Highest clip end across every track (`pos + asset length`).
    pub fn max_end(&self, alen: &dyn Fn(u32) -> u64) -> u64 {
        self.tracks
            .iter()
            .flat_map(|t| t.lanes.iter())
            .flat_map(|l| l.clips.iter())
            .map(|c| c.pos.saturating_add(alen(c.asset)))
            .max()
            .unwrap_or(0)
    }

    /// Project length in samples: the end of the last clip rounded up to a
    /// whole measure, never shorter than one measure (so the metronome grid
    /// and an empty project still transport).
    pub fn total_len(&self, alen: &dyn Fn(u32) -> u64, bar: u64) -> u64 {
        round_up_to(self.max_end(alen), bar).max(bar.max(1))
    }

    /// Insert `clip` into `(t, lane)` unconditionally (manual drops may
    /// overlap freely — everything stacks additively).
    pub fn insert(&mut self, t: usize, lane: usize, clip: Clip) {
        if let Some(tr) = self.tracks.get_mut(t) {
            while tr.lanes.len() <= lane {
                tr.add_lane();
            }
            tr.lanes[lane].clips.push(clip);
        }
    }

    /// Place a freshly recorded take: lane 0 first; if the new clip would land
    /// on top of existing content there, the first child lane free at that
    /// span is used, and if every child overlaps, a new child lane is created.
    /// Returns the lane the clip ended up in.
    pub fn place_auto(&mut self, t: usize, asset: u32, len: u64, pos: u64) -> usize {
        let tr = &mut self.tracks[t];
        let pick = if !tr.lanes[0].occupied(pos, len) {
            0
        } else {
            match tr.lanes.iter().enumerate().skip(1).find(|(_, l)| !l.occupied(pos, len)) {
                Some((k, _)) => k,
                None => tr.add_lane(),
            }
        };
        tr.lanes[pick].clips.push(Clip { asset, pos });
        pick
    }

    pub fn remove_clip(&mut self, t: usize, lane: usize, c: usize) -> Option<Clip> {
        self.tracks
            .get_mut(t)
            .and_then(|tr| tr.lanes.get_mut(lane))
            .filter(|l| c < l.clips.len())
            .map(|l| l.clips.remove(c))
    }

    pub fn move_clip(&mut self, t: usize, lane: usize, c: usize, pos: u64) {
        if let Some(cl) = self
            .tracks
            .get_mut(t)
            .and_then(|tr| tr.lanes.get_mut(lane))
            .and_then(|l| l.clips.get_mut(c))
        {
            cl.pos = pos;
        }
    }

    /// Every `(track, lane, clip)` index placing this asset.
    pub fn clips_of(&self, asset: u32) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for (t, tr) in self.tracks.iter().enumerate() {
            for (l, lane) in tr.lanes.iter().enumerate() {
                for (c, cl) in lane.clips.iter().enumerate() {
                    if cl.asset == asset {
                        out.push((t, l, c));
                    }
                }
            }
        }
        out
    }

    /// Drop every placement of `asset` (asset deleted from the rack); returns
    /// how many clips were removed.
    pub fn remove_asset(&mut self, asset: u32) -> usize {
        let mut n = 0;
        for tr in self.tracks.iter_mut() {
            for lane in tr.lanes.iter_mut() {
                let before = lane.clips.len();
                lane.clips.retain(|c| c.asset != asset);
                n += before - lane.clips.len();
            }
        }
        n
    }

    /// True when any lane of any track holds a clip.
    pub fn is_empty(&self) -> bool {
        self.tracks.iter().all(|t| t.lanes.iter().all(|l| l.clips.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test asset is 1000 samples long.
    fn alen(_asset: u32) -> u64 {
        1000
    }

    #[test]
    fn fresh_arrangement() {
        let a = Arrangement::new();
        assert_eq!(a.tracks.len(), MAX_LOOPS);
        assert!(a.tracks.iter().all(|t| t.lanes.len() == 1 && t.lanes[0].clips.is_empty()));
        assert!(a.is_empty());
    }

    #[test]
    fn place_auto_prefers_base_then_children_then_new() {
        let mut a = Arrangement::new();
        // Empty track -> lane 0.
        assert_eq!(a.place_auto(3, 0, 1000, 5_000), 0);
        // Overlap on lane 0 -> first child lane.
        assert_eq!(a.place_auto(3, 1, 1000, 5_500), 1);
        // Overlap on lanes 0 and 1 -> a new child is created.
        assert_eq!(a.place_auto(3, 2, 1000, 5_200), 2);
        assert_eq!(a.tracks[3].lanes.len(), 3);
        // A span clear of everything goes back to lane 0.
        assert_eq!(a.place_auto(3, 3, 1000, 20_000), 0);
        // Child lanes stack additively, never replace: every placement lives.
        let total: usize = a.tracks[3].lanes.iter().map(|l| l.clips.len()).sum();
        assert_eq!(total, 4);
    }

    #[test]
    fn occupied_boundary_touch_is_not_overlap() {
        let mut a = Arrangement::new();
        a.place_auto(0, 0, 1000, 0);
        // Clip at exactly `len` after the first touches but does not overlap.
        assert_eq!(a.place_auto(0, 1, 1000, 1000), 0);
        // One sample early forces a child lane.
        assert_eq!(a.place_auto(0, 2, 1000, 999), 1);
    }

    #[test]
    fn lane_removal_keeps_base() {
        let mut a = Arrangement::new();
        a.place_auto(1, 0, 1000, 0); // empty track: lands on lane 0...
        a.place_auto(1, 0, 1000, 500); // ...the overlapping take opens lane 1
        assert_eq!(a.tracks[1].lanes.len(), 2);
        assert!(!a.remove_lane(1, 0), "lane 0 can never be removed");
        assert!(a.remove_lane(1, 1));
        assert!(!a.remove_lane(1, 1), "no such lane");
        assert_eq!(a.tracks[1].lanes.len(), 1);
    }

    #[test]
    fn lengths_and_cleanup() {
        let mut a = Arrangement::new();
        // Empty project: one bar minimum.
        assert_eq!(a.total_len(&alen, 4_800), 4_800);
        // Clip ending off the grid rounds up to the next bar.
        a.insert(2, 0, Clip { asset: 7, pos: 4_900 });
        assert_eq!(a.max_end(&alen), 5_900);
        assert_eq!(a.total_len(&alen, 4_800), 9_600);
        // Clips-of / remove-asset round trip.
        a.place_auto(2, 7, 1000, 0); // same asset, overlap -> child lane
        assert_eq!(a.clips_of(7).len(), 2);
        assert_eq!(a.remove_asset(7), 2);
        assert!(a.is_empty());
        assert_eq!(a.total_len(&alen, 4_800), 4_800);
    }

    #[test]
    fn clear_track_drops_children() {
        let mut a = Arrangement::new();
        a.place_auto(4, 0, 1000, 0);
        a.place_auto(4, 1, 1000, 0); // child
        a.clear_track(4);
        assert_eq!(a.tracks[4].lanes.len(), 1);
        assert!(a.tracks[4].lanes[0].clips.is_empty());
    }

    #[test]
    fn remove_clip_targets_one_placement_only() {
        let mut a = Arrangement::default();
        a.insert(2, 0, Clip { asset: 7, pos: 0 });
        a.insert(2, 0, Clip { asset: 8, pos: 48_000 });
        a.insert(2, 1, Clip { asset: 9, pos: 0 });
        let gone = a.remove_clip(2, 0, 0).expect("clip at index 0");
        assert_eq!(gone.asset, 7);
        // The other lane is untouched, and the sibling clip shifts to 0.
        assert_eq!(a.tracks[2].lanes[1].clips.len(), 1);
        assert_eq!(a.tracks[2].lanes[0].clips.len(), 1);
        assert_eq!(a.tracks[2].lanes[0].clips[0].asset, 8);
        // Out-of-range indexes remove nothing.
        assert!(a.remove_clip(2, 0, 5).is_none());
        assert!(a.remove_clip(9, 0, 0).is_none());
        assert!(a.remove_clip(2, 4, 0).is_none());
    }
}
