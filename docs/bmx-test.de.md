# BMX in Breadbin – Prüfliste (Entwurf, ungetestete Vorschau)

Stand 2026-11-23. Nicht in Release 0.9.4, nur vorbereitet (Branch `bmx-preview`). Gebaut ohne Gerät nach dem BMX-Quelltext; Code in
`core/src/bmx.rs` (Annahmen dort als `ANNAHME:` markiert), Start-Ablauf in `core/src/launch.rs` (`launch_bmx`).
English version: `bmx-test.md`.

Vorbereitung: BMX mit Netzwerk, „Remote API“ an, Maschine C64. Ohne Oberfläche: `c64scan --probe <ip>`,
`c64scan <db> --launch <ip> /USER/disks/Breadbin <suchtext>`, `c64scan <db> --screenshot <ip> bild.png [voll]`.

1. **Erkennung/Passwort:** „Verbinden und prüfen“ erkennt BMX über `GET /state`; mit Passwort am Gerät kommt ohne Eingabe ein Hinweis.
2. **Netzsuche:** UDP 6464 (Broadcast) und HTTP-Suche – findet „Im Netz suchen“ den BMX ohne Entwicklermodus?
3. **Startordner:** Vorschlag `/USER/disks/Breadbin` (Module nach `/USER/carts/…`, Bänder nach `/USER/tapes/…`). Richtiges Laufwerk?
4. **Starten** je einmal: PRG, P00, T64, D64, D71, D81, G64, TAP, CRT (auch ein 1-MB-EasyFlash).
5. **Modul abziehen:** Vor jedem Nicht-CRT-Start `media.cartridge.detach` – startet danach das neue Spiel?
6. **Einlegen ohne Neustart** (Laufwerk 8) mitten im Spiel; zurück zu Seite 1 (Datei ist eingelegt → kein neuer Upload).
7. **Bildschirmfoto/automatisches Titelbild:** sitzt der Zuschnitt auf 320 × 200 (auch mit schwarzem Rahmen, Shader, NTSC)?
8. **Spielstände holen** und **Startordner leeren** (eingelegte Dateien kann BMX nicht löschen → Meldung verständlich?).
9. **Scan** eines Ordners auf dem BMX (`bmx://<ip>/USER/disks/…`, Seiten zu 16): Dauer bei 1000+ Dateien, Umlaute.
10. **Statusabfrage** (`/state`) stört ein laufendes Spiel nicht?

## Offene Fragen an das BMX-Projekt
- Bleibt `/bmx/api/v1` stabil?
- Antwortet die UDP-Suche (6464) ohne Entwicklermodus?
- Auflisten/Herunterladen/Löschen über `/files` und `/file` – so gedacht für Fremdprogramme?
- TAP/G64 per `media.autostart`? Auswerfen per `media.drive.8.detach` / `media.cartridge.detach`?
- Gibt es das Bildschirmfoto auch nur als Emulatorbild (ohne Skalierung und Balken)?
