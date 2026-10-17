//! Audio file import/export for loop recordings.
//!
//! Import decodes the formats listed in [`IMPORT_EXTENSIONS`] with symphonia
//! (WAV, MP3, OGG, FLAC, M4A/AAC, AIFF), downmixes to mono and resamples to
//! the engine rate with rubato. Export writes WAV and FLAC natively; MP3/OGG
//! are encoded through a system `ffmpeg` when one is installed.

use std::fs::File;
use std::path::Path;

use rubato::{
    audioadapter::Adapter, audioadapter_buffers::direct::SequentialSliceOfVecs, Fft, FixedSync,
    Resampler,
};
use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// File extensions offered in the import dialog — exactly the formats the
/// enabled symphonia decoders handle (see Cargo.toml features). The dialog
/// filter and the probe hint both come from this list, so a format is never
/// offered unless it can actually be decoded.
pub const IMPORT_EXTENSIONS: &[&str] =
    &["wav", "mp3", "ogg", "oga", "flac", "m4a", "aac", "aiff", "aif"];

/// Export formats. WAV/FLAC are pure-Rust; MP3/OGG need a system ffmpeg.
#[derive(Clone, Copy, PartialEq)]
pub enum ExportFormat {
    Wav,
    Flac,
    Mp3,
    Ogg,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 4] = [Self::Wav, Self::Flac, Self::Mp3, Self::Ogg];

    pub fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV (PCM 16-bit)",
            Self::Flac => "FLAC (lossless)",
            Self::Mp3 => "MP3 (via ffmpeg)",
            Self::Ogg => "OGG Vorbis (via ffmpeg)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
        }
    }

    pub fn needs_ffmpeg(self) -> bool {
        matches!(self, Self::Mp3 | Self::Ogg)
    }
}

/// Decode `path` to mono f32 samples at `target_rate` (resampled if needed).
pub fn decode_to_mono(path: &Path, target_rate: u32) -> Result<Vec<f32>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Hint the format registry with the file extension.
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("unsupported or corrupt audio file: {e}"))?;
    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| "file has no audio track".to_string())?;
    let src_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| "file has no sample rate".to_string())?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("unsupported codec: {e}"))?;
    let track_id = track.id;

    let mut mono: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        };
        // Convert this packet's planar samples to f32 and downmix to mono.
        let mut dest: AudioBuffer<f32> = buf.make_equivalent::<f32>();
        buf.convert(&mut dest);
        let n_chans = dest.spec().channels.count();
        let frames = dest.frames() as usize;
        // Hoist the channel plane slices out of the frame loop and turn the
        // per-sample divide into one reciprocal multiply.
        let chans: Vec<&[f32]> = (0..n_chans).map(|c| dest.chan(c)).collect();
        let inv = 1.0 / n_chans as f32;
        mono.reserve(mono.len() + frames);
        for f in 0..frames {
            let mut acc = 0.0f32;
            for ch in &chans {
                acc += ch[f];
            }
            mono.push(acc * inv);
        }
    }

    if mono.is_empty() {
        return Err("no audio samples decoded".into());
    }
    resample_to(&mono, src_rate, target_rate)
}

/// Resample `src` (at `src_rate`) to `dst_rate`; identity when already equal.
fn resample_to(src: &[f32], src_rate: u32, dst_rate: u32) -> Result<Vec<f32>, String> {
    if src_rate == dst_rate || src.is_empty() {
        return Ok(src.to_vec());
    }
    let mut resampler = Fft::<f32>::new(
        src_rate as usize,
        dst_rate as usize,
        1024,
        1,
        FixedSync::Input,
    )
    .map_err(|e| format!("resample setup: {e:?}"))?;
    let input_data = vec![src.to_vec()];
    let input = SequentialSliceOfVecs::new(&input_data, 1, src.len())
        .map_err(|e| format!("resample input: {e:?}"))?;
    let output = resampler
        .process_all(&input, src.len(), None)
        .map_err(|e| format!("resample: {e:?}"))?;
    let mut out = Vec::with_capacity(output.frames());
    for f in 0..output.frames() {
        out.push(output.read_sample(0, f).unwrap_or(0.0));
    }
    Ok(out)
}

/// Export mono f32 samples (engine rate) to `path` in the chosen format.
pub fn export(path: &Path, rate: u32, samples: &[f32], fmt: ExportFormat) -> Result<(), String> {
    if samples.is_empty() {
        return Err("nothing to export (no recorded audio)".into());
    }
    match fmt {
        ExportFormat::Wav => write_wav(path, rate, samples),
        ExportFormat::Flac => write_flac(path, rate, samples),
        ExportFormat::Mp3 | ExportFormat::Ogg => encode_with_ffmpeg(path, rate, samples, fmt),
    }
}

fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Write a canonical PCM-16 mono WAV.
fn write_wav(path: &Path, rate: u32, samples: &[f32]) -> Result<(), String> {
    let pcm = to_i16(samples);
    let data_len = (pcm.len() * 2) as u32;
    // WAV header: RIFF chunk with a single mono PCM fmt sub-chunk.
    let mut buf: Vec<u8> = Vec::with_capacity(44 + data_len as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36u32 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate (mono, 16-bit)
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    for s in &pcm {
        buf.extend_from_slice(&(*s as u16).to_le_bytes());
    }
    std::fs::write(path, &buf).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// Write a native FLAC stream (16-bit mono) via rusty_flac.
fn write_flac(path: &Path, rate: u32, samples: &[f32]) -> Result<(), String> {
    let pcm = to_i16(samples);
    let plane: Vec<i32> = pcm.iter().map(|&s| s as i32).collect();
    let mut enc = rusty_flac::Encoder::new(rate, 1, 16)
        .map_err(|e| format!("flac encoder init: {e:?}"))?;
    enc.push_planar(&[&plane])
        .map_err(|e| format!("flac encode: {e:?}"))?;
    let bytes = enc.finish();
    std::fs::write(path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// MP3/OGG export via a system ffmpeg (WAV on disk → encoded file).
fn encode_with_ffmpeg(
    path: &Path,
    rate: u32,
    samples: &[f32],
    fmt: ExportFormat,
) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let tmp = path.with_extension("simplooper-tmp.wav");
    write_wav(&tmp, rate, samples)?;
    let codec = match fmt {
        ExportFormat::Mp3 => "libmp3lame",
        _ => "libvorbis",
    };
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(&tmp)
        .args(["-codec:a", codec, "-vn"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status();
    let _ = std::fs::remove_file(&tmp);
    match status {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(format!("ffmpeg exited with {st}")),
        Err(e) => Err(format!(
            "MP3/OGG export needs ffmpeg on PATH, but it could not be run: {e}"
        )),
    }
}
