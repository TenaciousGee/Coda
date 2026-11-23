mod accuraterip;
mod cdrom;
mod config;
mod converter;
mod discogs;
mod dnd;
mod encoder;
mod formats;
mod gui;
mod integration;
mod metadata;
mod naming;
mod net;
mod ripper;
mod tags;

use config::Config;
use converter::NamingMode;
use encoder::{EncodeSettings, Outcome, Pool, PoolEvent};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

const HELP: &str = "Coda – secure CD ripper and audio converter

USAGE:
  coda                         open the app (Rip CD page)
  coda --convert PATHS...      open the app on the Convert page with files/folders queued
  coda convert --preset NAME [--naming tags|mirror|beside] [--no-recursive] PATHS...
                                     convert without the GUI (used by the right-click menu)
  coda presets                 list presets
  coda install-integration     add right-click menu entries to your file managers
  coda remove-integration      remove them again
";

fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=Coda", "--icon=coda", summary, body])
        .spawn();
}

fn headless_convert(args: &[String]) -> i32 {
    let cfg = Config::load();
    let mut preset = None;
    let mut mode = NamingMode::Preset;
    let mut recursive = true;
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--preset" => preset = it.next().cloned(),
            "--naming" => {
                mode = match it.next().map(|s| s.as_str()) {
                    Some("mirror") => NamingMode::Mirror,
                    Some("beside") => NamingMode::BesideSource,
                    _ => NamingMode::Preset,
                }
            }
            "--no-recursive" => recursive = false,
            p => paths.push(PathBuf::from(p)),
        }
    }
    let name = preset.unwrap_or_else(|| cfg.settings.last_convert_preset.clone());
    let Some(preset) = cfg.preset(&name).or(cfg.presets.first()).cloned() else {
        eprintln!("No such preset: {name}");
        return 2;
    };
    let items = converter::scan(&paths, recursive);
    if items.is_empty() {
        eprintln!("No audio files found.");
        notify("Coda", "No audio files found in the selection");
        return 1;
    }
    let jobs: Vec<_> = items.iter().flat_map(|i| converter::plan(i, &preset, mode, &cfg.settings)).collect();
    let total = jobs.len();
    let threads = cfg.settings.worker_threads();
    println!("Converting {} files ({} encodes) with preset '{}' on {} threads", items.len(), total, preset.name, threads);
    notify("Coda", &format!("Converting {} files to {}", items.len(), preset.name));
    let started = std::time::Instant::now();
    let stats = Arc::new(Mutex::new((0usize, 0usize, 0usize))); // ok, skipped, failed
    let names: Vec<String> = jobs.iter().map(|j| j.output.display().to_string()).collect();
    let sink = {
        let stats = stats.clone();
        Arc::new(move |ev: PoolEvent| {
            if let PoolEvent::Finished { id, result } = ev {
                let mut s = stats.lock().unwrap();
                let n = s.0 + s.1 + s.2 + 1;
                match result {
                    Ok(Outcome::Written(_, v)) => {
                        s.0 += 1;
                        println!("[{n}/{total}] ✓ {}{}", names[id], if v { " (verified)" } else { "" });
                    }
                    Ok(Outcome::Skipped(_)) => {
                        s.1 += 1;
                        println!("[{n}/{total}] – exists, skipped {}", names[id]);
                    }
                    Err(e) => {
                        s.2 += 1;
                        eprintln!("[{n}/{total}] ✗ {}: {e}", names[id]);
                    }
                }
            }
        }) as encoder::EventSink
    };
    let pool = Pool::new(
        threads,
        EncodeSettings { ffmpeg: cfg.settings.ffmpeg.clone(), existing: cfg.settings.existing.clone(), verify_lossless: cfg.settings.verify_lossless },
        sink,
        Arc::new(AtomicBool::new(false)),
    );
    for (i, j) in jobs.into_iter().enumerate() {
        pool.submit(i, j);
    }
    pool.join();
    let (ok, skipped, failed) = *stats.lock().unwrap();
    let msg = format!("{ok} converted, {skipped} skipped, {failed} failed in {:.1}s", started.elapsed().as_secs_f64());
    println!("{msg}");
    notify(if failed > 0 { "Coda: finished with errors" } else { "Coda: conversion finished" }, &msg);
    if failed > 0 { 1 } else { 0 }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("-h") | Some("--help") => {
            print!("{HELP}");
            0
        }
        Some("convert") => headless_convert(&args[1..]),
        Some("inspect") => {
            for p in &args[1..] {
                let m = tags::read(std::path::Path::new(p));
                println!(
                    "{p}\n  {} – {} – {} | track {}/{} disc {}/{} | {} | {} | cover {}",
                    m.artist, m.album, m.title, m.track, m.track_total, m.disc, m.disc_total, m.year, m.genre,
                    m.cover.as_ref().map(|c| format!("{} KB", c.len() / 1024)).unwrap_or_else(|| "none".into())
                );
            }
            0
        }
        Some("presets") => {
            for p in Config::load().presets {
                println!("{}", p.name);
            }
            0
        }
        Some("install-integration") => match integration::install(&Config::load()) {
            Ok(done) => {
                println!("Installed for: {}", done.join(", "));
                0
            }
            Err(e) => {
                eprintln!("{e:#}");
                1
            }
        },
        Some("remove-integration") => {
            let _ = integration::uninstall();
            println!("Removed.");
            0
        }
        Some("--convert") => gui::run(args[1..].iter().map(PathBuf::from).collect(), 1),
        Some(_) => {
            // paths passed directly (e.g. "Open With"): audio files go to the converter
            let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).filter(|p| p.exists()).collect();
            let page = if paths.is_empty() { 0 } else { 1 };
            gui::run(paths, page)
        }
        None => gui::run(vec![], 0),
    };
    std::process::exit(code);
}
