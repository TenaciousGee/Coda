//! Discogs as a third metadata source.
//!
//! Discogs has no disc-ID lookup, so we search (by the barcode the CD carries, or
//! by artist + album) and then check each candidate against the disc itself: the
//! release must have a disc with the same number of tracks, and candidates are
//! ranked by how closely their track lengths match the TOC.

use crate::metadata::{AlbumCandidate, CandTrack, RelTrack, ReleaseFull};
use crate::net;
use anyhow::{bail, Result};
use serde_json::Value;

fn api() -> String {
    // overridable only so the search flow can be tested against a local mock
    std::env::var("CODA_DISCOGS_API").unwrap_or_else(|_| "https://api.discogs.com".into())
}
/// Search results whose full details we fetch (each is one request).
const MAX_DETAILS: usize = 6;

pub enum Query {
    Barcode(String),
    Text { artist: String, album: String },
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default().trim().to_string()
}

fn get(url: &str, token: &str) -> Result<Value> {
    let auth = (!token.trim().is_empty()).then(|| format!("Discogs token={}", token.trim()));
    match net::get_json_with_auth(url, auth.as_deref()) {
        Ok(v) => Ok(v),
        Err(e) if net::status_of(&e) == Some(401) => {
            if token.trim().is_empty() {
                bail!("Discogs search needs a free personal access token: create one at discogs.com/settings/developers and paste it in Settings → Metadata")
            } else {
                bail!("Discogs rejected the access token in Settings → Metadata")
            }
        }
        Err(e) if net::status_of(&e) == Some(429) => bail!("Discogs rate limit reached; try again in a minute"),
        Err(e) => Err(e),
    }
}

/// "Artist (2)" -> "Artist", "Name*" -> "Name" (Discogs disambiguation and ANV markers).
fn clean_name(n: &str) -> String {
    let mut n = n.trim().trim_end_matches('*').trim().to_string();
    if n.ends_with(')') {
        if let Some(open) = n.rfind(" (") {
            if n[open + 2..n.len() - 1].chars().all(|c| c.is_ascii_digit()) {
                n.truncate(open);
            }
        }
    }
    n
}

fn artists(v: &Value) -> String {
    let Some(arr) = v.get("artists").and_then(|a| a.as_array()) else { return String::new() };
    let mut out = String::new();
    for (i, a) in arr.iter().enumerate() {
        let anv = s(a, "anv");
        out.push_str(&clean_name(if anv.is_empty() { a.get("name").and_then(|x| x.as_str()).unwrap_or("") } else { &anv }));
        if i + 1 < arr.len() {
            let join = s(a, "join");
            match join.as_str() {
                "" | "," => out.push_str(", "),
                j => {
                    out.push(' ');
                    out.push_str(j);
                    out.push(' ');
                }
            }
        }
    }
    out
}

fn seconds(d: &str) -> Option<f64> {
    let parts: Vec<&str> = d.trim().split(':').collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(parts.iter().fold(0.0, |acc, p| acc * 60.0 + p.parse::<f64>().unwrap_or(0.0)))
}

/// Disc number from a track position: "2-05" / "CD2-5" / "2.05" -> 2, "7" -> 1.
fn disc_of(pos: &str) -> u32 {
    let p = pos.trim();
    let prefix = if let Some((a, _)) = p.split_once('-') {
        a
    } else if let Some((a, b)) = p.split_once('.') {
        if !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && b.len() >= 2 && b.chars().all(|c| c.is_ascii_digit()) {
            a
        } else {
            return 1;
        }
    } else {
        return 1;
    };
    prefix.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(1).max(1)
}

struct DTrack {
    disc: u32,
    track: CandTrack,
    secs: Option<f64>,
}

fn tracks(rel: &Value, album_artist: &str) -> Vec<DTrack> {
    let mut out = Vec::new();
    for t in rel.get("tracklist").and_then(|t| t.as_array()).into_iter().flatten() {
        let kind = s(t, "type_");
        // "index" = one CD track made of named sections; headings are not tracks
        if kind != "track" && kind != "index" {
            continue;
        }
        let pos = if kind == "index" {
            t.get("sub_tracks").and_then(|x| x.as_array()).and_then(|x| x.first()).map(|x| s(x, "position")).unwrap_or_default()
        } else {
            s(t, "position")
        };
        let artist = artists(t);
        out.push(DTrack {
            disc: if kind == "index" { pos.split_once('-').map(|_| disc_of(&pos)).unwrap_or(1) } else { disc_of(&pos) },
            track: CandTrack { title: s(t, "title"), artist: if artist.is_empty() { album_artist.to_string() } else { artist }, ..Default::default() },
            secs: seconds(&s(t, "duration")),
        });
    }
    out
}

