//! Output path templates.
//!
//! Variables: `{artist} {albumartist} {album} {title} {track} {tracktotal}
//! {disc} {disctotal} {year} {genre} {composer} {filename} {reldir} {format}`.
//! A `[...]` group is dropped entirely when any variable inside it is empty,
//! e.g. `{album}[ (Disc {disc})]` only shows the disc on multi-disc sets.
//! `{albumartist}` falls back to `{artist}` and `{disc}` is empty on single-disc releases.

use crate::tags::TrackMeta;
use std::path::{Path, PathBuf};

pub struct NameContext<'a> {
    pub meta: &'a TrackMeta,
    /// Source file stem (converter) or empty (ripper).
    pub filename: &'a str,
    /// Directory of the source relative to the folder the user added.
    pub reldir: &'a str,
    pub format: &'a str,
}

fn value(ctx: &NameContext, key: &str, portable: bool) -> Option<String> {
    let m = ctx.meta;
    let v = match key {
        "artist" => m.artist.clone(),
        "albumartist" => if m.album_artist.is_empty() { m.artist.clone() } else { m.album_artist.clone() },
        "album" => m.album.clone(),
        "title" => m.title.clone(),
        "track" => if m.track > 0 { format!("{:02}", m.track) } else { String::new() },
        "tracktotal" => if m.track_total > 0 { format!("{:02}", m.track_total) } else { String::new() },
        "disc" => if m.disc > 0 && m.disc_total > 1 { m.disc.to_string() } else { String::new() },
        "disctotal" => if m.disc_total > 1 { m.disc_total.to_string() } else { String::new() },
        "year" => m.year_only(),
        "genre" => m.genre.clone(),
        "composer" => m.composer.clone(),
        "filename" => ctx.filename.to_string(),
        // reldir is already a path; keep its separators
        "reldir" => return Some(ctx.reldir.to_string()),
        "format" => ctx.format.to_string(),
        _ => return None,
    };
    Some(sanitize_component(&v, portable))
}

/// Make a single path component safe: no separators or control chars, and
/// when `portable` also none of the characters FAT/exFAT/NTFS reject.
pub fn sanitize_component(s: &str, portable: bool) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '/' | '\0' => '-',
            c if c.is_control() => ' ',
            '<' | '>' | '|' | '*' if portable => '_',
            ':' if portable => '-',
            '"' if portable => '\'',
            '?' | '\\' if portable => '_',
            c => c,
        })
        .collect();
    if portable {
        while out.ends_with('.') || out.ends_with(' ') {
            out.pop();
        }
    }
    let t = out.trim();
    if t == "." || t == ".." { "_".into() } else { t.to_string() }
}

fn expand_vars(seg: &str, ctx: &NameContext, portable: bool) -> (String, bool) {
    // returns (expanded, any_var_empty)
    let mut out = String::new();
    let mut empty = false;
    let mut rest = seg;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let key = &after[..end];
            match value(ctx, &key.to_ascii_lowercase(), portable) {
                Some(v) => {
                    if v.is_empty() {
                        empty = true;
                    }
                    out.push_str(&v);
                }
                None => out.push_str(&rest[start..start + end + 2]),
            }
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    (out, empty)
}

/// Render a template into a relative path (without extension).
pub fn render(template: &str, ctx: &NameContext, portable: bool) -> PathBuf {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('[') {
        out.push_str(&expand_vars(&rest[..open], ctx, portable).0);
        let after = &rest[open + 1..];
        if let Some(close) = after.find(']') {
            let (s, empty) = expand_vars(&after[..close], ctx, portable);
            if !empty {
                out.push_str(&s);
            }
            rest = &after[close + 1..];
        } else {
            out.push_str(&expand_vars(&rest[open..], ctx, portable).0);
            rest = "";
        }
    }
    out.push_str(&expand_vars(rest, ctx, portable).0);

    let mut p = PathBuf::new();
    for comp in out.split('/') {
        let c = comp.trim();
        if c.is_empty() || c == "." || c == ".." {
            continue;
        }
        p.push(c);
    }
    if p.as_os_str().is_empty() {
        p.push(if ctx.filename.is_empty() { "untitled" } else { ctx.filename });
    }
    p
}

/// Full destination file path for an output.
pub fn destination(root: &Path, template: &str, ctx: &NameContext, ext: &str, portable: bool) -> PathBuf {
    let rel = render(template, ctx, portable);
    let mut p = root.join(rel);
    let name = format!("{}.{}", p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), ext);
    p.set_file_name(name);
    p
}

/// `name.ext` -> `name (2).ext` etc. until the path is free.
pub fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 2.. {
        let cand = p.with_file_name(format!("{stem} ({n}).{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> TrackMeta {
        TrackMeta {
            title: "Song: Part 1/2?".into(),
            artist: "AC/DC".into(),
            album: "Back In Black".into(),
            track: 3,
            track_total: 10,
            disc: 1,
            disc_total: 1,
            year: "1980-07-25".into(),
            ..Default::default()
        }
    }

    #[test]
    fn single_disc_drops_group() {
        let m = meta();
        let ctx = NameContext { meta: &m, filename: "", reldir: "", format: "FLAC" };
        let p = render(crate::config::TAG_TEMPLATE, &ctx, true);
        assert_eq!(p, PathBuf::from("AC-DC/Back In Black/03 - Song- Part 1-2_"));
        let mut m2 = meta();
        m2.track = 0;
        let ctx = NameContext { meta: &m2, filename: "", reldir: "", format: "FLAC" };
        assert_eq!(render(crate::config::TAG_TEMPLATE, &ctx, true), PathBuf::from("AC-DC/Back In Black/Song- Part 1-2_"));
    }

    #[test]
    fn multi_disc_keeps_group() {
        let mut m = meta();
        m.disc = 2;
        m.disc_total = 2;
        m.album_artist = "Various".into();
        let ctx = NameContext { meta: &m, filename: "", reldir: "", format: "FLAC" };
        let p = render("{albumartist}/{album}[ (Disc {disc})]/{year} {track}", &ctx, false);
        assert_eq!(p, PathBuf::from("Various/Back In Black (Disc 2)/1980 03"));
    }

    #[test]
    fn reldir_and_extension() {
        let m = meta();
        let ctx = NameContext { meta: &m, filename: "orig name", reldir: "Rock/1980", format: "MP3" };
        let p = destination(Path::new("/out"), "{reldir}/{filename}", &ctx, "mp3", true);
        assert_eq!(p, PathBuf::from("/out/Rock/1980/orig name.mp3"));
    }
}
