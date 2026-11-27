//! Tag / re-tag existing files.
//!
//! Online sources describe *albums*, so files are grouped into albums, the album is
//! looked up as a whole, and each file is paired with a release track using its track
//! number, length and title. That is far more reliable than matching songs one by one
//! (the same song exists on singles, compilations and remasters with different tags).

use crate::config::Settings;
use crate::metadata::{self, ReleaseFull};
use crate::naming::{self, NameContext};
use crate::tags::{self, TrackMeta};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TagFile {
    pub path: PathBuf,
    pub current: TrackMeta,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    New,
    Searching,
    Matched(u32),
    /// candidates found, but none confident enough to apply automatically
    Review(u32),
    NoMatch,
    Edited,
    Saved,
    Error(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::New => "Not matched yet".into(),
            Status::Searching => "Searching…".into(),
            Status::Matched(p) => format!("{p}% match"),
            Status::Review(p) => format!("Review: best {p}%"),
            Status::NoMatch => "No match found".into(),
            Status::Edited => "Edited".into(),
            Status::Saved => "Saved".into(),
            Status::Error(e) => format!("Error: {e}"),
        }
    }
    /// colour state for the UI pill (see StatePill)
    pub fn state(&self) -> i32 {
        match self {
            Status::Saved => 2,
            Status::Matched(p) if *p >= 90 => 3,
            Status::Matched(_) | Status::Review(_) | Status::NoMatch => 4,
            Status::Error(_) => 5,
            Status::Searching => 1,
            Status::Edited => 3,
            Status::New => 0,
        }
    }
}

/// A scored release: which file goes with which release track, and how well it fits.
#[derive(Debug, Clone)]
pub struct Scored {
    pub release: ReleaseFull,
    /// per file: index into `release.tracks`
    pub mapping: Vec<Option<usize>>,
    /// per file: how well that file matched (0-100)
    pub file_scores: Vec<u32>,
    pub percent: u32,
}

