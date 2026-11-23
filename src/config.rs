//! Persistent settings and encoding presets, stored as TOML in
//! `$XDG_CONFIG_HOME/coda/config.toml`.

use crate::formats::{Format, OutputOptions};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ExistingFile {
    #[default]
    Skip,
    Overwrite,
    Rename,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Rip once; if AccurateRip confirms the track it is done, otherwise
    /// re-read and compare every frame (dBpoweramp style "secure, AR first").
    #[default]
    AccurateRipThenSecure,
    /// Always perform a full second verification pass.
    AlwaysSecure,
    /// Single burst read with AccurateRip check only.
    Burst,
}

/// One destination of a preset: a format + quality + where it goes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    pub enabled: bool,
    pub format: Format,
    #[serde(default)]
    pub options: OutputOptions,
    /// Root folder, `~` is expanded.
    pub dest_root: String,
    /// Naming template, without extension. See `naming.rs`.
    pub template: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub outputs: Vec<OutputSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Names of the accent colour schemes, in the order the UI defines them.
pub const SCHEMES: &[&str] = &["Aurora", "Ocean", "Emerald", "Sunset", "Rose", "Amber", "Graphite", "Nord"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme_mode: ThemeMode,
    /// Index into `SCHEMES`.
    pub theme_scheme: usize,
    pub ffmpeg: String,
    pub ffprobe: String,
    /// 0 = use every logical core.
    pub threads: usize,
    pub cd_device: String,
    /// Read offsets in samples, keyed by "VENDOR|MODEL".
    pub drive_offsets: BTreeMap<String, i32>,
    pub verify_mode: VerifyMode,
    /// How many times a mismatching frame may be re-read.
    pub max_retries: u32,
    /// Identical reads of a frame required before it is accepted.
    pub required_matches: u32,
    pub use_c2: bool,
    pub eject_after_rip: bool,
    pub write_log: bool,
    pub write_cue: bool,
    pub musicbrainz: bool,
    pub gnudb_fallback: bool,
    pub discogs_fallback: bool,
    /// Optional Discogs personal access token (discogs.com/settings/developers).
    pub discogs_token: String,
    pub contact_email: String,
    /// Artwork larger than this (longest side, px) is downscaled. 0 = never.
    pub art_max_px: u32,
    /// Artwork smaller than this triggers an attempt at a better source.
    pub art_min_px: u32,
    pub save_cover_file: bool,
    pub cover_file_name: String,
    pub portable_filenames: bool,
    pub existing: ExistingFile,
    pub verify_lossless: bool,
    pub last_rip_preset: String,
    pub last_convert_preset: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::System,
            theme_scheme: 0,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            threads: 0,
            cd_device: "/dev/sr0".into(),
            drive_offsets: BTreeMap::new(),
            verify_mode: VerifyMode::default(),
            max_retries: 20,
            required_matches: 2,
            use_c2: false,
            eject_after_rip: true,
            write_log: true,
            write_cue: true,
            musicbrainz: true,
            gnudb_fallback: true,
            discogs_fallback: true,
            discogs_token: String::new(),
            contact_email: String::new(),
            art_max_px: 3000,
            art_min_px: 1000,
            save_cover_file: true,
            cover_file_name: "cover.jpg".into(),
            portable_filenames: true,
            existing: ExistingFile::Skip,
            verify_lossless: true,
            last_rip_preset: String::new(),
            last_convert_preset: String::new(),
        }
    }
}

impl Settings {
    pub fn worker_threads(&self) -> usize {
        if self.threads == 0 {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        } else {
            self.threads
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub settings: Settings,
    pub presets: Vec<Preset>,
}

impl Default for Config {
    fn default() -> Self {
        Self { settings: Settings::default(), presets: default_presets() }
    }
}

pub const TAG_TEMPLATE: &str = "{albumartist}/{album}[ (Disc {disc})]/[{track} - ]{title}";

fn out(format: Format, dest: &str) -> OutputSpec {
    OutputSpec {
        enabled: true,
        format,
        options: OutputOptions::default_for(format),
        dest_root: dest.into(),
        template: TAG_TEMPLATE.into(),
    }
}

pub fn default_presets() -> Vec<Preset> {
    vec![
        Preset { name: "FLAC (archive)".into(), outputs: vec![out(Format::Flac, "~/Music/FLAC")] },
        Preset {
            name: "FLAC + MP3 V0 (portable copy)".into(),
            outputs: vec![out(Format::Flac, "~/Music/FLAC"), out(Format::Mp3, "~/Music/MP3")],
        },
        Preset {
            name: "FLAC + AAC 256 (Apple devices)".into(),
            outputs: vec![out(Format::Flac, "~/Music/FLAC"), out(Format::Aac, "~/Music/AAC")],
        },
        Preset { name: "MP3 V0".into(), outputs: vec![out(Format::Mp3, "~/Music/MP3")] },
        Preset { name: "Opus 160 kbps".into(), outputs: vec![out(Format::Opus, "~/Music/Opus")] },
        Preset { name: "Apple Lossless (ALAC)".into(), outputs: vec![out(Format::Alac, "~/Music/ALAC")] },
    ]
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("coda")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("coda")
}

pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else if p == "~" {
        dirs::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(p)
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("config.toml is invalid ({e}); using defaults");
                Self::default()
            }),
            Err(_) => {
                // first run: write the defaults so users can find and edit them
                let c = Self::default();
                let _ = c.save();
                c
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(config_dir())?;
        let tmp = Self::path().with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(tmp, Self::path())?;
        Ok(())
    }

    pub fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.name == name)
    }
}
