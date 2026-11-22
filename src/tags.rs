//! Track metadata model plus reading/writing tags with lofty.

use crate::formats::Format;
use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagExt};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: u32,
    pub track_total: u32,
    pub disc: u32,
    pub disc_total: u32,
    pub year: String,
    pub genre: String,
    pub composer: String,
    pub comment: String,
    pub isrc: String,
    pub mb_release_id: String,
    pub mb_recording_id: String,
    pub mb_track_id: String,
    pub mb_release_group_id: String,
    pub mb_artist_id: String,
    pub mb_album_artist_id: String,
    pub mb_disc_id: String,
    pub label: String,
    pub catalog: String,
    pub barcode: String,
    pub cdtoc: String,
    pub accuraterip: String,
    /// Front cover, already normalised to JPEG/PNG bytes.
    pub cover: Option<Arc<Vec<u8>>>,
    /// Source duration in seconds, when known (used for progress).
    pub duration: f64,
}

impl TrackMeta {
    /// `ffmpeg -metadata` arguments; used for formats lofty cannot write (WMA, TTA)
    /// and as a first pass for everything else.
    pub fn ffmpeg_metadata_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut kv = |k: &str, v: &str| {
            if !v.is_empty() {
                out.push("-metadata".to_string());
                out.push(format!("{k}={v}"));
            }
        };
        kv("title", &self.title);
        kv("artist", &self.artist);
        kv("album", &self.album);
        kv("album_artist", &self.album_artist);
        if self.track > 0 {
            let t = if self.track_total > 0 { format!("{}/{}", self.track, self.track_total) } else { self.track.to_string() };
            kv("track", &t);
        }
        if self.disc > 0 {
            let d = if self.disc_total > 0 { format!("{}/{}", self.disc, self.disc_total) } else { self.disc.to_string() };
            kv("disc", &d);
        }
        kv("date", &self.year);
        kv("genre", &self.genre);
        kv("composer", &self.composer);
        kv("comment", &self.comment);
        out
    }

    pub fn year_only(&self) -> String {
        self.year.chars().take(4).collect()
    }
}

/// Read tags (and the front cover) from an existing audio file.
pub fn read(path: &Path) -> TrackMeta {
    let mut m = TrackMeta::default();
    let Ok(file) = lofty::read_from_path(path) else {
        return m; // caller falls back to read_ffmpeg
    };
    m.duration = file.properties().duration().as_secs_f64();
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        let s = |k: ItemKey| tag.get_string(k).unwrap_or_default().to_string();
        m.title = tag.title().map(|c| c.into_owned()).unwrap_or_default();
        m.artist = tag.artist().map(|c| c.into_owned()).unwrap_or_default();
        m.album = tag.album().map(|c| c.into_owned()).unwrap_or_default();
        m.genre = tag.genre().map(|c| c.into_owned()).unwrap_or_default();
        m.comment = tag.comment().map(|c| c.into_owned()).unwrap_or_default();
        m.track = tag.track().unwrap_or(0);
        m.track_total = tag.track_total().unwrap_or(0);
        m.disc = tag.disk().unwrap_or(0);
        m.disc_total = tag.disk_total().unwrap_or(0);
        m.album_artist = s(ItemKey::AlbumArtist);
        m.composer = s(ItemKey::Composer);
        m.year = s(ItemKey::RecordingDate);
        if m.year.is_empty() {
            m.year = s(ItemKey::Year);
        }
        if m.year.is_empty() {
            m.year = s(ItemKey::ReleaseDate);
        }
        m.isrc = s(ItemKey::Isrc);
        m.label = s(ItemKey::Label);
        m.catalog = s(ItemKey::CatalogNumber);
        m.barcode = s(ItemKey::Barcode);
        m.mb_release_id = s(ItemKey::MusicBrainzReleaseId);
        m.mb_recording_id = s(ItemKey::MusicBrainzRecordingId);
        m.mb_track_id = s(ItemKey::MusicBrainzTrackId);
        m.mb_release_group_id = s(ItemKey::MusicBrainzReleaseGroupId);
        m.mb_artist_id = s(ItemKey::MusicBrainzArtistId);
        m.mb_album_artist_id = s(ItemKey::MusicBrainzReleaseArtistId);
        let pics = tag.pictures();
        let front = pics.iter().find(|p| p.pic_type() == PictureType::CoverFront).or(pics.first());
        if let Some(p) = front {
            m.cover = Some(Arc::new(p.data().to_vec()));
        }
    }
    if m.cover.is_none() {
        m.cover = ape_cover(path).map(Arc::new);
    }
    if m.title.is_empty() {
        m.title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    m
}

/// Front cover from an APEv2 tag (WavPack, Monkey's Audio), which lofty's generic
/// `Tag` conversion doesn't carry over.
fn ape_cover(path: &Path) -> Option<Vec<u8>> {
    use lofty::config::ParseOptions;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mut f = std::fs::File::open(path).ok()?;
    let tag = match ext.as_str() {
        "wv" => lofty::wavpack::WavPackFile::read_from(&mut f, ParseOptions::new()).ok()?.ape()?.clone(),
        "ape" => lofty::ape::ApeFile::read_from(&mut f, ParseOptions::new()).ok()?.ape()?.clone(),
        _ => return None,
    };
    let item = tag.get("Cover Art (Front)")?;
    match item.value() {
        lofty::tag::ItemValue::Binary(b) => Picture::from_ape_bytes("Cover Art (Front)", b).ok().map(|p| p.data().to_vec()),
        _ => None,
    }
}

