//! Output formats and how each one is encoded with ffmpeg.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Format {
    Flac,
    Mp3,
    Aac,
    Alac,
    Opus,
    Vorbis,
    Wma,
    Wav,
    Aiff,
    WavPack,
    Tta,
}

pub const ALL_FORMATS: [Format; 11] = [
    Format::Flac,
    Format::Mp3,
    Format::Aac,
    Format::Alac,
    Format::Opus,
    Format::Vorbis,
    Format::Wma,
    Format::Wav,
    Format::Aiff,
    Format::WavPack,
    Format::Tta,
];

/// Extensions accepted as converter input (anything ffmpeg can decode).
pub const INPUT_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "m4b", "mp4", "aac", "alac", "opus", "ogg", "oga", "wma", "wav", "wave",
    "aif", "aiff", "aifc", "wv", "ape", "tta", "mpc", "dsf", "dff", "mka", "webm", "ac3", "dts",
    "mp2", "caf", "w64", "tak", "spx", "shn",
];

pub fn is_audio_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| INPUT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// How the quality value of an output is interpreted.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum QualityMode {
    /// Lossless compression level (FLAC 0-8, WavPack 0-3).
    Level,
    /// VBR quality scale (MP3 V0-V9 where lower is better, Vorbis -1..10).
    Vbr,
    /// Target bitrate in kbps.
    Bitrate,
    /// Nothing to set (WAV, AIFF, ALAC, TTA).
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputOptions {
    pub mode: QualityMode,
    pub value: i32,
    /// Resample to this rate (Hz); 0 = keep source.
    #[serde(default)]
    pub sample_rate: u32,
    /// Bit depth for lossless/PCM outputs; 0 = keep source.
    #[serde(default)]
    pub bit_depth: u8,
    /// Downmix multichannel to stereo.
    #[serde(default)]
    pub stereo: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self::default_for(Format::Flac)
    }
}

impl OutputOptions {
    pub fn default_for(f: Format) -> Self {
        let (mode, value) = match f {
            Format::Flac => (QualityMode::Level, 8),
            Format::WavPack => (QualityMode::Level, 2),
            Format::Mp3 => (QualityMode::Vbr, 0),
            Format::Vorbis => (QualityMode::Vbr, 6),
            Format::Aac => (QualityMode::Bitrate, 256),
            Format::Opus => (QualityMode::Bitrate, 160),
            Format::Wma => (QualityMode::Bitrate, 192),
            Format::Alac | Format::Wav | Format::Aiff | Format::Tta => (QualityMode::None, 0),
        };
        Self { mode, value, sample_rate: 0, bit_depth: 0, stereo: false }
    }

