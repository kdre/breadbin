//! TeensyROM (Modul mit Teensy 4.1) über sein Fernsteuer-Protokoll auf TCP-Port 2112.
//!
//! **Ungetestet mit echter Hardware** – gebaut nach `docs/ControlComms.md` und dem Firmware-Quelltext
//! (`Source/Teensy/SerUSBIO.ino`, `FileTransfer.ino`, `RemoteControl.ino`, Stand 2026-11-20) und
//! geprüft gegen den Schein-TeensyROM in den Tests unten.
//!
//! - Am Gerät „Enable TCP Listener“ einschalten. USB-seriell (gleiches Protokoll) baut Breadbin nicht ein.
//! - Befehle: 2-Byte-Token und Zahlen **big-endian**, Antworten (Ack/Fail, Längen) **little-endian**.
//!   Auf `FailToken` folgt eine Textzeile mit `\n`, z. B. `Busy!`.
//! - Pfade nullterminiert, ohne Auffüllen; höchstens 357 Zeichen, der Ordner beim Starten unter 256.
//!   Breadbin schreibt sie wie bei den anderen Geräten mit dem Laufwerk vorne:
//!   `/SD/Breadbin/x.prg` → Laufwerk 1 (SD) und `/Breadbin/x.prg`; `/USB/…` → Laufwerk 0.
//! - Dateibefehle antworten `Busy!`, solange ein Programm läuft (auch in der „Minimal“-Firmware nach
//!   großen CRTs). Reset (`0x64EE`) holt das TeensyROM-Menü zurück; danach Version abfragen, bis
//!   `Boot: complete` kommt.
//! - PostFile legt Ordner selbst an, überschreibt aber keine vorhandene Datei („File already
//!   exists.“) → vorher löschen. CopyFile erwartet laut Firmware vor den Pfaden ein Laufwerksbyte
//!   (in der Doku fehlt es); Breadbin braucht CopyFile nicht.
//! - Verzeichnis (NDJSON): die Firmware zählt ab 1 und sendet ab „Zähler ≥ skip“ – skip 0 und 1
//!   liefern beide ab dem ersten Eintrag. Breadbin holt alles in einem Rutsch (skip 0, take 65535).
//! - Startbar: PRG/P00, CRT (über 850 KB nur von SD), SID. Ein D64/D71/D81 öffnet am C64 nur das
//!   Disk-Verzeichnis; Breadbin schickt stattdessen die einzige Programmdatei der Diskette als PRG.
//! - Eine Verbindung je Befehl (laut Doku erlaubt); so bleibt kein Rest einer Antwort liegen und ein
//!   Neustart des Menüs, der den Listener neu aufsetzt, stört nicht.

use crate::ftp::RemoteEntry;
use crate::tr;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Standard-Port des TCP-Listeners.
pub const PORT: u16 = 2112;

const RESET_TOKEN: u16 = 0x64EE;
const LAUNCH_TOKEN: u16 = 0x6444;
const VERSION_TOKEN: u16 = 0x6476;
const POST_FILE_TOKEN: u16 = 0x64BB;
const GET_FILE_TOKEN: u16 = 0x64B0;
const DELETE_FILE_TOKEN: u16 = 0x64CF;
const DIR_NDJSON_TOKEN: u16 = 0x64DE;

const ACK: u16 = 0x64CC;
const FAIL: u16 = 0x9B7F;
const RETRY: u16 = 0x9B7E;
const BAD_SID: u16 = 0x9B80;
const GOOD_SID: u16 = 0x9B81;
const START_DIR: u16 = 0x5A5A;
const END_DIR: u16 = 0xA5A5;

/// Längster Pfad einschließlich Laufwerksordner (`MaxNamePathLength` 358 mit Nullbyte).
const MAX_PATH: usize = 357;
/// Längster Ordner beim Starten (`MaxPathLength`).
const MAX_LAUNCH_DIR: usize = 255;
/// Größere CRTs startet der TeensyROM nur von SD (Bank-Tausch über die Karte).
pub const MAX_CRT_FROM_USB: usize = 850 * 1024;

/// Laufwerke in Breadbins Pfadschreibweise und ihre Nummer im Protokoll (`RegMenuTypes`).
const DRIVES: [(&str, u8); 2] = [("SD", 1), ("USB", 0)];

/// Ablehnung durch den TeensyROM (`FailToken` mit Textzeile).
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Ein Programm läuft (oder die Minimal-Firmware ist aktiv): Dateibefehle gehen erst nach Reset.
    Busy,
    /// PostFile: Datei gibt es schon.
    Exists,
    /// Ordner oder Datei fehlt.
    NotFound(String),
    Other(String),
}

impl Refusal {
    fn from_text(text: &str) -> Refusal {
        let t = text.trim();
        if t.starts_with("Busy!") {
            Refusal::Busy
        } else if t.starts_with("File already exists") {
            Refusal::Exists
        } else if t.contains("not found") || t.contains("not a directory") {
            Refusal::NotFound(t.to_string())
        } else {
            Refusal::Other(t.to_string())
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Refusal::Busy => tr!(
                "TeensyROM ist beschäftigt (ein Programm läuft) – Dateibefehle gehen erst im TeensyROM-Menü.",
                "TeensyROM is busy (a program is running) – file commands only work in the TeensyROM menu."
            ),
            Refusal::Exists => tr!("Datei gibt es schon", "File already exists"),
            Refusal::NotFound(t) | Refusal::Other(t) => {
                tr!("TeensyROM meldet: {t}", "TeensyROM reports: {t}")
            }
        };
        f.write_str(&s)
    }
}

