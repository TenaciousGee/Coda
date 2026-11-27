//! Glue between the Slint UI and the engines. All mutable app state lives in a
//! thread-local on the UI thread; worker threads post results back with
//! `upgrade_in_event_loop`.

slint::include_modules!();

use crate::cdrom::{self, Drive, DriveStatus};
use crate::config::{self, Config, ExistingFile, OutputSpec, Preset, ThemeMode, VerifyMode, SCHEMES, TAG_TEMPLATE};
use crate::converter::{self, NamingMode, SourceItem};
use crate::encoder::{EncodeSettings, Outcome, Pool, PoolEvent};
use crate::formats::{self, Format, OutputOptions, QualityMode, ALL_FORMATS};
use crate::metadata::{self, AlbumCandidate};
use crate::naming::{self, NameContext};
use crate::ripper::{self, DiscInfo, RipEvent, RipRequest};
use crate::tags::TrackMeta;
use crate::{accuraterip, integration};
use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, SharedString, VecModel, Weak};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct FileProg {
    frac: Vec<f32>,
    finished: usize,
    errors: Vec<String>,
    skipped: usize,
    verified: usize,
}

#[derive(Default)]
struct State {
    cfg: Config,
    drives: Vec<PathBuf>,
    drive_id: String,
    drive_vendor: String,
    drive_model: String,
    last_status: Option<DriveStatus>,
    disc: Option<DiscInfo>,
    candidate: Option<AlbumCandidate>,
    cover: Option<Arc<Vec<u8>>>,
    cover_req: u64,
    loading: bool,
    rip_cancel: Option<Arc<AtomicBool>>,
    rip_phase: Vec<(u8, f32)>,
    conv_cancel: Option<Arc<AtomicBool>>,
    files: Vec<SourceItem>,
    conv: Vec<FileProg>,
    conv_started: Option<Instant>,
    edit_index: usize,
    edit: Preset,
    just_ripped: bool,
    notes: Vec<String>,
    tag_groups: Vec<crate::tagger::Group>,
    tag_sel: usize,
    /// Desktop light/dark preference from the XDG settings portal (None = unknown).
    system_dark: Option<bool>,
    logo_px: u32,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    ST.with(|s| f(&mut s.borrow_mut()))
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn strings(v: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.into_iter().map(SharedString::from).collect::<Vec<_>>()))
}

fn toast(ui: &AppWindow, msg: impl AsRef<str>) {
    let text = msg.as_ref().to_string();
    ui.set_toast(ss(&text));
    let w = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(6), move || {
        if let Some(ui) = w.upgrade() {
            if ui.get_toast() == text.as_str() {
                ui.set_toast(ss(""));
            }
        }
    });
}

fn mmss(secs: f64) -> String {
    let s = secs.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn save_cfg(ui: &AppWindow) {
    if let Err(e) = with(|s| s.cfg.save()) {
        toast(ui, format!("Could not save settings: {e:#}"));
    }
}

// =================================================================== entry

pub fn run(initial: Vec<PathBuf>, page: i32) -> i32 {
    let ui = match AppWindow::new() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("Could not start the interface: {e}");
            return 1;
        }
    };
    // Wayland app_id / X11 class so the desktop matches the window to coda.desktop
    // (title bar and taskbar icon). Must happen after the runtime exists and before
    // the window is shown.
    if let Err(e) = slint::set_xdg_app_id("coda") {
        eprintln!("could not set application id: {e}");
    }
    let cfg = Config::load();
    if !Config::path().exists() {
        let _ = cfg.save();
    }
    with(|s| s.cfg = cfg);

    init_theme(&ui);
    ui.set_formats(strings(ALL_FORMATS.iter().map(|f| f.label().to_string())));
    ui.set_cores(std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1));
    ui.set_page(std::env::var("CODA_PAGE").ok().and_then(|p| p.parse().ok()).unwrap_or(page));
    load_settings_into_ui(&ui);
    refresh_presets(&ui);
    select_edit_preset(&ui, 0);
    refresh_drives(&ui);
    ffmpeg_status(&ui);
    ui.set_integration_status(ss(if integration::is_installed() { "Currently installed." } else { "Not installed." }));
    ui.set_tracks(ModelRc::new(VecModel::<TrackRow>::default()));
    ui.set_files(ModelRc::new(VecModel::<FileRow>::default()));
    ui.set_convert_status(ss("Ready"));

    wire_rip(&ui);
    if let (Some(_), Ok(n)) = (std::env::var_os("CODA_DEBUG"), std::env::var("CODA_DEBUG_NOTES")) {
        set_notes(&ui, n.split('|').map(String::from).collect());
    }
    wire_convert(&ui);
    wire_presets(&ui);
    wire_settings(&ui);
    wire_tagger(&ui);
    init_dnd(&ui);

    if !initial.is_empty() {
        if page == 4 {
            tag_add_paths(&ui, initial);
        } else {
            add_paths(&ui, initial);
        }
    }

    // watch the drive for disc insert/remove
    let timer = slint::Timer::default();
    {
        let w = ui.as_weak();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(2000), move || {
            let Some(ui) = w.upgrade() else { return };
            poll_drive(&ui);
        });
    }
    poll_drive(&ui);

    // render the logo once the window knows its real scale factor, and again if it changes
    let logo_timer = slint::Timer::default();
    {
        let w = ui.as_weak();
        logo_timer.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
            if let Some(ui) = w.upgrade() {
                render_logo(&ui);
            }
        });
    }

    if let Err(e) = ui.run() {
        eprintln!("{e}");
        return 1;
    }
    0
}

// =================================================================== drives & discs

fn current_device(ui: &AppWindow) -> PathBuf {
    with(|s| {
        s.drives
            .get(ui.get_drive_index().max(0) as usize)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(&s.cfg.settings.cd_device))
    })
}

fn refresh_drives(ui: &AppWindow) {
    let mut drives = cdrom::list_drives();
    let want = with(|s| PathBuf::from(&s.cfg.settings.cd_device));
    if drives.is_empty() && want.exists() {
        drives.push(want.clone());
    }
    let idx = drives.iter().position(|d| *d == want).unwrap_or(0);
    ui.set_drives(strings(drives.iter().map(|d| d.display().to_string())));
    ui.set_drive_index(idx as i32);
    with(|s| s.drives = drives.clone());
    identify_drive(ui);
}

fn identify_drive(ui: &AppWindow) {
    let dev = current_device(ui);
    match Drive::open(&dev) {
        Ok(d) => {
            let id = d.id();
            let label = format!("{} {}", d.vendor, d.model).trim().to_string();
            with(|s| {
                s.drive_id = id.clone();
                s.drive_vendor = d.vendor.clone();
                s.drive_model = d.model.clone();
            });
            ui.set_drive_label(ss(if label.is_empty() { dev.display().to_string() } else { format!("{label}\n{}", dev.display()) }));
            update_offset_text(ui, None);
        }
        Err(_) => {
            ui.set_drive_label(ss("No optical drive found"));
            ui.set_offset_text(ss(""));
        }
    }
}

fn update_offset_text(ui: &AppWindow, source: Option<&str>) {
    let (saved, id) = with(|s| (s.cfg.settings.drive_offsets.get(&s.drive_id).copied(), s.drive_id.clone()));
    let t = match saved {
        Some(o) => format!("Read offset {o:+} ({})", source.unwrap_or("saved")),
        None if id.is_empty() => String::new(),
        None => "Read offset unknown: insert a CD to look it up or detect it".into(),
    };
    if let Some(o) = saved {
        ui.set_s_offset(o);
    }
    ui.set_offset_text(ss(t));
}

fn poll_drive(ui: &AppWindow) {
    let busy = with(|s| s.loading || s.rip_cancel.is_some());
    if busy {
        return;
    }
    let dev = current_device(ui);
    let Ok(d) = Drive::open(&dev) else { return };
    let st = d.status();
    drop(d);
    let prev = with(|s| s.last_status.replace(st));
    if prev == Some(st) {
        return;
    }
    match st {
        DriveStatus::DiscOk => load_disc(ui),
        DriveStatus::NoDisc => {
            if with(|s| std::mem::take(&mut s.just_ripped)) {
                // keep the finished rip's results visible until the next disc arrives
                with(|s| s.disc = None);
                ui.set_disc_loaded(false);
                ui.set_disc_status(ss("Rip finished · insert the next CD"));
            } else {
                clear_disc(ui, "Insert an audio CD");
            }
        }
        DriveStatus::Busy => ui.set_disc_status(ss("Drive is spinning up…")),
    }
}

fn clear_disc(ui: &AppWindow, msg: &str) {
    with(|s| {
        s.disc = None;
        s.candidate = None;
        s.cover = None;
    });
    ui.set_disc_loaded(false);
    ui.set_tracks(ModelRc::new(VecModel::<TrackRow>::default()));
    ui.set_releases(strings(Vec::new()));
    ui.set_album(ss(""));
    ui.set_album_artist(ss(""));
    ui.set_year(ss(""));
    ui.set_genre(ss(""));
    ui.set_has_cover(false);
    ui.set_cover_info(ss(""));
    ui.set_ar_summary(ss(""));
    set_notes(ui, vec![]);
    ui.set_disc_status(ss(msg));
}

fn load_disc(ui: &AppWindow) {
    if with(|s| s.loading || s.rip_cancel.is_some()) {
        return;
    }
    with(|s| s.loading = true);
    ui.set_loading(true);
    ui.set_disc_loaded(false);
    ui.set_disc_status(ss("Reading disc…"));
    let dev = current_device(ui);
    let st = with(|s| s.cfg.settings.clone());
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let w2 = w.clone();
        let status = move |m: &str| {
            let m = m.to_string();
            let _ = w2.upgrade_in_event_loop(move |ui| ui.set_disc_status(ss(m)));
        };
        let res = ripper::load_disc(&dev, &st, &status);
        let cover = res.as_ref().ok().and_then(|i| i.candidates.first()).and_then(|c| {
            status("Fetching artwork…");
            metadata::fetch_cover(c, &st)
        });
        let _ = w.upgrade_in_event_loop(move |ui| {
            with(|s| s.loading = false);
            ui.set_loading(false);
            match res {
                Ok(info) => apply_disc(&ui, info, cover),
                Err(e) => clear_disc(&ui, &format!("{e:#}")),
            }
        });
    });
}