/// Sort key: timed matches by closeness, count-only matches after them.
fn rank(score: Option<f64>) -> f64 {
    score.unwrap_or(1000.0)
}

/// Build a candidate for the disc in `rel` that matches the TOC, with its score
/// (mean track-length difference in seconds, None if no durations are listed).
pub fn candidate_from_release(rel: &Value, lengths: &[f64]) -> Option<(AlbumCandidate, Option<f64>)> {
    let artist = artists(rel);
    let all = tracks(rel, &artist);
    let disc_total = all.iter().map(|t| t.disc).max().unwrap_or(1);
    // (disc, mean length difference in seconds; None when Discogs lists no durations)
    let mut best: Option<(u32, Option<f64>)> = None;
    for d in 1..=disc_total {
        let disc: Vec<&DTrack> = all.iter().filter(|t| t.disc == d).collect();
        if disc.len() != lengths.len() {
            continue;
        }
        let diffs: Vec<f64> = disc.iter().zip(lengths).filter_map(|(t, l)| t.secs.map(|s| (s - l).abs())).collect();
        let score = (!diffs.is_empty()).then(|| diffs.iter().sum::<f64>() / diffs.len() as f64);
        if best.is_none() || rank(score) < rank(best.unwrap().1) {
            best = Some((d, score));
        }
    }
    let (disc_no, score) = best?;
    // a real match is within a few seconds per track; far off means another edition
    if score.is_some_and(|s| s > 20.0) {
        return None;
    }
    let label = rel.get("labels").and_then(|l| l.as_array()).and_then(|l| l.first()).cloned().unwrap_or(Value::Null);
    let barcode = rel
        .get("identifiers")
        .and_then(|i| i.as_array())
        .and_then(|ids| ids.iter().find(|i| s(i, "type") == "Barcode"))
        .map(|i| s(i, "value").chars().filter(|c| c.is_ascii_digit()).collect())
        .unwrap_or_default();
    let first = |k: &str| rel.get(k).and_then(|x| x.as_array()).and_then(|x| x.first()).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let date = {
        let r = s(rel, "released");
        if r.is_empty() || r.starts_with('0') {
            rel.get("year").and_then(|y| y.as_u64()).filter(|y| *y > 0).map(|y| y.to_string()).unwrap_or_default()
        } else {
            r.trim_end_matches("-00").to_string()
        }
    };
    let images = rel.get("images").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    let cover_url = images
        .iter()
        .find(|i| s(i, "type") == "primary")
        .or(images.first())
        .map(|i| s(i, "uri"))
        .unwrap_or_default();
    let style = first("styles");
    Some((
        AlbumCandidate {
            source: "Discogs".into(),
            release_id: String::new(), // not a MusicBrainz ID; kept out of MB tags
            discogs_id: rel.get("id").and_then(|i| i.as_u64()).map(|i| i.to_string()).unwrap_or_default(),
            title: s(rel, "title"),
            artist,
            date,
            country: s(rel, "country"),
            label: label.get("name").and_then(|n| n.as_str()).map(clean_name).unwrap_or_default(),
            catalog: s(&label, "catno").replace("none", ""),
            barcode,
            genre: if style.is_empty() { first("genres") } else { style },
            disc: disc_no,
            disc_total,
            tracks: all.into_iter().filter(|t| t.disc == disc_no).map(|t| t.track).collect(),
            cover_url,
            ..Default::default()
        },
        score,
    ))
}