/// Tags via `ffmpeg -f ffmetadata` for containers lofty can't read (WMA, TTA, …).
pub fn read_ffmpeg(ffmpeg: &str, path: &Path, m: &mut TrackMeta) {
    let Ok(out) = std::process::Command::new(ffmpeg).args(["-hide_banner", "-nostdin", "-v", "error", "-i"]).arg(path).args(["-f", "ffmetadata", "-"]).output() else { return };
    let text = String::from_utf8_lossy(&out.stdout);
    let pair = |v: &str| -> (u32, u32) {
        let mut it = v.split('/');
        (it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0), it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0))
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.replace("\\=", "=").replace("\\;", ";");
        match k.to_ascii_lowercase().as_str() {
            "title" => m.title = v,
            "artist" => m.artist = v,
            "album" => m.album = v,
            "album_artist" | "albumartist" => m.album_artist = v,
            "genre" => m.genre = v,
            "composer" => m.composer = v,
            "comment" => m.comment = v,
            "date" | "year" => m.year = v,
            "track" => (m.track, m.track_total) = pair(&v),
            "disc" => (m.disc, m.disc_total) = pair(&v),
            _ => {}
        }
    }
}

/// Replace all tags in `path` with `meta` (including the cover).
pub fn write(path: &Path, format: Format, meta: &TrackMeta) -> Result<()> {
    if !format.lofty_taggable() {
        return Ok(()); // ffmpeg -metadata already handled it
    }
    let mut file = lofty::read_from_path(path).with_context(|| format!("re-reading {}", path.display()))?;
    let tag_type = file.primary_tag_type();
    let mut tag = Tag::new(tag_type);
    tag.set_title(meta.title.clone());
    let set = |t: &mut Tag, k: ItemKey, v: &str| {
        if !v.is_empty() {
            t.insert_text(k, v.to_string());
        }
    };
    set(&mut tag, ItemKey::TrackArtist, &meta.artist);
    set(&mut tag, ItemKey::AlbumTitle, &meta.album);
    set(&mut tag, ItemKey::AlbumArtist, &meta.album_artist);
    set(&mut tag, ItemKey::Genre, &meta.genre);
    set(&mut tag, ItemKey::Composer, &meta.composer);
    set(&mut tag, ItemKey::Comment, &meta.comment);
    set(&mut tag, ItemKey::RecordingDate, &meta.year);
    set(&mut tag, ItemKey::Isrc, &meta.isrc);
    set(&mut tag, ItemKey::Label, &meta.label);
    set(&mut tag, ItemKey::CatalogNumber, &meta.catalog);
    set(&mut tag, ItemKey::Barcode, &meta.barcode);
    set(&mut tag, ItemKey::MusicBrainzReleaseId, &meta.mb_release_id);
    set(&mut tag, ItemKey::MusicBrainzRecordingId, &meta.mb_recording_id);
    set(&mut tag, ItemKey::MusicBrainzTrackId, &meta.mb_track_id);
    set(&mut tag, ItemKey::MusicBrainzReleaseGroupId, &meta.mb_release_group_id);
    set(&mut tag, ItemKey::MusicBrainzArtistId, &meta.mb_artist_id);
    set(&mut tag, ItemKey::MusicBrainzReleaseArtistId, &meta.mb_album_artist_id);
    if meta.track > 0 {
        tag.set_track(meta.track);
    }
    if meta.track_total > 0 {
        tag.set_track_total(meta.track_total);
    }
    if meta.disc > 0 {
        tag.set_disk(meta.disc);
    }
    if meta.disc_total > 0 {
        tag.set_disk_total(meta.disc_total);
    }
    if let Some(cover) = &meta.cover {
        let mime = if cover.starts_with(&[0x89, b'P', b'N', b'G']) { MimeType::Png } else { MimeType::Jpeg };
        let pic = Picture::unchecked(cover.as_ref().clone()).pic_type(PictureType::CoverFront).mime_type(mime).build();
        tag.push_picture(pic);
    }
    if tag_type == lofty::tag::TagType::VorbisComments {
        // Vorbis comments take free-form keys: keep rip provenance alongside the standard tags.
        let mut vc = lofty::ogg::tag::VorbisComments::from(tag);
        for (k, v) in [("CDTOC", &meta.cdtoc), ("ACCURATERIPRESULT", &meta.accuraterip), ("MUSICBRAINZ_DISCID", &meta.mb_disc_id)] {
            if !v.is_empty() {
                vc.insert(k.to_string(), v.clone());
            }
        }
        vc.save_to_path(path, WriteOptions::default()).with_context(|| format!("tagging {}", path.display()))?;
    } else if tag_type == lofty::tag::TagType::Ape {
        // the generic writer drops pictures for APE; the dedicated type keeps them
        let ape = lofty::ape::ApeTag::from(tag);
        ape.save_to_path(path, WriteOptions::default()).with_context(|| format!("tagging {}", path.display()))?;
    } else {
        file.clear();
        file.insert_tag(tag);
        file.save_to_path(path, WriteOptions::default()).with_context(|| format!("tagging {}", path.display()))?;
    }
    Ok(())
}