fn apply_disc(ui: &AppWindow, info: DiscInfo, cover: Option<Vec<u8>>) {
    // remember offsets that came from the database or detection
    if let Some(o) = info.offset {
        if info.offset_source != "saved" {
            with(|s| {
                s.cfg.settings.drive_offsets.insert(info.drive_id.clone(), o);
            });
            save_cfg(ui);
        }
        with(|s| s.drive_id = info.drive_id.clone());
        update_offset_text(ui, Some(&info.offset_source));
    }
    let n = info.track_count();
    let ar = match &info.ar {
        Some(Some(a)) => format!("AccurateRip: in database ({} pressing entries) · ready for verified ripping", a.tracks.first().map(|t| t.len()).unwrap_or(0)),
        Some(None) => "AccurateRip: disc not in database · tracks will be verified by secure re-reading".into(),
        None => "AccurateRip unavailable · tracks will be verified by secure re-reading".into(),
    };
    ui.set_ar_summary(ss(ar));
    set_notes(ui, info.notes.clone());
    let releases: Vec<String> = if info.candidates.is_empty() {
        vec!["No metadata found: type it in".into()]
    } else {
        info.candidates.iter().map(|c| c.summary()).collect()
    };
    ui.set_releases(strings(releases));
    ui.set_release_index(0);
    let rows: Vec<TrackRow> = (0..n)
        .map(|i| TrackRow {
            selected: true,
            number: (i + 1) as i32,
            title: ss(format!("Track {}", i + 1)),
            artist: ss(""),
            length: ss(mmss(info.track_seconds(i))),
            status: ss("Ready"),
            progress: 0.0,
            state: 0,
        })
        .collect();
    ui.set_tracks(ModelRc::new(VecModel::from(rows)));
    ui.set_disc_status(ss(format!(
        "{n} tracks · {} · MusicBrainz {}",
        mmss(info.cdtoc.duration().to_f64_lossy()),
        info.cdtoc.musicbrainz_id()
    )));
    let first = info.candidates.first().cloned();
    with(|s| s.disc = Some(info));
    apply_candidate(ui, first);
    set_cover(ui, cover.map(Arc::new), "");
    ui.set_disc_loaded(true);
}

fn set_notes(ui: &AppWindow, notes: Vec<String>) {
    with(|s| s.notes = notes.clone());
    ui.set_notes(strings(notes));
}

/// Once a release has been found another way, earlier "no match"/lookup warnings are stale.
fn clear_metadata_notes(ui: &AppWindow) {
    let kept: Vec<String> = with(|s| s.notes.clone())
        .into_iter()
        .filter(|n| !["MusicBrainz", "GnuDB", "Discogs", "online match"].iter().any(|k| n.contains(k)))
        .collect();
    set_notes(ui, kept);
}

fn apply_candidate(ui: &AppWindow, c: Option<AlbumCandidate>) {
    let tracks = ui.get_tracks();
    if let Some(c) = &c {
        ui.set_album(ss(&c.title));
        ui.set_album_artist(ss(&c.artist));
        ui.set_year(ss(&c.date));
        ui.set_genre(ss(&c.genre));
        ui.set_disc_no(c.disc.max(1) as i32);
        ui.set_disc_total(c.disc_total.max(1) as i32);
        for i in 0..tracks.row_count() {
            if let (Some(mut r), Some(t)) = (tracks.row_data(i), c.tracks.get(i)) {
                r.title = ss(&t.title);
                r.artist = ss(&t.artist);
                tracks.set_row_data(i, r);
            }
        }
    } else {
        ui.set_album(ss("Unknown album"));
        ui.set_album_artist(ss("Unknown artist"));
    }
    with(|s| s.candidate = c);
}

fn set_cover(ui: &AppWindow, bytes: Option<Arc<Vec<u8>>>, source_hint: &str) {
    with(|s| s.cover = bytes.clone());
    let Some(b) = bytes else {
        ui.set_has_cover(false);
        ui.set_cover_info(ss("No artwork found · click the square to choose a file"));
        return;
    };
    match image::load_from_memory(&b) {
        Ok(img) => {
            let (w, h) = (img.width(), img.height());
            let thumb = img.thumbnail(352, 352).to_rgba8();
            let buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(thumb.as_raw(), thumb.width(), thumb.height());
            ui.set_cover(slint::Image::from_rgba8(buf));
            ui.set_has_cover(true);
            let kind = if b.starts_with(&[0x89, b'P']) { "PNG" } else { "JPEG" };
            ui.set_cover_info(ss(format!("Artwork {w}×{h} {kind} · {} KB{source_hint}", b.len() / 1024)));
        }
        Err(e) => {
            ui.set_has_cover(false);
            ui.set_cover_info(ss(format!("Artwork unreadable: {e}")));
        }
    }
}

fn track_state(text: &str) -> i32 {
    if text.starts_with("Done") {
        if text.contains("Suspect") || text.contains("Not accurate") && !text.contains("Secure") {
            4
        } else if text.contains("Done · Accurate") {
            2
        } else if text.contains("Done · Secure") {
            3
        } else {
            4
        }
    } else if text.starts_with("Failed") {
        5
    } else {
        1
    }
}