/// Search Discogs and return releases that fit this disc, best match first.
pub fn search(q: &Query, lengths: &[f64], token: &str) -> Result<Vec<AlbumCandidate>> {
    let filter = match q {
        Query::Barcode(b) => format!("barcode={}", net::urlencode(b)),
        Query::Text { artist, album } => {
            if album.trim().is_empty() {
                bail!("type the album (and ideally the artist) first");
            }
            let mut f = format!("release_title={}", net::urlencode(album.trim()));
            if !artist.trim().is_empty() {
                f.push_str(&format!("&artist={}", net::urlencode(artist.trim())));
            }
            f
        }
    };
    let api = api();
    let url = format!("{api}/database/search?type=release&format=CD&per_page=25&{filter}");
    let results = get(&url, token)?.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let mut found: Vec<(AlbumCandidate, Option<f64>)> = Vec::new();
    for r in results.iter().take(MAX_DETAILS) {
        let Some(id) = r.get("id").and_then(|i| i.as_u64()) else { continue };
        let Ok(rel) = get(&format!("{api}/releases/{id}"), token) else { continue };
        if let Some(c) = candidate_from_release(&rel, lengths) {
            found.push(c);
        }
    }
    found.sort_by(|a, b| rank(a.1).total_cmp(&rank(b.1)));
    Ok(found.into_iter().map(|(c, _)| c).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release() -> Value {
        serde_json::from_str(r#"{
          "id": 12345, "title": "Double Feature", "year": 2001, "released": "2001-03-00", "country": "UK",
          "artists": [{"name": "Band (3)", "anv": "", "join": "&"}, {"name": "Singer*", "anv": "The Singer", "join": ""}],
          "labels": [{"name": "Label (2)", "catno": "LAB 001CD"}],
          "identifiers": [{"type": "Matrix / Runout", "value": "X"}, {"type": "Barcode", "value": "5 012345 678900"}],
          "genres": ["Electronic"], "styles": ["Trip Hop", "Downtempo"],
          "images": [{"type": "secondary", "uri": "https://img/back.jpg"}, {"type": "primary", "uri": "https://img/front.jpg"}],
          "tracklist": [
            {"type_": "heading", "position": "", "title": "CD 1"},
            {"type_": "track", "position": "1-1", "title": "One", "duration": "3:00"},
            {"type_": "track", "position": "1-2", "title": "Two", "duration": "4:00"},
            {"type_": "heading", "position": "", "title": "CD 2"},
            {"type_": "track", "position": "2-1", "title": "Three", "duration": "5:01", "artists": [{"name": "Guest (7)", "anv": "", "join": ""}]},
            {"type_": "index", "position": "", "title": "Suite", "sub_tracks": [{"position": "2-2a", "title": "I"}, {"position": "2-2b", "title": "II"}]}
          ]
        }"#).unwrap()
    }

    #[test]
    fn matches_correct_disc_by_lengths() {
        // disc 2 has 2 tracks, the first 301 s long
        let (c, score) = candidate_from_release(&release(), &[300.0, 420.0]).unwrap();
        assert_eq!((c.disc, c.disc_total), (2, 2));
        assert!(score.unwrap() <= 1.0);
        assert_eq!(c.artist, "Band & The Singer");
        assert_eq!(c.tracks[0].artist, "Guest");
        assert_eq!(c.tracks[1].title, "Suite");
        assert_eq!(c.tracks[1].artist, "Band & The Singer");
        assert_eq!((c.label.as_str(), c.catalog.as_str()), ("Label", "LAB 001CD"));
        assert_eq!(c.barcode, "5012345678900");
        assert_eq!(c.genre, "Trip Hop");
        assert_eq!(c.date, "2001-03");
        assert_eq!(c.cover_url, "https://img/front.jpg");
        assert_eq!(c.discogs_id, "12345");
    }

    #[test]
    fn full_release_numbers_tracks_per_disc() {
        let r = release_full(&release());
        assert_eq!(r.album.disc_total, 2);
        let pos: Vec<(u32, u32)> = r.tracks.iter().map(|t| (t.disc, t.number)).collect();
        assert_eq!(pos, vec![(1, 1), (1, 2), (2, 1), (2, 2)]);
        assert_eq!(r.tracks[2].secs, Some(301.0));
        assert_eq!(r.album.discogs_id, "12345");
    }

    #[test]
    fn rejects_wrong_edition() {
        // right track count, but lengths a minute out: a different edition
        assert!(candidate_from_release(&release(), &[240.0, 240.0]).is_none());
        // no disc with three tracks
        assert!(candidate_from_release(&release(), &[1.0, 2.0, 3.0]).is_none());
    }

    /// Full search against a mock server (run with CODA_DISCOGS_API set).
    #[test]
    #[ignore]
    fn search_against_mock() {
        let r = search(&Query::Text { artist: "Band".into(), album: "Double Feature".into() }, &[300.0, 420.0], "secret");
        let found = r.unwrap();
        // release 2 (exact lengths) must rank above release 1 (a few seconds out); release 3 (wrong edition) rejected
        assert_eq!(found.iter().map(|c| c.discogs_id.as_str()).collect::<Vec<_>>(), vec!["2", "1"]);
        let err = search(&Query::Barcode("123".into()), &[300.0, 420.0], "").unwrap_err().to_string();
        assert!(err.contains("personal access token"), "{err}");
    }

    #[test]
    fn positions_and_durations() {
        assert_eq!(disc_of("7"), 1);
        assert_eq!(disc_of("CD2-05"), 2);
        assert_eq!(disc_of("3.04"), 3);
        assert_eq!(disc_of("A1"), 1);
        assert_eq!(seconds("1:02:03"), Some(3723.0));
        assert_eq!(seconds(""), None);
        assert_eq!(clean_name("Prince (2)*"), "Prince");
        assert_eq!(clean_name("Band (UK)"), "Band (UK)");
    }
}

/// The whole release (all discs, with lengths) for the tagger.
pub fn release_full(rel: &Value) -> ReleaseFull {
    let artist = artists(rel);
    let all = tracks(rel, &artist);
    let mut per_disc: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let rel_tracks: Vec<RelTrack> = all
        .into_iter()
        .map(|t| {
            let n = per_disc.entry(t.disc).or_insert(0);
            *n += 1;
            RelTrack { disc: t.disc, number: *n, track: t.track, secs: t.secs }
        })
        .collect();
    let label = rel.get("labels").and_then(|l| l.as_array()).and_then(|l| l.first()).cloned().unwrap_or(Value::Null);
    let first = |k: &str| rel.get(k).and_then(|x| x.as_array()).and_then(|x| x.first()).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let style = first("styles");
    let images = rel.get("images").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    let date = {
        let r = s(rel, "released");
        if r.is_empty() || r.starts_with('0') {
            rel.get("year").and_then(|y| y.as_u64()).filter(|y| *y > 0).map(|y| y.to_string()).unwrap_or_default()
        } else {
            r.trim_end_matches("-00").to_string()
        }
    };
    let album = AlbumCandidate {
        source: "Discogs".into(),
        discogs_id: rel.get("id").and_then(|i| i.as_u64()).map(|i| i.to_string()).unwrap_or_default(),
        title: s(rel, "title"),
        artist,
        date,
        country: s(rel, "country"),
        label: label.get("name").and_then(|n| n.as_str()).map(clean_name).unwrap_or_default(),
        catalog: s(&label, "catno").replace("none", ""),
        barcode: rel
            .get("identifiers")
            .and_then(|i| i.as_array())
            .and_then(|ids| ids.iter().find(|i| s(i, "type") == "Barcode"))
            .map(|i| s(i, "value").chars().filter(|c| c.is_ascii_digit()).collect())
            .unwrap_or_default(),
        genre: if style.is_empty() { first("genres") } else { style },
        disc: 1,
        disc_total: rel_tracks.iter().map(|t| t.disc).max().unwrap_or(1),
        cover_url: images.iter().find(|i| s(i, "type") == "primary").or(images.first()).map(|i| s(i, "uri")).unwrap_or_default(),
        ..Default::default()
    };
    ReleaseFull { album, tracks: rel_tracks }
}

/// Search by artist/album and return full releases (no TOC filtering; the tagger
/// scores them against the files).
pub fn search_full(artist: &str, album: &str, token: &str, limit: usize) -> Result<Vec<ReleaseFull>> {
    if album.trim().is_empty() {
        bail!("enter an album title to search");
    }
    let mut f = format!("release_title={}", net::urlencode(album.trim()));
    if !artist.trim().is_empty() {
        f.push_str(&format!("&artist={}", net::urlencode(artist.trim())));
    }
    let api = api();
    let results = get(&format!("{api}/database/search?type=release&per_page=25&{f}"), token)?
        .get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for r in results.iter().take(limit) {
        let Some(id) = r.get("id").and_then(|i| i.as_u64()) else { continue };
        if let Ok(rel) = get(&format!("{api}/releases/{id}"), token) {
            out.push(release_full(&rel));
        }
    }
    Ok(out)
}

