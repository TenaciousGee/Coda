//! AccurateRip: track checksums (v1 + v2), database lookup, drive-offset table
//! and key-disc offset detection using the "frame 450" offset-finding CRCs.

use crate::cdrom::SAMPLES_PER_SECTOR;
use crate::net;
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;

/// Samples skipped at the start of the first track / end of the last track
/// (5 sectors minus one sample), as defined by AccurateRip.
const SKIP: u64 = (5 * SAMPLES_PER_SECTOR) as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackCrc {
    pub v1: u32,
    pub v2: u32,
}

/// Checksums for one track's PCM (16-bit stereo little-endian, offset-corrected).
pub fn track_crc(pcm: &[u8], is_first: bool, is_last: bool) -> TrackCrc {
    let n = (pcm.len() / 4) as u64;
    let from = if is_first { SKIP } else { 1 };
    let to = if is_last { n.saturating_sub(SKIP) } else { n };
    let mut v1: u32 = 0;
    let mut v2: u32 = 0;
    for (i, s) in pcm.chunks_exact(4).enumerate() {
        let mult = i as u64 + 1;
        if mult < from || mult > to {
            continue;
        }
        let sample = u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64;
        v1 = v1.wrapping_add((sample * mult) as u32);
        let p = sample * mult;
        v2 = v2.wrapping_add((p & 0xffff_ffff) as u32).wrapping_add((p >> 32) as u32);
    }
    TrackCrc { v1, v2 }
}

/// v1-style CRC of exactly one sector of samples with the multiplier reset to 1
/// (the "offset finding" checksum stored for frame 450 of each track).
pub fn frame_crc(samples: &[u8]) -> u32 {
    samples
        .chunks_exact(4)
        .take(SAMPLES_PER_SECTOR)
        .enumerate()
        .fold(0u32, |acc, (i, s)| acc.wrapping_add(u32::from_le_bytes([s[0], s[1], s[2], s[3]]).wrapping_mul(i as u32 + 1)))
}

#[derive(Debug, Clone, Copy)]
pub struct ArEntry {
    pub confidence: u8,
    pub crc: u32,
    pub crc450: u32,
}

/// Database entries for a disc: `tracks[i]` lists one entry per known pressing.
#[derive(Debug, Clone, Default)]
pub struct ArData {
    pub tracks: Vec<Vec<ArEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArResult {
    /// Matched; `confidence` = submissions agreeing, `total` = all submissions for the track.
    Accurate { v2: bool, confidence: u32, total: u32 },
    Mismatch { total: u32 },
    NotInDatabase,
}

impl ArResult {
    pub fn is_accurate(&self) -> bool {
        matches!(self, ArResult::Accurate { .. })
    }
    pub fn describe(&self) -> String {
        match self {
            ArResult::Accurate { v2, confidence, total } => {
                format!("Accurate (AR{} confidence {confidence} of {total})", if *v2 { "v2" } else { "v1" })
            }
            ArResult::Mismatch { total } => format!("Not accurate ({total} submissions, none match)"),
            ArResult::NotInDatabase => "Not in AccurateRip database".into(),
        }
    }
}

impl ArData {
    pub fn parse(bin: &[u8], id: &cdtoc::AccurateRip) -> Result<Self> {
        let header: [u8; 13] = (*id).into();
        let n = header[0] as usize;
        let size = 13 + 9 * n;
        let mut tracks = vec![Vec::new(); n];
        for chunk in bin.chunks_exact(size) {
            if chunk[..13] != header {
                continue;
            }
            for (t, e) in chunk[13..].chunks_exact(9).enumerate() {
                let entry = ArEntry {
                    confidence: e[0],
                    crc: u32::from_le_bytes([e[1], e[2], e[3], e[4]]),
                    crc450: u32::from_le_bytes([e[5], e[6], e[7], e[8]]),
                };
                if entry.crc != 0 || entry.crc450 != 0 {
                    tracks[t].push(entry);
                }
            }
        }
        if tracks.iter().all(|t| t.is_empty()) {
            return Err(anyhow!("empty AccurateRip response"));
        }
        Ok(Self { tracks })
    }

