//! Asset model + rack folder scanning.
//!
//! An *asset* is a decoded mono audio buffer at the engine sample rate plus
//! the file it came from. The GUI owns the mutable list (drag-and-drop, the
//! asset rack and recordings all flow through it) and pushes
//! `Arc<Vec<Arc<[f32]>>>` snapshots (indexed by asset id) into the audio
//! engine — the realtime path only ever reads those Arcs.
//!
//! The rack scans *rack folders* (the first entry is the primary folder that
//! recordings are written to). Scanning is cheap — it only lists candidate
//! files; decoding happens once per file the first time it is needed and the
//! data is then cached in the asset list.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::audio_io::IMPORT_EXTENSIONS;

/// One decoded audio asset. `path` is where it is stored (or will be stored:
/// a fresh recording with no folder yet has an empty path and `dirty = true`).
pub struct Asset {
    pub id: u32,
    /// Display name (file stem).
    pub name: String,
    pub path: PathBuf,
    pub data: Arc<[f32]>,
    /// In-memory content no longer matches `path` (trim/derive): the next
    /// save writes it out before anything references the old file.
    pub dirty: bool,
}

impl Asset {
    pub fn samples(&self) -> u64 {
        self.data.len() as u64
    }
}

/// A decoded sample buffer shared between GUI and engine.
pub type AssetData = Arc<[f32]>;

/// One file offered by a rack folder.
#[derive(Clone, Debug, PartialEq)]
pub struct RackFile {
    pub name: String,
    pub path: PathBuf,
}

/// True when a file's extension is a format the import pipeline decodes.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMPORT_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// List the audio files directly inside `dir` (sorted by name, case-insensitive).
/// Missing/unreadable directories are an error the rack shows inline instead
/// of silently disappearing — the folder may simply not exist right now.
pub fn scan_folder(dir: &Path) -> Result<Vec<RackFile>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut files: Vec<RackFile> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_file() && is_audio_file(&p) {
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            files.push(RackFile { name, path: p });
        }
    }
    files.sort_by_key(|f| f.name.to_lowercase());
    Ok(files)
}

/// A rack folder entry: the path plus its last scan result.
pub struct RackFolder {
    pub path: String,
    pub files: Vec<RackFile>,
    /// Populated when the folder could not be read.
    pub err: Option<String>,
}

impl RackFolder {
    pub fn new(path: impl Into<String>) -> Self {
        RackFolder { path: path.into(), files: Vec::new(), err: None }
    }

    /// Re-scan this folder in place.
    pub fn rescan(&mut self) {
        let p = self.path.trim();
        if p.is_empty() {
            self.files.clear();
            self.err = None;
            return;
        }
        match scan_folder(Path::new(p)) {
            Ok(f) => {
                self.files = f;
                self.err = None;
            }
            Err(e) => {
                self.files.clear();
                self.err = Some(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_filter() {
        assert!(is_audio_file(Path::new("/x/y.WAV")));
        assert!(is_audio_file(Path::new("/x/y.flac")));
        assert!(!is_audio_file(Path::new("/x/y.rs")));
        assert!(!is_audio_file(Path::new("/x/noext")));
    }

    #[test]
    fn scan_sorts_filters_and_errors() {
        let dir = std::env::temp_dir().join(format!("sp-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["beta.wav", "Alpha.mp3", "notes.txt", "sub"] {
            let p = dir.join(name);
            if name == "sub" {
                std::fs::create_dir(&p).unwrap();
            } else {
                std::fs::write(&p, b"x").unwrap();
            }
        }
        let files = scan_folder(&dir).unwrap();
        assert_eq!(
            files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "beta"],
            "only audio files, sorted case-insensitively"
        );
        // A missing folder is an inline error, not a panic.
        let missing = dir.join("nope");
        assert!(scan_folder(&missing).is_err());
        let mut rf = RackFolder::new(dir.join("nope").display().to_string());
        rf.rescan();
        assert!(rf.err.is_some());
        rf.path = dir.display().to_string();
        rf.rescan();
        assert!(rf.err.is_none() && rf.files.len() == 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