impl std::error::Error for Refusal {}

/// Ist der Fehler eine bestimmte Ablehnung?
pub fn is_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refusal>() == Some(&Refusal::Busy)
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Status {
    /// „TeensyROM“ oder „TeensyROM+“.
    pub product: String,
    /// Fassung bis zum ersten Leerzeichen oder `(`, z. B. `v0.7.2`.
    pub version: String,
    /// Minimal-Firmware (nach dem Start großer CRTs): nur Reset, Start und Version gehen.
    pub minimal: bool,
    /// Vierte Zeile, z. B. `C64  PAL Vid  50 Hz` (nur volle Firmware).
    pub machine: String,
    /// `Some(true)` bei `Boot: complete`, `Some(false)` bei `Boot: in progress`, `None` ohne Zeile.
    pub boot_complete: Option<bool>,
}

impl Status {
    /// Liest die Antwort auf VersionInfo. `None`, wenn sie nicht nach TeensyROM aussieht.
    pub fn parse(text: &str) -> Option<Status> {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let fw = lines
            .iter()
            .find_map(|l| l.strip_prefix("FW:"))
            .map(str::trim)?;
        let (product, rest) = fw.split_once(' ').unwrap_or((fw, ""));
        if !product.starts_with("TeensyROM") {
            return None;
        }
        let version = rest
            .trim()
            .split([' ', '('])
            .next()
            .unwrap_or("")
            .to_string();
        let boot_complete = lines
            .iter()
            .find_map(|l| l.strip_prefix("Boot:"))
            .map(|b| b.trim().starts_with("complete"));
        let machine = lines
            .iter()
            .find(|l| (l.starts_with("C64") || l.starts_with("C128")) && l.contains("Hz"))
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        Some(Status {
            product: product.to_string(),
            version,
            minimal: fw.contains("(minimal)"),
            machine,
            boot_complete,
        })
    }
}

/// Hinweise zu einem TeensyROM: ungetestet, und was damit nicht geht.
pub fn hints(st: &Status) -> Vec<String> {
    let mut out = vec![tr!(
        "TeensyROM-Unterstützung ist ungetestet (gebaut nach der Protokoll-Doku, ohne echtes Gerät) – Rückmeldungen willkommen.",
        "TeensyROM support is untested (built from the protocol documentation, without a real device) – feedback welcome."
    )];
    if st.minimal {
        out.push(tr!(
            "TeensyROM läuft gerade mit der Minimal-Firmware (großes Modul): Breadbin setzt ihn vor dem nächsten Hochladen zurück.",
            "TeensyROM is running its minimal firmware (large cartridge): Breadbin resets it before the next upload."
        ));
    }
    out.push(tr!(
        "TeensyROM startet PRG, P00, T64 (erstes Programm), CRT und Disketten mit nur einer Programmdatei. Mehrteilige Disk-Spiele, G64 und TAP gehen nicht; kein Diskwechsel, keine Bildschirmfotos. CRTs über 850 KB nur aus einem Startordner auf der SD-Karte.",
        "TeensyROM launches PRG, P00, T64 (first program), CRT and disks with a single program file. Multi-file disk games, G64 and TAP do not work; no disk swaps, no screenshots. CRTs over 850 KB only from a launch folder on the SD card."
    ));
    out
}

/// `/SD/ordner/datei` → (1, `/ordner/datei`); `/USB` → (0, `/`).
fn split_drive(path: &str) -> Result<(u8, String)> {
    let p = path.trim_start_matches('/');
    let (drive, rest) = p.split_once('/').unwrap_or((p, ""));
    let Some(&(_, num)) = DRIVES.iter().find(|(n, _)| n.eq_ignore_ascii_case(drive)) else {
        bail!(tr!(
            "Pfad {path} nennt kein TeensyROM-Laufwerk (/SD/… oder /USB/…)",
            "Path {path} names no TeensyROM drive (/SD/… or /USB/…)"
        ));
    };
    let rest = format!("/{}", rest.trim_end_matches('/'));
    if rest.len() > MAX_PATH {
        bail!(tr!(
            "Pfad zu lang für TeensyROM: {path}",
            "Path too long for TeensyROM: {path}"
        ));
    }
    if rest.contains('\0') {
        bail!(tr!("Ungültiger Pfad: {path}", "Invalid path: {path}"));
    }
    Ok((num, rest))
}

/// Liegt der Pfad auf dem USB-Stick des TeensyROM?
pub fn is_usb_path(path: &str) -> bool {
    matches!(split_drive(path), Ok((0, _)))
}

/// Antwort auf `GetFile`/`PostFile` mit Datei.
fn checksum(data: &[u8]) -> u16 {
    data.iter().fold(0u32, |s, &b| s.wrapping_add(b as u32)) as u16
}

/// Ein Eintrag der NDJSON-Liste: `{"type":"file","name":"x.prg","size":123}`.
/// Die Firmware maskiert Namen nicht; bei Zeichen, die JSON brechen, liest Breadbin von Hand.
fn parse_dir_line(line: &str) -> Option<RemoteEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // Ein Backslash im Namen ist kein JSON-Escape (die Firmware maskiert nichts) → dann von Hand.
    if let Some(v) = (!line.contains('\\'))
        .then(|| serde_json::from_str::<serde_json::Value>(line).ok())
        .flatten()
    {
        return Some(RemoteEntry {
            name: v["name"].as_str()?.to_string(),
            is_dir: v["type"].as_str() == Some("dir"),
            size: v["size"].as_u64().unwrap_or(0),
            modify: 0,
        });
    }
    let is_dir = line.starts_with("{\"type\":\"dir\"");
    let start = line.find("\"name\":\"")? + 8;
    let end = if is_dir {
        line.rfind("\"}")?
    } else {
        line.rfind("\",\"size\":")?
    };
    let size = if is_dir {
        0
    } else {
        line[end + 9..]
            .trim_end_matches('}')
            .trim()
            .parse()
            .unwrap_or(0)
    };
    (end > start).then(|| RemoteEntry {
        name: line[start..end].to_string(),
        is_dir,
        size,
        modify: 0,
    })
}

