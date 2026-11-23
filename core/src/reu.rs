//! REU-Abbilder, die ein Spiel beim Start braucht (z. B. Myst 64REU: PRG plus 16-MB-`.reu`).
//!
//! Liegt neben der Startdatei eine `.reu` mit gleichem Namen, lädt Breadbin sie mit:
//! - VICE: `-reu -reusize <KB> -reuimage <datei> +reuimagerw` (das Abbild bleibt unverändert),
//! - Denise: `-reu <KB> <datei.reu>` vor der Startdatei (Denise erkennt das Abbild an der Endung),
//! - Ultimate: Datei auf den Stick, REU und Command Interface per REST einschalten (nur im RAM,
//!   nicht ins Flash), REU-Größe passend setzen, dann lädt ein kleiner Lader (`core/c64/reu.asm`)
//!   die Datei über die UCI ins REU. Danach startet das Spiel wie gewohnt; der REU-Inhalt übersteht
//!   den Reset. Die vorherigen Einstellungen merkt sich Breadbin und stellt sie beim nächsten Start
//!   ohne REU-Abbild auf demselben Gerät wieder her.

use crate::db;
use crate::ftp;
use crate::remote::Remote;
use crate::tr;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Der REU-Lader als fertiges PRG (gebaut mit 64tass aus `core/c64/reu.asm`).
pub const REU_PRG: &[u8] = include_bytes!("../c64/reu.prg");
pub const REU_LADER_NAME: &str = "breadbin-reu.prg";
/// Größte REU (16 MB).
pub const MAX_REU: u64 = 16 * 1024 * 1024;
const KENNUNG: &[u8] = b"BBREUPAR";

/// Wo das REU-Abbild zu einem Spiel liegt.
#[derive(Debug, Clone, PartialEq)]
pub enum Companion {
    /// Datei auf dem Rechner (auch Netzlaufwerk).
    Local { path: PathBuf, size: u64 },
    /// Datei auf einem Gerät: Ort der Quelle (`ftp://host/…`) und voller Pfad dort.
    Remote {
        location: String,
        path: String,
        size: u64,
    },
}

impl Companion {
    pub fn size(&self) -> u64 {
        match self {
            Companion::Local { size, .. } | Companion::Remote { size, .. } => *size,
        }
    }

    /// Inhalt lesen.
    pub fn read(&self, password_for: &dyn Fn(&str) -> String) -> Result<Vec<u8>> {
        match self {
            Companion::Local { path, .. } => Ok(std::fs::read(path)?),
            Companion::Remote { location, path, .. } => {
                let (mut f, _, _) = Remote::for_location(location, password_for)?;
                let data = f.retrieve(path, MAX_REU + 1)?;
                f.quit();
                Ok(data)
            }
        }
    }
}

/// Name der passenden REU-Datei in `names`: gleicher Stamm wie `file`, Endung `.reu` (Groß-/
/// Kleinschreibung egal).
pub fn matching_name<'a>(file: &str, names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let stem = Path::new(file)
        .file_stem()?
        .to_string_lossy()
        .to_lowercase();
    names.into_iter().find(|n| {
        let p = Path::new(n);
        p.extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("reu"))
            && p.file_stem()
                .is_some_and(|s| s.to_string_lossy().to_lowercase() == stem)
    })
}

/// Sucht das REU-Abbild neben den Fundorten eines Eintrags (nicht in ZIP-Archiven).
/// Lokale Fundorte zuerst; Geräte nur, wenn `with_devices` (das braucht eine Verbindung).
pub fn find(
    d: &db::Details,
    with_devices: bool,
    password_for: &dyn Fn(&str) -> String,
) -> Option<Companion> {
    for loc in d.locations.iter().filter(|l| l.member.is_empty()) {
        if loc.source_kind == "ultimate" {
            continue;
        }
        let full = Path::new(&loc.source_location).join(&loc.path);
        let (Some(dir), Some(name)) = (full.parent(), full.file_name()) else {
            continue;
        };
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if let Some(n) = matching_name(&name.to_string_lossy(), names.iter().map(String::as_str)) {
            let path = dir.join(n);
            if let Ok(m) = std::fs::metadata(&path)
                && m.is_file()
                && m.len() > 0
                && m.len() <= MAX_REU
            {
                return Some(Companion::Local {
                    path,
                    size: m.len(),
                });
            }
        }
    }
    if !with_devices {
        return None;
    }
    for loc in d
        .locations
        .iter()
        .filter(|l| l.member.is_empty() && l.source_kind == "ultimate")
    {
        let Some((_, root)) = ftp::split_location(&loc.source_location) else {
            continue;
        };
        let full = format!("{}/{}", root.trim_end_matches('/'), loc.path);
        let (dir, name) = full.rsplit_once('/').unwrap_or(("", full.as_str()));
        let Ok((mut f, _, _)) = Remote::for_location(&loc.source_location, password_for) else {
            continue;
        };
        let entries = f
            .list(if dir.is_empty() { "/" } else { dir })
            .unwrap_or_default();
        f.quit();
        let found = matching_name(
            name,
            entries
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.name.as_str()),
        )
        .and_then(|n| entries.iter().find(|e| e.name == n));
        if let Some(e) = found
            && e.size > 0
            && e.size <= MAX_REU
        {
            return Some(Companion::Remote {
                location: loc.source_location.clone(),
                path: format!("{dir}/{}", e.name),
                size: e.size,
            });
        }
    }
    None
}

