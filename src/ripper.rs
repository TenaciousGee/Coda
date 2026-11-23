//! Disc loading and the secure rip engine.
//!
//! Strategy (the same idea dBpoweramp uses): read each track at full speed once
//! with the drive's read offset corrected. If the AccurateRip checksum matches the
//! database, the rip is proven bit-perfect and we move on. Otherwise every sector
//! is read again after defeating the drive cache; sectors that differ between
//! passes (or that the drive flagged with C2 errors) are re-read until the same
//! data comes back `required_matches` times, or `max_retries` is reached.

use crate::accuraterip::{self, ArData, ArResult, TrackCrc};
use crate::cdrom::{DiscToc, Drive, MAX_CHUNK, SAMPLES_PER_SECTOR, SECTOR_BYTES};
use crate::config::{expand_home, Preset, Settings, VerifyMode};
use crate::encoder::{EncodeJob, EncodeSettings, Outcome, Pool, PoolEvent, TempFile};
use crate::metadata::{self, AlbumCandidate};
use crate::naming::{destination, NameContext};
use crate::tags::TrackMeta;
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct DiscInfo {
    pub device: PathBuf,
    pub vendor: String,
    pub model: String,
    pub drive_id: String,
    pub toc: DiscToc,
    pub cdtoc: cdtoc::Toc,
    pub offset: Option<i32>,
    pub offset_source: String,
    /// `Some(None)` = looked up, disc not in database. `None` = lookup failed.
    pub ar: Option<Option<ArData>>,
    pub candidates: Vec<AlbumCandidate>,
    pub mcn: Option<String>,
    pub isrcs: Vec<Option<String>>,
    pub c2: bool,
    pub notes: Vec<String>,
}

impl DiscInfo {
    pub fn track_count(&self) -> usize {
        self.toc.audio_tracks().len()
    }
    pub fn track_seconds(&self, idx: usize) -> f64 {
        let (a, b) = self.toc.audio_range(idx);
        (b - a) as f64 / 75.0
    }
    pub fn ar_data(&self) -> Option<&ArData> {
        self.ar.as_ref().and_then(|a| a.as_ref())
    }
}

