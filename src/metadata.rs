//! Disc metadata (MusicBrainz, GnuDB fallback) and cover art
//! (Cover Art Archive originals, iTunes high-resolution fallback).

use crate::config::Settings;
use crate::net;
use anyhow::{bail, Result};
use serde_json::Value;
use std::io::Cursor;

#[derive(Debug, Clone, Default)]
pub struct CandTrack {
    pub title: String,
    pub artist: String,
    pub artist_id: String,
    pub recording_id: String,
    pub track_id: String,
    pub isrc: String,
}

#[derive(Debug, Clone, Default)]
pub struct AlbumCandidate {
    pub source: String,
    pub release_id: String,
    pub release_group_id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: String,
    pub date: String,
    pub country: String,
    pub label: String,
    pub catalog: String,
    pub barcode: String,
    pub genre: String,
    pub disc: u32,
    pub disc_total: u32,
    pub tracks: Vec<CandTrack>,
    /// Discogs release ID, when the candidate came from Discogs
    pub discogs_id: String,
    /// Front cover URL offered by the source (Discogs)
    pub cover_url: String,
}

impl AlbumCandidate {
    pub fn summary(&self) -> String {
        let mut bits = vec![format!("{} – {}", self.artist, self.title)];
        let extra: Vec<&str> = [self.date.as_str(), self.country.as_str(), self.label.as_str(), self.catalog.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        if !extra.is_empty() {
            bits.push(format!("({})", extra.join(", ")));
        }
        if self.disc_total > 1 {
            bits.push(format!("disc {}/{}", self.disc, self.disc_total));
        }
        bits.push(format!("[{}]", self.source));
        bits.join(" ")
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string()
}

fn credit(v: &Value) -> (String, String) {
    let Some(arr) = v.get("artist-credit").and_then(|a| a.as_array()) else { return (String::new(), String::new()) };
    let name = arr.iter().map(|c| format!("{}{}", s(c, "name"), s(c, "joinphrase"))).collect::<String>();
    let id = arr.first().and_then(|c| c.get("artist")).map(|a| s(a, "id")).unwrap_or_default();
    (name, id)
}

fn top_genre(v: &Value) -> Option<String> {
    let g = v.get("genres")?.as_array()?;
    g.iter()
        .max_by_key(|x| x.get("count").and_then(|c| c.as_i64()).unwrap_or(0))
        .map(|x| s(x, "name"))
        .map(|n| {
            // title-case for display ("progressive rock" -> "Progressive Rock")
            n.split(' ')
                .map(|w| {
                    let mut c = w.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
}

/// MusicBrainz lookup by disc ID (falls back to fuzzy TOC matching).
pub fn musicbrainz(toc: &cdtoc::Toc) -> Result<Vec<AlbumCandidate>> {
    let disc_id = toc.musicbrainz_id().to_string();
    let audio = toc.audio_sectors();
    let toc_param = format!(
        "1+{}+{}+{}",
        audio.len(),
        toc.audio_leadout(),
        audio.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("+")
    );
    let url = |inc: &str| format!("https://musicbrainz.org/ws/2/discid/{disc_id}?toc={toc_param}&cdstubs=no&inc={inc}&fmt=json");
    let full = "artist-credits+recordings+release-groups+labels+isrcs+genres";
    let minimal = "artist-credits+recordings+release-groups+labels";
    let v = match mb_get(&url(full)) {
        Ok(v) => v,
        Err(e) if net::is_not_found(&e) => return Ok(vec![]),
        // if the server ever rejects an optional include, fall back to the essentials
        Err(e) if net::status_of(&e) == Some(400) => match mb_get(&url(minimal)) {
            Ok(v) => v,
            Err(e) if net::is_not_found(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        },
        Err(e) => return Err(e),
    };
    Ok(parse_musicbrainz(&v, &disc_id, audio.len()))
}

/// MusicBrainz allows about one request per second and answers 503 when busy or
/// when that is exceeded, so back off and retry a few times before giving up.
fn mb_get(url: &str) -> anyhow::Result<Value> {
    let waits = [1500u64, 3000, 5000];
    let mut attempt = 0;
    loop {
        match net::get_json(url) {
            Err(e) if net::is_transient(&e) && attempt < waits.len() => {
                std::thread::sleep(std::time::Duration::from_millis(waits[attempt]));
                attempt += 1;
            }
            r => return r,
        }
    }
}

pub fn parse_musicbrainz(v: &Value, disc_id: &str, n_tracks: usize) -> Vec<AlbumCandidate> {
    let releases = v.get("releases").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in releases {
        let media = r.get("media").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        // the medium that carries our disc ID, else the first one with the right track count
        let medium = media
            .iter()
            .find(|m| m.get("discs").and_then(|d| d.as_array()).map(|d| d.iter().any(|x| s(x, "id") == disc_id)).unwrap_or(false))
            .or_else(|| media.iter().find(|m| m.get("track-count").and_then(|c| c.as_u64()) == Some(n_tracks as u64)));
        let Some(medium) = medium else { continue };
        let (artist, artist_id) = credit(&r);
        let rg = r.get("release-group").cloned().unwrap_or(Value::Null);
        let li = r.get("label-info").and_then(|l| l.as_array()).and_then(|l| l.first()).cloned().unwrap_or(Value::Null);
        let tracks = medium
            .get("tracks")
            .and_then(|t| t.as_array())
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        let rec = t.get("recording").cloned().unwrap_or(Value::Null);
                        let (ta, taid) = credit(t);
                        CandTrack {
                            title: s(t, "title"),
                            artist: if ta.is_empty() { artist.clone() } else { ta },
                            artist_id: taid,
                            recording_id: s(&rec, "id"),
                            track_id: s(t, "id"),
                            isrc: rec.get("isrcs").and_then(|i| i.as_array()).and_then(|i| i.first()).and_then(|i| i.as_str()).unwrap_or_default().to_string(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(AlbumCandidate {
            source: "MusicBrainz".into(),
            release_id: s(&r, "id"),
            release_group_id: s(&rg, "id"),
            title: s(&r, "title"),
            artist,
            artist_id,
            date: s(&r, "date"),
            country: s(&r, "country"),
            label: li.get("label").map(|l| s(l, "name")).unwrap_or_default(),
            catalog: s(&li, "catalog-number"),
            barcode: s(&r, "barcode"),
            genre: top_genre(&r).or_else(|| top_genre(&rg)).unwrap_or_default(),
            disc: medium.get("position").and_then(|p| p.as_u64()).unwrap_or(1) as u32,
            disc_total: media.len().max(1) as u32,
            tracks,
            ..Default::default()
        });
    }
    out
}

/// GnuDB (freedb successor) fallback via the CDDB-over-HTTP protocol.
pub fn gnudb(toc: &cdtoc::Toc, contact: &str) -> Result<Vec<AlbumCandidate>> {
    let id = toc.cddb_id().to_string();
    let audio = toc.audio_sectors();
    let user = if contact.contains('@') { contact.replace('@', "+") } else { "coda+localhost".into() };
    let base = "https://gnudb.gnudb.org/~cddb/cddb.cgi";
    let hello = format!("&hello={}+coda+{}&proto=6", user, env!("CARGO_PKG_VERSION"));
    let q = format!(
        "{base}?cmd=cddb+query+{id}+{}+{}+{}{hello}",
        audio.len(),
        audio.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("+"),
        toc.leadout() / 75
    );
    let resp = net::get_text(&q)?;
    let mut lines = resp.lines();
    let first = lines.next().unwrap_or_default();
    let matches: Vec<(String, String)> = match &first.get(..3) {
        Some("200") => {
            let p: Vec<&str> = first.splitn(4, ' ').collect();
            vec![(p.get(1).unwrap_or(&"").to_string(), p.get(2).unwrap_or(&"").to_string())]
        }
        Some("210") | Some("211") => lines
            .take_while(|l| *l != ".")
            .filter_map(|l| {
                let p: Vec<&str> = l.splitn(3, ' ').collect();
                (p.len() >= 2).then(|| (p[0].to_string(), p[1].to_string()))
            })
            .collect(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for (cat, did) in matches.into_iter().take(5) {
        let r = net::get_text(&format!("{base}?cmd=cddb+read+{cat}+{did}{hello}"))?;
        let mut c = AlbumCandidate { source: "GnuDB".into(), disc: 1, disc_total: 1, ..Default::default() };
        let mut titles = vec![String::new(); audio.len()];
        let mut dtitle = String::new();
        for l in r.lines() {
            if let Some((k, v)) = l.split_once('=') {
                match k {
                    "DTITLE" => dtitle.push_str(v),
                    "DYEAR" => c.date = v.trim().into(),
                    "DGENRE" => c.genre = v.trim().into(),
                    k if k.starts_with("TTITLE") => {
                        if let Ok(n) = k[6..].parse::<usize>() {
                            if let Some(t) = titles.get_mut(n) {
                                t.push_str(v);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let (artist, album) = dtitle.split_once(" / ").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((String::new(), dtitle.clone()));
        c.artist = artist.clone();
        c.title = album;
        c.tracks = titles
            .into_iter()
            .map(|t| {
                // "Artist / Title" inside a track title marks a compilation
                let (ta, tt) = t.split_once(" / ").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((artist.clone(), t.clone()));
                CandTrack { title: tt, artist: ta, ..Default::default() }
            })
            .collect();
        out.push(c);
    }
    Ok(out)
}

// ------------------------------------------------------------------ artwork

fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()
}

/// Downscale oversize art and re-encode anything that is not JPEG/PNG-sane.
pub fn normalise_art(bytes: Vec<u8>, max_px: u32) -> Result<Vec<u8>> {
    let Some((w, h)) = dims(&bytes) else { bail!("unrecognised image") };
    let is_jpeg = bytes.starts_with(&[0xFF, 0xD8]);
    let too_big = max_px > 0 && w.max(h) > max_px;
    // large PNG scans bloat every file, so convert them to high quality JPEG
    if !too_big && (is_jpeg || bytes.len() < 1_500_000) {
        return Ok(bytes);
    }
    let mut img = image::load_from_memory(&bytes)?;
    if too_big {
        img = img.resize(max_px, max_px, image::imageops::FilterType::Lanczos3);
    }
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92);
    img.to_rgb8().write_with_encoder(enc)?;
    Ok(out)
}

fn caa(release_id: &str, group_id: &str) -> Option<Vec<u8>> {
    if !release_id.is_empty() {
        if let Ok(b) = net::get_bytes(&format!("https://coverartarchive.org/release/{release_id}/front")) {
            return Some(b);
        }
    }
    if !group_id.is_empty() {
        if let Ok(b) = net::get_bytes(&format!("https://coverartarchive.org/release-group/{group_id}/front")) {
            return Some(b);
        }
    }
    None
}

/// iTunes Search API exposes artwork at up to 3000x3000 by rewriting the size in the URL.
fn itunes(artist: &str, album: &str, px: u32) -> Option<Vec<u8>> {
    let term = net::urlencode(&format!("{artist} {album}"));
    let v = net::get_json(&format!("https://itunes.apple.com/search?term={term}&entity=album&limit=10")).ok()?;
    let results = v.get("results")?.as_array()?;
    let norm = |x: &str| x.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    let want = norm(album);
    let best = results
        .iter()
        .find(|r| norm(&s(r, "collectionName")) == want)
        .or_else(|| results.iter().find(|r| norm(&s(r, "collectionName")).contains(&want)))?;
    let url = s(best, "artworkUrl100").replace("100x100bb", &format!("{px}x{px}bb"));
    net::get_bytes(&url).ok()
}

/// Best available front cover, normalised. Prefers the Cover Art Archive
/// original; tries iTunes when CAA has nothing or only a small image.
pub fn fetch_cover(c: &AlbumCandidate, st: &Settings) -> Option<Vec<u8>> {
    let mut best = caa(&c.release_id, &c.release_group_id);
    let small = |b: &Option<Vec<u8>>| b.as_ref().and_then(|b| dims(b)).map(|(w, h)| w.min(h) < st.art_min_px).unwrap_or(true);
    let area = |b: &Vec<u8>| dims(b).map(|(w, h)| w as u64 * h as u64).unwrap_or(0);
    let consider = |alt: Option<Vec<u8>>, best: &mut Option<Vec<u8>>| {
        if let Some(alt) = alt {
            if best.as_ref().map(|b| area(&alt) > area(b)).unwrap_or(true) {
                *best = Some(alt);
            }
        }
    };
    if small(&best) {
        let px = if st.art_max_px == 0 { 3000 } else { st.art_max_px.min(3000) };
        consider(itunes(&c.artist, &c.title, px), &mut best);
    }
    // Discogs images are usually around 600 px, so they're the last resort
    if small(&best) && !c.cover_url.is_empty() {
        let auth = (!st.discogs_token.trim().is_empty()).then(|| format!("Discogs token={}", st.discogs_token.trim()));
        consider(net::get_bytes_with_auth(&c.cover_url, auth.as_deref()).ok(), &mut best);
    }
    best.and_then(|b| normalise_art(b, st.art_max_px).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_musicbrainz_release() {
        let v: Value = serde_json::from_str(r#"{
          "releases": [{
            "id": "rel-1", "title": "Big Album", "date": "1999-05-01", "country": "GB", "barcode": "5012345678900",
            "artist-credit": [{"name": "Main Act", "joinphrase": " feat. ", "artist": {"id": "a1"}}, {"name": "Guest", "joinphrase": "", "artist": {"id": "a2"}}],
            "release-group": {"id": "rg-1", "genres": [{"name": "trip hop", "count": 3}, {"name": "electronic", "count": 1}]},
            "label-info": [{"catalog-number": "CAT 001", "label": {"name": "Some Label"}}],
            "media": [
              {"position": 1, "track-count": 3, "discs": [{"id": "other"}], "tracks": []},
              {"position": 2, "track-count": 2, "discs": [{"id": "DISC2"}], "tracks": [
                {"id": "t1", "title": "Opener", "recording": {"id": "r1", "isrcs": ["GBAAA9900001"]}},
                {"id": "t2", "title": "Closer", "artist-credit": [{"name": "Someone Else", "joinphrase": "", "artist": {"id": "a9"}}], "recording": {"id": "r2"}}
              ]}
            ]
          }]
        }"#).unwrap();
        let c = &parse_musicbrainz(&v, "DISC2", 2)[0];
        assert_eq!(c.artist, "Main Act feat. Guest");
        assert_eq!((c.disc, c.disc_total), (2, 2));
        assert_eq!(c.genre, "Trip Hop");
        assert_eq!((c.label.as_str(), c.catalog.as_str()), ("Some Label", "CAT 001"));
        assert_eq!(c.tracks.len(), 2);
        assert_eq!(c.tracks[0].artist, "Main Act feat. Guest");
        assert_eq!(c.tracks[0].isrc, "GBAAA9900001");
        assert_eq!(c.tracks[1].artist, "Someone Else");
        assert_eq!(c.tracks[1].recording_id, "r2");
    }
}