/// Kleinste REU-Größe in KB, in die `bytes` passt (128 KB bis 16 MB).
pub fn size_kb(bytes: u64) -> u32 {
    let mut kb = 128u32;
    while (kb as u64) * 1024 < bytes && kb < 16384 {
        kb *= 2;
    }
    kb
}

/// Wert der Ultimate-Einstellung „REU Size“ zu einer Größe in KB („512 KB“, „16 MB“).
pub fn ultimate_size_value(kb: u32) -> String {
    if kb >= 1024 {
        format!("{} MB", kb / 1024)
    } else {
        format!("{kb} KB")
    }
}

/// REU-Lader mit Länge und vollem Pfad auf dem Gerät (ASCII).
pub fn loader(path: &str, len: u64) -> Result<Vec<u8>> {
    if !path.is_ascii() || path.len() > 200 {
        bail!(tr!(
            "Pfad der REU-Datei ist zu lang oder enthält Sonderzeichen: {path}",
            "Path of the REU file is too long or contains special characters: {path}"
        ));
    }
    let mut prg = REU_PRG.to_vec();
    let pos = prg
        .windows(KENNUNG.len())
        .position(|w| w == KENNUNG)
        .expect("Kennung im REU-Lader");
    // Aufbau nach der Kennung: LOAD_REU-Befehl (6 Byte), Länge (4), Pfadlänge (1), OPEN-Befehl (3), Pfad
    let p = pos + KENNUNG.len() + 6;
    prg[p..p + 4].copy_from_slice(&(len.min(MAX_REU) as u32).to_le_bytes());
    prg[p + 4] = path.len() as u8;
    let start = p + 5 + 3;
    prg[start..start + path.len()].copy_from_slice(path.as_bytes());
    Ok(prg)
}

/// Optionen für den Emulator: VICE und Denise; andere Emulatoren bekommen nichts.
pub fn emulator_args(emulator: &str, image: &Path, size: u64) -> Option<Vec<String>> {
    let kb = size_kb(size).to_string();
    let img = image.to_string_lossy().into_owned();
    if crate::emulator::is_vice(emulator) {
        Some(vec![
            "-reu".into(),
            "-reusize".into(),
            kb,
            "-reuimage".into(),
            img,
            "+reuimagerw".into(),
        ])
    } else if crate::emulator::is_denise(emulator) {
        Some(vec!["-reu".into(), kb, img])
    } else {
        None
    }
}

/// Schlüssel in den Einstellungen für die gemerkten Ultimate-Einstellungen eines Geräts.
pub fn restore_key(host: &str) -> String {
    format!("reu_restore:{}", host.trim().to_ascii_lowercase())
}

/// Einstellungen, die der REU-Start auf dem Ultimate ändert (Kategorie „C64 and Cartridge Settings“).
pub const ULTIMATE_ITEMS: [&str; 3] = ["RAM Expansion Unit", "REU Size", "Command Interface"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passender_name() {
        let names = ["myst64reu-0.93.prg", "MYST64REU-0.93.REU", "andere.reu"];
        assert_eq!(
            matching_name("myst64reu-0.93.prg", names),
            Some("MYST64REU-0.93.REU")
        );
        assert_eq!(matching_name("spiel.d64", names), None);
    }

    #[test]
    fn groessen() {
        assert_eq!(size_kb(1), 128);
        assert_eq!(size_kb(128 * 1024), 128);
        assert_eq!(size_kb(128 * 1024 + 1), 256);
        assert_eq!(size_kb(16 * 1024 * 1024), 16384);
        assert_eq!(size_kb(u64::MAX), 16384);
        assert_eq!(ultimate_size_value(512), "512 KB");
        assert_eq!(ultimate_size_value(16384), "16 MB");
    }

    #[test]
    fn lader_parameter() {
        let prg = loader("/USB0/Breadbin/myst.reu", 16 * 1024 * 1024).unwrap();
        assert_eq!(&prg[..2], &[0x01, 0x08]);
        assert_eq!(prg.len(), REU_PRG.len());
        let pos = prg.windows(8).position(|w| w == KENNUNG).unwrap() + 8;
        assert_eq!(&prg[pos..pos + 6], &[0x01, 0x21, 0, 0, 0, 0]);
        assert_eq!(&prg[pos + 6..pos + 10], &[0, 0, 0, 1]);
        assert_eq!(prg[pos + 10] as usize, "/USB0/Breadbin/myst.reu".len());
        assert_eq!(&prg[pos + 11..pos + 14], &[0x01, 0x02, 0x01]);
        assert_eq!(&prg[pos + 14..pos + 37], b"/USB0/Breadbin/myst.reu");
        assert!(loader("/USB0/Grüße.reu", 10).is_err());
    }

    #[test]
    fn emulator_optionen() {
        let img = Path::new("/tmp/a.reu");
        assert_eq!(
            emulator_args("/usr/bin/x64sc", img, 16 * 1024 * 1024).unwrap(),
            [
                "-reu",
                "-reusize",
                "16384",
                "-reuimage",
                "/tmp/a.reu",
                "+reuimagerw"
            ]
        );
        assert_eq!(
            emulator_args("/Applications/denise.app", img, 200_000).unwrap(),
            ["-reu", "256", "/tmp/a.reu"]
        );
        assert!(emulator_args("/opt/x/emu", img, 1).is_none());
    }
}
