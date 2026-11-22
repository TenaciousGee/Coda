//! One encode = one ffmpeg process (single-threaded), run in parallel by `Pool`.

use crate::config::ExistingFile;
use crate::formats::{Format, OutputOptions};
use crate::tags::{self, TrackMeta};
use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{unbounded, Sender};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct EncodeJob {
    pub input: PathBuf,
    pub output: PathBuf,
    pub format: Format,
    pub options: OutputOptions,
    pub meta: TrackMeta,
    /// Delete `input` once every job referencing it is done (rip temp WAVs).
    #[allow(dead_code)] // held so the temp WAV lives until every encode finishes
    pub temp_input: Option<Arc<TempFile>>,
}

/// A temporary file removed when the last job holding it is dropped.
#[derive(Debug)]
pub struct TempFile(pub PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Debug, Clone)]
pub struct EncodeSettings {
    pub ffmpeg: String,
    pub existing: ExistingFile,
    pub verify_lossless: bool,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Written(PathBuf, bool), // path, verified
    Skipped(PathBuf),
}

fn muxer(f: Format) -> &'static str {
    match f {
        Format::Flac => "flac",
        Format::Mp3 => "mp3",
        Format::Aac | Format::Alac => "ipod",
        Format::Opus => "opus",
        Format::Vorbis => "ogg",
        Format::Wma => "asf",
        Format::Wav => "wav",
        Format::Aiff => "aiff",
        Format::WavPack => "wv",
        Format::Tta => "tta",
    }
}

/// What the source actually contains, from ffmpeg's stream description.
#[derive(Debug, Clone, Default)]
pub struct SourceInfo {
    #[allow(dead_code)]
    pub codec: String,
    pub sample_rate: u32,
    /// Integer bit depth for PCM-like sources; None for float/lossy.
    pub bits: Option<u8>,
    pub lossless: bool,
}

pub fn probe(ffmpeg: &str, path: &Path) -> SourceInfo {
    let out = Command::new(ffmpeg).args(["-hide_banner", "-nostdin", "-i"]).arg(path).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default();
    parse_probe(&text)
}

pub fn parse_probe(text: &str) -> SourceInfo {
    let Some(line) = text.lines().find(|l| l.contains("Audio: ")) else { return SourceInfo::default() };
    let desc = &line[line.find("Audio: ").unwrap() + 7..];
    let codec: String = desc.chars().take_while(|c| !matches!(c, ' ' | ',')).collect();
    let parts: Vec<&str> = desc.split(',').map(|p| p.trim()).collect();
    let sample_rate = parts.iter().find_map(|p| p.strip_suffix(" Hz")).and_then(|r| r.trim().parse().ok()).unwrap_or(0);
    let lossless = codec.starts_with("pcm_")
        || ["flac", "alac", "wavpack", "ape", "tta", "tak", "mlp", "truehd", "wmalossless", "shorten", "mp4als"].contains(&codec.as_str());
    let bits = if desc.contains("(24 bit)") || desc.contains("(20 bit)") {
        Some(24)
    } else if parts.iter().any(|p| p.starts_with("s16") || p.starts_with("u8")) {
        Some(16)
    } else if parts.iter().any(|p| p.starts_with("s32")) {
        Some(32)
    } else {
        None // float or unknown
    };
    SourceInfo { codec, sample_rate, bits: if lossless { bits } else { None }, lossless }
}

/// Fill "keep source" choices with concrete values the target can hold.
fn effective_options(format: Format, o: &OutputOptions, src: &SourceInfo) -> OutputOptions {
    let mut o = o.clone();
    if matches!(format, Format::Wav | Format::Aiff) && o.bit_depth == 0 {
        // keep hi-res PCM intact; lossy/float sources become 16-bit
        o.bit_depth = src.bits.map(|b| if b > 16 { b } else { 16 }).unwrap_or(16);
    }
    let max_rate = match format {
        Format::Wma | Format::Mp3 => 48_000,
        _ => u32::MAX,
    };
    let rate = if o.sample_rate > 0 { o.sample_rate } else { src.sample_rate };
    if rate > max_rate {
        o.sample_rate = if rate % 44_100 == 0 { 44_100 } else { 48_000 };
    }
    o
}