fn wire_rip(ui: &AppWindow) {
    let w = ui.as_weak();
    ui.on_drive_selected(move |i| {
        let ui = w.unwrap();
        if let Some(d) = with(|s| s.drives.get(i as usize).cloned()) {
            with(|s| {
                s.cfg.settings.cd_device = d.display().to_string();
                s.last_status = None;
            });
            ui.set_s_device(ss(d.display().to_string()));
            save_cfg(&ui);
        }
        identify_drive(&ui);
        poll_drive(&ui);
    });
    let w = ui.as_weak();
    ui.on_refresh_drives(move || refresh_drives(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_load_disc(move || load_disc(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_eject(move || {
        let ui = w.unwrap();
        match Drive::open(&current_device(&ui)).and_then(|d| d.eject()) {
            Ok(()) => clear_disc(&ui, "Insert an audio CD"),
            Err(e) => toast(&ui, format!("{e:#}")),
        }
    });
    let w = ui.as_weak();
    ui.on_release_selected(move |i| {
        let ui = w.unwrap();
        let c = with(|s| s.disc.as_ref().and_then(|d| d.candidates.get(i as usize).cloned()));
        let Some(c) = c else { return };
        apply_candidate(&ui, Some(c.clone()));
        let req = with(|s| {
            s.cover_req += 1;
            s.cover_req
        });
        let st = with(|s| s.cfg.settings.clone());
        ui.set_cover_info(ss("Fetching artwork…"));
        let w = ui.as_weak();
        std::thread::spawn(move || {
            let cover = metadata::fetch_cover(&c, &st);
            let _ = w.upgrade_in_event_loop(move |ui| {
                if with(|s| s.cover_req) == req {
                    set_cover(&ui, cover.map(Arc::new), "");
                }
            });
        });
    });
    let w = ui.as_weak();
    ui.on_track_title_edited(move |i, t| {
        let m = w.unwrap().get_tracks();
        if let Some(mut r) = m.row_data(i as usize) {
            r.title = t;
            m.set_row_data(i as usize, r);
        }
    });
    let w = ui.as_weak();
    ui.on_track_artist_edited(move |i, t| {
        let m = w.unwrap().get_tracks();
        if let Some(mut r) = m.row_data(i as usize) {
            r.artist = t;
            m.set_row_data(i as usize, r);
        }
    });
    let w = ui.as_weak();
    ui.on_track_toggled(move |i, on| {
        let m = w.unwrap().get_tracks();
        if let Some(mut r) = m.row_data(i as usize) {
            r.selected = on;
            m.set_row_data(i as usize, r);
        }
    });
    let w = ui.as_weak();
    ui.on_select_all(move |on| {
        let m = w.unwrap().get_tracks();
        for i in 0..m.row_count() {
            if let Some(mut r) = m.row_data(i) {
                r.selected = on;
                m.set_row_data(i, r);
            }
        }
    });
    let w = ui.as_weak();
    ui.on_choose_cover(move || {
        let w = w.clone();
        let max = with(|s| s.cfg.settings.art_max_px);
        std::thread::spawn(move || {
            let Some(f) = rfd::FileDialog::new().set_title("Choose album artwork").add_filter("Images", &["jpg", "jpeg", "png"]).pick_file() else { return };
            let r = std::fs::read(&f).map_err(anyhow::Error::from).and_then(|b| metadata::normalise_art(b, max));
            let _ = w.upgrade_in_event_loop(move |ui| match r {
                Ok(b) => set_cover(&ui, Some(Arc::new(b)), " · from file"),
                Err(e) => toast(&ui, format!("Could not use that image: {e:#}")),
            });
        });
    });
    let w = ui.as_weak();
    ui.on_rip_preset_selected(move |i| {
        let ui = w.unwrap();
        with(|s| {
            if let Some(p) = s.cfg.presets.get(i as usize) {
                s.cfg.settings.last_rip_preset = p.name.clone();
            }
        });
        save_cfg(&ui);
        refresh_summaries(&ui);
    });
    let w = ui.as_weak();
    ui.on_dismiss_notes(move || set_notes(&w.unwrap(), vec![]));
    let w = ui.as_weak();
    ui.on_search_discogs(move || search_discogs(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_start_rip(move || start_rip(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_cancel_rip(move || {
        if let Some(c) = with(|s| s.rip_cancel.clone()) {
            c.store(true, Ordering::Relaxed);
            w.unwrap().set_rip_status(ss("Cancelling…"));
        }
    });
}

/// Manual Discogs search from the album/artist fields (or the disc's barcode if the
/// album field is empty). Matches are added to the release list and the best one applied.
fn search_discogs(ui: &AppWindow) {
    let Some((lengths, mcn)) = with(|s| s.disc.as_ref().map(|d| ((0..d.track_count()).map(|i| d.track_seconds(i)).collect::<Vec<_>>(), d.mcn.clone()))) else {
        return;
    };
    let album = ui.get_album().trim().to_string();
    let artist = ui.get_album_artist().trim().to_string();
    let unknown = album.is_empty() || album == "Unknown album";
    let query = match (unknown, mcn) {
        (true, Some(b)) => crate::discogs::Query::Barcode(b),
        (true, None) => {
            toast(ui, "Type the album title (and artist) to search Discogs");
            return;
        }
        (false, _) => crate::discogs::Query::Text { artist: if artist == "Unknown artist" { String::new() } else { artist }, album },
    };
    let st = with(|s| s.cfg.settings.clone());
    ui.set_discogs_busy(true);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = crate::discogs::search(&query, &lengths, &st.discogs_token);
        let cover = r.as_ref().ok().and_then(|f| f.first()).and_then(|c| metadata::fetch_cover(c, &st));
        let _ = w.upgrade_in_event_loop(move |ui| {
            ui.set_discogs_busy(false);
            let found = match r {
                Ok(f) => f,
                Err(e) => return toast(&ui, crate::net::describe("Discogs", &e)),
            };
            if found.is_empty() {
                return toast(&ui, "No Discogs release matches this disc's track count and lengths");
            }
            let n = found.len();
            let first = found[0].clone();
            let idx = with(|s| {
                let d = s.disc.as_mut()?;
                // drop the "nothing found" state and earlier Discogs results for this disc
                d.candidates.retain(|c| c.source != "Discogs");
                let at = d.candidates.len();
                d.candidates.extend(found);
                Some((at, d.candidates.iter().map(|c| c.summary()).collect::<Vec<_>>()))
            });
            let Some((at, names)) = idx else { return };
            ui.set_releases(strings(names));
            ui.set_release_index(at as i32);
            apply_candidate(&ui, Some(first));
            clear_metadata_notes(&ui);
            with(|s| s.cover_req += 1);
            if cover.is_some() {
                set_cover(&ui, cover.map(Arc::new), " · Discogs match");
            }
            toast(&ui, if n == 1 { "Found 1 matching Discogs release".to_string() } else { format!("Found {n} matching Discogs releases, best match selected") });
        });
    });
}

fn build_metas(ui: &AppWindow) -> Option<(DiscInfo, Vec<TrackMeta>, Vec<bool>)> {
    with(|s| {
        let disc = s.disc.clone()?;
        let cand = s.candidate.clone();
        let rows = ui.get_tracks();
        let n = rows.row_count();
        let album_artist = ui.get_album_artist().to_string();
        let mut metas = Vec::with_capacity(n);
        let mut selected = Vec::with_capacity(n);
        for i in 0..n {
            let r = rows.row_data(i)?;
            let ct = cand.as_ref().and_then(|c| c.tracks.get(i));
            let artist = if r.artist.is_empty() { album_artist.clone() } else { r.artist.to_string() };
            selected.push(r.selected);
            metas.push(TrackMeta {
                title: r.title.to_string(),
                artist,
                album: ui.get_album().to_string(),
                album_artist: album_artist.clone(),
                track: (i + 1) as u32,
                track_total: n as u32,
                disc: ui.get_disc_no() as u32,
                disc_total: ui.get_disc_total() as u32,
                year: ui.get_year().to_string(),
                genre: ui.get_genre().to_string(),
                isrc: disc.isrcs.get(i).cloned().flatten().or_else(|| ct.map(|t| t.isrc.clone())).unwrap_or_default(),
                mb_release_id: cand.as_ref().map(|c| c.release_id.clone()).unwrap_or_default(),
                mb_release_group_id: cand.as_ref().map(|c| c.release_group_id.clone()).unwrap_or_default(),
                mb_album_artist_id: cand.as_ref().map(|c| c.artist_id.clone()).unwrap_or_default(),
                mb_recording_id: ct.map(|t| t.recording_id.clone()).unwrap_or_default(),
                mb_track_id: ct.map(|t| t.track_id.clone()).unwrap_or_default(),
                mb_artist_id: ct.map(|t| t.artist_id.clone()).unwrap_or_default(),
                mb_disc_id: disc.cdtoc.musicbrainz_id().to_string(),
                discogs_release_id: cand.as_ref().map(|c| c.discogs_id.clone()).unwrap_or_default(),
                label: cand.as_ref().map(|c| c.label.clone()).unwrap_or_default(),
                catalog: cand.as_ref().map(|c| c.catalog.clone()).unwrap_or_default(),
                barcode: cand.as_ref().map(|c| c.barcode.clone()).filter(|b| !b.is_empty()).or_else(|| disc.mcn.clone()).unwrap_or_default(),
                cdtoc: disc.cdtoc.to_string(),
                cover: s.cover.clone(),
                ..Default::default()
            });
        }
        Some((disc, metas, selected))
    })
}

fn start_rip(ui: &AppWindow) {
    let Some((mut info, metas, selected)) = build_metas(ui) else { return };
    if !selected.iter().any(|&s| s) {
        toast(ui, "Select at least one track");
        return;
    }
    // a manually entered offset in Settings wins
    if let Some(o) = with(|s| s.cfg.settings.drive_offsets.get(&info.drive_id).copied()) {
        info.offset = Some(o);
    }
    if info.offset.is_none() {
        ui.set_page(3);
        toast(ui, "Set your drive's read offset first (Settings → Read offset), or insert a well-known CD so it can be detected");
        return;
    }
    let preset_idx = ui.get_rip_preset_index().max(0) as usize;
    let Some((preset, settings)) = with(|s| s.cfg.presets.get(preset_idx).cloned().map(|p| (p, s.cfg.settings.clone()))) else { return };
    let cancel = Arc::new(AtomicBool::new(false));
    let n = metas.len();
    with(|s| {
        s.rip_cancel = Some(cancel.clone());
        s.rip_phase = vec![(0, 0.0); n];
    });
    let rows = ui.get_tracks();
    for i in 0..rows.row_count() {
        if let Some(mut r) = rows.row_data(i) {
            r.status = ss(if r.selected { "Queued" } else { "Skipped" });
            r.state = 0;
            r.progress = 0.0;
            rows.set_row_data(i, r);
        }
    }
    ui.set_ripping(true);
    ui.set_rip_progress(0.0);
    ui.set_rip_status(ss("Starting…"));
    let n_sel = selected.iter().filter(|&&s| s).count().max(1) as f32;
    let w: Weak<AppWindow> = ui.as_weak();
    let sink = Arc::new(move |ev: RipEvent| {
        let _ = w.upgrade_in_event_loop(move |ui| on_rip_event(&ui, ev, n_sel));
    });
    std::thread::spawn(move || ripper::rip(RipRequest { info, metas, selected, preset, settings }, cancel, sink));
}

fn on_rip_event(ui: &AppWindow, ev: RipEvent, n_sel: f32) {
    match ev {
        RipEvent::Track { idx, text, progress } => {
            let rows = ui.get_tracks();
            let encoding = text.starts_with("Encoding") || text.starts_with("Done");
            let overall = with(|s| {
                if let Some(p) = s.rip_phase.get_mut(idx) {
                    *p = (if encoding { 2 } else { 1 }, progress);
                }
                s.rip_phase.iter().map(|(ph, f)| match ph { 1 => f * 0.6, 2 => 0.6 + f * 0.4, _ => 0.0 }).sum::<f32>() / n_sel
            });
            if let Some(mut r) = rows.row_data(idx) {
                r.state = track_state(&text);
                r.status = ss(&text);
                r.progress = progress;
                rows.set_row_data(idx, r);
            }
            ui.set_rip_progress(overall);
            if !encoding {
                ui.set_rip_status(ss(format!("Track {}: {text} {:.0}%", idx + 1, progress * 100.0)));
            }
        }
        RipEvent::Status(s) => ui.set_rip_status(ss(s)),
        RipEvent::Finished { ok, summary } => {
            with(|s| {
                s.rip_cancel = None;
                s.last_status = None; // re-detect (disc may have been ejected)
                s.just_ripped = ok;
            });
            ui.set_ripping(false);
            ui.set_rip_progress(1.0);
            ui.set_ar_summary(ss(&summary));
            toast(ui, if ok { format!("Rip complete: {summary}") } else { format!("Rip stopped: {summary}") });
        }
    }
}

// =================================================================== convert

fn add_paths(ui: &AppWindow, paths: Vec<PathBuf>) -> usize {
    if ui.get_converting() {
        toast(ui, "A conversion is running: add more files when it has finished, or cancel it");
        return 0;
    }
    let items = converter::scan(&paths, ui.get_recursive());
    let added = with(|s| {
        let before = s.files.len();
        for i in items {
            if !s.files.iter().any(|f| f.path == i.path) {
                s.files.push(i);
            }
        }
        s.files.len() - before
    });
    rebuild_files(ui);
    if !paths.is_empty() {
        ui.set_page(1);
    }
    added
}

/// Files dropped onto the window (any page) go to the converter queue.
fn on_drop(ui: &AppWindow, paths: Vec<PathBuf>) {
    if std::env::var_os("CODA_DEBUG").is_some() {
        eprintln!("coda: dropped {paths:?}");
    }
    ui.set_drop_hover(false);
    if paths.is_empty() {
        return;
    }
    if ui.get_page() == 4 {
        tag_add_paths(ui, paths);
        return;
    }
    if ui.get_converting() {
        toast(ui, "A conversion is running: add more files when it has finished, or cancel it");
        return;
    }
    let n = add_paths(ui, paths);
    toast(ui, match n {
        0 => "No new audio files in what you dropped".to_string(),
        1 => "Added 1 file".to_string(),
        n => format!("Added {n} files"),
    });
}

fn init_dnd(ui: &AppWindow) {
    use crate::dnd::{DndEvent, X11Collector};
    use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};

    // X11: winit reports each hovered/dropped file separately; gather them into one drop
    thread_local! { static X11_DROP: RefCell<X11Collector> = RefCell::new(X11Collector::default()); }
    let w = ui.as_weak();
    ui.window().on_winit_window_event(move |_, ev| {
        match ev {
            WindowEvent::HoveredFile(_) => {
                if let Some(ui) = w.upgrade() {
                    ui.set_drop_hover(true);
                }
            }
            WindowEvent::HoveredFileCancelled => {
                if let Some(ui) = w.upgrade() {
                    ui.set_drop_hover(false);
                }
            }
            WindowEvent::DroppedFile(p) => {
                let first = X11_DROP.with(|c| {
                    let mut c = c.borrow_mut();
                    c.push(p.clone());
                    c.pending_len() == 1
                });
                if first {
                    let w = w.clone();
                    slint::Timer::single_shot(Duration::from_millis(60), move || {
                        let paths = X11_DROP.with(|c| c.borrow_mut().take());
                        if let Some(ui) = w.upgrade() {
                            on_drop(&ui, paths);
                        }
                    });
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });

    // Wayland: start our own drag-and-drop listener once the native window exists
    let w = ui.as_weak();
    let tries = std::cell::Cell::new(0);
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        use slint::winit_030::winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        tries.set(tries.get() + 1);
        let Some(ui) = w.upgrade() else { return };
        let display = ui.window().with_winit_window(|win| match win.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(h)) => Some(Some(h.display.as_ptr())),
            Ok(_) => Some(None), // X11: handled by winit above
            Err(_) => None,
        });
        let done = match display.flatten() {
            Some(Some(ptr)) => {
                let sink = Box::new(|ev: DndEvent| {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = UI.with(|u| u.borrow().as_ref().and_then(|w| w.upgrade())) else { return };
                        match ev {
                            DndEvent::Hover(h) => ui.set_drop_hover(h),
                            DndEvent::Drop(paths) => on_drop(&ui, paths),
                        }
                    });
                });
                // SAFETY: the display belongs to the app window, which lives until exit
                if let Err(e) = unsafe { crate::dnd::start_wayland(ptr, sink) } {
                    eprintln!("drag and drop unavailable: {e}");
                }
                true
            }
            Some(None) => true,
            None => tries.get() > 40,
        };
        if done {
            DND_TIMER.with(|t| t.borrow_mut().take());
        }
    });
    DND_TIMER.with(|t| *t.borrow_mut() = Some(timer));
}

thread_local! {
    static DND_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

fn rebuild_files(ui: &AppWindow) {
    let rows: Vec<FileRow> = with(|s| {
        s.files
            .iter()
            .map(|f| FileRow {
                name: ss(f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                folder: ss(f.path.parent().map(|p| p.display().to_string()).unwrap_or_default()),
                status: ss("Queued"),
                progress: 0.0,
                state: 0,
            })
            .collect()
    });
    let n = rows.len();
    ui.set_files(ModelRc::new(VecModel::from(rows)));
    if !ui.get_converting() {
        ui.set_convert_status(ss(if n == 0 { "Ready".into() } else { format!("{n} files queued") }));
    }
}

fn pick_paths(ui: &AppWindow, folders: bool) {
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let picked = if folders {
            rfd::FileDialog::new().set_title("Add folders").pick_folders()
        } else {
            rfd::FileDialog::new().set_title("Add audio files").add_filter("Audio", formats::INPUT_EXTENSIONS).pick_files()
        };
        if let Some(p) = picked {
            let _ = w.upgrade_in_event_loop(move |ui| {
                add_paths(&ui, p);
            });
        }
    });
}

fn wire_convert(ui: &AppWindow) {
    let w = ui.as_weak();
    ui.on_add_files(move || pick_paths(&w.unwrap(), false));
    let w = ui.as_weak();
    ui.on_add_folder(move || pick_paths(&w.unwrap(), true));
    let w = ui.as_weak();
    ui.on_clear_files(move || {
        with(|s| s.files.clear());
        rebuild_files(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_remove_file(move |i| {
        with(|s| {
            if (i as usize) < s.files.len() {
                s.files.remove(i as usize);
            }
        });
        rebuild_files(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_convert_preset_selected(move |i| {
        let ui = w.unwrap();
        with(|s| {
            if let Some(p) = s.cfg.presets.get(i as usize) {
                s.cfg.settings.last_convert_preset = p.name.clone();
            }
        });
        save_cfg(&ui);
        refresh_summaries(&ui);
    });
    let w = ui.as_weak();
    ui.on_start_convert(move || start_convert(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_cancel_convert(move || {
        if let Some(c) = with(|s| s.conv_cancel.clone()) {
            c.store(true, Ordering::Relaxed);
            w.unwrap().set_convert_status(ss("Cancelling…"));
        }
    });
}

fn start_convert(ui: &AppWindow) {
    let idx = ui.get_convert_preset_index().max(0) as usize;
    let Some((preset, settings, files)) = with(|s| s.cfg.presets.get(idx).cloned().map(|p| (p, s.cfg.settings.clone(), s.files.clone()))) else { return };
    let n_out = preset.outputs.iter().filter(|o| o.enabled).count();
    if n_out == 0 {
        toast(ui, "This preset has no enabled outputs");
        return;
    }
    let mode = match ui.get_naming_mode() {
        1 => NamingMode::Mirror,
        2 => NamingMode::BesideSource,
        _ => NamingMode::Preset,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    with(|s| {
        s.conv_cancel = Some(cancel.clone());
        s.conv = (0..files.len()).map(|_| FileProg { frac: vec![0.0; n_out], ..Default::default() }).collect();
        s.conv_started = Some(Instant::now());
    });
    rebuild_files(ui);
    ui.set_converting(true);
    ui.set_convert_progress(0.0);
    let threads = settings.worker_threads();
    ui.set_convert_status(ss(format!("Reading tags… ({threads} encoders)")));
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let w2 = w.clone();
        let sink = Arc::new(move |ev: PoolEvent| {
            let _ = w2.upgrade_in_event_loop(move |ui| on_convert_event(&ui, ev, n_out));
        });
        let pool = Pool::new(
            threads,
            EncodeSettings { ffmpeg: settings.ffmpeg.clone(), existing: settings.existing.clone(), verify_lossless: settings.verify_lossless },
            sink,
            cancel.clone(),
        );
        for (fi, item) in files.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            for (oi, job) in converter::plan(item, &preset, mode, &settings).into_iter().enumerate() {
                pool.submit(fi * 100 + oi, job);
            }
        }
        pool.join();
        let _ = w.upgrade_in_event_loop(move |ui| finish_convert(&ui));
    });
}

fn on_convert_event(ui: &AppWindow, ev: PoolEvent, n_out: usize) {
    let (id, frac, result) = match ev {
        PoolEvent::Started { id } => (id, 0.0, None),
        PoolEvent::Progress { id, fraction } => (id, fraction, None),
        PoolEvent::Finished { id, result } => (id, 1.0, Some(result)),
    };
    let (fi, oi) = (id / 100, id % 100);
    let rows = ui.get_files();
    let (row_update, overall, done_files, total) = with(|s| {
        let total = s.conv.len();
        let Some(p) = s.conv.get_mut(fi) else { return (None, 0.0, 0, total) };
        if let Some(f) = p.frac.get_mut(oi) {
            *f = frac;
        }
        if let Some(r) = &result {
            p.finished += 1;
            match r {
                Ok(Outcome::Written(_, v)) => p.verified += *v as usize,
                Ok(Outcome::Skipped(_)) => p.skipped += 1,
                Err(e) => p.errors.push(e.clone()),
            }
        }
        let avg = p.frac.iter().sum::<f32>() / n_out as f32;
        let (text, state) = if p.finished >= n_out {
            if !p.errors.is_empty() {
                (format!("Failed: {}", p.errors[0]), 5)
            } else if p.skipped == n_out {
                ("Skipped (already exists)".to_string(), 4)
            } else if p.verified > 0 {
                (format!("Done · {} verified lossless", p.verified), 2)
            } else {
                ("Done".to_string(), 3)
            }
        } else {
            (format!("Encoding {}/{} outputs", p.finished + 1, n_out), 1)
        };
        let all: f32 = s.conv.iter().map(|p| p.frac.iter().sum::<f32>()).sum::<f32>() / (total * n_out).max(1) as f32;
        let done = s.conv.iter().filter(|p| p.finished >= n_out).count();
        (Some((text, state, avg)), all, done, total)
    });
    if let Some((text, state, avg)) = row_update {
        if let Some(mut r) = rows.row_data(fi) {
            r.status = ss(text);
            r.state = state;
            r.progress = avg;
            rows.set_row_data(fi, r);
        }
    }
    ui.set_convert_progress(overall);
    ui.set_convert_status(ss(format!("Converting… {done_files}/{total} files")));
}

fn finish_convert(ui: &AppWindow) {
    let (ok, failed, skipped, secs, cancelled) = with(|s| {
        let ok = s.conv.iter().filter(|p| p.errors.is_empty() && p.finished > 0 && p.skipped < p.frac.len()).count();
        let failed = s.conv.iter().filter(|p| !p.errors.is_empty()).count();
        let skipped = s.conv.iter().filter(|p| p.errors.is_empty() && p.skipped == p.frac.len()).count();
        let secs = s.conv_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let cancelled = s.conv_cancel.as_ref().map(|c| c.load(Ordering::Relaxed)).unwrap_or(false);
        s.conv_cancel = None;
        (ok, failed, skipped, secs, cancelled)
    });
    ui.set_converting(false);
    ui.set_convert_progress(1.0);
    let msg = format!(
        "{}{ok} converted, {skipped} skipped, {failed} failed in {}",
        if cancelled { "Cancelled · " } else { "" },
        mmss(secs)
    );
    ui.set_convert_status(ss(&msg));
    toast(ui, msg);
}

// =================================================================== presets

fn preset_summary(p: &Preset) -> String {
    p.outputs
        .iter()
        .filter(|o| o.enabled)
        .map(|o| format!("{} {} → {}", o.format.label(), o.options.describe(o.format), o.dest_root))
        .collect::<Vec<_>>()
        .join("  ·  ")
}

fn refresh_summaries(ui: &AppWindow) {
    with(|s| {
        let p = &s.cfg.presets;
        let r = p.get(ui.get_rip_preset_index().max(0) as usize).map(preset_summary).unwrap_or_default();
        let c = p.get(ui.get_convert_preset_index().max(0) as usize).map(preset_summary).unwrap_or_default();
        ui.set_rip_preset_summary(ss(r));
        ui.set_convert_preset_summary(ss(c));
    });
}

fn refresh_presets(ui: &AppWindow) {
    let (names, ri, ci) = with(|s| {
        let names: Vec<String> = s.cfg.presets.iter().map(|p| p.name.clone()).collect();
        let ri = names.iter().position(|n| *n == s.cfg.settings.last_rip_preset).unwrap_or(0);
        let ci = names.iter().position(|n| *n == s.cfg.settings.last_convert_preset).unwrap_or(0);
        (names, ri, ci)
    });
    ui.set_preset_names(strings(names));
    ui.set_rip_preset_index(ri as i32);
    ui.set_convert_preset_index(ci as i32);
    refresh_summaries(ui);
}

fn mode_label(m: QualityMode) -> &'static str {
    match m {
        QualityMode::Level => "Compression",
        QualityMode::Vbr => "VBR",
        QualityMode::Bitrate => "Bitrate",
        QualityMode::None => "Lossless",
    }
}

fn mode_from_label(s: &str) -> QualityMode {
    match s {
        "Compression" => QualityMode::Level,
        "VBR" => QualityMode::Vbr,
        "Bitrate" => QualityMode::Bitrate,
        _ => QualityMode::None,
    }
}

fn value_range(f: Format, m: QualityMode) -> (i32, i32, &'static str) {
    match (f, m) {
        (Format::Flac, _) => (0, 8, "Level (0 fast – 8 small)"),
        (Format::WavPack, _) => (0, 3, "Level (0 – 3)"),
        (Format::Mp3, QualityMode::Vbr) => (0, 9, "Quality V0 best – V9"),
        (Format::Vorbis, QualityMode::Vbr) => (-1, 10, "Quality (-1 – 10)"),
        (Format::Mp3, _) | (Format::Wma, _) => (32, 320, "Bitrate kbps"),
        (Format::Aac, _) => (32, 512, "Bitrate kbps"),
        (Format::Opus, _) => (6, 510, "Bitrate kbps"),
        (Format::Vorbis, _) => (45, 500, "Bitrate kbps"),
        _ => (0, 0, ""),
    }
}

fn sample_meta() -> TrackMeta {
    TrackMeta {
        title: "Third Song".into(),
        artist: "The Example Band".into(),
        album_artist: "The Example Band".into(),
        album: "Sample Album".into(),
        track: 3,
        track_total: 11,
        disc: 1,
        disc_total: 1,
        year: "2024".into(),
        genre: "Rock".into(),
        ..Default::default()
    }
}

fn output_row(o: &OutputSpec, portable: bool) -> OutputRow {
    let modes: Vec<String> = o.format.quality_modes().iter().map(|m| mode_label(*m).to_string()).collect();
    let (min, max, label) = value_range(o.format, o.options.mode);
    let m = sample_meta();
    let ctx = NameContext { meta: &m, filename: "original file", reldir: "Rock/Sample Album", format: o.format.label() };
    let preview = naming::destination(&config::expand_home(&o.dest_root), &o.template, &ctx, o.format.extension(), portable);
    OutputRow {
        enabled: o.enabled,
        format: ss(o.format.label()),
        modes: strings(modes),
        mode: ss(mode_label(o.options.mode)),
        value: o.options.value,
        value_min: min,
        value_max: max,
        value_label: ss(label),
        dest: ss(&o.dest_root),
        template: ss(&o.template),
        sample_rate: ss(if o.options.sample_rate == 0 { "Keep".into() } else { o.options.sample_rate.to_string() }),
        bit_depth: ss(if o.options.bit_depth == 0 { "Keep".into() } else { o.options.bit_depth.to_string() }),
        stereo: o.options.stereo,
        preview: ss(preview.display().to_string()),
    }
}

fn show_edit_outputs(ui: &AppWindow) {
    let rows: Vec<OutputRow> = with(|s| s.edit.outputs.iter().map(|o| output_row(o, s.cfg.settings.portable_filenames)).collect());
    ui.set_edit_outputs(ModelRc::new(VecModel::from(rows)));
}

fn select_edit_preset(ui: &AppWindow, i: usize) {
    let found = with(|s| {
        let i = i.min(s.cfg.presets.len().saturating_sub(1));
        s.cfg.presets.get(i).cloned().map(|p| {
            s.edit_index = i;
            s.edit = p.clone();
            (i, p.name)
        })
    });
    if let Some((i, name)) = found {
        ui.set_edit_preset_index(i as i32);
        ui.set_edit_preset_name(ss(name));
        show_edit_outputs(ui);
    }
}

fn new_output(f: Format) -> OutputSpec {
    OutputSpec {
        enabled: true,
        format: f,
        options: OutputOptions::default_for(f),
        dest_root: format!("~/Music/{}", f.label().split(' ').next().unwrap_or("Music")),
        template: TAG_TEMPLATE.into(),
    }
}

fn wire_presets(ui: &AppWindow) {
    let w = ui.as_weak();
    ui.on_preset_select(move |i| select_edit_preset(&w.unwrap(), i as usize));
    let w = ui.as_weak();
    ui.on_preset_new(move || {
        let ui = w.unwrap();
        let i = with(|s| {
            let mut name = "New preset".to_string();
            let mut n = 2;
            while s.cfg.presets.iter().any(|p| p.name == name) {
                name = format!("New preset {n}");
                n += 1;
            }
            s.cfg.presets.push(Preset { name, outputs: vec![new_output(Format::Flac)] });
            s.cfg.presets.len() - 1
        });
        save_cfg(&ui);
        refresh_presets(&ui);
        select_edit_preset(&ui, i);
    });
    let w = ui.as_weak();
    ui.on_preset_duplicate(move || {
        let ui = w.unwrap();
        let i = with(|s| {
            let mut p = s.edit.clone();
            p.name = format!("{} (copy)", s.cfg.presets[s.edit_index].name);
            s.cfg.presets.push(p);
            s.cfg.presets.len() - 1
        });
        save_cfg(&ui);
        refresh_presets(&ui);
        select_edit_preset(&ui, i);
    });
    let w = ui.as_weak();
    ui.on_preset_delete(move || {
        let ui = w.unwrap();
        let i = with(|s| {
            if s.cfg.presets.len() > 1 {
                s.cfg.presets.remove(s.edit_index);
            }
            s.edit_index.saturating_sub(1)
        });
        save_cfg(&ui);
        refresh_presets(&ui);
        select_edit_preset(&ui, i);
        reinstall_integration_if_present(&ui);
    });
    let w = ui.as_weak();
    ui.on_preset_save(move || {
        let ui = w.unwrap();
        let name = ui.get_edit_preset_name().trim().to_string();
        let res = with(|s| {
            if name.is_empty() {
                return Err("Give the preset a name");
            }
            if s.cfg.presets.iter().enumerate().any(|(i, p)| i != s.edit_index && p.name == name) {
                return Err("Another preset already has that name");
            }
            let old = s.cfg.presets[s.edit_index].name.clone();
            s.edit.name = name.clone();
            s.cfg.presets[s.edit_index] = s.edit.clone();
            if s.cfg.settings.last_rip_preset == old {
                s.cfg.settings.last_rip_preset = name.clone();
            }
            if s.cfg.settings.last_convert_preset == old {
                s.cfg.settings.last_convert_preset = name.clone();
            }
            Ok(s.edit_index)
        });
        match res {
            Ok(i) => {
                save_cfg(&ui);
                refresh_presets(&ui);
                select_edit_preset(&ui, i);
                reinstall_integration_if_present(&ui);
                toast(&ui, "Preset saved");
            }
            Err(e) => toast(&ui, e),
        }
    });
    let w = ui.as_weak();
    ui.on_output_add(move || {
        with(|s| s.edit.outputs.push(new_output(Format::Mp3)));
        show_edit_outputs(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_output_remove(move |i| {
        with(|s| {
            if s.edit.outputs.len() > 1 && (i as usize) < s.edit.outputs.len() {
                s.edit.outputs.remove(i as usize);
            }
        });
        show_edit_outputs(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_output_changed(move |i, row| {
        let ui = w.unwrap();
        let portable = with(|s| s.cfg.settings.portable_filenames);
        let updated = with(|s| {
            let o = s.edit.outputs.get_mut(i as usize)?;
            let fmt = Format::from_label(&row.format).unwrap_or(o.format);
            if fmt != o.format {
                let old_short = o.format.label().split(' ').next().unwrap_or("").to_string();
                let new_short = fmt.label().split(' ').next().unwrap_or("").to_string();
                if o.dest_root.ends_with(&old_short) {
                    o.dest_root = format!("{}{}", &o.dest_root[..o.dest_root.len() - old_short.len()], new_short);
                }
                o.format = fmt;
                o.options = OutputOptions::default_for(fmt);
            } else {
                let mode = mode_from_label(&row.mode);
                if mode != o.options.mode && fmt.quality_modes().contains(&mode) {
                    o.options.mode = mode;
                    o.options.value = match (fmt, mode) {
                        (Format::Mp3, QualityMode::Vbr) => 0,
                        (Format::Mp3, QualityMode::Bitrate) => 320,
                        (Format::Vorbis, QualityMode::Vbr) => 6,
                        (Format::Vorbis, QualityMode::Bitrate) => 256,
                        _ => o.options.value,
                    };
                } else {
                    o.options.value = row.value;
                }
                o.options.sample_rate = row.sample_rate.parse().unwrap_or(0);
                o.options.bit_depth = row.bit_depth.parse().unwrap_or(0);
                o.options.stereo = row.stereo;
            }
            o.enabled = row.enabled;
            o.dest_root = if fmt == o.format && row.dest.as_str() != o.dest_root && !row.dest.is_empty() { row.dest.to_string() } else { o.dest_root.clone() };
            o.template = row.template.to_string();
            Some(output_row(o, portable))
        });
        if let Some(r) = updated {
            ui.get_edit_outputs().set_row_data(i as usize, r);
        }
    });
    let w = ui.as_weak();
    ui.on_browse_dest(move |i| {
        let w = w.clone();
        std::thread::spawn(move || {
            if let Some(d) = rfd::FileDialog::new().set_title("Destination folder").pick_folder() {
                let _ = w.upgrade_in_event_loop(move |ui| {
                    let mut dest = d.display().to_string();
                    if let Some(h) = dirs::home_dir() {
                        if let Ok(rel) = d.strip_prefix(&h) {
                            dest = format!("~/{}", rel.display());
                        }
                    }
                    let portable = with(|s| s.cfg.settings.portable_filenames);
                    let r = with(|s| {
                        s.edit.outputs.get_mut(i as usize).map(|o| {
                            o.dest_root = dest;
                            output_row(o, portable)
                        })
                    });
                    if let Some(r) = r {
                        ui.get_edit_outputs().set_row_data(i as usize, r);
                    }
                });
            }
        });
    });
}

fn reinstall_integration_if_present(ui: &AppWindow) {
    if integration::is_installed() {
        let cfg = with(|s| s.cfg.clone());
        if let Err(e) = integration::install(&cfg) {
            toast(ui, format!("Could not update right-click menus: {e:#}"));
        }
    }
}

// =================================================================== settings

fn load_settings_into_ui(ui: &AppWindow) {
    with(|s| {
        let st = &s.cfg.settings;
        ui.set_s_ffmpeg(ss(&st.ffmpeg));
        ui.set_s_threads(st.threads as i32);
        ui.set_s_device(ss(&st.cd_device));
        ui.set_s_verify_mode(match st.verify_mode {
            VerifyMode::AccurateRipThenSecure => 0,
            VerifyMode::AlwaysSecure => 1,
            VerifyMode::Burst => 2,
        });
        ui.set_s_retries(st.max_retries as i32);
        ui.set_s_matches(st.required_matches as i32);
        ui.set_s_c2(st.use_c2);
        ui.set_s_eject(st.eject_after_rip);
        ui.set_s_log(st.write_log);
        ui.set_s_cue(st.write_cue);
        ui.set_s_mb(st.musicbrainz);
        ui.set_s_gnudb(st.gnudb_fallback);
        ui.set_s_discogs(st.discogs_fallback);
        ui.set_s_discogs_token(ss(&st.discogs_token));
        ui.set_s_contact(ss(&st.contact_email));
        ui.set_s_art_max(st.art_max_px as i32);
        ui.set_s_art_min(st.art_min_px as i32);
        ui.set_s_cover_file(st.save_cover_file);
        ui.set_s_cover_name(ss(&st.cover_file_name));
        ui.set_s_portable(st.portable_filenames);
        ui.set_s_existing(match st.existing {
            ExistingFile::Skip => 0,
            ExistingFile::Overwrite => 1,
            ExistingFile::Rename => 2,
        });
        ui.set_s_verify_lossless(st.verify_lossless);
    });
}

fn ffmpeg_status(ui: &AppWindow) {
    let ff = with(|s| s.cfg.settings.ffmpeg.clone());
    let ver = std::process::Command::new(&ff)
        .arg("-version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(|l| l.split(" Copyright").next().unwrap_or(l).to_string()));
    let msg = match ver {
        None => "ffmpeg not found: install it (e.g. 'sudo pacman -S ffmpeg') or set the full path.".to_string(),
        Some(v) => {
            let missing = formats::missing_encoders(&ff);
            if missing.is_empty() {
                format!("{v} · all encoders available")
            } else {
                format!("{v} · missing encoders for: {}", missing.iter().map(|f| f.label()).collect::<Vec<_>>().join(", "))
            }
        }
    };
    ui.set_ffmpeg_status(ss(msg));
}

fn wire_settings(ui: &AppWindow) {
    let w = ui.as_weak();
    ui.on_settings_save(move || {
        let ui = w.unwrap();
        with(|s| {
            let st = &mut s.cfg.settings;
            st.ffmpeg = ui.get_s_ffmpeg().trim().to_string();
            st.threads = ui.get_s_threads().max(0) as usize;
            st.cd_device = ui.get_s_device().trim().to_string();
            st.verify_mode = match ui.get_s_verify_mode() {
                1 => VerifyMode::AlwaysSecure,
                2 => VerifyMode::Burst,
                _ => VerifyMode::AccurateRipThenSecure,
            };
            st.max_retries = ui.get_s_retries().max(1) as u32;
            st.required_matches = ui.get_s_matches().max(1) as u32;
            st.use_c2 = ui.get_s_c2();
            st.eject_after_rip = ui.get_s_eject();
            st.write_log = ui.get_s_log();
            st.write_cue = ui.get_s_cue();
            st.musicbrainz = ui.get_s_mb();
            st.gnudb_fallback = ui.get_s_gnudb();
            st.discogs_fallback = ui.get_s_discogs();
            st.discogs_token = ui.get_s_discogs_token().trim().to_string();
            st.contact_email = ui.get_s_contact().trim().to_string();
            st.art_max_px = ui.get_s_art_max().max(0) as u32;
            st.art_min_px = ui.get_s_art_min().max(0) as u32;
            st.save_cover_file = ui.get_s_cover_file();
            st.cover_file_name = ui.get_s_cover_name().trim().to_string();
            st.portable_filenames = ui.get_s_portable();
            st.existing = match ui.get_s_existing() {
                1 => ExistingFile::Overwrite,
                2 => ExistingFile::Rename,
                _ => ExistingFile::Skip,
            };
            st.verify_lossless = ui.get_s_verify_lossless();
            if !s.drive_id.is_empty() {
                let o = ui.get_s_offset();
                st.drive_offsets.insert(s.drive_id.clone(), o);
            }
        });
        save_cfg(&ui);
        update_offset_text(&ui, None);
        ffmpeg_status(&ui);
        toast(&ui, "Settings saved");
    });
    let w = ui.as_weak();
    ui.on_lookup_offset(move || {
        let (v, m) = with(|s| (s.drive_vendor.clone(), s.drive_model.clone()));
        if m.is_empty() {
            toast(&w.unwrap(), "No drive detected");
            return;
        }
        let w = w.clone();
        std::thread::spawn(move || {
            let r = accuraterip::drive_offset_from_db(&v, &m);
            let _ = w.upgrade_in_event_loop(move |ui| match r {
                Ok(Some(o)) => {
                    ui.set_s_offset(o as i32);
                    toast(&ui, format!("AccurateRip lists {v} {m} with offset {o:+}. Press Save settings to keep it."));
                }
                Ok(None) => toast(&ui, format!("{v} {m} is not in the AccurateRip list; insert a popular CD and use Detect.")),
                Err(e) => toast(&ui, crate::net::describe("AccurateRip", &e)),
            });
        });
    });
    let w = ui.as_weak();
    ui.on_detect_offset(move || {
        let ui = w.unwrap();
        let Some(info) = with(|s| s.disc.clone()) else {
            toast(&ui, "Insert a CD first (ideally a popular one, so it is in the AccurateRip database)");
            return;
        };
        let Some(ar) = info.ar_data().cloned() else {
            toast(&ui, "This disc is not in the AccurateRip database; try a more popular CD");
            return;
        };
        toast(&ui, "Detecting offset…");
        let w = ui.as_weak();
        std::thread::spawn(move || {
            let r = Drive::open(&info.device).and_then(|d| ripper::detect_offset(&d, &info.toc, &ar));
            let _ = w.upgrade_in_event_loop(move |ui| match r {
                Ok(Some((o, c))) => {
                    ui.set_s_offset(o);
                    toast(&ui, format!("Detected read offset {o:+} (confidence {c}). Press Save settings to keep it."));
                }
                Ok(None) => toast(&ui, "No match found with this disc; try another popular CD"),
                Err(e) => toast(&ui, format!("Detection failed: {e:#}")),
            });
        });
    });
    let w = ui.as_weak();
    ui.on_install_integration(move || {
        let ui = w.unwrap();
        let cfg = with(|s| s.cfg.clone());
        match integration::install(&cfg) {
            Ok(done) => {
                ui.set_integration_status(ss("Currently installed."));
                toast(&ui, format!("Right-click menus installed for: {}", done.join(", ")));
            }
            Err(e) => toast(&ui, format!("{e:#}")),
        }
    });
    let w = ui.as_weak();
    ui.on_remove_integration(move || {
        let ui = w.unwrap();
        let _ = integration::uninstall();
        ui.set_integration_status(ss("Not installed."));
        toast(&ui, "Right-click menus removed");
    });
}

// =================================================================== appearance

/// Light/dark preference from org.freedesktop.portal.Settings (KDE, GNOME and most
/// other desktops implement it). 1 = dark, 2 = light, 0 = no preference.
fn portal_prefers_dark(conn: &zbus::blocking::Connection) -> Option<bool> {
    use zbus::zvariant::Value;
    let call = |method: &str| {
        conn.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            method,
            &("org.freedesktop.appearance", "color-scheme"),
        )
        .ok()
    };
    let msg = call("ReadOne").or_else(|| call("Read"))?;
    let v: zbus::zvariant::OwnedValue = msg.body().deserialize().ok()?;
    // the older "Read" method wraps the value in one more variant
    let n = match &*v {
        Value::U32(n) => *n,
        Value::Value(inner) => match inner.as_ref() {
            Value::U32(n) => *n,
            _ => return None,
        },
        _ => return None,
    };
    match n {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

fn apply_theme(ui: &AppWindow) {
    let (mode, scheme, system) = with(|s| (s.cfg.settings.theme_mode.clone(), s.cfg.settings.theme_scheme, s.system_dark));
    let dark = match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => system.unwrap_or(true),
    };
    let theme = ui.global::<Theme>();
    theme.set_dark(dark);
    theme.set_scheme(scheme.min(SCHEMES.len() - 1) as i32);
    ui.set_s_theme_mode(match mode {
        ThemeMode::System => 0,
        ThemeMode::Light => 1,
        ThemeMode::Dark => 2,
    });
}

fn init_theme(ui: &AppWindow) {
    ui.set_scheme_names(strings(SCHEMES.iter().map(|s| s.to_string())));
    // read the system preference before the first frame so there's no flash of the wrong mode
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let conn = zbus::blocking::Connection::session().ok();
        let first = conn.as_ref().and_then(portal_prefers_dark);
        let _ = tx.send(first);
        // then follow changes made in system settings while Coda is open
        let Some(conn) = conn else { return };
        let mut last = first;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = portal_prefers_dark(&conn);
            if now != last && now.is_some() {
                last = now;
                let r = slint::invoke_from_event_loop(move || {
                    ST.with(|s| s.borrow_mut().system_dark = now);
                    if let Some(ui) = UI.with(|u| u.borrow().as_ref().and_then(|w| w.upgrade())) {
                        apply_theme(&ui);
                    }
                });
                if r.is_err() {
                    return; // event loop gone
                }
            }
        }
    });
    let first = rx.recv_timeout(Duration::from_millis(400)).ok().flatten();
    with(|s| s.system_dark = first);
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    apply_theme(ui);

    let w = ui.as_weak();
    ui.on_theme_mode_selected(move |i| {
        let ui = w.unwrap();
        with(|s| {
            s.cfg.settings.theme_mode = match i {
                1 => ThemeMode::Light,
                2 => ThemeMode::Dark,
                _ => ThemeMode::System,
            }
        });
        save_cfg(&ui);
        apply_theme(&ui);
    });
    let w = ui.as_weak();
    ui.on_theme_scheme_selected(move |i| {
        let ui = w.unwrap();
        with(|s| s.cfg.settings.theme_scheme = i.max(0) as usize);
        save_cfg(&ui);
        apply_theme(&ui);
    });
}

thread_local! {
    static UI: RefCell<Option<Weak<AppWindow>>> = const { RefCell::new(None) };
}

/// Draw the sidebar logo at exactly its physical pixel size from the 512 px master,
/// with Lanczos filtering, so it stays crisp at any display scale (no GPU downscaling).
fn render_logo(ui: &AppWindow) {
    const LOGICAL: f32 = 38.0;
    let px = (LOGICAL * ui.window().scale_factor()).round().max(16.0) as u32;
    if with(|s| s.logo_px == px) {
        return;
    }
    let Some((_, bytes)) = integration::ICONS.iter().find(|(s, _)| *s == 512) else { return };
    let Ok(img) = image::load_from_memory(bytes) else { return };
    let img = img.resize_exact(px, px, image::imageops::FilterType::Lanczos3).to_rgba8();
    let buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), px, px);
    ui.set_logo(slint::Image::from_rgba8(buf));
    with(|s| s.logo_px = px);
}

// =================================================================== tagger

use crate::tagger::{self, Group, Status};

fn thumb(bytes: &[u8], px: u32) -> Option<(slint::Image, String)> {
    let img = image::load_from_memory(bytes).ok()?;
    let info = format!("{}×{}", img.width(), img.height());
    let t = img.thumbnail(px, px).to_rgba8();
    let buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(t.as_raw(), t.width(), t.height());
    Some((slint::Image::from_rgba8(buf), info))
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Most common non-empty value among the ticked files.
fn common<F: Fn(&TrackMeta) -> String>(metas: &[TrackMeta], keep: &[bool], f: F) -> String {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (m, k) in metas.iter().zip(keep.iter().chain(std::iter::repeat(&true))) {
        if *k {
            let v = f(m);
            if !v.is_empty() {
                *counts.entry(v).or_default() += 1;
            }
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(v, _)| v).unwrap_or_default()
}

// album-level fields shown in the Current -> New panel
const TAG_FIELDS: [&str; 6] = ["Album", "Album artist", "Year", "Genre", "Label", "Catalogue no."];

fn field_get(m: &TrackMeta, i: usize) -> String {
    match i {
        0 => m.album.clone(),
        1 => m.album_artist.clone(),
        2 => m.year.clone(),
        3 => m.genre.clone(),
        4 => m.label.clone(),
        _ => m.catalog.clone(),
    }
}

fn field_set(m: &mut TrackMeta, i: usize, v: &str) {
    let v = v.to_string();
    match i {
        0 => m.album = v,
        1 => m.album_artist = v,
        2 => m.year = v,
        3 => m.genre = v,
        4 => m.label = v,
        _ => m.catalog = v,
    }
}

fn refresh_tag_list(ui: &AppWindow) {
    let rows: Vec<TagGroupRow> = with(|s| {
        s.tag_groups
            .iter()
            .map(|g| TagGroupRow {
                title: ss(g.title()),
                subtitle: ss(format!("{} · {} file{}", if g.artist().is_empty() { "Unknown artist".into() } else { g.artist() }, g.files.len(), if g.files.len() == 1 { "" } else { "s" })),
                status: ss(g.status.label()),
                state: g.status.state(),
            })
            .collect()
    });
    ui.set_tag_groups(ModelRc::new(VecModel::from(rows)));
    ui.set_tag_sel(with(|s| s.tag_sel) as i32);
}

/// Push the selected group into the editor panels.
fn show_group(ui: &AppWindow) {
    let Some(g) = with(|s| s.tag_groups.get(s.tag_sel).cloned()) else {
        ui.set_tg_fields(ModelRc::new(VecModel::<TagFieldRow>::default()));
        ui.set_tg_tracks(ModelRc::new(VecModel::<TagTrackRow>::default()));
        ui.set_tg_releases(strings(vec![]));
        return;
    };
    ui.set_tg_artist(ss(&g.query_artist));
    ui.set_tg_album(ss(&g.query_album));
    ui.set_tg_releases(strings(g.results.iter().map(|r| r.summary())));
    ui.set_tg_release_index(g.chosen.map(|c| c as i32).unwrap_or(-1));
    let currents: Vec<TrackMeta> = g.files.iter().map(|f| f.current.clone()).collect();
    let fields: Vec<TagFieldRow> = TAG_FIELDS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let cur = common(&currents, &[], |m| field_get(m, i));
            let new = common(&g.proposed, &g.apply, |m| field_get(m, i));
            TagFieldRow { label: ss(*label), changed: cur != new, current: ss(cur), new: ss(new) }
        })
        .collect();
    ui.set_tg_fields(ModelRc::new(VecModel::from(fields)));
    let scored = g.chosen.and_then(|c| g.results.get(c));
    let tracks: Vec<TagTrackRow> = g
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let c = &f.current;
            let p = &g.proposed[i];
            let num = track_num;
            let (note, state) = match scored {
                Some(sc) => match sc.mapping[i] {
                    Some(_) => {
                        let fs = sc.file_scores[i];
                        (format!("✓ {fs}%"), if fs >= 85 { 2 } else { 4 })
                    }
                    None => ("No match".to_string(), 4),
                },
                None => (if g.status == Status::Saved { "Saved".into() } else { "Manual".into() }, if g.status == Status::Saved { 2 } else { 0 }),
            };
            let cur_title = if c.title.is_empty() { file_name(&f.path) } else { c.title.clone() };
            TagTrackRow {
                apply: g.apply.get(i).copied().unwrap_or(false),
                cur_line: ss(if c.track > 0 { format!("{} · {}", num(c), cur_title) } else { cur_title }),
                cur_sub: ss(format!("{}{}", if c.artist.is_empty() { String::new() } else { format!("{} · ", c.artist) }, file_name(&f.path))),
                num: ss(num(p)),
                title: ss(&p.title),
                artist: ss(&p.artist),
                num_changed: num(p) != num(c),
                title_changed: p.title != c.title,
                artist_changed: p.artist != c.artist,
                note: ss(note),
                note_state: state,
            }
        })
        .collect();
    ui.set_tg_tracks(ModelRc::new(VecModel::from(tracks)));
    match g.current_cover().and_then(|c| thumb(&c, 192)) {
        Some((img, _)) => {
            ui.set_tg_cur_cover(img);
            ui.set_tg_has_cur_cover(true);
        }
        None => ui.set_tg_has_cur_cover(false),
    }
    let cur_info = g.current_cover().and_then(|c| thumb(&c, 8)).map(|(_, i)| i);
    match g.new_cover.as_ref().and_then(|c| thumb(c, 192)) {
        Some((img, info)) => {
            ui.set_tg_new_cover(img);
            ui.set_tg_has_new_cover(true);
            ui.set_tg_cover_info(ss(format!("Current {} · new {info}", cur_info.unwrap_or_else(|| "none".into()))));
        }
        None => {
            ui.set_tg_has_new_cover(false);
            ui.set_tg_cover_info(ss(match cur_info {
                Some(i) => format!("Current artwork {i}"),
                None => "No artwork yet".into(),
            }));
        }
    }
    ui.set_tg_replace_cover(g.replace_cover);
    let ticked = g.apply.iter().filter(|a| **a).count();
    ui.set_tg_status(ss(match &g.status {
        Status::Error(e) => e.clone(),
        st => format!("{} · {ticked} of {} files will be written", st.label(), g.files.len()),
    }));
    refresh_tag_list(ui);
}

fn with_group<R>(f: impl FnOnce(&mut Group) -> R) -> Option<R> {
    with(|s| {
        let i = s.tag_sel;
        s.tag_groups.get_mut(i).map(f)
    })
}

fn tag_add_paths(ui: &AppWindow, paths: Vec<PathBuf>) {
    if ui.get_tg_busy() {
        toast(ui, "Coda is busy with these albums: add more when it has finished");
        return;
    }
    let (ff, recursive) = (with(|s| s.cfg.settings.ffmpeg.clone()), ui.get_recursive());
    ui.set_tg_busy(true);
    ui.set_tg_status(ss("Reading tags…"));
    ui.set_page(4);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let groups = tagger::load(&paths, recursive, &ff);
        let _ = w.upgrade_in_event_loop(move |ui| {
            ui.set_tg_busy(false);
            let known: Vec<PathBuf> = with(|s| s.tag_groups.iter().flat_map(|g| g.files.iter().map(|f| f.path.clone())).collect());
            let fresh: Vec<Group> = groups
                .into_iter()
                .filter(|g| g.files.iter().any(|f| !known.contains(&f.path)))
                .collect();
            if fresh.is_empty() {
                toast(&ui, "No new audio files in that selection");
                show_group(&ui);
                return;
            }
            let n = fresh.len();
            with(|s| {
                s.tag_sel = s.tag_groups.len();
                s.tag_groups.extend(fresh);
            });
            show_group(&ui);
            toast(&ui, format!("Added {n} album{}: press Search, or Auto-match all", if n == 1 { "" } else { "s" }));
        });
    });
}

/// Search + pick the best match + fetch its artwork, for one group (worker thread).
fn match_group(mut g: Group, artist: String, album: String, source: i32, st: &config::Settings, auto_threshold: Option<u32>) -> Group {
    g.query_artist = artist.clone();
    g.query_album = album.clone();
    match tagger::search(&g.files, &artist, &album, source, st) {
        Ok(results) if !results.is_empty() => {
            g.results = results;
            let best = g.results[0].percent;
            if auto_threshold.map(|t| best >= t).unwrap_or(true) {
                tagger::propose(&mut g, 0);
                fetch_group_cover(&mut g, st);
            } else {
                // leave the candidates in the Match list for the user to choose from
                g.chosen = None;
                g.status = Status::Review(best);
            }
        }
        Ok(_) => {
            g.results.clear();
            g.status = Status::NoMatch;
        }
        Err(e) => g.status = Status::Error(format!("{e:#}")),
    }
    g
}

fn fetch_group_cover(g: &mut Group, st: &config::Settings) {
    let Some(sc) = g.chosen.and_then(|c| g.results.get(c)) else { return };
    let new = metadata::fetch_cover(&sc.release.album, st);
    let area = |b: &[u8]| image::ImageReader::new(std::io::Cursor::new(b)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok()).map(|(w, h)| w as u64 * h as u64).unwrap_or(0);
    let cur = g.current_cover().map(|c| area(&c)).unwrap_or(0);
    // replace by default only when it's an upgrade (no art yet, or a bigger image)
    g.replace_cover = new.as_ref().map(|n| area(n) > cur).unwrap_or(false);
    g.new_cover = new.map(Arc::new);
}

fn wire_tagger(ui: &AppWindow) {
    let pick = |ui: &AppWindow, folders: bool| {
        let w = ui.as_weak();
        std::thread::spawn(move || {
            let picked = if folders {
                rfd::FileDialog::new().set_title("Add album folders").pick_folders()
            } else {
                rfd::FileDialog::new().set_title("Add audio files").add_filter("Audio", formats::INPUT_EXTENSIONS).pick_files()
            };
            if let Some(p) = picked {
                let _ = w.upgrade_in_event_loop(move |ui| tag_add_paths(&ui, p));
            }
        });
    };
    let w = ui.as_weak();
    ui.on_tag_add_files(move || pick(&w.unwrap(), false));
    let w = ui.as_weak();
    ui.on_tag_add_folder(move || pick(&w.unwrap(), true));
    let w = ui.as_weak();
    ui.on_tag_clear(move || {
        with(|s| {
            s.tag_groups.clear();
            s.tag_sel = 0;
        });
        let ui = w.unwrap();
        refresh_tag_list(&ui);
        show_group(&ui);
    });
    let w = ui.as_weak();
    ui.on_tag_select(move |i| {
        let ui = w.unwrap();
        // keep what was typed in the search boxes for the group we're leaving
        let (a, b) = (ui.get_tg_artist().to_string(), ui.get_tg_album().to_string());
        with_group(|g| {
            g.query_artist = a;
            g.query_album = b;
        });
        with(|s| s.tag_sel = (i.max(0) as usize).min(s.tag_groups.len().saturating_sub(1)));
        show_group(&ui);
    });
    let w = ui.as_weak();
    ui.on_tag_search(move || {
        let ui = w.unwrap();
        let Some(g) = with(|s| s.tag_groups.get(s.tag_sel).cloned()) else { return };
        let (artist, album, source) = (ui.get_tg_artist().to_string(), ui.get_tg_album().to_string(), ui.get_tg_source());
        let st = with(|s| s.cfg.settings.clone());
        let sel = with(|s| s.tag_sel);
        ui.set_tg_busy(true);
        ui.set_tg_status(ss("Searching… (MusicBrainz allows one request a second, so this takes a few seconds)"));
        let w = ui.as_weak();
        std::thread::spawn(move || {
            let g = match_group(g, artist, album, source, &st, None);
            let _ = w.upgrade_in_event_loop(move |ui| {
                ui.set_tg_busy(false);
                if let Status::Error(e) = &g.status {
                    toast(&ui, e.clone());
                } else if g.results.is_empty() {
                    toast(&ui, "No matching releases: check the artist and album spelling, or try another source");
                }
                with(|s| {
                    if let Some(slot) = s.tag_groups.get_mut(sel) {
                        *slot = g;
                    }
                });
                show_group(&ui);
            });
        });
    });
    let w = ui.as_weak();
    ui.on_tag_release_selected(move |i| {
        let ui = w.unwrap();
        let sel = with(|s| s.tag_sel);
        with_group(|g| tagger::propose(g, i.max(0) as usize));
        show_group(&ui);
        let Some(mut g) = with(|s| s.tag_groups.get(sel).cloned()) else { return };
        let st = with(|s| s.cfg.settings.clone());
        ui.set_tg_cover_info(ss("Fetching artwork…"));
        let w = ui.as_weak();
        std::thread::spawn(move || {
            fetch_group_cover(&mut g, &st);
            let _ = w.upgrade_in_event_loop(move |ui| {
                with(|s| {
                    if let Some(slot) = s.tag_groups.get_mut(sel) {
                        // only if the user hasn't picked yet another release meanwhile
                        if slot.chosen == g.chosen {
                            slot.new_cover = g.new_cover;
                            slot.replace_cover = g.replace_cover;
                        }
                    }
                });
                show_group(&ui);
            });
        });
    });
    let w = ui.as_weak();
    ui.on_tag_field_edited(move |i, v| {
        with_group(|g| {
            for (k, p) in g.proposed.iter_mut().enumerate() {
                if g.apply.get(k).copied().unwrap_or(false) {
                    field_set(p, i as usize, v.as_str());
                }
            }
            if matches!(g.status, Status::New | Status::NoMatch | Status::Review(_) | Status::Saved) {
                g.status = Status::Edited;
            }
        });
        refresh_after_edit(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_tag_track_edited(move |i, field, v| {
        with_group(|g| {
            let Some(p) = g.proposed.get_mut(i as usize) else { return };
            match field {
                0 => {
                    // "5" or "2-05" (disc-track)
                    let (d, t) = match v.split_once('-') {
                        Some((d, t)) => (d.trim().parse().ok(), t.trim().parse().ok()),
                        None => (None, v.trim().parse().ok()),
                    };
                    if let Some(d) = d {
                        p.disc = d;
                    }
                    p.track = t.unwrap_or(0);
                }
                1 => p.title = v.to_string(),
                _ => p.artist = v.to_string(),
            }
            if matches!(g.status, Status::New | Status::NoMatch | Status::Review(_) | Status::Saved) {
                g.status = Status::Edited;
            }
        });
        refresh_after_edit(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_tag_track_toggled(move |i, on| {
        with_group(|g| {
            if let Some(a) = g.apply.get_mut(i as usize) {
                *a = on;
            }
        });
        show_group(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_tag_track_all(move |on| {
        with_group(|g| g.apply.iter_mut().for_each(|a| *a = on));
        show_group(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_tag_reset(move || {
        with_group(|g| {
            g.reset_to_current();
            g.status = Status::New;
        });
        show_group(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_tag_cover_toggled(move |on| {
        with_group(|g| g.replace_cover = on);
        show_group(&w.unwrap());
    });
    let w = ui.as_weak();
    ui.on_tag_choose_cover(move || {
        let w = w.clone();
        let max = with(|s| s.cfg.settings.art_max_px);
        std::thread::spawn(move || {
            let Some(f) = rfd::FileDialog::new().set_title("Choose album artwork").add_filter("Images", &["jpg", "jpeg", "png"]).pick_file() else { return };
            let r = std::fs::read(&f).map_err(anyhow::Error::from).and_then(|b| metadata::normalise_art(b, max));
            let _ = w.upgrade_in_event_loop(move |ui| match r {
                Ok(b) => {
                    with_group(|g| {
                        g.new_cover = Some(Arc::new(b));
                        g.replace_cover = true;
                    });
                    show_group(&ui);
                }
                Err(e) => toast(&ui, format!("Could not use that image: {e:#}")),
            });
        });
    });
    let w = ui.as_weak();
    ui.on_tag_save(move || {
        let ui = w.unwrap();
        let sel = with(|s| s.tag_sel);
        let Some(g) = with(|s| s.tag_groups.get(sel).cloned()) else { return };
        if !g.apply.iter().any(|a| *a) {
            return toast(&ui, "No files are ticked");
        }
        save_groups(&ui, vec![(sel, g)]);
    });
    let w = ui.as_weak();
    ui.on_tag_save_all(move || {
        let ui = w.unwrap();
        let todo: Vec<(usize, Group)> = with(|s| {
            s.tag_groups
                .iter()
                .enumerate()
                .filter(|(_, g)| (g.chosen.is_some() || g.status == Status::Edited) && g.status != Status::Saved && g.apply.iter().any(|a| *a))
                .map(|(i, g)| (i, g.clone()))
                .collect()
        });
        if todo.is_empty() {
            return toast(&ui, "Nothing to save yet: match or edit some albums first");
        }
        save_groups(&ui, todo);
    });
    let w = ui.as_weak();
    ui.on_tag_auto_all(move || {
        let ui = w.unwrap();
        let todo: Vec<(usize, Group)> = with(|s| {
            s.tag_groups
                .iter()
                .enumerate()
                .filter(|(_, g)| g.chosen.is_none() && g.status != Status::Saved)
                .map(|(i, g)| (i, g.clone()))
                .collect()
        });
        if todo.is_empty() {
            return toast(&ui, "Every album already has a match");
        }
        let st = with(|s| s.cfg.settings.clone());
        let source = ui.get_tg_source();
        let total = todo.len();
        ui.set_tg_busy(true);
        let w = ui.as_weak();
        std::thread::spawn(move || {
            let mut matched = 0;
            for (n, (idx, mut g)) in todo.into_iter().enumerate() {
                let w2 = w.clone();
                let _ = w2.upgrade_in_event_loop(move |ui| {
                    with(|s| {
                        if let Some(slot) = s.tag_groups.get_mut(idx) {
                            slot.status = Status::Searching;
                        }
                    });
                    refresh_tag_list(&ui);
                    ui.set_tg_status(ss(format!("Matching album {} of {total}…", n + 1)));
                });
                let (a, b) = (g.query_artist.clone(), g.query_album.clone());
                // only accept confident matches automatically; the rest wait for review
                g = match_group(g, a, b, source, &st, Some(80));
                if g.chosen.is_some() {
                    matched += 1;
                }
                let _ = w.upgrade_in_event_loop(move |ui| {
                    with(|s| {
                        if let Some(slot) = s.tag_groups.get_mut(idx) {
                            *slot = g;
                        }
                    });
                    show_group(&ui);
                });
            }
            let _ = w.upgrade_in_event_loop(move |ui| {
                ui.set_tg_busy(false);
                show_group(&ui);
                toast(&ui, format!("Matched {matched} of {total} albums. Review them, then Save all matched"));
            });
        });
    });
}

/// Track number as shown in the editor: "05", or "2-05" on multi-disc albums.
fn track_num(m: &TrackMeta) -> String {
    if m.track == 0 {
        String::new()
    } else if m.disc_total > 1 && m.disc > 0 {
        format!("{}-{:02}", m.disc, m.track)
    } else {
        format!("{:02}", m.track)
    }
}

/// After an edit, write the edited values (not just the highlight flags) back into the
/// row models. The track list is virtualised: rows scrolled out of view are rebuilt
/// from the model when they come back, so the model must hold what the user typed.
/// The text box being typed in keeps its own text, so this doesn't disturb typing.
fn refresh_after_edit(ui: &AppWindow) {
    let Some(g) = with(|s| s.tag_groups.get(s.tag_sel).cloned()) else { return };
    let fields = ui.get_tg_fields();
    let currents: Vec<TrackMeta> = g.files.iter().map(|f| f.current.clone()).collect();
    for i in 0..fields.row_count() {
        if let Some(mut r) = fields.row_data(i) {
            let cur = common(&currents, &[], |m| field_get(m, i));
            let new = common(&g.proposed, &g.apply, |m| field_get(m, i));
            let changed = cur != new;
            if r.changed != changed || r.new.as_str() != new {
                r.changed = changed;
                r.new = ss(new);
                fields.set_row_data(i, r);
            }
        }
    }
    let tracks = ui.get_tg_tracks();
    for i in 0..tracks.row_count() {
        if let (Some(mut r), Some(p), Some(f)) = (tracks.row_data(i), g.proposed.get(i), g.files.get(i)) {
            let c = &f.current;
            let (num, nc, tc, ac) = (track_num(p), track_num(p) != track_num(c), p.title != c.title, p.artist != c.artist);
            if r.num.as_str() != num || r.title.as_str() != p.title || r.artist.as_str() != p.artist || r.num_changed != nc || r.title_changed != tc || r.artist_changed != ac {
                r.num = ss(num);
                r.title = ss(&p.title);
                r.artist = ss(&p.artist);
                r.num_changed = nc;
                r.title_changed = tc;
                r.artist_changed = ac;
                tracks.set_row_data(i, r);
            }
        }
    }
    refresh_tag_list(ui);
}

fn save_groups(ui: &AppWindow, todo: Vec<(usize, Group)>) {
    let st = with(|s| s.cfg.settings.clone());
    let rename = ui.get_tg_rename().then(|| ui.get_tg_rename_template().to_string());
    ui.set_tg_busy(true);
    ui.set_tg_status(ss("Writing tags…"));
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let mut written = 0;
        let mut errors = Vec::new();
        let mut done = Vec::new();
        for (idx, mut g) in todo {
            let (n, e) = tagger::save(&mut g, rename.as_deref(), &st);
            written += n;
            errors.extend(e);
            // after saving, the files' tags are the new baseline
            g.results.clear();
            g.chosen = None;
            let status = g.status.clone();
            g.reset_to_current();
            g.status = status;
            done.push((idx, g));
        }
        let _ = w.upgrade_in_event_loop(move |ui| {
            with(|s| {
                for (idx, g) in done {
                    if let Some(slot) = s.tag_groups.get_mut(idx) {
                        *slot = g;
                    }
                }
            });
            ui.set_tg_busy(false);
            show_group(&ui);
            if errors.is_empty() {
                toast(&ui, format!("Saved tags to {written} file{}", if written == 1 { "" } else { "s" }));
            } else {
                toast(&ui, format!("Saved {written} files; {} failed: {}", errors.len(), errors[0]));
            }
        });
    });
}