    pub fn check(&self, track_idx: usize, crc: TrackCrc) -> ArResult {
        let Some(entries) = self.tracks.get(track_idx) else { return ArResult::NotInDatabase };
        if entries.is_empty() {
            return ArResult::NotInDatabase;
        }
        let total: u32 = entries.iter().map(|e| e.confidence as u32).sum();
        let v2: u32 = entries.iter().filter(|e| e.crc == crc.v2).map(|e| e.confidence as u32).sum();
        if v2 > 0 {
            return ArResult::Accurate { v2: true, confidence: v2, total };
        }
        let v1: u32 = entries.iter().filter(|e| e.crc == crc.v1).map(|e| e.confidence as u32).sum();
        if v1 > 0 {
            return ArResult::Accurate { v2: false, confidence: v1, total };
        }
        ArResult::Mismatch { total }
    }
}

/// Fetch database entries for a disc. `Ok(None)` = disc not in the database.
pub fn lookup(toc: &cdtoc::Toc) -> Result<Option<ArData>> {
    let id = toc.accuraterip_id();
    match net::get_bytes(&id.checksum_url()) {
        Ok(bin) => Ok(Some(ArData::parse(&bin, &id)?)),
        Err(e) if net::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Look up a drive in AccurateRip's published offset table (cached for 30 days).
pub fn drive_offset_from_db(vendor: &str, model: &str) -> Result<Option<i16>> {
    let cache = crate::config::cache_dir().join("DriveOffsets.bin");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e.as_secs() < 30 * 86_400).unwrap_or(false))
        .unwrap_or(false);
    let raw = if fresh {
        std::fs::read(&cache)?
    } else {
        let raw = net::get_bytes(cdtoc::AccurateRip::DRIVE_OFFSET_URL)?;
        let _ = std::fs::create_dir_all(cache.parent().unwrap());
        let _ = std::fs::write(&cache, &raw);
        raw
    };
    let table = cdtoc::AccurateRip::parse_drive_offsets(&raw).map_err(|e| anyhow!("{e}"))?;
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase();
    let (v, m) = (norm(vendor), norm(model));
    // exact vendor+model first, then model only (vendor strings vary, e.g. "HL-DT-ST")
    let exact = table.iter().find(|((tv, tm), _)| norm(tv) == v && norm(tm) == m);
    let loose = || table.iter().find(|((_, tm), _)| norm(tm) == m);
    Ok(exact.or_else(loose).map(|(_, o)| *o))
}

/// Given raw (uncorrected, offset 0) samples covering frame 450 of a track ±`range`
/// samples, find every read offset whose frame CRC matches a database entry.
/// `window` must start at sample `450*588 - range` relative to the track start.
/// Returns (offset, confidence) candidates.
pub fn detect_offsets(window: &[u8], range: i32, entries: &[ArEntry]) -> Vec<(i32, u32)> {
    let mut found = BTreeMap::new();
    let want: Vec<_> = entries.iter().filter(|e| e.crc450 != 0).collect();
    if want.is_empty() {
        return vec![];
    }
    for off in -range..=range {
        let start = ((off + range) as usize) * 4;
        let end = start + SAMPLES_PER_SECTOR * 4;
        if end > window.len() {
            break;
        }
        let c = frame_crc(&window[start..end]);
        for e in &want {
            if e.crc450 == c {
                *found.entry(off).or_insert(0u32) += e.confidence as u32;
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_skips_edges() {
        // 20 sectors of a constant sample
        let pcm: Vec<u8> = std::iter::repeat([1u8, 0, 0, 0]).take(588 * 20).flatten().collect();
        let n = 588 * 20u64;
        let all: u64 = (1..=n).sum();
        let mid: u64 = (SKIP..=n - SKIP).sum();
        assert_eq!(track_crc(&pcm, false, false).v1, all as u32);
        assert_eq!(track_crc(&pcm, true, true).v1, mid as u32);
        assert_eq!(track_crc(&pcm, true, false).v1, (SKIP..=n).sum::<u64>() as u32);
    }

    #[test]
    fn v2_folds_high_bits() {
        // one sample 0xFFFFFFFF at multiplier 2 -> product 0x1_FFFF_FFFE
        let pcm = [0u8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let c = track_crc(&pcm, false, false);
        assert_eq!(c.v1, 0xFFFF_FFFE);
        assert_eq!(c.v2, 0xFFFF_FFFE_u32.wrapping_add(1));
    }

    #[test]
    fn offset_detection_finds_shift() {
        // pseudo-random audio; database CRC computed at the true position
        let mut x: u32 = 12345;
        let audio: Vec<u8> = (0..(588 * 460 + 4000))
            .flat_map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                x.to_le_bytes()
            })
            .collect();
        let true_start = 450 * 588;
        let crc = frame_crc(&audio[true_start * 4..(true_start + 588) * 4]);
        let drive_offset = 667;
        let range = 1000;
        // the drive returns data `drive_offset` samples early -> true sample s at index s + offset
        let mut shifted = vec![0u8; drive_offset * 4];
        shifted.extend_from_slice(&audio);
        let wstart = (true_start - range) * 4;
        let window = &shifted[wstart..wstart + (2 * range + 588 + 1) * 4];
        let r = detect_offsets(window, range as i32, &[ArEntry { confidence: 5, crc: 0, crc450: crc }]);
        assert_eq!(r, vec![(drive_offset as i32, 5)]);
    }
}
