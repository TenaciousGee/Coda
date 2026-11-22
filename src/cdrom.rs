//! Minimal Linux optical drive access: TOC via the cdrom ioctls, audio sectors via
//! SCSI READ CD (0xBE) passed through SG_IO, which gives us raw CD-DA, optional C2
//! error pointers and full control over caching behaviour.

#![allow(clippy::upper_case_acronyms)]

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const SECTOR_BYTES: usize = 2352;
pub const C2_BYTES: usize = 294;
pub const SAMPLES_PER_SECTOR: usize = 588;
/// Largest transfer most drives/HBAs accept in one READ CD (27 sectors < 64 KiB).
pub const MAX_CHUNK: u32 = 26;

const CDROMREADTOCHDR: libc::c_ulong = 0x5305;
const CDROMREADTOCENTRY: libc::c_ulong = 0x5306;
const CDROMEJECT: libc::c_ulong = 0x5309;
const CDROM_DRIVE_STATUS: libc::c_ulong = 0x5326;
const CDROM_LOCKDOOR: libc::c_ulong = 0x5329;
const CDROM_LBA: u8 = 0x01;
const CDROM_LEADOUT: u8 = 0xAA;
const CDS_DISC_OK: i32 = 4;
const SG_IO: libc::c_ulong = 0x2285;
const SG_DXFER_FROM_DEV: i32 = -3;

#[repr(C)]
#[derive(Default)]
struct CdromTocHdr {
    first: u8,
    last: u8,
}

#[repr(C)]
#[derive(Default)]
struct CdromTocEntry {
    track: u8,
    adr_ctrl: u8, // adr:4 (low), ctrl:4 (high) on little-endian
    format: u8,
    lba: i32, // union cdrom_addr, 4-byte aligned
    datamode: u8,
}

#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

#[derive(Debug, Clone)]
pub struct TocTrack {
    pub number: u8,
    /// Start sector (LBA, 0 = first sector after the 2 s lead-in).
    pub lba: u32,
    pub audio: bool,
}

#[derive(Debug, Clone)]
pub struct DiscToc {
    pub tracks: Vec<TocTrack>,
    pub leadout: u32,
}

impl DiscToc {
    pub fn audio_tracks(&self) -> Vec<&TocTrack> {
        self.tracks.iter().filter(|t| t.audio).collect()
    }

    /// Data track that follows the audio session (Enhanced CD / CD-Extra).
    pub fn trailing_data(&self) -> Option<&TocTrack> {
        let last_audio = self.tracks.iter().rposition(|t| t.audio)?;
        self.tracks.get(last_audio + 1).filter(|t| !t.audio)
    }

    pub fn leading_data(&self) -> Option<&TocTrack> {
        self.tracks.first().filter(|t| !t.audio)
    }

    /// Last sector + 1 of the audio session.
    pub fn audio_end(&self) -> u32 {
        match self.trailing_data() {
            // second session starts 11 400 sectors after the end of the first
            Some(d) => d.lba.saturating_sub(11_400),
            None => self.leadout,
        }
    }

    /// Sector range [start, end) for the n-th audio track (0-based), gaps appended
    /// to the previous track, which is what AccurateRip expects.
    pub fn audio_range(&self, idx: usize) -> (u32, u32) {
        let audio = self.audio_tracks();
        let start = audio[idx].lba;
        let end = if idx + 1 < audio.len() { audio[idx + 1].lba } else { self.audio_end() };
        (start, end)
    }

    pub fn to_cdtoc(&self) -> Result<cdtoc::Toc> {
        let audio: Vec<u32> = self.audio_tracks().iter().map(|t| t.lba + 150).collect();
        let data = self.trailing_data().or(self.leading_data()).map(|t| t.lba + 150);
        let leadout = if self.trailing_data().is_some() { self.leadout + 150 } else { self.audio_end() + 150 };
        cdtoc::Toc::from_parts(audio, data, leadout).map_err(|e| anyhow!("unusable TOC: {e}"))
    }
}

