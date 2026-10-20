//! Bundled TR-808 starter pack.
//!
//! Fourteen one-shots sampled directly from a real Roland TR-808 Rhythm
//! Composer (serial 103852) by Michael Fischer / Technopolis in 1994 — the
//! classic "TR808WAV" set published on machines.hyperreal.org with **no
//! licensing restrictions** ("ABSOLUTELY FREE"), redistributed here via the
//! ISC-licensed `@fluid-music/tr-808` npm repackaging (see
//! `samples/808/CREDITS.md` for the full chain).
//!
//! The WAVs are embedded in the binary (`include_bytes!`) so every install —
//! package, desktop copy, or `cargo run` — ships them. On startup the app
//! seeds them into `$XDG_DATA_HOME/simplooper/samples/808` (default
//! `~/.local/share/...`) — files are (re)written whenever the on-disk copy is
//! missing or a different size, so a deleted file comes back but a live
//! install never fights itself. The seeded folder becomes the asset-rack
//! default when the user has configured no rack folders, which is what makes
//! the pack the app's *default samples*: the first folder, so takes record
//! into it and every 808 voice is one drag away.

use std::path::{Path, PathBuf};

/// Folder name (under the app data dir) the pack is seeded into.
pub const PACK_DIRNAME: &str = "808";

/// Every pack file: (file name, embedded 16-bit / 44.1 kHz / mono WAV).
pub const FILES: &[(&str, &[u8])] = &[
    ("808-kick.wav", include_bytes!("../samples/808/808-kick.wav")),
    ("808-sub.wav", include_bytes!("../samples/808/808-sub.wav")),
    ("808-snare.wav", include_bytes!("../samples/808/808-snare.wav")),
    ("808-snare-long.wav", include_bytes!("../samples/808/808-snare-long.wav")),
    ("808-clap.wav", include_bytes!("../samples/808/808-clap.wav")),
    ("808-hat-closed.wav", include_bytes!("../samples/808/808-hat-closed.wav")),
    ("808-hat-open.wav", include_bytes!("../samples/808/808-hat-open.wav")),
    ("808-crash.wav", include_bytes!("../samples/808/808-crash.wav")),
    ("808-cowbell.wav", include_bytes!("../samples/808/808-cowbell.wav")),
    ("808-tom-low.wav", include_bytes!("../samples/808/808-tom-low.wav")),
    ("808-tom-mid.wav", include_bytes!("../samples/808/808-tom-mid.wav")),
    ("808-tom-high.wav", include_bytes!("../samples/808/808-tom-high.wav")),
    ("808-rimshot.wav", include_bytes!("../samples/808/808-rimshot.wav")),
    ("808-clave.wav", include_bytes!("../samples/808/808-clave.wav")),
];

/// `$XDG_DATA_HOME/simplooper` (default `~/.local/share/simplooper`).
pub fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local")
                .join("share")
        });
    base.join("simplooper")
}

/// The folder the pack is seeded into.
pub fn pack_dir() -> PathBuf {
    data_dir().join("samples").join(PACK_DIRNAME)
}

/// True when `path` is the seeded pack folder (compared trimmed/absolute).
pub fn is_pack_dir(path: &str) -> bool {
    let p = Path::new(path.trim());
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(p)
    } else {
        p.to_path_buf()
    };
    // Lexically normalise "./" and "../" segments (the folder string may
    // come from a config file written with a slightly different form).
    let norm = |q: &Path| -> PathBuf {
        let mut out = PathBuf::new();
        for c in q.components() {
            match c {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    };
    norm(&abs) == norm(&pack_dir())
}

/// Seed the pack: create the folder and (re)write every file that is missing
/// or has a different size than the embedded copy. Returns the folder path;
/// unwritable files are skipped silently (the rack still lists what exists).
pub fn seed_pack() -> PathBuf {
    let dir = pack_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return dir;
    }
    for (name, data) in FILES {
        let p = dir.join(name);
        let fresh = match std::fs::metadata(&p) {
            Ok(m) => m.len() as usize != data.len(),
            Err(_) => true,
        };
        if fresh {
            let _ = std::fs::write(&p, data);
        }
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_pack_is_valid_wav_audio() {
        assert_eq!(FILES.len(), 14);
        for (name, data) in FILES {
            assert!(name.starts_with("808-") && name.ends_with(".wav"), "{name}");
            assert!(data.len() > 44, "{name}: truncated");
            assert_eq!(&data[0..4], b"RIFF", "{name}: not a RIFF file");
            assert_eq!(&data[8..12], b"WAVE", "{name}: not a WAVE file");
            // Every entry distinct (no accidentally duplicated asset).
            assert_eq!(
                FILES.iter().filter(|(_, d)| *d == *data).count(),
                1,
                "{name}: duplicate content"
            );
        }
    }

    #[test]
    fn pack_dir_follows_xdg_and_is_comparable() {
        let dir = pack_dir();
        assert!(dir.ends_with("samples/808"));
        let s = dir.to_string_lossy().into_owned();
        assert!(is_pack_dir(&s));
        assert!(is_pack_dir(&format!("{s}/")));
        assert!(!is_pack_dir("/somewhere/else"));
    }

    #[test]
    fn seeds_into_a_scratch_xdg_home() {
        // Redirect XDG so the test never touches the real data dir.
        let base = std::env::temp_dir().join(format!("sp-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_DATA_HOME", &base);
        let dir = seed_pack();
        assert_eq!(dir, base.join("simplooper").join("samples").join("808"));
        for (name, data) in FILES {
            let p = dir.join(name);
            let on = std::fs::read(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(on.len(), data.len());
        }
        // Idempotent re-seed keeps files (sizes match → no rewrite churn).
        seed_pack();
        // A deleted file comes back.
        std::fs::remove_file(dir.join("808-kick.wav")).unwrap();
        seed_pack();
        assert!(dir.join("808-kick.wav").exists());
        let _ = std::fs::remove_dir_all(&base);
        std::env::remove_var("XDG_DATA_HOME");
    }
}
