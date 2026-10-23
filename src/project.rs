//! Project save format — the `.slooper` file.
//!
//! A project is a small JSON document (custom, versioned, first field is the
//! magic `format` string) that captures everything needed to rebuild a
//! session: tempo/grid, quantize + snap settings, per-track mixer levels,
//! effects chains, the track/lane/clip arrangement, and the asset table.
//!
//! Audio is never embedded: every asset references a WAV file by path.
//! Absolute paths survive directly; a vanished file is re-resolved by file
//! name against the project's own `audio/` folder and the rack folders
//! (see [`resolve_asset`]). The GUI writes derived/edited assets into
//! `<project dir>/<project name> audio/` at save time.
//!
//! Layout on disk: `<project dir>/<name>.slooper`, one file per project.
//! [`list_projects`] enumerates them for the Load window.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::TrackFxSave;

/// Magic value of the `format` field.
pub const FORMAT: &str = "slooper-project";
/// Format version written by this build. v2 (0.3.0) moved the mixer volume
/// to the -inf..=+6 dB MIDI scale (115 = 0 dB, was 127 = 0 dB); v1 files are
/// migrated on load. v3 (0.6.0) adds non-destructive clip trims
/// (`cut`/`play_len`, defaulted when absent) and the configured track count.
pub const VERSION: u32 = 3;
/// File extension (no dot) of project files.
pub const EXT: &str = "slooper";

/// One asset in the table; array index == asset id used by the clips.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjAsset {
    pub name: String,
    /// Stored path (usually absolute; re-resolved by name when missing).
    pub file: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProjClip {
    pub asset: u32,
    /// Timeline start in samples at the engine rate.
    pub pos: u64,
    /// Source samples skipped at the start (left trim; v3, default 0).
    #[serde(default)]
    pub cut: u64,
    /// Played length in samples; null/absent = rest of the source (v3).
    #[serde(default)]
    pub play_len: Option<u64>,
    /// Per-clip gain, linear (v8; absent = 1.0 — never serde-default 0!).
    #[serde(default = "unity_gain")]
    pub gain: f32,
    /// Fade-in / fade-out lengths in samples (v8; absent = 0).
    #[serde(default)]
    pub fade_in: u64,
    #[serde(default)]
    pub fade_out: u64,
}

fn unity_gain() -> f32 {
    1.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjTrack {
    pub vol: u8,
    pub gain: u8,
    pub pan: u8,
    /// Per-track play/stop ("armed to sound while the transport runs").
    pub on: bool,
    #[serde(default)]
    pub fx: Vec<TrackFxSave>,
    /// Full mixer chain (scripts + plugins). Preferred over `fx` when
    /// present; old files (fx only) still load.
    #[serde(default)]
    pub chain: Vec<crate::config::ChainSave>,
    /// Lane 0 = the track itself, lanes 1.. = child tracks.
    pub lanes: Vec<Vec<ProjClip>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProjMetro {
    pub bpm: u32,
    /// Beats per measure (time-signature numerator).
    pub num: u32,
    /// Metronome click subdivision (clicks per beat).
    pub sub: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProjQuantize {
    /// Quantize grid: evenly-spaced steps per beat (SUBDIVISIONS table).
    pub q_sub: u32,
    /// Drag/drop snap grid.
    pub snap_sub: u32,
    /// Auto-quantize fresh recordings.
    pub auto: bool,
    /// Timeline loop toggle.
    pub loop_tl: bool,
}

/// The full `.slooper` document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub format: String,
    pub version: u32,
    pub name: String,
    pub metro: ProjMetro,
    pub quantize: ProjQuantize,
    /// Saved playhead position (samples).
    pub playhead: u64,
    pub tracks: Vec<ProjTrack>,
    pub assets: Vec<ProjAsset>,
    /// Configured number of tracks (v3; absent on older files → default).
    #[serde(default)]
    pub n_tracks: Option<usize>,
    /// Per-track synth stems + slider values (index = track).
    #[serde(default)]
    pub track_synths: Vec<Option<String>>,
    #[serde(default)]
    pub track_synth_params: Vec<Vec<f32>>,
}

impl Project {
    /// Magic + version check (v1 is accepted and migrated by [`load`]).
    fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err("not a slooper project file".into());
        }
        if self.version == 0 || self.version > VERSION {
            return Err(format!("unsupported project version {}", self.version));
        }
        Ok(())
    }
}