/// Read the disc, identify the drive offset, query AccurateRip and metadata.
pub fn load_disc(device: &Path, st: &Settings, status: &dyn Fn(&str)) -> Result<DiscInfo> {
    status("Reading table of contents…");
    let drive = Drive::open(device)?;
    let toc = drive.read_toc()?;
    let cdtoc = toc.to_cdtoc()?;
    let mut notes = Vec::new();

    status("Looking up AccurateRip…");
    let ar = match accuraterip::lookup(&cdtoc) {
        Ok(a) => Some(a),
        Err(e) => {
            notes.push(format!("{}. Rips will be verified by secure re-reading instead.", crate::net::describe("AccurateRip", &e)));
            None
        }
    };

    let drive_id = drive.id();
    let (mut offset, mut offset_source) = match st.drive_offsets.get(&drive_id) {
        Some(o) => (Some(*o), "saved".to_string()),
        None => (None, String::new()),
    };
    if offset.is_none() {
        status("Looking up drive read offset…");
        match accuraterip::drive_offset_from_db(&drive.vendor, &drive.model) {
            Ok(Some(o)) => {
                offset = Some(o as i32);
                offset_source = "AccurateRip drive database".into();
            }
            Ok(None) => notes.push(format!("{} {} is not in the AccurateRip drive offset list", drive.vendor, drive.model)),
            Err(e) => notes.push(format!("{} (drive offset list).", crate::net::describe("AccurateRip", &e))),
        }
    }
    if offset.is_none() {
        if let Some(Some(ar)) = &ar {
            status("Detecting read offset from this disc (key disc)…");
            match detect_offset(&drive, &toc, ar) {
                Ok(Some((o, conf))) => {
                    offset = Some(o);
                    offset_source = format!("detected with this disc (confidence {conf})");
                }
                Ok(None) => notes.push("Could not detect the read offset with this disc; try another popular CD".into()),
                Err(e) => notes.push(format!("Offset detection didn't finish: {}", e.root_cause())),
            }
        }
    }

    let mcn = drive.mcn();
    let isrcs = toc.audio_tracks().iter().map(|t| drive.isrc(t.number)).collect();
    let c2 = st.use_c2 && drive.supports_c2(toc.audio_tracks()[0].lba);

    let mut candidates = Vec::new();
    let mut mb_failed = false;
    if st.musicbrainz {
        status("Searching MusicBrainz…");
        match metadata::musicbrainz(&cdtoc) {
            Ok(c) => candidates.extend(c),
            Err(e) => {
                mb_failed = true;
                notes.push(format!("{}. Press Reload to try again.", crate::net::describe("MusicBrainz", &e)));
            }
        }
    }
    if candidates.is_empty() && st.gnudb_fallback {
        status("Searching GnuDB…");
        match metadata::gnudb(&cdtoc, &st.contact_email) {
            Ok(c) => candidates.extend(c),
            Err(e) => notes.push(format!("{}.", crate::net::describe("GnuDB", &e))),
        }
    }
    // Discogs when MusicBrainz had nothing: by the disc's own barcode, else by the
    // artist/album GnuDB suggested (Discogs adds label, catalogue number, styles, art).
    let has_mb = candidates.iter().any(|c| c.source == "MusicBrainz");
    if !has_mb && st.discogs_fallback {
        let query = mcn.clone().map(crate::discogs::Query::Barcode).or_else(|| {
            candidates.first().map(|c| crate::discogs::Query::Text { artist: c.artist.clone(), album: c.title.clone() })
        });
        if query.is_none() {
            notes.push("This disc has no barcode for Discogs: type the album and artist, then press Search Discogs.".into());
        }
        if let Some(q) = query {
            status("Searching Discogs…");
            let lengths: Vec<f64> = (0..toc.audio_tracks().len()).map(|i| {
                let (a, b) = toc.audio_range(i);
                (b - a) as f64 / 75.0
            }).collect();
            match crate::discogs::search(&q, &lengths, &st.discogs_token) {
                // Discogs results are richer than GnuDB's, so list them first
                Ok(found) => {
                    let mut merged = found;
                    merged.append(&mut candidates);
                    candidates = merged;
                }
                Err(e) => notes.push(format!("{}.", crate::net::describe("Discogs", &e))),
            }
        }
    }
    if candidates.is_empty() && !mb_failed && st.musicbrainz && !notes.iter().any(|n| n.contains("Search Discogs")) {
        notes.insert(0, "No online match for this disc yet: type the details in, or press Search Discogs.".into());
    }
    Ok(DiscInfo {
        device: device.into(),
        vendor: drive.vendor.clone(),
        model: drive.model.clone(),
        drive_id,
        toc,
        cdtoc,
        offset,
        offset_source,
        ar,
        candidates,
        mcn,
        isrcs,
        c2,
        notes,
    })
}

// ------------------------------------------------------------- low level reads

struct Reader<'a> {
    drive: &'a Drive,
    /// Physical sectors outside [0, limit) cannot be read and are returned as silence.
    limit: u32,
    c2: bool,
}

struct Block {
    data: Vec<u8>,
    /// per sector: C2 flagged or unreadable
    suspect: Vec<bool>,
    errors: u32,
}

impl Reader<'_> {
    /// Read physical sectors [s0, s1), may start negative.
    fn read(&self, s0: i64, s1: i64, cancel: &AtomicBool, prog: &dyn Fn(f32)) -> Result<Block> {
        let n = (s1 - s0) as usize;
        let mut data = vec![0u8; n * SECTOR_BYTES];
        let mut suspect = vec![false; n];
        let mut errors = 0;
        let mut s = s0;
        while s < s1 {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            if s < 0 || s >= self.limit as i64 {
                s += 1;
                continue;
            }
            let count = (MAX_CHUNK as i64).min(s1 - s).min(self.limit as i64 - s) as u32;
            let at = (s - s0) as usize;
            match self.drive.read_cdda(s as u32, count, self.c2) {
                Ok((audio, c2)) => {
                    data[at * SECTOR_BYTES..(at + count as usize) * SECTOR_BYTES].copy_from_slice(&audio);
                    if let Some(c2) = c2 {
                        for (i, e) in c2.chunks_exact(crate::cdrom::C2_BYTES).enumerate() {
                            if e.iter().any(|&b| b != 0) {
                                suspect[at + i] = true;
                            }
                        }
                    }
                }
                Err(_) => {
                    // fall back to single sectors so one bad sector doesn't sink the chunk
                    for i in 0..count {
                        let ok = (0..3).find_map(|_| self.drive.read_cdda(s as u32 + i, 1, self.c2).ok());
                        let k = at + i as usize;
                        match ok {
                            Some((audio, c2)) => {
                                data[k * SECTOR_BYTES..(k + 1) * SECTOR_BYTES].copy_from_slice(&audio);
                                suspect[k] = c2.map(|c| c.iter().any(|&b| b != 0)).unwrap_or(false);
                            }
                            None => {
                                suspect[k] = true;
                                errors += 1;
                            }
                        }
                    }
                }
            }
            s += count as i64;
            prog((s - s0) as f32 / n as f32);
        }
        Ok(Block { data, suspect, errors })
    }

    /// Push the drive's cache away from `near` by reading somewhere far off.
    fn defeat_cache(&self, near: i64) {
        let far: i64 = if near > self.limit as i64 / 2 { 0 } else { self.limit as i64 - 2 * MAX_CHUNK as i64 - 1 };
        let far = far.max(0) as u32;
        let _ = self.drive.read_cdda(far, MAX_CHUNK, false);
        let _ = self.drive.read_cdda(far + MAX_CHUNK, MAX_CHUNK.min(self.limit.saturating_sub(far + MAX_CHUNK)).max(1), false);
    }
}

fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}
fn ceil_div(a: i64, b: i64) -> i64 {
    -(-a).div_euclid(b)
}

/// Raw samples [first, first+count) at offset 0 (used for offset detection).
fn read_physical_samples(r: &Reader, first: i64, count: usize, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let s0 = floor_div(first, SAMPLES_PER_SECTOR as i64);
    let s1 = ceil_div(first + count as i64, SAMPLES_PER_SECTOR as i64);
    let b = r.read(s0, s1, cancel, &|_| {})?;
    let start = ((first - s0 * SAMPLES_PER_SECTOR as i64) * 4) as usize;
    Ok(b.data[start..start + count * 4].to_vec())
}

/// Key-disc read offset detection: compare frame-450 CRCs from the database
/// against every candidate offset within ±2940 samples, over up to 3 tracks.
pub fn detect_offset(drive: &Drive, toc: &DiscToc, ar: &ArData) -> Result<Option<(i32, u32)>> {
    const RANGE: i32 = 2940;
    let r = Reader { drive, limit: toc.audio_end(), c2: false };
    let cancel = AtomicBool::new(false);
    let mut votes: HashMap<i32, (u32, u32)> = HashMap::new(); // offset -> (confidence, tracks)
    let mut tried = 0;
    for idx in 0..toc.audio_tracks().len() {
        let (a, b) = toc.audio_range(idx);
        let Some(entries) = ar.tracks.get(idx) else { continue };
        if b - a < 460 || entries.iter().all(|e| e.crc450 == 0) {
            continue;
        }
        let first = a as i64 * SAMPLES_PER_SECTOR as i64 + 450 * SAMPLES_PER_SECTOR as i64 - RANGE as i64;
        let window = read_physical_samples(&r, first, (2 * RANGE) as usize + SAMPLES_PER_SECTOR + 1, &cancel)?;
        for (off, conf) in accuraterip::detect_offsets(&window, RANGE, entries) {
            let v = votes.entry(off).or_default();
            v.0 += conf;
            v.1 += 1;
        }
        tried += 1;
        if tried >= 3 {
            break;
        }
    }
    let need_tracks = if tried >= 2 { 2 } else { 1 };
    Ok(votes
        .into_iter()
        .filter(|(_, (_, t))| *t >= need_tracks)
        .max_by_key(|(_, (c, t))| (*t, *c))
        .map(|(o, (c, _))| (o, c)))
}

// ------------------------------------------------------------- track rip

pub struct TrackRead {
    pub pcm: Vec<u8>,
    pub crc: TrackCrc,
    pub ar: ArResult,
    pub copy_crc: u32,
    pub resolved: u32,
    pub unresolved: u32,
    pub read_errors: u32,
    pub passes: u32,
}

impl TrackRead {
    pub fn status(&self) -> String {
        if self.ar.is_accurate() {
            self.ar.describe()
        } else if self.unresolved > 0 || self.read_errors > 0 {
            format!("Suspect: {} sectors could not be verified", self.unresolved.max(self.read_errors))
        } else if self.passes >= 2 {
            format!("Secure ({}){}", self.ar.describe(), if self.resolved > 0 { format!(", {} sectors re-read", self.resolved) } else { String::new() })
        } else {
            format!("Unverified ({})", self.ar.describe())
        }
    }
}

