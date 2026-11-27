//! File manager integration (per-user, no root needed).
//!
//! * every desktop: "Open With → Convert with Coda" via a MIME-typed .desktop file
//! * KDE Dolphin: service menu with a "Coda" submenu (one entry per preset)
//! * GNOME Nautilus / MATE Caja: Scripts menu entries
//! * Cinnamon Nemo: .nemo_action files
//! * Xfce Thunar: custom actions in uca.xml
//! * PCManFM-Qt / LXQt and other FileManager-Actions consumers: Type=Action desktop files

use crate::config::{Config, Preset};
use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const MIME: &str = "audio/*;inode/directory;";
const THUNAR_MARK: &str = "<!-- coda -->";

fn data() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
}
fn conf() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "coda".into())
}

/// Quote an argument for a desktop-entry Exec line.
fn exec_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('"');
    o
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn write_file(p: &Path, content: &str, executable: bool) -> Result<()> {
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, content).with_context(|| format!("writing {}", p.display()))?;
    if executable {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn paths() -> Vec<PathBuf> {
    vec![
        data().join("applications/coda.desktop"),
        data().join("applications/coda-convert.desktop"),
        data().join("kio/servicemenus/coda.desktop"),
        data().join("nemo/actions"),
        data().join("nautilus/scripts/Coda"),
        conf().join("caja/scripts/Coda"),
        data().join("file-manager/actions/coda-convert.desktop"),
        data().join("file-manager/actions/coda-tag.desktop"),
    ]
}

pub fn is_installed() -> bool {
    data().join("kio/servicemenus/coda.desktop").exists() || data().join("applications/coda-convert.desktop").exists()
}

/// Application icon at the standard hicolor sizes, embedded in the binary.
pub const ICONS: &[(u32, &[u8])] = &[
    (16, include_bytes!("../assets/coda-16.png")),
    (20, include_bytes!("../assets/coda-20.png")),
    (22, include_bytes!("../assets/coda-22.png")),
    (24, include_bytes!("../assets/coda-24.png")),
    (32, include_bytes!("../assets/coda-32.png")),
    (48, include_bytes!("../assets/coda-48.png")),
    (64, include_bytes!("../assets/coda-64.png")),
    (128, include_bytes!("../assets/coda-128.png")),
    (256, include_bytes!("../assets/coda-256.png")),
    (512, include_bytes!("../assets/coda-512.png")),
];

fn icon_path(size: u32) -> PathBuf {
    data().join(format!("icons/hicolor/{size}x{size}/apps/coda.png"))
}

fn install_icons() -> Result<()> {
    for (size, bytes) in ICONS {
        let p = icon_path(*size);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, bytes)?;
    }
    let _ = std::process::Command::new("gtk-update-icon-cache").args(["-f", "-t"]).arg(data().join("icons/hicolor")).output();
    Ok(())
}