/// Strip characters that would be hostile in a file name.
pub fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_control() || c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == ' ').to_string();
    if cleaned.is_empty() {
        "project".into()
    } else {
        cleaned
    }
}

/// `<project dir>/<name>.slooper`.
pub fn project_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.{}", sanitize_name(name), EXT))
}

pub fn save(path: &Path, pr: &Project) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let s = serde_json::to_string_pretty(pr).map_err(|e| e.to_string())?;
    std::fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load(path: &Path) -> Result<Project, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut pr: Project = serde_json::from_str(&s)
        .map_err(|e| format!("{}: invalid project file ({e})", path.display()))?;
    pr.validate()?;
    if pr.version == 1 {
        // v1 volumes were MIDI-linear (127 = 0 dB); convert to the dB scale.
        for t in &mut pr.tracks {
            t.vol = crate::config::vol_midi_from_legacy(t.vol);
        }
    }
    Ok(pr)
}

/// All project files in `dir` (newest first); empty when the dir is missing.
pub fn list_projects(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some(EXT) {
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            out.push((name, mtime, p));
        }
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.1));
    out.into_iter().map(|(n, _, p)| (n, p)).collect()
}

/// Re-resolve an asset file: the stored path if it exists, otherwise the
/// file *name* searched in `extra_dirs` (project audio dir, rack folders).
/// The basename fallback is what keeps a project working when the asset
/// folders moved between sessions.
pub fn resolve_asset(file: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let p = PathBuf::from(file);
    if p.exists() {
        return Some(p);
    }
    let name = p.file_name()?;
    for d in extra_dirs {
        let cand = d.join(name);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Project {
        Project {
            n_tracks: Some(16),
            format: FORMAT.into(),
            version: VERSION,
            name: "demo".into(),
            metro: ProjMetro { bpm: 96, num: 6, sub: 3 },
            quantize: ProjQuantize { q_sub: 4, snap_sub: 2, auto: true, loop_tl: false },
            playhead: 12_345,
            tracks: vec![ProjTrack {
                vol: 100,
                gain: 80,
                pan: 64,
                on: true,
                fx: vec![TrackFxSave { stem: "template".into(), params: vec![1.0] }],
                chain: Vec::new(),
                lanes: vec![vec![ProjClip { asset: 0, pos: 0, cut: 0, play_len: None, gain: 1.0, fade_in: 0, fade_out: 0 }], vec![]],
            }],
            assets: vec![ProjAsset { name: "kick".into(), file: "/x/kick.wav".into() }],
            track_synths: vec![Some("piano".into())],
            track_synth_params: vec![vec![0.5]],
        }
    }

    #[test]
    fn v09_project_roundtrip_keeps_clip_properties() {
        // Old files (no clip-property keys) must load at UNITY gain — a
        // plain serde default of 0.0 would silently mute every legacy clip.
        let json = r#"{"asset": 3, "pos": 10, "cut": 5, "play_len": null}"#;
        let pc: ProjClip = serde_json::from_str(json).unwrap();
        assert_eq!(pc.gain, 1.0);
        assert_eq!((pc.fade_in, pc.fade_out), (0, 0));
        // Round trip keeps the properties.
        let mut pc = pc;
        pc.gain = 0.25;
        pc.fade_in = 4800;
        pc.fade_out = 9600;
        let back: ProjClip = serde_json::from_str(&serde_json::to_string(&pc).unwrap()).unwrap();
        assert_eq!((back.gain, back.fade_in, back.fade_out), (0.25, 4800, 9600));
        // Full document with properties in it still loads.
        let mut p = sample();
        p.tracks[0].lanes[0][0].gain = 0.5;
        p.tracks[0].lanes[0][0].fade_in = 100;
        let s = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tracks[0].lanes[0][0].gain, 0.5);
        assert_eq!(back.tracks[0].lanes[0][0].fade_in, 100);
    }

    #[test]
    fn v1_project_volumes_migrate_on_load() {
        // v1 saved 127 = 0 dB (MIDI-linear); the loader converts to the
        // dB scale where 115 = 0 dB.
        let dir = std::env::temp_dir().join(format!("sp-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("old.slooper");
        std::fs::write(&p, r#"{"format":"slooper-project","version":1,"name":"old",
            "metro":{"bpm":120,"num":4,"sub":3},
            "quantize":{"q_sub":3,"snap_sub":3,"auto":false,"loop_tl":true},
            "playhead":0,
            "tracks":[{"vol":127,"gain":80,"pan":64,"on":true,"fx":[],"lanes":[[],[]]}],
            "n_tracks":null,"assets":[]}"#).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.tracks[0].vol, crate::ui::VOL_UNITY_MIDI, "127 -> 0 dB");
        assert_eq!(loaded.tracks[0].gain, 80, "gain domain unchanged");
        assert_eq!(loaded.tracks[0].pan, 64);
        // Future versions are still refused.
        let bad = p.with_file_name("future.slooper");
        std::fs::write(&bad, r#"{"format":"slooper-project","version":99,"name":"x","metro":{"bpm":120,"num":4,"sub":3},"quantize":{"q_sub":3,"snap_sub":3,"auto":false,"loop_tl":true},"playhead":0,"tracks":[],"n_tracks":null,"assets":[]}"#).unwrap();
        assert!(load(&bad).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_and_magic_check() {
        let dir = std::env::temp_dir().join(format!("sp-proj-{}", std::process::id()));
        let p = project_path(&dir, "my song/2");
        let pr = sample();
        save(&p, &pr).expect("save");
        let back = load(&p).expect("load");
        assert_eq!(back.name, "demo");
        assert_eq!(back.metro.bpm, 96);
        assert!(back.quantize.auto && !back.quantize.loop_tl);
        assert_eq!(back.playhead, 12_345);
        assert_eq!(back.tracks[0].lanes[0][0].pos, 0);
        assert_eq!(back.tracks[0].fx[0].stem, "template");
        assert_eq!(back.assets[0].file, "/x/kick.wav");
        assert_eq!(back.n_tracks, Some(16));
        assert_eq!(back.track_synths, vec![Some("piano".into())]);
        // Wrong magic is rejected with a clear message.
        let bad = dir.join("bad.slooper");
        std::fs::write(&bad, r#"{"format":"nope","version":1}"#).unwrap();
        assert!(load(&bad).is_err());
        // Corrupt JSON is rejected too.
        let worse = dir.join("worse.slooper");
        std::fs::write(&worse, "{").unwrap();
        assert!(load(&worse).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_paths_and_listing() {
        assert_eq!(sanitize_name("my song/2"), "my song-2");
        assert_eq!(sanitize_name("   "), "project");
        assert_eq!(sanitize_name("../../etc/passwd"), "-..-etc-passwd");
        let p = project_path(Path::new("/p"), "two words");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "two words.slooper");

        // list_projects: only .slooper files, and a missing dir is empty.
        let dir = std::env::temp_dir().join(format!("sp-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.slooper"), "{}").unwrap();
        std::fs::write(dir.join("b.txt"), "{}").unwrap();
        let list = list_projects(&dir);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "a");
        assert!(list_projects(&dir.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_resolution_falls_back_to_name() {
        let dir = std::env::temp_dir().join(format!("sp-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kick.wav"), b"RIFF").unwrap();
        // Missing absolute path -> found by name in the extra dir.
        let r = resolve_asset("/gone/kick.wav", std::slice::from_ref(&dir)).expect("found");
        assert_eq!(r, dir.join("kick.wav"));
        assert!(resolve_asset("/gone/nope.wav", std::slice::from_ref(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