pub struct RipParams {
    pub offset: i32,
    pub mode: VerifyMode,
    pub max_retries: u32,
    pub required_matches: u32,
    pub c2: bool,
}

pub fn read_track(
    drive: &Drive,
    toc: &DiscToc,
    idx: usize,
    ar: Option<&ArData>,
    p: &RipParams,
    cancel: &AtomicBool,
    prog: &dyn Fn(f32, &str),
) -> Result<TrackRead> {
    let n_tracks = toc.audio_tracks().len();
    let (a, b) = toc.audio_range(idx);
    let sps = SAMPLES_PER_SECTOR as i64;
    // physical sample range = true range shifted by the drive's read offset
    let p0 = a as i64 * sps + p.offset as i64;
    let p1 = b as i64 * sps + p.offset as i64;
    let s0 = floor_div(p0, sps);
    let s1 = ceil_div(p1, sps);
    let cut = ((p0 - s0 * sps) * 4) as usize;
    let len = ((p1 - p0) * 4) as usize;
    let r = Reader { drive, limit: toc.audio_end(), c2: p.c2 };
    let check = |data: &[u8]| -> (TrackCrc, ArResult) {
        let crc = accuraterip::track_crc(&data[cut..cut + len], idx == 0, idx + 1 == n_tracks);
        let res = ar.map(|d| d.check(idx, crc)).unwrap_or(ArResult::NotInDatabase);
        (crc, res)
    };

    let first_share = if p.mode == VerifyMode::AlwaysSecure { 0.45 } else { 0.9 };
    let mut pass1 = r.read(s0, s1, cancel, &|f| prog(f * first_share, "Ripping"))?;
    let (crc, res) = check(&pass1.data);
    let done = |data: Vec<u8>, crc, res, resolved, unresolved, errors, passes| {
        let pcm = data[cut..cut + len].to_vec();
        let copy_crc = crc32(&pcm);
        TrackRead { pcm, crc, ar: res, copy_crc, resolved, unresolved, read_errors: errors, passes }
    };
    let clean = pass1.errors == 0 && !pass1.suspect.iter().any(|&s| s);
    if p.mode == VerifyMode::Burst || (p.mode == VerifyMode::AccurateRipThenSecure && res.is_accurate() && clean) {
        prog(1.0, "Ripped");
        return Ok(done(pass1.data, crc, res, 0, 0, pass1.errors, 1));
    }

    // ---- pass 2: full re-read with the cache defeated
    r.defeat_cache(s0);
    let base = if p.mode == VerifyMode::AlwaysSecure { 0.45 } else { 0.5 };
    let pass2 = r.read(s0, s1, cancel, &|f| prog(base + f * 0.4, "Verifying"))?;
    let n = (s1 - s0) as usize;
    let sec = |d: &[u8], i: usize| d[i * SECTOR_BYTES..(i + 1) * SECTOR_BYTES].to_vec();
    let mut suspects: Vec<usize> = (0..n)
        .filter(|&i| pass1.suspect[i] || pass2.suspect[i] || pass1.data[i * SECTOR_BYTES..(i + 1) * SECTOR_BYTES] != pass2.data[i * SECTOR_BYTES..(i + 1) * SECTOR_BYTES])
        .collect();
    // per suspect sector: distinct readings -> times seen
    let mut seen: HashMap<usize, Vec<(Vec<u8>, u32)>> = HashMap::new();
    let add = |seen: &mut HashMap<usize, Vec<(Vec<u8>, u32)>>, i: usize, d: Vec<u8>, trusted: bool| {
        if !trusted {
            return;
        }
        let v = seen.entry(i).or_default();
        match v.iter_mut().find(|(x, _)| *x == d) {
            Some(e) => e.1 += 1,
            None => v.push((d, 1)),
        }
    };
    for &i in &suspects {
        add(&mut seen, i, sec(&pass1.data, i), !pass1.suspect[i]);
        add(&mut seen, i, sec(&pass2.data, i), !pass2.suspect[i]);
    }
    let total_suspect = suspects.len().max(1);
    let initial = suspects.len();
    let mut round = 0;
    while !suspects.is_empty() && round < p.max_retries {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        round += 1;
        prog(0.9 + 0.1 * (1.0 - suspects.len() as f32 / total_suspect as f32), &format!("Re-reading {} sectors (try {round})", suspects.len()));
        r.defeat_cache(s0 + suspects[0] as i64);
        // read contiguous runs of suspects
        let mut k = 0;
        while k < suspects.len() {
            let start = suspects[k];
            let mut end = start + 1;
            while k + (end - start) < suspects.len() && suspects[k + (end - start)] == end && end - start < MAX_CHUNK as usize {
                end += 1;
            }
            let blk = r.read(s0 + start as i64, s0 + end as i64, cancel, &|_| {})?;
            for (j, i) in (start..end).enumerate() {
                add(&mut seen, i, sec(&blk.data, j), !blk.suspect[j]);
            }
            k += end - start;
        }
        suspects.retain(|i| seen.get(i).map(|v| v.iter().map(|e| e.1).max().unwrap_or(0) < p.required_matches).unwrap_or(true));
    }
    // commit the most frequent reading of every suspect sector
    for (i, v) in &seen {
        if let Some((d, _)) = v.iter().max_by_key(|e| e.1) {
            pass1.data[i * SECTOR_BYTES..(i + 1) * SECTOR_BYTES].copy_from_slice(d);
        }
    }
    let unresolved = suspects.len() as u32;
    let (crc, res) = check(&pass1.data);
    prog(1.0, "Ripped");
    Ok(done(pass1.data, crc, res, (initial as u32).saturating_sub(unresolved), unresolved, pass1.errors.min(pass2.errors), 2))
}