/// MD5 of the decoded PCM, normalised to s32 so differing containers compare.
fn pcm_md5(ffmpeg: &str, path: &Path) -> Result<String> {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-c:a", "pcm_s32le", "-f", "md5", "-"])
        .output()
        .context("running ffmpeg for verification")?;
    if !out.status.success() {
        bail!("verification decode failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn run(job: &EncodeJob, s: &EncodeSettings, cancel: &AtomicBool, progress: &dyn Fn(f32)) -> Result<Outcome> {
    let mut dest = job.output.clone();
    if dest == job.input {
        // converting next to the source in the same format: never overwrite the source
        dest = crate::naming::unique_path(&dest);
    }
    if dest.exists() {
        match s.existing {
            ExistingFile::Skip => return Ok(Outcome::Skipped(dest)),
            ExistingFile::Overwrite => {}
            ExistingFile::Rename => dest = crate::naming::unique_path(&dest),
        }
    }
    let dir = dest.parent().ok_or_else(|| anyhow!("bad output path"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.dwpart.{}",
        dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        job.format.extension()
    ));

    let src = probe(&s.ffmpeg, &job.input);
    let opts = effective_options(job.format, &job.options, &src);
    // lofty cannot write WMA, so embed its cover through ffmpeg (ASF WM/Picture)
    let wma_cover = match (&job.meta.cover, job.format) {
        (Some(c), Format::Wma) => {
            let p = tmp.with_extension("dwcover.jpg");
            std::fs::write(&p, c.as_slice()).ok().map(|_| TempFile(p))
        }
        _ => None,
    };
    let mut cmd = Command::new(&s.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"]).arg(&job.input);
    if let Some(c) = &wma_cover {
        cmd.arg("-i").arg(&c.0).args(["-map", "1:v", "-c:v", "copy", "-disposition:v", "attached_pic"]);
    } else {
        cmd.arg("-vn");
    }
    cmd.args(["-map", "0:a:0", "-map_metadata", "0", "-threads", "1"])
        .args(job.format.ffmpeg_args(&opts, &s.ffmpeg))
        .args(job.meta.ffmpeg_metadata_args())
        .args(["-progress", "pipe:1", "-nostats", "-f", muxer(job.format)])
        .arg(&tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().with_context(|| format!("starting {} (is ffmpeg installed?)", s.ffmpeg))?;

    let stdout = child.stdout.take().unwrap();
    let total = job.meta.duration.max(0.0);
    let (ltx, lrx) = unbounded::<f32>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=") {
                if let (Ok(us), true) = (us.trim().parse::<f64>(), total > 0.0) {
                    let _ = ltx.send(((us / 1e6) / total).clamp(0.0, 1.0) as f32);
                }
            }
        }
    });
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&tmp);
            bail!("cancelled");
        }
        while let Ok(p) = lrx.try_recv() {
            progress(p * 0.95);
        }
        if let Some(st) = child.try_wait()? {
            break st;
        }
        std::thread::sleep(Duration::from_millis(60));
    };
    let _ = reader.join();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("ffmpeg failed: {}", err.lines().last().unwrap_or("unknown error"));
    }

    let mut verified = false;
    // Only meaningful when nothing about the audio was meant to change.
    let same_depth = job.options.bit_depth == 0 || src.bits.map(|b| job.options.bit_depth >= b).unwrap_or(false);
    if s.verify_lossless && src.lossless && src.bits.is_some() && job.format.is_lossless() && same_depth && job.options.sample_rate == 0 && !job.options.stereo {
        let a = pcm_md5(&s.ffmpeg, &job.input)?;
        let b = pcm_md5(&s.ffmpeg, &tmp)?;
        if a != b {
            let _ = std::fs::remove_file(&tmp);
            bail!("lossless verification failed: decoded audio differs from source");
        }
        verified = true;
    }

    if let Err(e) = tags::write(&tmp, job.format, &job.meta) {
        // Tagging problems should not lose a good encode; report but keep the file.
        eprintln!("warning: {e:#}");
    }
    std::fs::rename(&tmp, &dest).with_context(|| format!("moving into place {}", dest.display()))?;
    progress(1.0);
    Ok(Outcome::Written(dest, verified))
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum PoolEvent {
    Started { id: usize },
    Progress { id: usize, fraction: f32 },
    Finished { id: usize, result: Result<Outcome, String> },
}

pub type EventSink = Arc<dyn Fn(PoolEvent) + Send + Sync>;

/// Fixed-size worker pool; each worker runs one single-threaded ffmpeg at a time,
/// so N workers saturate N cores.
pub struct Pool {
    tx: Option<Sender<(usize, EncodeJob)>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    #[allow(dead_code)]
    pub cancel: Arc<AtomicBool>,
}

impl Pool {
    pub fn new(threads: usize, settings: EncodeSettings, sink: EventSink, cancel: Arc<AtomicBool>) -> Self {
        let (tx, rx) = unbounded::<(usize, EncodeJob)>();
        let workers = (0..threads.max(1))
            .map(|_| {
                let rx = rx.clone();
                let sink = sink.clone();
                let s = settings.clone();
                let cancel = cancel.clone();
                std::thread::spawn(move || {
                    for (id, job) in rx.iter() {
                        if cancel.load(Ordering::Relaxed) {
                            sink(PoolEvent::Finished { id, result: Err("cancelled".into()) });
                            continue;
                        }
                        sink(PoolEvent::Started { id });
                        let sink2 = sink.clone();
                        let last = std::cell::Cell::new(-1.0f32);
                        let prog = move |f: f32| {
                            if f - last.get() >= 0.01 || f >= 1.0 {
                                last.set(f);
                                sink2(PoolEvent::Progress { id, fraction: f });
                            }
                        };
                        let r = run(&job, &s, &cancel, &prog).map_err(|e| format!("{e:#}"));
                        sink(PoolEvent::Finished { id, result: r });
                    }
                })
            })
            .collect();
        Self { tx: Some(tx), workers, cancel }
    }

    pub fn submit(&self, id: usize, job: EncodeJob) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((id, job));
        }
    }

    /// Stop accepting jobs and wait for the queue to drain.
    pub fn join(mut self) {
        self.tx.take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_parsing() {
        let a = parse_probe("  Stream #0:0: Audio: pcm_s24le ([1][0][0][0] / 0x0001), 96000 Hz, 2 channels, s32 (24 bit), 4608 kb/s");
        assert!(a.lossless);
        assert_eq!((a.sample_rate, a.bits), (96_000, Some(24)));
        let b = parse_probe("  Stream #0:0: Audio: mp3 (mp3float), 44100 Hz, stereo, fltp, 128 kb/s");
        assert!(!b.lossless);
        assert_eq!(b.bits, None);
        let c = parse_probe("  Stream #0:0: Audio: flac, 44100 Hz, stereo, s16");
        assert_eq!((c.codec.as_str(), c.bits), ("flac", Some(16)));
        let wma = effective_options(Format::Wma, &OutputOptions::default_for(Format::Wma), &a);
        assert_eq!(wma.sample_rate, 48_000);
        let wav = effective_options(Format::Wav, &OutputOptions::default_for(Format::Wav), &a);
        assert_eq!(wav.bit_depth, 24);
    }
}
