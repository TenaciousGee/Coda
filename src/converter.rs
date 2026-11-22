//! Batch conversion: expand files/folders, plan one job per (file, output), run on the pool.

use crate::config::{expand_home, Preset, Settings};
use crate::encoder::EncodeJob;
use crate::formats::is_audio_file;
use crate::naming::{destination, NameContext};
use crate::tags;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamingMode {
    /// Use each output's destination folder + tag template.
    Preset,
    /// Output folder + the source's folder structure and file name.
    Mirror,
    /// Write next to the source file.
    BesideSource,
}

#[derive(Debug, Clone)]
pub struct SourceItem {
    pub path: PathBuf,
    /// Directory relative to the folder the user added ("" for loose files).
    pub reldir: String,
}

fn walk(dir: &Path, root: &Path, recursive: bool, out: &mut Vec<SourceItem>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if recursive {
                walk(&p, root, recursive, out);
            }
        } else if is_audio_file(&p) {
            let reldir = p
                .parent()
                .and_then(|d| d.strip_prefix(root).ok())
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(SourceItem { path: p, reldir });
        }
    }
}

/// Expand files and folders. A folder's own name becomes the top of `reldir`
/// so mirrored output keeps it (e.g. adding `~/Music/Rock` gives `Rock/...`).
pub fn scan(inputs: &[PathBuf], recursive: bool) -> Vec<SourceItem> {
    let mut out = Vec::new();
    for i in inputs {
        if i.is_dir() {
            let root = i.parent().unwrap_or(i);
            walk(i, root, recursive, &mut out);
        } else if i.is_file() && is_audio_file(i) {
            out.push(SourceItem { path: i.clone(), reldir: String::new() });
        }
    }
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Plan every encode for one source file.
pub fn plan(item: &SourceItem, preset: &Preset, mode: NamingMode, settings: &Settings) -> Vec<EncodeJob> {
    let mut meta = tags::read(&item.path);
    if meta.title.is_empty() && meta.artist.is_empty() {
        tags::read_ffmpeg(&settings.ffmpeg, &item.path, &mut meta);
    }
    if meta.title.is_empty() {
        meta.title = item.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    if meta.cover.is_none() {
        meta.cover = extract_cover(&settings.ffmpeg, &item.path).map(std::sync::Arc::new);
    }
    let stem = item.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    preset
        .outputs
        .iter()
        .filter(|o| o.enabled)
        .map(|o| {
            let ctx = NameContext { meta: &meta, filename: &stem, reldir: &item.reldir, format: o.format.label() };
            let ext = o.format.extension();
            let output = match mode {
                NamingMode::Preset => destination(&expand_home(&o.dest_root), &o.template, &ctx, ext, settings.portable_filenames),
                NamingMode::Mirror => destination(&expand_home(&o.dest_root), "{reldir}/{filename}", &ctx, ext, settings.portable_filenames),
                NamingMode::BesideSource => {
                    let dir = item.path.parent().unwrap_or(Path::new("."));
                    dir.join(format!("{stem}.{ext}"))
                }
            };
            EncodeJob { input: item.path.clone(), output, format: o.format, options: o.options.clone(), meta: meta.clone(), temp_input: None }
        })
        .collect()
}

/// Attached picture via ffmpeg, for containers lofty can't read pictures from
/// (WMA, TTA, APE-tagged WavPack/Monkey's Audio).
fn extract_cover(ffmpeg: &str, path: &Path) -> Option<Vec<u8>> {
    let out = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-an", "-map", "0:v:0?", "-c:v", "copy", "-frames:v", "1", "-f", "image2pipe", "-"])
        .output()
        .ok()?;
    let b = out.stdout;
    (b.len() > 100 && (b.starts_with(&[0xFF, 0xD8]) || b.starts_with(&[0x89, b'P', b'N', b'G']))).then_some(b)
}