// ------------------------------------------------------------- utilities

pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

pub fn write_wav(path: &Path, pcm: &[u8]) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let len = pcm.len() as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // channels
    f.write_all(&44_100u32.to_le_bytes())?;
    f.write_all(&(44_100u32 * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&len.to_le_bytes())?;
    f.write_all(pcm)?;
    f.flush()?;
    Ok(())
}

fn msf(sectors: u32) -> String {
    format!("{:02}:{:02}:{:02}", sectors / 75 / 60, (sectors / 75) % 60, sectors % 75)
}

// ------------------------------------------------------------- full rip

#[derive(Debug, Clone)]
pub enum RipEvent {
    Track { idx: usize, text: String, progress: f32 },
    Status(String),
    Finished { ok: bool, summary: String },
}

pub type RipSink = Arc<dyn Fn(RipEvent) + Send + Sync>;

pub struct RipRequest {
    pub info: DiscInfo,
    /// One entry per audio track (cover included).
    pub metas: Vec<TrackMeta>,
    pub selected: Vec<bool>,
    pub preset: Preset,
    pub settings: Settings,
}

pub fn rip(req: RipRequest, cancel: Arc<AtomicBool>, sink: RipSink) {
    let r = rip_inner(&req, &cancel, &sink);
    match r {
        Ok(summary) => sink(RipEvent::Finished { ok: true, summary }),
        Err(e) => sink(RipEvent::Finished { ok: false, summary: format!("{e:#}") }),
    }
}