impl Scored {
    pub fn summary(&self) -> String {
        let a = &self.release.album;
        let matched = self.mapping.iter().filter(|m| m.is_some()).count();
        let mut bits = vec![format!("{}% · {} – {}", self.percent, a.artist, a.title)];
        let extra: Vec<&str> = [a.date.as_str(), a.country.as_str(), a.label.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
        if !extra.is_empty() {
            bits.push(format!("({})", extra.join(", ")));
        }
        bits.push(format!("{matched}/{} files · {} tracks", self.mapping.len(), self.release.tracks.len()));
        bits.push(format!("[{}]", a.source));
        bits.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub files: Vec<TagFile>,
    pub folder: PathBuf,
    pub query_artist: String,
    pub query_album: String,
    pub results: Vec<Scored>,
    pub chosen: Option<usize>,
    /// proposed tags per file (editable)
    pub proposed: Vec<TrackMeta>,
    /// files to write
    pub apply: Vec<bool>,
    pub new_cover: Option<Arc<Vec<u8>>>,
    pub replace_cover: bool,
    pub status: Status,
}

impl Group {
    pub fn title(&self) -> String {
        let a = most_common(self.files.iter().map(|f| f.current.album.as_str()));
        if a.is_empty() {
            self.folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Loose files".into())
        } else {
            a
        }
    }
    pub fn artist(&self) -> String {
        let aa = most_common(self.files.iter().map(|f| f.current.album_artist.as_str()));
        if aa.is_empty() {
            most_common(self.files.iter().map(|f| f.current.artist.as_str()))
        } else {
            aa
        }
    }
    pub fn current_cover(&self) -> Option<Arc<Vec<u8>>> {
        self.files.iter().find_map(|f| f.current.cover.clone())
    }
    /// Start over from the files' own tags (manual tagging).
    pub fn reset_to_current(&mut self) {
        self.chosen = None;
        self.proposed = self.files.iter().map(|f| f.current.clone()).collect();
        self.apply = vec![true; self.files.len()];
        self.new_cover = None;
        self.replace_cover = false;
    }
}

fn most_common<'a>(it: impl Iterator<Item = &'a str>) -> String {
    let mut c: HashMap<&str, usize> = HashMap::new();
    for s in it.filter(|s| !s.trim().is_empty()) {
        *c.entry(s).or_default() += 1;
    }
    c.into_iter().max_by_key(|(s, n)| (*n, std::cmp::Reverse(s.len()))).map(|(s, _)| s.to_string()).unwrap_or_default()
}

/// "Artist - Album (2001)" style folder names -> (artist, album)
fn guess_from_folder(folder: &Path) -> (String, String) {
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut name = name.trim().to_string();
    // drop a trailing "(2001)" / "[FLAC]" etc.
    while name.ends_with(')') || name.ends_with(']') {
        let open = if name.ends_with(')') { name.rfind('(') } else { name.rfind('[') };
        match open {
            Some(i) if i > 0 => name = name[..i].trim().to_string(),
            _ => break,
        }
    }
    match name.split_once(" - ") {
        Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
        None => (String::new(), name),
    }
}

/// Read files and group them into albums.
pub fn load(paths: &[PathBuf], recursive: bool, ffmpeg: &str) -> Vec<Group> {
    let items = crate::converter::scan(paths, recursive);
    let mut groups: Vec<(String, Group)> = Vec::new();
    for item in items {
        let mut m = tags::read(&item.path);
        if m.title.is_empty() && m.artist.is_empty() {
            tags::read_ffmpeg(ffmpeg, &item.path, &mut m);
        }
        let folder = item.path.parent().map(Path::to_path_buf).unwrap_or_default();
        // same album tag (+ album artist) = same album, even across CD1/CD2 folders
        let key = if m.album.trim().is_empty() {
            format!("dir|{}", folder.display())
        } else if m.album_artist.trim().is_empty() {
            format!("album|{}|{}", m.album.to_lowercase(), folder.parent().map(|p| p.display().to_string()).unwrap_or_default())
        } else {
            format!("album|{}|{}", m.album.to_lowercase(), m.album_artist.to_lowercase())
        };
        let file = TagFile { path: item.path, current: m };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.files.push(file),
            None => groups.push((
                key,
                Group {
                    files: vec![file],
                    folder,
                    query_artist: String::new(),
                    query_album: String::new(),
                    results: vec![],
                    chosen: None,
                    proposed: vec![],
                    apply: vec![],
                    new_cover: None,
                    replace_cover: false,
                    status: Status::New,
                },
            )),
        }
    }
    groups
        .into_iter()
        .map(|(_, mut g)| {
            g.files.sort_by(|a, b| {
                (a.current.disc, a.current.track, a.path.file_name()).cmp(&(b.current.disc, b.current.track, b.path.file_name()))
            });
            let (fa, fb) = guess_from_folder(&g.folder);
            g.query_artist = Some(g.artist()).filter(|s| !s.is_empty()).unwrap_or(fa);
            let album = most_common(g.files.iter().map(|f| f.current.album.as_str()));
            g.query_album = if album.is_empty() { fb } else { album };
            g.reset_to_current();
            g
        })
        .collect()
}

// ------------------------------------------------------------------ matching

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Titles like "Track 1", "Audio Track 03" or "01" carry no information.
fn is_placeholder(t: &str) -> bool {
    let n = norm(t);
    let letters: String = n.chars().filter(|c| c.is_alphabetic()).collect();
    n.is_empty() || letters.is_empty() || ["track", "audiotrack", "piste", "titel", "untitled", "unknown"].contains(&letters.as_str())
}

const MAX_COST: f64 = 55.0;

