# BMX in Breadbin – test checklist (draft, untested preview)

As of 2026-11-23. Not part of release 0.9.4, only prepared (branch `bmx-preview`). Built without a device from the BMX source code; code in
`core/src/bmx.rs` (assumptions marked `ANNAHME:` = "assumption"), launch flow in `core/src/launch.rs` (`launch_bmx`).
Deutsche Fassung: `bmx-test.de.md`.

Setup: BMX with networking, "Remote API" on, machine C64. Without the UI: `c64scan --probe <ip>`,
`c64scan <db> --launch <ip> /USER/disks/Breadbin <search>`, `c64scan <db> --screenshot <ip> shot.png [voll]`.

1. **Detection/password:** "Connect and check" detects BMX via `GET /state`; with a device password and none entered, a hint appears.
2. **Network search:** UDP 6464 (broadcast) plus HTTP sweep – does "Search network" find BMX without developer mode?
3. **Launch folder:** suggested `/USER/disks/Breadbin` (cartridges to `/USER/carts/…`, tapes to `/USER/tapes/…`). Right drive?
4. **Launch** once each: PRG, P00, T64, D64, D71, D81, G64, TAP, CRT (also a 1 MB EasyFlash).
5. **Cartridge removal:** before every non-CRT launch Breadbin sends `media.cartridge.detach` – does the new game start?
6. **Insert without restart** (drive 8) during a game; back to side 1 (file is inserted → no new upload).
7. **Screenshot/automatic title picture:** is the crop to 320 × 200 right (also with black border, shaders, NTSC)?
8. **Fetch saved games** and **empty launch folder** (BMX cannot delete inserted files → is the message clear?).
9. **Scan** of a folder on the BMX (`bmx://<ip>/USER/disks/…`, pages of 16): duration with 1000+ files, non-ASCII names.
10. **Status polling** (`/state`) does not disturb a running game?

## Open questions to the BMX project
- Will `/bmx/api/v1` stay stable?
- Does the UDP search (6464) answer without developer mode?
- Listing/downloading/deleting via `/files` and `/file` – intended for third-party programs?
- TAP/G64 via `media.autostart`? Ejecting via `media.drive.8.detach` / `media.cartridge.detach`?
- Is there a screenshot of just the emulator picture (without scaling and bars)?