    pub fn describe(&self, f: Format) -> String {
        match (f, self.mode) {
            (Format::Mp3, QualityMode::Vbr) => format!("VBR V{}", self.value),
            (_, QualityMode::Vbr) => format!("VBR q{}", self.value),
            (_, QualityMode::Bitrate) => format!("{} kbps", self.value),
            (_, QualityMode::Level) => format!("level {}", self.value),
            (_, QualityMode::None) => "lossless".into(),
        }
    }
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Flac => "FLAC",
            Format::Mp3 => "MP3",
            Format::Aac => "AAC (M4A)",
            Format::Alac => "Apple Lossless (ALAC)",
            Format::Opus => "Opus",
            Format::Vorbis => "Ogg Vorbis",
            Format::Wma => "WMA",
            Format::Wav => "WAV",
            Format::Aiff => "AIFF",
            Format::WavPack => "WavPack",
            Format::Tta => "TTA",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        ALL_FORMATS.iter().copied().find(|f| f.label() == s)
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::Aac | Format::Alac => "m4a",
            Format::Opus => "opus",
            Format::Vorbis => "ogg",
            Format::Wma => "wma",
            Format::Wav => "wav",
            Format::Aiff => "aiff",
            Format::WavPack => "wv",
            Format::Tta => "tta",
        }
    }

    pub fn is_lossless(self) -> bool {
        matches!(
            self,
            Format::Flac | Format::Alac | Format::Wav | Format::Aiff | Format::WavPack | Format::Tta
        )
    }

    /// Whether `lofty` can write tags and artwork to this container.
    pub fn lofty_taggable(self) -> bool {
        !matches!(self, Format::Wma | Format::Tta)
    }

    /// Modes that make sense for the format, first one is the default.
    pub fn quality_modes(self) -> &'static [QualityMode] {
        match self {
            Format::Flac | Format::WavPack => &[QualityMode::Level],
            Format::Mp3 => &[QualityMode::Vbr, QualityMode::Bitrate],
            Format::Vorbis => &[QualityMode::Vbr, QualityMode::Bitrate],
            Format::Aac | Format::Opus | Format::Wma => &[QualityMode::Bitrate],
            _ => &[QualityMode::None],
        }
    }

    /// Opus is always 48 kHz and lossy formats ignore bit depth.
    fn supports_bit_depth(self) -> bool {
        matches!(self, Format::Flac | Format::Alac | Format::Wav | Format::Aiff | Format::WavPack | Format::Tta)
    }

    /// ffmpeg arguments for the audio encoder (placed after `-i`).
    pub fn ffmpeg_args(self, o: &OutputOptions, ffmpeg: &str) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        let mut push = |s: &str| a.push(s.to_string());
        let enc = encoders(ffmpeg);
        match self {
            Format::Flac => {
                push("-c:a");
                push("flac");
                push("-compression_level");
                push(&o.value.clamp(0, 12).to_string());
            }
            Format::Mp3 => {
                push("-c:a");
                push("libmp3lame");
                if o.mode == QualityMode::Bitrate {
                    push("-b:a");
                    push(&format!("{}k", o.value.clamp(32, 320)));
                } else {
                    push("-q:a");
                    push(&o.value.clamp(0, 9).to_string());
                }
                push("-id3v2_version");
                push("3");
            }
            Format::Aac => {
                push("-c:a");
                if enc.contains("libfdk_aac") {
                    push("libfdk_aac");
                } else {
                    push("aac");
                }
                push("-b:a");
                push(&format!("{}k", o.value.clamp(32, 512)));
                push("-movflags");
                push("+faststart");
            }
            Format::Alac => {
                push("-c:a");
                push("alac");
            }
            Format::Opus => {
                push("-c:a");
                push(if enc.contains("libopus") { "libopus" } else { "opus" });
                push("-b:a");
                push(&format!("{}k", o.value.clamp(6, 510)));
                if !enc.contains("libopus") {
                    push("-strict");
                    push("-2");
                }
            }
            Format::Vorbis => {
                push("-c:a");
                push("libvorbis");
                if o.mode == QualityMode::Bitrate {
                    push("-b:a");
                    push(&format!("{}k", o.value.clamp(45, 500)));
                } else {
                    push("-q:a");
                    push(&o.value.clamp(-1, 10).to_string());
                }
            }
            Format::Wma => {
                push("-c:a");
                push("wmav2");
                push("-b:a");
                push(&format!("{}k", o.value.clamp(32, 320)));
            }
            Format::Wav | Format::Aiff => {
                // bit depth is always concrete here (see encoder::effective_options)
                let le = self == Format::Wav;
                let codec = match o.bit_depth {
                    24 => if le { "pcm_s24le" } else { "pcm_s24be" },
                    32 => if le { "pcm_s32le" } else { "pcm_s32be" },
                    _ => if le { "pcm_s16le" } else { "pcm_s16be" },
                };
                push("-c:a");
                push(codec);
                if le {
                    // files over 4 GB need RF64
                    push("-rf64");
                    push("auto");
                }
            }
            Format::WavPack => {
                push("-c:a");
                push("wavpack");
                push("-compression_level");
                push(&o.value.clamp(0, 8).to_string());
            }
            Format::Tta => {
                push("-c:a");
                push("tta");
            }
        }
        if o.sample_rate > 0 && self != Format::Opus {
            push("-ar");
            push(&o.sample_rate.to_string());
        }
        if o.stereo {
            push("-ac");
            push("2");
        }
        if o.bit_depth > 0 && self.supports_bit_depth() && !matches!(self, Format::Wav | Format::Aiff) {
            let planar = matches!(self, Format::Alac | Format::WavPack);
            let fmt = match (o.bit_depth, planar) {
                (16, false) => "s16",
                (16, true) => "s16p",
                (_, false) => "s32",
                (_, true) => "s32p",
            };
            push("-sample_fmt");
            push(fmt);
            if o.bit_depth == 24 && matches!(self, Format::Flac | Format::Alac) {
                push("-bits_per_raw_sample");
                push("24");
            }
        }
        a
    }
}

/// Encoders available in the local ffmpeg build (cached).
pub fn encoders(ffmpeg: &str) -> &'static str {
    static ENC: OnceLock<String> = OnceLock::new();
    ENC.get_or_init(|| {
        std::process::Command::new(ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    })
}

/// Formats whose encoder is missing from the local ffmpeg build.
pub fn missing_encoders(ffmpeg: &str) -> Vec<Format> {
    let enc = encoders(ffmpeg);
    if enc.is_empty() {
        return ALL_FORMATS.to_vec();
    }
    ALL_FORMATS
        .iter()
        .copied()
        .filter(|f| {
            let needed: &[&str] = match f {
                Format::Mp3 => &["libmp3lame"],
                Format::Vorbis => &["libvorbis"],
                Format::Opus => &["libopus", " opus "],
                Format::Wma => &["wmav2"],
                Format::WavPack => &["wavpack"],
                _ => &[],
            };
            !needed.is_empty() && !needed.iter().any(|n| enc.contains(n))
        })
        .collect()
}