/// Lower is better. Length and track number carry most weight; the title mostly
/// matters when there's no track number (it's often the very thing that's wrong).
fn cost(f: &TrackMeta, t: &metadata::RelTrack, single_disc: bool) -> f64 {
    let diff = match (f.duration > 0.0, t.secs) {
        (true, Some(s)) => Some((f.duration - s).abs()),
        _ => None,
    };
    let dur = diff.map(|d| (d.min(60.0) / 60.0) * 40.0).unwrap_or(10.0);
    let num_match = f.track > 0 && f.track == t.number && (single_disc || f.disc == 0 || f.disc == t.disc);
    let num = if f.track == 0 { 8.0 } else if num_match { 0.0 } else { 25.0 };
    let title = if num_match && diff.is_some_and(|d| d <= 3.0) {
        0.0 // number and length agree: a different title is the error being fixed
    } else if is_placeholder(&f.title) {
        6.0
    } else {
        let (a, b) = (norm(&f.title), norm(&t.track.title));
        if a == b {
            0.0
        } else if a.contains(&b) || b.contains(&a) {
            4.0
        } else {
            12.0
        }
    };
    dur + num + title
}

pub fn score(files: &[TagFile], release: ReleaseFull) -> Scored {
    let single_disc = release.tracks.iter().all(|t| t.disc == release.tracks.first().map(|f| f.disc).unwrap_or(1));
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        for (ti, t) in release.tracks.iter().enumerate() {
            pairs.push((cost(&f.current, t, single_disc), fi, ti));
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut mapping = vec![None; files.len()];
    let mut file_scores = vec![0u32; files.len()];
    let mut used = vec![false; release.tracks.len()];
    for (c, fi, ti) in pairs {
        if c > 45.0 || mapping[fi].is_some() || used[ti] {
            continue;
        }
        mapping[fi] = Some(ti);
        used[ti] = true;
        file_scores[fi] = ((1.0 - c / MAX_COST).max(0.0) * 100.0).round() as u32;
    }
    let percent = if files.is_empty() { 0 } else { (file_scores.iter().sum::<u32>() as f64 / files.len() as f64).round() as u32 };
    Scored { release, mapping, file_scores, percent }
}

/// Look the group up in the chosen sources and return releases best-first.
/// `sources`: 0 = all, 1 = MusicBrainz, 2 = Discogs.
pub fn search(files: &[TagFile], artist: &str, album: &str, sources: i32, st: &Settings) -> Result<Vec<Scored>> {
    let mut releases: Vec<ReleaseFull> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    if sources != 2 && st.musicbrainz {
        // files that already carry a MusicBrainz release ID: look that up directly
        let known = most_common(files.iter().map(|f| f.current.mb_release_id.as_str()));
        if !known.is_empty() {
            if let Ok(r) = metadata::mb_lookup_release(&known) {
                releases.push(r);
            }
        }
        match metadata::mb_search_releases(artist, album, 6) {
            Ok(ids) => {
                for id in ids.into_iter().filter(|id| *id != known).take(4) {
                    match metadata::mb_lookup_release(&id) {
                        Ok(r) => releases.push(r),
                        Err(e) => errors.push(crate::net::describe("MusicBrainz", &e)),
                    }
                }
            }
            Err(e) => errors.push(crate::net::describe("MusicBrainz", &e)),
        }
    }
    if sources != 1 && st.discogs_fallback {
        match crate::discogs::search_full(artist, album, &st.discogs_token, 4) {
            Ok(r) => releases.extend(r),
            Err(e) => errors.push(crate::net::describe("Discogs", &e)),
        }
    }
    if releases.is_empty() && !errors.is_empty() {
        anyhow::bail!("{}", errors.join("; "));
    }
    let mut scored: Vec<Scored> = releases.into_iter().map(|r| score(files, r)).filter(|s| s.percent > 0).collect();
    scored.sort_by(|a, b| b.percent.cmp(&a.percent));
    Ok(scored)
}

/// Proposed tags for every file from a scored release. Unmatched files keep their
/// own tags and are left unticked.
pub fn propose(g: &mut Group, idx: usize) {
    let Some(sc) = g.results.get(idx).cloned() else { return };
    let rel = &sc.release;
    let a = &rel.album;
    let disc_total = rel.tracks.iter().map(|t| t.disc).max().unwrap_or(1);
    let mb = a.source == "MusicBrainz";
    g.chosen = Some(idx);
    g.proposed = g
        .files
        .iter()
        .zip(&sc.mapping)
        .map(|(f, m)| {
            let mut p = f.current.clone();
            let Some(ti) = m else { return p };
            let t = &rel.tracks[*ti];
            p.title = t.track.title.clone();
            p.artist = t.track.artist.clone();
            p.album = a.title.clone();
            p.album_artist = a.artist.clone();
            p.track = t.number;
            p.track_total = rel.tracks.iter().filter(|x| x.disc == t.disc).count() as u32;
            p.disc = t.disc;
            p.disc_total = disc_total;
            if !a.date.is_empty() {
                p.year = a.date.clone();
            }
            if !a.genre.is_empty() {
                p.genre = a.genre.clone();
            }
            p.label = a.label.clone();
            p.catalog = a.catalog.clone();
            p.barcode = a.barcode.clone();
            if !t.track.isrc.is_empty() {
                p.isrc = t.track.isrc.clone();
            }
            if mb {
                p.mb_release_id = a.release_id.clone();
                p.mb_release_group_id = a.release_group_id.clone();
                p.mb_album_artist_id = a.artist_id.clone();
                p.mb_recording_id = t.track.recording_id.clone();
                p.mb_track_id = t.track.track_id.clone();
                p.mb_artist_id = t.track.artist_id.clone();
            } else {
                p.discogs_release_id = a.discogs_id.clone();
            }
            p
        })
        .collect();
    g.apply = sc.mapping.iter().map(|m| m.is_some()).collect();
    g.status = Status::Matched(sc.percent);
}

/// Write the ticked files. Returns (written, errors). Optionally renames each file
/// within its folder from the new tags.
pub fn save(g: &mut Group, rename_template: Option<&str>, st: &Settings) -> (usize, Vec<String>) {
    let cover = if g.replace_cover { g.new_cover.clone() } else { None };
    let mut written = 0;
    let mut errors = Vec::new();
    for i in 0..g.files.len() {
        if !g.apply.get(i).copied().unwrap_or(false) {
            continue;
        }
        let p = g.proposed[i].clone();
        let path = g.files[i].path.clone();
        if let Err(e) = tags::update(&path, &p, cover.as_ref().map(|c| c.as_slice()), &st.ffmpeg) {
            errors.push(format!("{}: {e:#}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
            continue;
        }
        let mut final_path = path.clone();
        if let Some(t) = rename_template.filter(|t| !t.trim().is_empty()) {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let ctx = NameContext { meta: &p, filename: &stem, reldir: "", format: "" };
            let rendered = naming::render(t, &ctx, st.portable_filenames);
            if let (Some(name), Some(ext), Some(dir)) = (rendered.file_name(), path.extension(), path.parent()) {
                let target = dir.join(format!("{}.{}", name.to_string_lossy(), ext.to_string_lossy()));
                if target != path {
                    let target = naming::unique_path(&target);
                    match std::fs::rename(&path, &target) {
                        Ok(()) => final_path = target,
                        Err(e) => errors.push(format!("rename {}: {e}", path.display())),
                    }
                }
            }
        }
        // refresh what the file now holds
        let mut now = tags::read(&final_path);
        if now.title.is_empty() && now.artist.is_empty() {
            tags::read_ffmpeg(&st.ffmpeg, &final_path, &mut now);
        }
        g.files[i] = TagFile { path: final_path, current: now };
        written += 1;
    }
    if errors.is_empty() && written > 0 {
        g.status = Status::Saved;
    } else if !errors.is_empty() {
        g.status = Status::Error(format!("{} file(s) failed", errors.len()));
    }
    (written, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{AlbumCandidate, CandTrack, RelTrack};

    fn file(track: u32, title: &str, secs: f64) -> TagFile {
        TagFile { path: PathBuf::from(format!("/x/{track}.flac")), current: TrackMeta { track, title: title.into(), duration: secs, ..Default::default() } }
    }
    fn rel(tracks: &[(&str, f64)]) -> ReleaseFull {
        ReleaseFull {
            album: AlbumCandidate { title: "Album".into(), artist: "Band".into(), source: "MusicBrainz".into(), release_id: "r1".into(), ..Default::default() },
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (t, s))| RelTrack { disc: 1, number: i as u32 + 1, track: CandTrack { title: t.to_string(), artist: "Band".into(), ..Default::default() }, secs: Some(*s) })
                .collect(),
        }
    }

    #[test]
    fn matches_by_number_and_length_despite_bad_titles() {
        let files = vec![file(1, "track01", 200.0), file(2, "Wrong Name", 181.0), file(3, "", 240.0)];
        let s = score(&files, rel(&[("Intro", 201.0), ("Song", 180.0), ("Outro", 239.0)]));
        assert_eq!(s.mapping, vec![Some(0), Some(1), Some(2)]);
        assert!(s.percent >= 50, "{}", s.percent);
    }

    #[test]
    fn untagged_files_match_by_length_and_title() {
        let files = vec![file(0, "Outro", 239.0), file(0, "Intro", 200.0)];
        let s = score(&files, rel(&[("Intro", 201.0), ("Song", 180.0), ("Outro", 239.0)]));
        assert_eq!(s.mapping, vec![Some(2), Some(0)]);
    }

    #[test]
    fn partial_album_and_wrong_album() {
        // two of three tracks present: still a confident match
        let files = vec![file(1, "Intro", 201.0), file(3, "Outro", 239.0)];
        let good = score(&files, rel(&[("Intro", 201.0), ("Song", 180.0), ("Outro", 239.0)]));
        assert!(good.percent >= 90, "{}", good.percent);
        // a different album with other lengths and titles scores much lower
        let bad = score(&files, rel(&[("Alpha", 400.0), ("Beta", 90.0), ("Gamma", 500.0)]));
        assert!(bad.percent + 40 < good.percent, "good {} bad {}", good.percent, bad.percent);
    }

    #[test]
    fn proposal_fills_tags_and_unticks_unmatched() {
        let files = vec![file(1, "x", 201.0), file(9, "Bonus", 999.0)];
        let mut g = Group {
            files: files.clone(),
            folder: PathBuf::from("/x"),
            query_artist: String::new(),
            query_album: String::new(),
            results: vec![score(&files, rel(&[("Intro", 201.0)]))],
            chosen: None,
            proposed: vec![],
            apply: vec![],
            new_cover: None,
            replace_cover: false,
            status: Status::New,
        };
        propose(&mut g, 0);
        assert_eq!(g.proposed[0].title, "Intro");
        assert_eq!(g.proposed[0].album, "Album");
        assert_eq!(g.proposed[0].mb_release_id, "r1");
        assert_eq!((g.proposed[0].track, g.proposed[0].track_total), (1, 1));
        assert_eq!(g.apply, vec![true, false]);
        assert_eq!(g.proposed[1].title, "Bonus"); // unmatched keeps its own tags
    }

    #[test]
    fn exact_number_and_length_is_full_confidence() {
        let files = vec![file(1, "Track 1", 8.0), file(2, "Track 2", 6.0), file(3, "Audio Track 03", 5.0)];
        let s = score(&files, rel(&[("Alpha", 8.0), ("Beta", 6.0), ("Gamma", 5.0)]));
        assert_eq!(s.percent, 100);
        assert!(is_placeholder("Track 1") && is_placeholder("03") && !is_placeholder("Alpha"));
    }

    #[test]
    fn folder_guess() {
        assert_eq!(guess_from_folder(Path::new("/m/Seether - 2002-2013 (2013) [FLAC]")), ("Seether".into(), "2002-2013".into()));
        assert_eq!(guess_from_folder(Path::new("/m/Some Album")), ("".into(), "Some Album".into()));
    }
}