/// Eine TCP-Verbindung für einen Befehl.
struct Conn {
    stream: TcpStream,
    host: String,
}

impl Conn {
    fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data).map_err(|e| self.io_error(e))
    }

    fn token(&mut self, t: u16) -> Result<()> {
        self.send(&t.to_be_bytes())
    }

    /// Laufwerksbyte und nullterminierter Pfad.
    fn path(&mut self, drive: u8, path: &str) -> Result<()> {
        let mut b = Vec::with_capacity(path.len() + 2);
        b.push(drive);
        b.extend_from_slice(path.as_bytes());
        b.push(0);
        self.send(&b)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.stream.read_exact(buf).map_err(|e| self.io_error(e))
    }

    fn u16(&mut self) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read_exact(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// Textzeile bis `\n` (höchstens 512 Zeichen, sonst bis zur Zeitgrenze).
    fn line(&mut self) -> String {
        let _ = self.stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        while out.len() < 512 {
            match self.stream.read(&mut b) {
                Ok(1) if b[0] == b'\n' => break,
                Ok(1) => out.push(b[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&out).trim().to_string()
    }

    /// Erwartet `AckToken`; `FailToken` + Text wird zur [`Refusal`]. SID-Meldungen werden übersprungen.
    fn ack(&mut self) -> Result<()> {
        loop {
            match self.u16()? {
                ACK => return Ok(()),
                FAIL => return Err(anyhow!(Refusal::from_text(&self.line()))),
                BAD_SID | GOOD_SID => continue,
                RETRY => bail!(tr!(
                    "TeensyROM lehnt den Befehl ab (Retry) – bitte noch einmal versuchen.",
                    "TeensyROM declines the command (retry) – please try again."
                )),
                other => bail!(tr!(
                    "Unerwartete Antwort vom TeensyROM: 0x{:04X}",
                    "Unexpected reply from TeensyROM: 0x{:04X}",
                    other
                )),
            }
        }
    }

    fn timeout(&mut self, d: Duration) {
        let _ = self.stream.set_read_timeout(Some(d));
    }

    fn io_error(&self, e: std::io::Error) -> anyhow::Error {
        if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
            anyhow!(tr!(
                "TeensyROM {} antwortet nicht rechtzeitig ({e})",
                "TeensyROM {} does not answer in time ({e})",
                self.host
            ))
        } else if e.kind() == ErrorKind::UnexpectedEof {
            anyhow!(tr!(
                "TeensyROM {} hat die Verbindung beendet",
                "TeensyROM {} closed the connection",
                self.host
            ))
        } else {
            anyhow!(tr!(
                "Verbindung zu TeensyROM {} gestört: {e}",
                "Connection to TeensyROM {} disturbed: {e}",
                self.host
            ))
        }
    }
}

pub struct TeensyRom {
    host: String,
    connect_timeout: Duration,
    timeout: Duration,
}

/// Adresse mit Port: `host` oder `host:port` (Standard 2112).
fn socket_addr(host: &str) -> Result<SocketAddr> {
    let h = host.trim();
    let with_port = if h
        .rsplit_once(':')
        .is_some_and(|(a, p)| !a.is_empty() && !a.contains(':') && p.parse::<u16>().is_ok())
    {
        h.to_string()
    } else {
        format!("{h}:{PORT}")
    };
    with_port
        .to_socket_addrs()?
        .next()
        .with_context(|| tr!("{h} nicht auflösbar", "Cannot resolve {h}"))
}

impl TeensyRom {
    pub fn new(host: &str) -> TeensyRom {
        TeensyRom {
            host: host.trim().to_string(),
            connect_timeout: Duration::from_secs(4),
            // Die TeensyROM Suite wartet bis zu 20 s auf eine Bestätigung.
            timeout: Duration::from_secs(20),
        }
    }

    fn quick(host: &str) -> TeensyRom {
        TeensyRom {
            host: host.trim().to_string(),
            connect_timeout: Duration::from_millis(1500),
            timeout: Duration::from_millis(1500),
        }
    }

    fn open(&self) -> Result<Conn> {
        let addr = socket_addr(&self.host)?;
        let stream = TcpStream::connect_timeout(&addr, self.connect_timeout).map_err(|e| {
            if e.kind() == ErrorKind::ConnectionRefused {
                anyhow!(tr!(
                    "{} antwortet, aber nicht auf Port {} – am TeensyROM „Enable TCP Listener“ einschalten.",
                    "{} answers, but not on port {} – turn on “Enable TCP Listener” on the TeensyROM.",
                    self.host,
                    addr.port()
                ))
            } else {
                crate::ftp::unreachable_error(&self.host, &e, "TCP")
            }
        })?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let _ = stream.set_nodelay(true);
        Ok(Conn {
            stream,
            host: self.host.clone(),
        })
    }

    /// Fassung, Firmware-Art und Boot-Zustand (geht immer, auch während ein Programm läuft).
    pub fn version(&self) -> Result<Status> {
        let mut c = self.open()?;
        c.token(VERSION_TOKEN)?;
        c.ack()?;
        // Der Text hat kein Ende-Zeichen: lesen bis zur Zeile `Boot:` bzw. bis eine Pause entsteht.
        let mut text = Vec::new();
        let mut buf = [0u8; 512];
        let start = Instant::now();
        c.timeout(Duration::from_millis(600));
        while start.elapsed() < Duration::from_secs(3) && text.len() < 4096 {
            match c.stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    text.extend_from_slice(&buf[..n]);
                    let s = String::from_utf8_lossy(&text);
                    if s.lines().any(|l| l.trim().starts_with("Boot:")) && s.ends_with('\n') {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&text);
        Status::parse(&text).with_context(|| {
            tr!(
                "{} antwortet nicht wie ein TeensyROM: {}",
                "{} does not answer like a TeensyROM: {}",
                self.host,
                text.trim().chars().take(120).collect::<String>()
            )
        })
    }

    /// Setzt den C64 zurück ins TeensyROM-Menü (auch aus der Minimal-Firmware).
    pub fn reset(&self) -> Result<()> {
        let mut c = self.open()?;
        c.token(RESET_TOKEN)?;
        // Antwort ist nur Text: „Reset cmd received“.
        let line = c.line();
        if !line.contains("Reset") {
            bail!(tr!(
                "TeensyROM bestätigt den Reset nicht ({line})",
                "TeensyROM does not confirm the reset ({line})"
            ));
        }
        Ok(())
    }

    /// Wartet, bis das Menü gestartet ist (`Boot: complete` oder keine Boot-Zeile).
    pub fn wait_ready(&self, max: Duration, poll: Duration) -> Result<Status> {
        let start = Instant::now();
        let mut last_err = None;
        while start.elapsed() < max {
            std::thread::sleep(poll);
            match self.version() {
                Ok(st) if st.boot_complete != Some(false) && !st.minimal => return Ok(st),
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow!(tr!(
                "TeensyROM-Menü ist nach dem Reset nicht bereit",
                "TeensyROM menu is not ready after the reset"
            ))
        }))
    }

    /// Reset und warten, bis Dateibefehle wieder gehen.
    pub fn reset_and_wait(&self) -> Result<()> {
        self.reset()?;
        self.wait_ready(Duration::from_secs(30), Duration::from_millis(700))?;
        Ok(())
    }

    /// Startet eine Datei auf dem Gerät (`/SD/…` oder `/USB/…`). Beendet, was gerade läuft.
    pub fn launch(&self, path: &str) -> Result<()> {
        let (drive, p) = split_drive(path)?;
        let dir_len = p.rfind('/').unwrap_or(0);
        if dir_len > MAX_LAUNCH_DIR {
            bail!(tr!(
                "Ordner zu lang zum Starten auf dem TeensyROM: {path}",
                "Folder too long to launch on the TeensyROM: {path}"
            ));
        }
        let mut c = self.open()?;
        c.token(LAUNCH_TOKEN)?;
        c.ack()?;
        c.path(drive, &p)?;
        c.ack()
    }

    /// Lädt eine Datei hoch. Gibt es sie schon: mit `overwrite` vorher löschen, sonst `Ok(false)`.
    pub fn upload(&self, path: &str, data: &[u8], overwrite: bool) -> Result<bool> {
        match self.post(path, data) {
            Err(e) if e.downcast_ref::<Refusal>() == Some(&Refusal::Exists) => {
                if !overwrite {
                    return Ok(false);
                }
                self.delete(path)?;
                self.post(path, data)?;
                Ok(true)
            }
            other => other.map(|_| true),
        }
    }

    fn post(&self, path: &str, data: &[u8]) -> Result<()> {
        let (drive, p) = split_drive(path)?;
        let len = u32::try_from(data.len()).context("Datei zu groß")?;
        let mut c = self.open()?;
        c.token(POST_FILE_TOKEN)?;
        c.ack()?;
        let mut head = Vec::with_capacity(p.len() + 8);
        head.extend_from_slice(&len.to_be_bytes());
        head.extend_from_slice(&checksum(data).to_be_bytes());
        c.send(&head)?;
        c.path(drive, &p)?;
        c.ack()?;
        c.send(data)?;
        // Schreiben auf SD/USB kann dauern.
        c.timeout(Duration::from_secs(60));
        c.ack()
    }

    pub fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        let (drive, p) = split_drive(path)?;
        let mut c = self.open()?;
        c.token(GET_FILE_TOKEN)?;
        c.ack()?;
        c.path(drive, &p)?;
        c.ack()?;
        let len = c.u32()?;
        // 16-Bit-Prüfsumme, auf 4 Byte erweitert.
        let sum = c.u32()? as u16;
        if len as u64 > limit {
            bail!(tr!(
                "Datei zu groß: {path} ({len} Byte)",
                "File too large: {path} ({len} bytes)"
            ));
        }
        let mut data = vec![0u8; len as usize];
        c.timeout(Duration::from_secs(30));
        c.read_exact(&mut data)?;
        c.ack()?;
        if checksum(&data) != sum {
            bail!(tr!(
                "Prüfsumme falsch beim Laden von {path}",
                "Wrong checksum while loading {path}"
            ));
        }
        Ok(data)
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        let (drive, p) = split_drive(path)?;
        let mut c = self.open()?;
        c.token(DELETE_FILE_TOKEN)?;
        c.ack()?;
        c.path(drive, &p)?;
        c.ack()
    }

    /// Ordnerinhalt; ein fehlender Ordner gibt eine leere Liste (wie beim Ultimate).
    /// `/` liefert die Laufwerke, die antworten.
    pub fn list(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        if path.trim_matches('/').is_empty() {
            return Ok(self
                .volumes()?
                .into_iter()
                .map(|name| RemoteEntry {
                    name,
                    is_dir: true,
                    size: 0,
                    modify: 0,
                })
                .collect());
        }
        match self.dir(path, u16::MAX) {
            Err(e) if matches!(e.downcast_ref::<Refusal>(), Some(Refusal::NotFound(_))) => {
                Ok(Vec::new())
            }
            r => r,
        }
    }

    fn dir(&self, path: &str, take: u16) -> Result<Vec<RemoteEntry>> {
        let (drive, p) = split_drive(path)?;
        let mut c = self.open()?;
        c.token(DIR_NDJSON_TOKEN)?;
        c.ack()?;
        let mut head = vec![drive];
        head.extend_from_slice(&0u16.to_be_bytes()); // skip
        head.extend_from_slice(&take.to_be_bytes());
        head.extend_from_slice(p.as_bytes());
        head.push(0);
        c.send(&head)?;
        c.ack()?;
        match c.u16()? {
            START_DIR => {}
            FAIL => return Err(anyhow!(Refusal::from_text(&c.line()))),
            other => bail!(tr!(
                "Unerwartete Antwort vom TeensyROM: 0x{:04X}",
                "Unexpected reply from TeensyROM: 0x{:04X}",
                other
            )),
        }
        // Zeilen bis zum Endzeichen 0xA5 0xA5 am Zeilenanfang.
        let mut out = Vec::new();
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            c.read_exact(&mut b)?;
            if line.is_empty() && b[0] == END_DIR.to_le_bytes()[0] {
                c.read_exact(&mut b)?;
                if b[0] == END_DIR.to_le_bytes()[1] {
                    break;
                }
                bail!(tr!(
                    "Verzeichnisliste vom TeensyROM unlesbar",
                    "Unreadable directory listing from TeensyROM"
                ));
            }
            if line.is_empty() && b[0] == (FAIL & 0xFF) as u8 {
                c.read_exact(&mut b)?;
                if b[0] == (FAIL >> 8) as u8 {
                    return Err(anyhow!(Refusal::from_text(&c.line())));
                }
                line.push((FAIL & 0xFF) as u8);
            }
            if b[0] == b'\n' {
                if let Some(e) = parse_dir_line(&String::from_utf8_lossy(&line))
                    && e.name != "."
                    && e.name != ".."
                {
                    out.push(e);
                }
                line.clear();
            } else {
                line.push(b[0]);
                if line.len() > 4096 {
                    bail!(tr!(
                        "Verzeichnisliste vom TeensyROM unlesbar",
                        "Unreadable directory listing from TeensyROM"
                    ));
                }
            }
        }
        Ok(out)
    }

    pub fn is_dir(&self, path: &str) -> Result<bool> {
        if path.trim_matches('/').is_empty() {
            return Ok(true);
        }
        match self.dir(path, 0) {
            Ok(_) => Ok(true),
            Err(e) if matches!(e.downcast_ref::<Refusal>(), Some(Refusal::NotFound(_))) => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Ordner legt PostFile selbst an – hier nichts zu tun.
    pub fn mkdir(&self, _path: &str) -> Result<()> {
        Ok(())
    }

    /// Laufwerke, die antworten (`SD`, `USB`).
    pub fn volumes(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (name, _) in DRIVES {
            match self.dir(&format!("/{name}"), 0) {
                Ok(_) => out.push(name.to_string()),
                Err(e) if is_busy(&e) => return Err(e),
                Err(_) => {}
            }
        }
        Ok(out)
    }
}

/// Ist unter `host` ein TeensyROM mit eingeschaltetem TCP-Listener? `Ok(None)`: Port zu oder
/// antwortet anders.
pub fn detect(host: &str) -> Option<Status> {
    let mut t = TeensyRom::quick(host);
    t.timeout = Duration::from_secs(3);
    t.version().ok()
}

/// Kurze Abfrage für die Statusanzeige.
pub fn quick_status(host: &str) -> Option<Status> {
    TeensyRom::quick(host).version().ok()
}

#[cfg(test)]
pub(crate) mod mock {
    //! Schein-TeensyROM: TCP-Server, der das Protokoll nach Doku und Firmware nachstellt.
    use super::*;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct State {
        /// Dateien je (Laufwerk, Pfad).
        pub files: BTreeMap<(u8, String), Vec<u8>>,
        /// Läuft ein Programm? Dann antworten Dateibefehle „Busy!“.
        pub busy: bool,
        pub minimal: bool,
        /// Gestartete Dateien.
        pub launched: Vec<(u8, String)>,
        pub resets: usize,
        /// Nach einem Reset so oft „Boot: in progress“ melden.
        pub boot_polls: usize,
        /// SD-Karte fehlt.
        pub no_sd: bool,
        /// Alle Befehle mit Token, zum Nachsehen.
        pub log: Vec<u16>,
    }

    pub struct Mock {
        pub addr: String,
        pub state: Arc<Mutex<State>>,
    }

    pub fn start() -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State::default()));
        let st = state.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let st = st.clone();
                std::thread::spawn(move || {
                    let _ = serve(s, &st);
                });
            }
        });
        Mock { addr, state }
    }

    fn rd(s: &mut TcpStream, n: usize) -> std::io::Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        s.read_exact(&mut b)?;
        Ok(b)
    }

    fn uint(s: &mut TcpStream, n: usize) -> std::io::Result<u32> {
        Ok(rd(s, n)?.iter().fold(0u32, |a, &b| (a << 8) | b as u32))
    }

    fn cstr(s: &mut TcpStream) -> std::io::Result<String> {
        let mut out = Vec::new();
        loop {
            let b = rd(s, 1)?[0];
            if b == 0 {
                break;
            }
            out.push(b);
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn tok(s: &mut TcpStream, t: u16) -> std::io::Result<()> {
        s.write_all(&t.to_le_bytes())
    }

    fn fail(s: &mut TcpStream, msg: &str) -> std::io::Result<()> {
        tok(s, FAIL)?;
        s.write_all(msg.as_bytes())?;
        s.write_all(b"\n")
    }

    fn serve(mut s: TcpStream, st: &Arc<Mutex<State>>) -> std::io::Result<()> {
        loop {
            let first = match rd(&mut s, 1) {
                Ok(b) => b[0],
                Err(_) => return Ok(()),
            };
            if first != 0x64 {
                continue; // die Firmware wertet andere Bytes als Textbefehle aus
            }
            let t = 0x6400 | rd(&mut s, 1)?[0] as u16;
            st.lock().unwrap().log.push(t);
            match t {
                RESET_TOKEN => {
                    let mut g = st.lock().unwrap();
                    g.resets += 1;
                    g.busy = false;
                    g.minimal = false;
                    g.boot_polls = g.boot_polls.max(2);
                    drop(g);
                    s.write_all(b"Reset cmd received\r\n")?;
                    // Das Menü startet neu: Verbindung geht verloren.
                    return Ok(());
                }
                VERSION_TOKEN => {
                    let mut g = st.lock().unwrap();
                    tok(&mut s, ACK)?;
                    if g.minimal {
                        s.write_all(b"\n  FW: TeensyROM+ v0.7.2(minimal)\n      Jul 27 2026, 09:41:32\n  Teensy: 816MHz  59.1C  UID: 19307720\n")?;
                    } else {
                        let boot = if g.boot_polls > 0 {
                            g.boot_polls -= 1;
                            "in progress"
                        } else {
                            "complete"
                        };
                        // In Stücken schicken wie ein langsamer Sender.
                        s.write_all(
                            b"\n  FW: TeensyROM+ v0.9-pr65*\n      Jul 27 2026, 09:41:32\n",
                        )?;
                        std::thread::sleep(Duration::from_millis(30));
                        s.write_all(
                            format!("  Teensy: 816MHz  59.1C  UID: 19307720\n  C64  PAL Vid  50 Hz\n  Boot: {boot}\n")
                                .as_bytes(),
                        )?;
                    }
                }
                LAUNCH_TOKEN => {
                    tok(&mut s, ACK)?;
                    let drive = rd(&mut s, 1)?[0];
                    let path = cstr(&mut s)?;
                    let mut g = st.lock().unwrap();
                    if drive > 2 {
                        continue; // Firmware: ohne Antwort zurück
                    }
                    tok(&mut s, ACK)?;
                    g.launched.push((drive, path));
                    g.busy = true;
                    g.minimal = false;
                }
                _ if st.lock().unwrap().busy || st.lock().unwrap().minimal => {
                    fail(&mut s, "Busy!")?;
                }
                POST_FILE_TOKEN => {
                    tok(&mut s, ACK)?;
                    let len = uint(&mut s, 4)? as usize;
                    let sum = uint(&mut s, 2)?;
                    let drive = uint(&mut s, 1)? as u8;
                    let path = cstr(&mut s)?;
                    if drive == 1 && st.lock().unwrap().no_sd {
                        fail(
                            &mut s,
                            &format!("Specified storage device was not found: {drive}"),
                        )?;
                        continue;
                    }
                    if st
                        .lock()
                        .unwrap()
                        .files
                        .contains_key(&(drive, path.clone()))
                    {
                        fail(&mut s, "File already exists.")?;
                        continue;
                    }
                    tok(&mut s, ACK)?;
                    let data = rd(&mut s, len)?;
                    let cs =
                        (sum.wrapping_sub(data.iter().map(|&b| b as u32).sum::<u32>())) & 0xFFFF;
                    if cs != 0 {
                        fail(&mut s, &format!("CS Failed! RCS:{cs}"))?;
                        continue;
                    }
                    st.lock().unwrap().files.insert((drive, path), data);
                    tok(&mut s, ACK)?;
                }
                GET_FILE_TOKEN => {
                    tok(&mut s, ACK)?;
                    let drive = uint(&mut s, 1)? as u8;
                    let path = cstr(&mut s)?;
                    let data = st.lock().unwrap().files.get(&(drive, path)).cloned();
                    let Some(data) = data else {
                        fail(&mut s, "File not found. (Error 4)")?;
                        continue;
                    };
                    tok(&mut s, ACK)?;
                    s.write_all(&(data.len() as u32).to_le_bytes())?;
                    s.write_all(&(checksum(&data) as u32).to_le_bytes())?;
                    s.write_all(&data)?;
                    tok(&mut s, ACK)?;
                }
                DELETE_FILE_TOKEN => {
                    tok(&mut s, ACK)?;
                    let drive = uint(&mut s, 1)? as u8;
                    let path = cstr(&mut s)?;
                    if st
                        .lock()
                        .unwrap()
                        .files
                        .remove(&(drive, path.clone()))
                        .is_some()
                    {
                        tok(&mut s, ACK)?;
                    } else {
                        fail(&mut s, &format!("File not found: {path}"))?;
                    }
                }
                DIR_NDJSON_TOKEN => {
                    tok(&mut s, ACK)?;
                    let drive = uint(&mut s, 1)? as u8;
                    let skip = uint(&mut s, 2)? as usize;
                    let take = uint(&mut s, 2)? as usize;
                    let path = cstr(&mut s)?;
                    let g = st.lock().unwrap();
                    if drive == 1 && g.no_sd {
                        fail(
                            &mut s,
                            &format!("Specified storage device was not found: {drive}"),
                        )?;
                        continue;
                    }
                    let prefix = if path == "/" {
                        "/".to_string()
                    } else {
                        format!("{path}/")
                    };
                    let mut items: BTreeMap<String, Option<usize>> = BTreeMap::new();
                    let mut exists = path == "/";
                    let mut is_file = false;
                    for ((d, p), data) in &g.files {
                        if *d != drive {
                            continue;
                        }
                        if let Some(rest) = p.strip_prefix(&prefix) {
                            exists = true;
                            match rest.split_once('/') {
                                Some((dir, _)) => {
                                    items.insert(dir.to_string(), None);
                                }
                                None => {
                                    items.insert(rest.to_string(), Some(data.len()));
                                }
                            }
                        } else if p == &path {
                            is_file = true;
                        }
                    }
                    drop(g);
                    if is_file {
                        fail(&mut s, "Path is not a directory. (Error 6)")?;
                        continue;
                    }
                    if !exists {
                        fail(&mut s, "Directory not found. (Error 5)")?;
                        continue;
                    }
                    tok(&mut s, ACK)?;
                    tok(&mut s, START_DIR)?;
                    // Firmware-Eigenheit: Zähler ab 1, Ausgabe ab Zähler ≥ skip.
                    let mut count = 0;
                    let mut sent = 0;
                    for (name, size) in items {
                        if sent >= take {
                            break;
                        }
                        count += 1;
                        if count >= skip {
                            sent += 1;
                            let line = match size {
                                None => format!("{{\"type\":\"dir\",\"name\":\"{name}\"}}\r\n"),
                                Some(n) => format!(
                                    "{{\"type\":\"file\",\"name\":\"{name}\",\"size\":{n}}}\r\n"
                                ),
                            };
                            s.write_all(line.as_bytes())?;
                        }
                    }
                    tok(&mut s, END_DIR)?;
                }
                other => {
                    s.write_all(format!("Unk cmd: 0x{other:04x}\n").as_bytes())?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock;
    use super::*;

    fn tr(m: &mock::Mock) -> TeensyRom {
        TeensyRom::new(&m.addr)
    }

    #[test]
    fn pfade() {
        assert_eq!(
            split_drive("/SD/Breadbin/x.prg").unwrap(),
            (1, "/Breadbin/x.prg".into())
        );
        assert_eq!(split_drive("/usb").unwrap(), (0, "/".into()));
        assert_eq!(split_drive("/USB/a/").unwrap(), (0, "/a".into()));
        assert!(split_drive("/USB0/x").is_err());
        assert!(split_drive("/").is_err());
        assert!(split_drive(&format!("/SD/{}", "a".repeat(400))).is_err());
        assert!(is_usb_path("/USB/Breadbin"));
        assert!(!is_usb_path("/SD/Breadbin"));
        assert_eq!(socket_addr("127.0.0.1").unwrap().port(), PORT);
        assert_eq!(socket_addr("127.0.0.1:2113").unwrap().port(), 2113);
    }

    #[test]
    fn versionstext_lesen() {
        let st = Status::parse(
            "\n  FW: TeensyROM+ v0.7.2\n      Jul 27 2026, 09:41:32\n  Teensy: 816MHz  59.1C  UID: 19307720\n  C128  NTSC Vid  60 Hz\n  Boot: complete\n",
        )
        .unwrap();
        assert_eq!(st.product, "TeensyROM+");
        assert_eq!(st.version, "v0.7.2");
        assert_eq!(st.machine, "C128 NTSC Vid 60 Hz");
        assert_eq!(st.boot_complete, Some(true));
        assert!(!st.minimal);
        let min = Status::parse("  FW: TeensyROM v0.7.2(minimal)\n  Teensy: 600MHz\n").unwrap();
        assert!(min.minimal);
        assert_eq!(min.version, "v0.7.2");
        assert_eq!(min.boot_complete, None);
        assert_eq!(
            Status::parse("  FW: TeensyROM+ v0.9-pr65*\n  Boot: in progress\n")
                .unwrap()
                .version,
            "v0.9-pr65*"
        );
        assert!(Status::parse("SSH-2.0-OpenSSH").is_none());
        assert_eq!(hints(&st).len(), 2);
        assert_eq!(hints(&min).len(), 3);
    }

    #[test]
    fn verzeichniszeilen() {
        let f =
            parse_dir_line(r#"{"type":"file","name":"Giana_1234abcd.prg","size":4711}"#).unwrap();
        assert_eq!(
            (f.name.as_str(), f.is_dir, f.size),
            ("Giana_1234abcd.prg", false, 4711)
        );
        let d = parse_dir_line("{\"type\":\"dir\",\"name\":\"Spiele\"}\r").unwrap();
        assert!(d.is_dir);
        // Unmaskierter Backslash im Namen bricht JSON – trotzdem lesen.
        let odd = parse_dir_line(r#"{"type":"file","name":"a\b.prg","size":3}"#).unwrap();
        assert_eq!((odd.name.as_str(), odd.size), (r"a\b.prg", 3));
        assert!(parse_dir_line("").is_none());
    }

    #[test]
    fn hochladen_auflisten_laden_loeschen() {
        let m = mock::start();
        let t = tr(&m);
        let st = t.version().unwrap();
        assert_eq!(st.product, "TeensyROM+");
        assert_eq!(st.boot_complete, Some(true));

        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
        assert!(
            t.upload("/SD/Breadbin/Spiel_0123abcd.prg", &data, false)
                .unwrap()
        );
        // Gleicher Name noch einmal: ohne Überschreiben nichts tun, mit Überschreiben ersetzen.
        assert!(
            !t.upload("/SD/Breadbin/Spiel_0123abcd.prg", b"anders", false)
                .unwrap()
        );
        assert_eq!(
            t.download("/SD/Breadbin/Spiel_0123abcd.prg", 1 << 20)
                .unwrap(),
            data
        );
        assert!(
            t.upload("/SD/Breadbin/Spiel_0123abcd.prg", b"\x01\x08neu", true)
                .unwrap()
        );
        assert_eq!(
            t.download("/SD/Breadbin/Spiel_0123abcd.prg", 100).unwrap(),
            b"\x01\x08neu"
        );
        assert!(
            t.download("/SD/Breadbin/Spiel_0123abcd.prg", 2).is_err(),
            "Grenze"
        );

        t.upload("/SD/Breadbin/Unter/b.crt", b"C64 CARTRIDGE", false)
            .unwrap();
        let mut names: Vec<_> = t
            .list("/SD/Breadbin")
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.is_dir, e.size))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                ("Spiel_0123abcd.prg".into(), false, 5),
                ("Unter".into(), true, 0)
            ]
        );
        assert!(t.is_dir("/SD/Breadbin").unwrap());
        assert!(t.is_dir("/SD/Breadbin/Unter").unwrap());
        assert!(!t.is_dir("/SD/Fehlt").unwrap());
        assert!(!t.is_dir("/SD/Breadbin/Spiel_0123abcd.prg").unwrap());
        assert!(
            t.list("/SD/Fehlt").unwrap().is_empty(),
            "fehlender Ordner = leer"
        );
        let vols: Vec<_> = t.list("/").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(vols, ["SD", "USB"]);

        t.delete("/SD/Breadbin/Unter/b.crt").unwrap();
        let e = t.delete("/SD/Breadbin/Unter/b.crt").unwrap_err();
        assert!(e.to_string().contains("File not found"), "{e}");
        let e = t.download("/SD/nix.prg", 100).unwrap_err();
        assert!(matches!(
            e.downcast_ref::<Refusal>(),
            Some(Refusal::NotFound(_))
        ));

        let log = m.state.lock().unwrap().log.clone();
        assert!(log.contains(&POST_FILE_TOKEN) && log.contains(&DIR_NDJSON_TOKEN));
    }

    #[test]
    fn starten_beschaeftigt_reset() {
        let m = mock::start();
        let t = tr(&m);
        t.upload("/USB/x.prg", b"\x01\x08\x00", false).unwrap();
        t.launch("/USB/x.prg").unwrap();
        assert_eq!(
            m.state.lock().unwrap().launched,
            [(0, "/x.prg".to_string())]
        );
        // Programm läuft: Dateibefehle werden abgelehnt, Version und Start gehen weiter.
        let e = t.upload("/USB/y.prg", b"\x01\x08\x00", false).unwrap_err();
        assert!(is_busy(&e), "{e}");
        assert!(e.to_string().contains("beschäftigt"));
        assert!(t.version().is_ok());
        t.reset().unwrap();
        // Nach dem Reset meldet das Menü zweimal „in progress“.
        let st = t
            .wait_ready(Duration::from_secs(5), Duration::from_millis(10))
            .unwrap();
        assert_eq!(st.boot_complete, Some(true));
        assert!(t.upload("/USB/y.prg", b"\x01\x08\x00", false).unwrap());
        assert_eq!(m.state.lock().unwrap().resets, 1);
    }

    #[test]
    fn minimal_firmware_und_fehler() {
        let m = mock::start();
        m.state.lock().unwrap().minimal = true;
        let t = tr(&m);
        assert!(t.version().unwrap().minimal);
        assert!(is_busy(&t.list("/SD").unwrap_err()));
        t.reset_and_wait().unwrap();
        assert!(!t.version().unwrap().minimal);

        m.state.lock().unwrap().no_sd = true;
        let e = t.upload("/SD/a.prg", b"xx", false).unwrap_err();
        assert!(
            e.to_string().contains("storage device was not found"),
            "{e}"
        );
        assert_eq!(t.volumes().unwrap(), ["USB"]);
        assert!(t.launch(&format!("/SD/{}/x.prg", "d".repeat(300))).is_err());
    }

    #[test]
    fn port_zu_und_fremder_dienst() {
        // Freien Port belegen und gleich wieder schließen → Verbindung abgelehnt.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        let e = TeensyRom::new(&addr).version().unwrap_err();
        assert!(e.to_string().contains("Enable TCP Listener"), "{e}");
        assert!(detect(&addr).is_none());

        // Dienst, der etwas anderes antwortet.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let _ = s.write_all(b"SSH-2.0-OpenSSH\r\n");
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        assert!(detect(&addr).is_none());
    }
}