fn rip_inner(req: &RipRequest, cancel: &Arc<AtomicBool>, sink: &RipSink) -> Result<String> {
    let info = &req.info;
    let st = &req.settings;
    let offset = info.offset.ok_or_else(|| anyhow!("The drive read offset is unknown. Set it in Settings, or insert a popular CD so it can be detected."))?;
    let outputs: Vec<_> = req.preset.outputs.iter().filter(|o| o.enabled).cloned().collect();
    if outputs.is_empty() {
        bail!("Preset '{}' has no enabled outputs", req.preset.name);
    }
    let drive = Drive::open(&info.device)?;
    drive.lock(true);
    let tmpdir = crate::config::cache_dir().join(format!("rip-{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir)?;

    // encode progress per track: fractions per output
    let n = info.track_count();
    let enc_state: Arc<Mutex<HashMap<usize, Vec<f32>>>> = Arc::new(Mutex::new(HashMap::new()));
    let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let written: Arc<Mutex<HashMap<(usize, usize), PathBuf>>> = Arc::new(Mutex::new(HashMap::new()));
    let ar_text: Arc<Mutex<HashMap<usize, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let n_out = outputs.len();
    let pool_sink = {
        let sink = sink.clone();
        let enc_state = enc_state.clone();
        let failures = failures.clone();
        let written = written.clone();
        let ar_text = ar_text.clone();
        Arc::new(move |ev: PoolEvent| {
            let (id, frac, fin) = match &ev {
                PoolEvent::Started { id } => (*id, 0.0, None),
                PoolEvent::Progress { id, fraction } => (*id, *fraction, None),
                PoolEvent::Finished { id, result } => (*id, 1.0, Some(result.clone())),
            };
            let (t, o) = (id / 100, id % 100);
            if let Some(Err(e)) = &fin {
                failures.lock().unwrap().push(format!("Track {} output {}: {e}", t + 1, o + 1));
            }
            if let Some(Ok(Outcome::Written(p, _) | Outcome::Skipped(p))) = &fin {
                written.lock().unwrap().insert((t, o), p.clone());
            }
            let mut m = enc_state.lock().unwrap();
            let v = m.entry(t).or_insert_with(|| vec![0.0; n_out]);
            v[o] = frac;
            let avg = v.iter().sum::<f32>() / n_out as f32;
            let all_done = v.iter().all(|&x| x >= 1.0);
            drop(m);
            let ar = ar_text.lock().unwrap().get(&t).cloned().unwrap_or_default();
            let text = if all_done { format!("Done · {ar}") } else { format!("Encoding · {ar}") };
            sink(RipEvent::Track { idx: t, text, progress: avg });
        }) as crate::encoder::EventSink
    };
    let pool = Pool::new(
        st.worker_threads(),
        EncodeSettings { ffmpeg: st.ffmpeg.clone(), existing: st.existing.clone(), verify_lossless: st.verify_lossless },
        pool_sink,
        cancel.clone(),
    );

    let params = RipParams { offset, mode: st.verify_mode.clone(), max_retries: st.max_retries, required_matches: st.required_matches.max(1), c2: info.c2 };
    let mut log = String::new();
    let mut reads: Vec<Option<(TrackCrc, ArResult, u32, String)>> = vec![None; n];
    let mut dests: Vec<Vec<PathBuf>> = vec![Vec::new(); n];

    for idx in 0..n {
        if !req.selected.get(idx).copied().unwrap_or(false) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let s = sink.clone();
        let tr = read_track(&drive, &info.toc, idx, info.ar_data(), &params, cancel, &move |f, what| {
            s(RipEvent::Track { idx, text: what.to_string(), progress: f });
        })?;
        let status = tr.status();
        ar_text.lock().unwrap().insert(idx, status.clone());
        sink(RipEvent::Track { idx, text: format!("Encoding · {status}"), progress: 0.0 });

        let wav = tmpdir.join(format!("track{:02}.wav", idx + 1));
        write_wav(&wav, &tr.pcm)?;
        let temp = Arc::new(TempFile(wav.clone()));
        let mut meta = req.metas[idx].clone();
        meta.duration = tr.pcm.len() as f64 / 4.0 / 44_100.0;
        meta.accuraterip = status.clone();
        for (o_i, o) in outputs.iter().enumerate() {
            let ctx = NameContext { meta: &meta, filename: "", reldir: "", format: o.format.label() };
            let out = destination(&expand_home(&o.dest_root), &o.template, &ctx, o.format.extension(), st.portable_filenames);
            dests[idx].push(out.clone());
            pool.submit(
                idx * 100 + o_i,
                EncodeJob { input: wav.clone(), output: out, format: o.format, options: o.options.clone(), meta: meta.clone(), temp_input: Some(temp.clone()) },
            );
        }
        reads[idx] = Some((tr.crc, tr.ar.clone(), tr.copy_crc, status));
        drop(temp);
    }
    drive.lock(false);
    sink(RipEvent::Status("Finishing encodes…".into()));
    pool.join();
    let _ = std::fs::remove_dir_all(&tmpdir);

    if cancel.load(Ordering::Relaxed) {
        bail!("Rip cancelled");
    }

    // ---- log, cue, cover file per output folder
    let album = req.metas.first().cloned().unwrap_or_default();
    let written = written.lock().unwrap().clone();
    let all_selected = req.selected.iter().all(|&s| s);
    let mut folders: Vec<PathBuf> = Vec::new();
    for o_i in 0..n_out {
        if let Some(dir) = (0..n).find_map(|t| written.get(&(t, o_i)).and_then(|p| p.parent().map(|d| d.to_path_buf()))) {
            if !folders.contains(&dir) {
                folders.push(dir.clone());
                if st.save_cover_file {
                    if let Some(c) = &album.cover {
                        let _ = std::fs::write(dir.join(&st.cover_file_name), c.as_slice());
                    }
                }
                if st.write_cue && all_selected {
                    let cue = cue_sheet(req, &|t| written.get(&(t, o_i)).cloned());
                    let _ = std::fs::write(dir.join(format!("{}.cue", crate::naming::sanitize_component(&album.album, true))), cue);
                }
            }
        }
    }

    let accurate = reads.iter().flatten().filter(|r| r.1.is_accurate()).count();
    let ripped = reads.iter().flatten().count();
    let fails = failures.lock().unwrap().clone();
    log.push_str(&format!(
        "Coda {} rip log · {}\n\nDrive: {} {} ({})\nRead offset: {offset:+} ({})\nVerification: {:?}, max retries {}, matches required {}, C2 {}\n\
         Album: {} – {}\nAccurateRip ID: {}\nMusicBrainz disc ID: {}\nCDDB ID: {}\n{}\nPreset: {}\n\n",
        env!("CARGO_PKG_VERSION"),
        chrono::Local::now().format("%Y-%m-%d %H:%M"),
        info.vendor,
        info.model,
        info.device.display(),
        info.offset_source,
        st.verify_mode,
        st.max_retries,
        st.required_matches,
        if info.c2 { "on" } else { "off" },
        album.album_artist,
        album.album,
        info.cdtoc.accuraterip_id(),
        info.cdtoc.musicbrainz_id(),
        info.cdtoc.cddb_id(),
        info.mcn.as_ref().map(|m| format!("MCN: {m}\n")).unwrap_or_default(),
        req.preset.name
    ));
    for (idx, r) in reads.iter().enumerate() {
        let Some((crc, ar, copy, status)) = r else { continue };
        let (a, b) = info.toc.audio_range(idx);
        log.push_str(&format!(
            "Track {:2}  {} – {}  [{}]\n  AR v1 {:08X}  AR v2 {:08X}  Copy CRC {:08X}\n  {}\n  {}\n",
            idx + 1,
            msf(a),
            msf(b - a),
            req.metas[idx].title,
            crc.v1,
            crc.v2,
            copy,
            status,
            ar.describe_log()
        ));
        for p in &dests[idx] {
            log.push_str(&format!("  → {}\n", p.display()));
        }
    }
    log.push_str(&format!("\n{accurate} of {ripped} tracks accurately ripped (AccurateRip).\n"));
    if !fails.is_empty() {
        log.push_str("\nEncoding problems:\n");
        for f in &fails {
            log.push_str(&format!("  {f}\n"));
        }
    }
    if st.write_log {
        for d in &folders {
            let _ = std::fs::write(d.join(format!("{}.log", crate::naming::sanitize_component(&album.album, true))), &log);
        }
    }
    if st.eject_after_rip {
        let _ = drive.eject();
    }
    let mut summary = format!("{ripped} tracks ripped, {accurate} verified accurate by AccurateRip");
    if !fails.is_empty() {
        summary.push_str(&format!("; {} encode(s) failed", fails.len()));
    }
    Ok(summary)
}

fn cue_sheet(req: &RequestLike, file_of: &dyn Fn(usize) -> Option<PathBuf>) -> String {
    let info = &req.info;
    let album = req.metas.first().cloned().unwrap_or_default();
    let q = |s: &str| s.replace('"', "'");
    let mut c = String::new();
    if !album.genre.is_empty() {
        c.push_str(&format!("REM GENRE \"{}\"\n", q(&album.genre)));
    }
    if !album.year.is_empty() {
        c.push_str(&format!("REM DATE {}\n", album.year_only()));
    }
    c.push_str(&format!("REM DISCID {}\nREM COMMENT \"Coda {}\"\n", info.cdtoc.cddb_id(), env!("CARGO_PKG_VERSION")));
    if let Some(m) = &info.mcn {
        c.push_str(&format!("CATALOG {m}\n"));
    }
    c.push_str(&format!("PERFORMER \"{}\"\nTITLE \"{}\"\n", q(&album.album_artist), q(&album.album)));
    for (i, m) in req.metas.iter().enumerate() {
        let name = file_of(i).and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_default();
        c.push_str(&format!("FILE \"{}\" WAVE\n  TRACK {:02} AUDIO\n    TITLE \"{}\"\n    PERFORMER \"{}\"\n", q(&name), i + 1, q(&m.title), q(&m.artist)));
        if let Some(Some(isrc)) = info.isrcs.get(i) {
            c.push_str(&format!("    ISRC {isrc}\n"));
        }
        c.push_str("    INDEX 01 00:00:00\n");
    }
    c
}

type RequestLike = RipRequest;