pub fn install(cfg: &Config) -> Result<Vec<String>> {
    uninstall()?;
    install_icons()?;
    let exe = exe();
    let q_exe = exec_quote(&exe);
    let mut done = Vec::new();

    // --- application + Open With
    write_file(
        &data().join("applications/coda.desktop"),
        &format!(
            "[Desktop Entry]\nType=Application\nName=Coda\nGenericName=CD Ripper & Audio Converter\n\
             Comment=Secure CD ripping and fast audio conversion\nExec={q_exe} %F\nIcon=coda\n\
             Terminal=false\nStartupWMClass=coda\nCategories=AudioVideo;Audio;\nMimeType=x-content/audio-cdda;\nKeywords=rip;cd;flac;mp3;convert;\n"
        ),
        false,
    )?;
    write_file(
        &data().join("applications/coda-convert.desktop"),
        &format!(
            "[Desktop Entry]\nType=Application\nName=Convert with Coda\nExec={q_exe} --convert %F\n\
             Icon=coda\nTerminal=false\nNoDisplay=true\nMimeType={MIME}\n"
        ),
        false,
    )?;
    let _ = std::process::Command::new("update-desktop-database").arg(data().join("applications")).output();
    done.push("Open With menu (all desktops)".into());

    let presets: Vec<&Preset> = cfg.presets.iter().collect();

    // --- KDE Dolphin (Plasma 5 & 6)
    let mut kde = format!(
        "[Desktop Entry]\nType=Service\nX-KDE-ServiceTypes=KonqPopupMenu/Plugin\nMimeType={MIME}\n\
         X-KDE-Submenu=Coda\nIcon=coda\nActions=dw_open;dw_tag;{}\n\n\
         [Desktop Action dw_open]\nName=Convert…\nIcon=coda\nExec={q_exe} --convert %F\n\n\
         [Desktop Action dw_tag]\nName=Tag…\nIcon=coda\nExec={q_exe} --tag %F\n",
        (0..presets.len()).map(|i| format!("dw_p{i};")).collect::<String>()
    );
    for (i, p) in presets.iter().enumerate() {
        kde.push_str(&format!(
            "\n[Desktop Action dw_p{i}]\nName=Convert to {}\nIcon=audio-x-generic\nExec={q_exe} convert --preset {} %F\n",
            p.name,
            exec_quote(&p.name)
        ));
    }
    write_file(&data().join("kio/servicemenus/coda.desktop"), &kde, true)?;
    done.push("Dolphin (KDE)".into());

    // --- Nautilus & Caja scripts
    for (dir, var, label) in [
        (data().join("nautilus/scripts/Coda"), "NAUTILUS_SCRIPT_SELECTED_FILE_PATHS", "Nautilus (GNOME)"),
        (conf().join("caja/scripts/Coda"), "CAJA_SCRIPT_SELECTED_FILE_PATHS", "Caja (MATE)"),
    ] {
        let script = |args: &str| {
            format!(
                "#!/bin/sh\n# generated by Coda\nIFS='\n'\nset -f\nexec {} {args} ${var}\n",
                sh_quote(&exe)
            )
        };
        write_file(&dir.join("Convert…"), &script("--convert"), true)?;
        write_file(&dir.join("Tag…"), &script("--tag"), true)?;
        for p in &presets {
            let name = format!("Convert to {}", p.name.replace('/', "-"));
            write_file(&dir.join(name), &script(&format!("convert --preset {}", sh_quote(&p.name))), true)?;
        }
        done.push(label.into());
    }

    // --- Nemo actions
    let nemo = data().join("nemo/actions");
    let nemo_action = |file: &str, name: &str, args: &str| {
        write_file(
            &nemo.join(file),
            &format!(
                "[Nemo Action]\nName={name}\nComment=Coda\nExec={q_exe} {args} %F\nIcon-Name=coda\n\
                 Selection=notnone\nMimetypes=audio/*;inode/directory;\nExtensions=any;\n"
            ),
            false,
        )
    };
    nemo_action("coda-ripper-convert.nemo_action", "Coda: Convert…", "--convert")?;
    nemo_action("coda-ripper-tag.nemo_action", "Coda: Tag…", "--tag")?;
    for (i, p) in presets.iter().enumerate() {
        nemo_action(&format!("coda-ripper-preset{i}.nemo_action"), &format!("Coda: {}", p.name), &format!("convert --preset {}", exec_quote(&p.name)))?;
    }
    done.push("Nemo (Cinnamon)".into());

    // --- FileManager-Actions spec (PCManFM-Qt, LXQt, older Caja/Nautilus-actions)
    write_file(
        &data().join("file-manager/actions/coda-convert.desktop"),
        &format!(
            "[Desktop Entry]\nType=Action\nName=Convert with Coda\nIcon=coda\nProfiles=main;\n\n\
             [X-Action-Profile main]\nMimeTypes={MIME}\nExec={q_exe} --convert %F\n"
        ),
        false,
    )?;
    write_file(
        &data().join("file-manager/actions/coda-tag.desktop"),
        &format!(
            "[Desktop Entry]\nType=Action\nName=Tag with Coda\nIcon=coda\nProfiles=main;\n\n\
             [X-Action-Profile main]\nMimeTypes={MIME}\nExec={q_exe} --tag %F\n"
        ),
        false,
    )?;
    done.push("PCManFM-Qt / LXQt".into());

    // --- Thunar uca.xml (merge, keep the user's own actions)
    let uca = conf().join("Thunar/uca.xml");
    let mut actions = format!(
        "{THUNAR_MARK}<action><icon>coda</icon><name>Convert with Coda…</name><unique-id>coda-0</unique-id>\
         <command>{} --convert %F</command><description>Coda</description><patterns>*</patterns>\
         <directories/><audio-files/></action>\n\
         {THUNAR_MARK}<action><icon>coda</icon><name>Tag with Coda…</name><unique-id>coda-tag</unique-id>\
         <command>{} --tag %F</command><description>Coda</description><patterns>*</patterns>\
         <directories/><audio-files/></action>\n",
        xml_escape(&sh_quote(&exe)),
        xml_escape(&sh_quote(&exe))
    );
    for (i, p) in presets.iter().enumerate() {
        actions.push_str(&format!(
            "{THUNAR_MARK}<action><icon>audio-x-generic</icon><name>Coda: {}</name><unique-id>coda-p{i}</unique-id>\
             <command>{}</command><description>Coda</description><patterns>*</patterns><directories/><audio-files/></action>\n",
            xml_escape(&p.name),
            xml_escape(&format!("{} convert --preset {} %F", sh_quote(&exe), sh_quote(&p.name)))
        ));
    }
    let existing = std::fs::read_to_string(&uca).unwrap_or_else(|_| "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n".into());
    if let Some(pos) = existing.rfind("</actions>") {
        let merged = format!("{}{}{}", &existing[..pos], actions, &existing[pos..]);
        write_file(&uca, &merged, false)?;
        done.push("Thunar (Xfce)".into());
    }
    Ok(done)
}

pub fn uninstall() -> Result<()> {
    for (size, _) in ICONS {
        let _ = std::fs::remove_file(icon_path(*size));
    }
    for p in paths() {
        if p.is_dir() {
            if p.ends_with("nemo/actions") {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().starts_with("coda-ripper") {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
            } else {
                let _ = std::fs::remove_dir_all(&p);
            }
        } else {
            let _ = std::fs::remove_file(&p);
        }
    }
    let uca = conf().join("Thunar/uca.xml");
    if let Ok(s) = std::fs::read_to_string(&uca) {
        let kept: String = s.lines().filter(|l| !l.starts_with(THUNAR_MARK)).map(|l| format!("{l}\n")).collect();
        let _ = std::fs::write(&uca, kept);
    }
    Ok(())
}
