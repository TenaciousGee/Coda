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
    pub discogs_release_id: String,
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
        for (k, v) in [("CDTOC", &meta.cdtoc), ("ACCURATERIPRESULT", &meta.accuraterip), ("MUSICBRAINZ_DISCID", &meta.mb_disc_id), ("DISCOGS_RELEASE_ID", &meta.discogs_release_id)] {
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

// =================================================================== in-place update (tagger)

/// Fields Coda manages when re-tagging. Everything else in the file (ReplayGain,
/// lyrics, comments, custom fields, other pictures) is preserved.
fn apply_fields(tag: &mut Tag, m: &TrackMeta, cover: Option<&[u8]>) {
    // core fields: set, or remove when cleared
    for (k, v) in [
        (ItemKey::TrackTitle, &m.title),
        (ItemKey::TrackArtist, &m.artist),
        (ItemKey::AlbumTitle, &m.album),
        (ItemKey::AlbumArtist, &m.album_artist),
        (ItemKey::Genre, &m.genre),
        (ItemKey::RecordingDate, &m.year),
        (ItemKey::Composer, &m.composer),
    ] {
        if v.trim().is_empty() {
            tag.remove_key(k);
        } else {
            tag.insert_text(k, v.trim().to_string());
        }
    }
    if !m.year.trim().is_empty() {
        tag.remove_key(ItemKey::Year); // avoid a stale second date field
    }
    for (n, set, remove) in [
        (m.track, Tag::set_track as fn(&mut Tag, u32), Tag::remove_track as fn(&mut Tag)),
        (m.track_total, Tag::set_track_total, Tag::remove_track_total),
        (m.disc, Tag::set_disk, Tag::remove_disk),
        (m.disc_total, Tag::set_disk_total, Tag::remove_disk_total),
    ] {
        if n > 0 {
            set(tag, n);
        } else {
            remove(tag);
        }
    }
    // identifiers and release details: only ever added or updated, never removed
    for (k, v) in [
        (ItemKey::Isrc, &m.isrc),
        (ItemKey::Label, &m.label),
        (ItemKey::CatalogNumber, &m.catalog),
        (ItemKey::Barcode, &m.barcode),
        (ItemKey::MusicBrainzReleaseId, &m.mb_release_id),
        (ItemKey::MusicBrainzRecordingId, &m.mb_recording_id),
        (ItemKey::MusicBrainzTrackId, &m.mb_track_id),
        (ItemKey::MusicBrainzReleaseGroupId, &m.mb_release_group_id),
        (ItemKey::MusicBrainzArtistId, &m.mb_artist_id),
        (ItemKey::MusicBrainzReleaseArtistId, &m.mb_album_artist_id),
    ] {
        if !v.is_empty() {
            tag.insert_text(k, v.clone());
        }
    }
    if let Some(c) = cover {
        // M4A (and some taggers elsewhere) store covers untyped, so treat "Other" as the front cover too
        tag.remove_picture_type(PictureType::CoverFront);
        tag.remove_picture_type(PictureType::Other);
        tag.push_picture(front_picture(c));
    }
}

fn front_picture(c: &[u8]) -> Picture {
    let mime = if c.starts_with(&[0x89, b'P', b'N', b'G']) { MimeType::Png } else { MimeType::Jpeg };
    Picture::unchecked(c.to_vec()).pic_type(PictureType::CoverFront).mime_type(mime).build()
}

/// Normalises lofty getters that return `Option<&T>` (optional tags) or `&T`
/// (tags every file of that type has, like Ogg's Vorbis comments).
trait OptTag<T> {
    fn get_or_default(self) -> T;
}
impl<T: Clone + Default> OptTag<T> for Option<&T> {
    fn get_or_default(self) -> T {
        self.cloned().unwrap_or_default()
    }
}
impl<T: Clone> OptTag<T> for &T {
    fn get_or_default(self) -> T {
        self.clone()
    }
}

/// Update tags in place. `cover = Some(..)` replaces the front cover; `None` keeps
/// whatever artwork the file has.
pub fn update(path: &Path, m: &TrackMeta, cover: Option<&[u8]>, ffmpeg: &str) -> Result<()> {
    use lofty::config::ParseOptions;
    use lofty::file::FileType;
    use lofty::tag::{MergeTag, SplitTag};

    let ft = lofty::probe::Probe::open(path).ok().and_then(|p| p.guess_file_type().ok()).and_then(|p| p.file_type());
    let open_rw = || std::fs::OpenOptions::new().read(true).write(true).open(path).with_context(|| format!("opening {}", path.display()));

    // read the concrete file, split its tag into (unmanaged remainder, generic), edit
    // the generic part, merge back and save: unmanaged items survive untouched.
    macro_rules! via {
        ($file:ty, $get:ident, $set:ident, $tag:ty, $cover:expr, $post:expr) => {{
            let mut f = <$file>::read_from(&mut std::fs::File::open(path)?, ParseOptions::new())?;
            let existing: $tag = OptTag::get_or_default(f.$get());
            let (rem, mut generic) = existing.split_tag();
            apply_fields(&mut generic, m, $cover);
            let mut merged: $tag = rem.merge_tag(generic);
            #[allow(clippy::redundant_closure_call)]
            ($post)(&mut merged);
            f.$set(merged);
            f.save_to(&mut open_rw()?, WriteOptions::default())?;
            Ok(())
        }};
    }
    type Vc = lofty::ogg::tag::VorbisComments;
    type Id3 = lofty::id3::v2::Id3v2Tag;
    let discogs = |vc: &mut Vc| {
        if !m.discogs_release_id.is_empty() {
            vc.insert("DISCOGS_RELEASE_ID".to_string(), m.discogs_release_id.clone());
        }
    };
    let none_id3 = |_: &mut Id3| {};
    match ft {
        Some(FileType::Flac) => {
            // FLAC keeps pictures in their own blocks, outside the Vorbis comment
            let mut f = lofty::flac::FlacFile::read_from(&mut std::fs::File::open(path)?, ParseOptions::new())?;
            let existing = f.vorbis_comments().cloned().unwrap_or_default();
            let (rem, mut generic) = existing.split_tag();
            apply_fields(&mut generic, m, None);
            let mut merged: Vc = rem.merge_tag(generic);
            discogs(&mut merged);
            f.set_vorbis_comments(merged);
            if let Some(c) = cover {
                use lofty::ogg::OggPictureStorage;
                f.remove_picture_type(PictureType::CoverFront);
                let pic = front_picture(c);
                let info = lofty::picture::PictureInformation::from_picture(&pic).unwrap_or_default();
                f.insert_picture(pic, Some(info))?;
            }
            f.save_to(&mut open_rw()?, WriteOptions::default())?;
            Ok(())
        }
        Some(FileType::Opus) => via!(lofty::ogg::OpusFile, vorbis_comments, set_vorbis_comments, Vc, cover, discogs),
        Some(FileType::Vorbis) => via!(lofty::ogg::VorbisFile, vorbis_comments, set_vorbis_comments, Vc, cover, discogs),
        Some(FileType::Speex) => via!(lofty::ogg::SpeexFile, vorbis_comments, set_vorbis_comments, Vc, cover, discogs),
        Some(FileType::Mpeg) => via!(lofty::mpeg::MpegFile, id3v2, set_id3v2, Id3, cover, none_id3),
        Some(FileType::Wav) => via!(lofty::iff::wav::WavFile, id3v2, set_id3v2, Id3, cover, none_id3),
        Some(FileType::Aiff) => via!(lofty::iff::aiff::AiffFile, id3v2, set_id3v2, Id3, cover, none_id3),
        Some(FileType::Aac) => via!(lofty::aac::AacFile, id3v2, set_id3v2, Id3, cover, none_id3),
        Some(FileType::Mp4) => via!(lofty::mp4::Mp4File, ilst, set_ilst, lofty::mp4::Ilst, cover, |_: &mut lofty::mp4::Ilst| {}),
        Some(FileType::Ape) => via!(lofty::ape::ApeFile, ape, set_ape, lofty::ape::ApeTag, cover, |_: &mut lofty::ape::ApeTag| {}),
        Some(FileType::WavPack) => via!(lofty::wavpack::WavPackFile, ape, set_ape, lofty::ape::ApeTag, cover, |_: &mut lofty::ape::ApeTag| {}),
        Some(FileType::Mpc) => via!(lofty::musepack::MpcFile, ape, set_ape, lofty::ape::ApeTag, cover, |_: &mut lofty::ape::ApeTag| {}),
        _ => update_ffmpeg(path, m, cover, ffmpeg),
    }
}

/// WMA, TTA, Matroska and others lofty can't write: remux with ffmpeg (stream copy,
/// no re-encoding), keeping existing metadata and overriding the managed keys.
fn update_ffmpeg(path: &Path, m: &TrackMeta, cover: Option<&[u8]>, ffmpeg: &str) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
    let tmp = path.with_file_name(format!(".{}.coda-tag.{ext}", path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()));
    let cover_tmp = cover.map(|c| {
        let p = path.with_file_name(".coda-cover.jpg");
        let _ = std::fs::write(&p, c);
        p
    });
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"]).arg(path);
    if let Some(c) = &cover_tmp {
        cmd.arg("-i").arg(c).args(["-map", "0:a", "-map", "1:v", "-c", "copy", "-disposition:v", "attached_pic"]);
    } else {
        cmd.args(["-map", "0", "-c", "copy"]);
    }
    cmd.args(["-map_metadata", "0"]);
    let mut kv = |k: &str, v: String| {
        cmd.arg("-metadata").arg(format!("{k}={v}"));
    };
    kv("title", m.title.clone());
    kv("artist", m.artist.clone());
    kv("album", m.album.clone());
    kv("album_artist", m.album_artist.clone());
    kv("genre", m.genre.clone());
    kv("date", m.year.clone());
    kv("composer", m.composer.clone());
    kv("track", if m.track == 0 { String::new() } else if m.track_total > 0 { format!("{}/{}", m.track, m.track_total) } else { m.track.to_string() });
    kv("disc", if m.disc == 0 { String::new() } else if m.disc_total > 0 { format!("{}/{}", m.disc, m.disc_total) } else { m.disc.to_string() });
    let out = cmd.arg(&tmp).output().context("running ffmpeg")?;
    if let Some(c) = cover_tmp {
        let _ = std::fs::remove_file(c);
    }
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("ffmpeg could not rewrite the tags: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod update_tests {
    use super::*;
    use std::process::Command;

    fn ffprobe_all(p: &Path) -> String {
        let o = Command::new("ffprobe").args(["-v", "error", "-show_entries", "format_tags:stream_tags:stream=codec_type", "-of", "flat"]).arg(p).output().unwrap();
        String::from_utf8_lossy(&o.stdout).to_lowercase()
    }

    /// Re-tagging changes managed fields only: a custom field and the artwork survive.
    #[test]
    #[ignore] // needs ffmpeg; run with --ignored
    fn retag_preserves_unmanaged_data() {
        let dir = std::env::temp_dir().join(format!("coda-retag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cover = dir.join("c.jpg");
        Command::new("ffmpeg").args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=red:size=64x64", "-frames:v", "1"]).arg(&cover).status().unwrap();
        let cover_bytes = std::fs::read(&cover).unwrap();
        let blue = dir.join("b.jpg");
        Command::new("ffmpeg").args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=blue:size=80x80", "-frames:v", "1"]).arg(&blue).status().unwrap();
        let blue_bytes = std::fs::read(&blue).unwrap();
        let cases: &[(&str, &[&str], Format)] = &[
            ("flac", &["-c:a", "flac"], Format::Flac),
            ("mp3", &["-c:a", "libmp3lame", "-id3v2_version", "3"], Format::Mp3),
            ("m4a", &["-c:a", "aac"], Format::Aac),
            ("opus", &["-c:a", "libopus"], Format::Opus),
            ("ogg", &["-c:a", "libvorbis"], Format::Vorbis),
            ("wv", &["-c:a", "wavpack"], Format::WavPack),
            ("aiff", &["-c:a", "pcm_s16be", "-write_id3v2", "1"], Format::Aiff),
            ("wma", &["-c:a", "wmav2"], Format::Wma),
        ];
        let mut failures = Vec::new();
        for (ext, args, fmt) in cases {
            let p = dir.join(format!("t.{ext}"));
            let st = Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-ac", "2"])
                .args(*args)
                .args(["-metadata", "title=Old Title", "-metadata", "codakeep=preserve-me"])
                .arg(&p)
                .status()
                .unwrap();
            assert!(st.success(), "{ext}");
            if *ext == "m4a" {
                // custom M4A fields are iTunes freeform atoms (what Picard, Mp3tag and iTunes write)
                use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
                let mut f = Mp4File::read_from(&mut std::fs::File::open(&p).unwrap(), lofty::config::ParseOptions::new()).unwrap();
                let mut ilst: Ilst = f.ilst().cloned().unwrap_or_default();
                ilst.insert(Atom::new(
                    AtomIdent::Freeform { mean: "com.apple.iTunes".into(), name: "CODAKEEP".into() },
                    AtomData::UTF8("preserve-me".into()),
                ));
                f.set_ilst(ilst);
                f.save_to(&mut std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap(), WriteOptions::default()).unwrap();
            }
            let had_custom = ffprobe_all(&p).contains("preserve-me");
            // add artwork through the update path itself: the custom field must survive this too
            let base = TrackMeta { title: "Old Title".into(), ..Default::default() };
            update(&p, &base, Some(&cover_bytes), "ffmpeg").unwrap();
            let custom_after_cover = ffprobe_all(&p).contains("preserve-me");

            // 1) change title, keep cover
            let mut new = TrackMeta { title: "New Title".into(), artist: "Someone".into(), track: 3, track_total: 9, ..Default::default() };
            new.album = "Album".into();
            update(&p, &new, None, "ffmpeg").unwrap();
            let after = read(&p);
            let probe = ffprobe_all(&p);
            let title_ok = probe.contains("new title");
            let custom_ok = had_custom && custom_after_cover && probe.contains("preserve-me");
            let cover_kept = after.cover.as_ref().map(|c| c.len()).unwrap_or(0) > 0 || probe.contains("codec_type=\"video\"");
            // 2) replace cover
            update(&p, &new, Some(&blue_bytes), "ffmpeg").unwrap();
            let replaced = read(&p).cover.map(|c| c.as_slice() == blue_bytes.as_slice()).unwrap_or(!fmt.lofty_taggable());
            println!("{ext:5} title:{title_ok} custom-field:{custom_ok} (present before: {had_custom}) cover-kept:{cover_kept} cover-replaced:{replaced}");
            if !(title_ok && custom_ok && cover_kept && replaced) {
                failures.push(*ext);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(failures.is_empty(), "failed: {failures:?}");
    }
}