pub struct Drive {
    file: File,
    #[allow(dead_code)]
    pub path: PathBuf,
    pub vendor: String,
    pub model: String,
    pub revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveStatus {
    DiscOk,
    NoDisc,
    Busy,
}

/// Optical drives present on the system.
pub fn list_drives() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/block")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with("sr").then(|| PathBuf::from(format!("/dev/{n}")))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl Drive {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("opening {} (are you in the 'optical' group?)", path.display()))?;
        let mut d = Drive { file, path: path.into(), vendor: String::new(), model: String::new(), revision: String::new() };
        if let Ok((v, m, r)) = d.inquiry() {
            d.vendor = v;
            d.model = m;
            d.revision = r;
        }
        Ok(d)
    }

    /// Key used to remember this drive's read offset.
    pub fn id(&self) -> String {
        format!("{}|{}", self.vendor, self.model)
    }

    pub fn status(&self) -> DriveStatus {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), CDROM_DRIVE_STATUS, 0) };
        match r {
            CDS_DISC_OK => DriveStatus::DiscOk,
            3 => DriveStatus::Busy, // CDS_DRIVE_NOT_READY
            _ => DriveStatus::NoDisc,
        }
    }

    fn sg(&self, cdb: &[u8], buf: &mut [u8]) -> Result<()> {
        let mut sense = [0u8; 32];
        let mut h = SgIoHdr {
            interface_id: b'S' as i32,
            dxfer_direction: SG_DXFER_FROM_DEV,
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: buf.len() as u32,
            dxferp: buf.as_mut_ptr() as *mut libc::c_void,
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: 30_000,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO, &mut h as *mut SgIoHdr) };
        if r < 0 {
            bail!("SG_IO failed: {}", std::io::Error::last_os_error());
        }
        if h.status != 0 || h.host_status != 0 || (h.driver_status & 0x0f) != 0 {
            let key = if h.sb_len_wr > 2 { sense[2] & 0x0f } else { 0 };
            let asc = if h.sb_len_wr > 13 { (sense[12], sense[13]) } else { (0, 0) };
            bail!("SCSI error status={:#x} sense key={key:#x} asc/ascq={:#x}/{:#x}", h.status, asc.0, asc.1);
        }
        Ok(())
    }

    fn inquiry(&self) -> Result<(String, String, String)> {
        let mut buf = [0u8; 36];
        self.sg(&[0x12, 0, 0, 0, 36, 0], &mut buf)?;
        let s = |r: std::ops::Range<usize>| String::from_utf8_lossy(&buf[r]).trim().to_string();
        Ok((s(8..16), s(16..32), s(32..36)))
    }

    pub fn read_toc(&self) -> Result<DiscToc> {
        let fd = self.file.as_raw_fd();
        let mut hdr = CdromTocHdr::default();
        if unsafe { libc::ioctl(fd, CDROMREADTOCHDR, &mut hdr as *mut _) } < 0 {
            bail!("no readable disc in drive ({})", std::io::Error::last_os_error());
        }
        let mut tracks = Vec::new();
        for n in hdr.first..=hdr.last {
            let mut e = CdromTocEntry { track: n, format: CDROM_LBA, ..Default::default() };
            if unsafe { libc::ioctl(fd, CDROMREADTOCENTRY, &mut e as *mut _) } < 0 {
                bail!("reading TOC entry {n}: {}", std::io::Error::last_os_error());
            }
            let ctrl = e.adr_ctrl >> 4;
            tracks.push(TocTrack { number: n, lba: e.lba.max(0) as u32, audio: ctrl & 0x04 == 0 });
        }
        let mut lo = CdromTocEntry { track: CDROM_LEADOUT, format: CDROM_LBA, ..Default::default() };
        if unsafe { libc::ioctl(fd, CDROMREADTOCENTRY, &mut lo as *mut _) } < 0 {
            bail!("reading lead-out: {}", std::io::Error::last_os_error());
        }
        if tracks.iter().all(|t| !t.audio) {
            bail!("this disc has no audio tracks");
        }
        Ok(DiscToc { tracks, leadout: lo.lba as u32 })
    }

    /// Read `count` raw CD-DA sectors. With `c2`, also returns 294 bytes of C2
    /// error pointers per sector (a set bit marks an unreliable byte).
    pub fn read_cdda(&self, lba: u32, count: u32, c2: bool) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let per = SECTOR_BYTES + if c2 { C2_BYTES } else { 0 };
        let mut buf = vec![0u8; per * count as usize];
        let cdb = [
            0xBE,
            0x04, // expected sector type: CD-DA
            (lba >> 24) as u8,
            (lba >> 16) as u8,
            (lba >> 8) as u8,
            lba as u8,
            (count >> 16) as u8,
            (count >> 8) as u8,
            count as u8,
            if c2 { 0x10 | 0x02 } else { 0x10 }, // user data (+ C2 error bits)
            0x00,
            0x00,
        ];
        self.sg(&cdb, &mut buf)?;
        if !c2 {
            return Ok((buf, None));
        }
        let mut audio = Vec::with_capacity(SECTOR_BYTES * count as usize);
        let mut errs = Vec::with_capacity(C2_BYTES * count as usize);
        for s in buf.chunks_exact(per) {
            audio.extend_from_slice(&s[..SECTOR_BYTES]);
            errs.extend_from_slice(&s[SECTOR_BYTES..]);
        }
        Ok((audio, Some(errs)))
    }

    /// Probe whether the drive reports C2 pointers.
    pub fn supports_c2(&self, lba: u32) -> bool {
        self.read_cdda(lba, 1, true).is_ok()
    }

    fn read_subq(&self, format: u8, track: u8) -> Result<[u8; 24]> {
        let mut buf = [0u8; 24];
        self.sg(&[0x42, 0x00, 0x40, format, 0, 0, track, 0, 24, 0], &mut buf)?;
        Ok(buf)
    }

    /// Media catalogue number (UPC/EAN barcode) if the disc carries one.
    pub fn mcn(&self) -> Option<String> {
        let b = self.read_subq(0x02, 0).ok()?;
        if b[8] & 0x80 == 0 {
            return None;
        }
        let s: String = b[9..22].iter().map(|&c| c as char).collect();
        (s.chars().all(|c| c.is_ascii_digit()) && s.chars().any(|c| c != '0')).then_some(s)
    }

    pub fn isrc(&self, track: u8) -> Option<String> {
        let b = self.read_subq(0x03, track).ok()?;
        if b[8] & 0x80 == 0 {
            return None;
        }
        let s: String = b[9..21].iter().map(|&c| c as char).collect();
        (s.chars().all(|c| c.is_ascii_alphanumeric()) && !s.chars().all(|c| c == '0')).then_some(s)
    }

    pub fn eject(&self) -> Result<()> {
        let fd = self.file.as_raw_fd();
        unsafe {
            libc::ioctl(fd, CDROM_LOCKDOOR, 0);
            if libc::ioctl(fd, CDROMEJECT, 0) < 0 {
                bail!("eject failed: {}", std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn lock(&self, on: bool) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), CDROM_LOCKDOOR, on as libc::c_int);
        }
    }
}
