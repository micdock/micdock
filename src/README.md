# MicDock – Rust workspace

```
crates/micdock-core/   device logic, no GUI: hid (protocol), devices (units, storage mode), recordings,
                       firmware, fwupdate, guide, paths
crates/micdock-cli/    rodego2, ugg2wav, micdock-firmware
crates/micdock-app/    the desktop app, built with libcosmic
```

```bash
cargo build --release --examples   # target/release/{micdock,rodego2,ugg2wav,micdock-firmware}, examples/probe
cargo test                         # incl. the firmware-installer simulator
cargo test -p micdock-app -- --ignored   # plays a test tone on the audio device
cargo clippy --all-targets         # kept warning-free
cargo fmt
```

`MICDOCK_DEBUG=1 micdock` logs every device poll (full / light / receiver-only).

## Status

| Part | State |
|---|---|
| `hid` — hidraw, report-9 GET/SET, record / button / merged-split modes, storage switch, delete-all | done; checked on real units |
| `devices` — probing, storage mode, drive ↔ serial, mount/eject (via `udisksctl` for now), setters | done; checked on real units |
| `recordings` — UGG + WAV reading, session grouping, WAV/MP3 export | done; output was byte-identical to the former Python version |
| `firmware` — RØDE manifest, MD5-verified download, package parts | done |
| `fwupdate` — the installer, behind a `Transport` trait | done; 6 simulator tests (incl. RØDE's real 2.5.0 package); `plan` identical; **not yet run on a unit** |
| `rodego2`, `ugg2wav`, `micdock-firmware` | done; `rodego2 out` refuses the known-destructive report-1 commands |
| `guide` — RØDE's user guide: fetch, clean-up, offline cache, app chapters left out | done |
| desktop app (libcosmic) | sidebar with the units (see below for polling), Audio page (record mode, Ø button, merged/split, RX gain mode + output gain + power saver, TX low-sensitivity pad, linked TXs), Recordings page (connect TX storage with warning, list per TX, built-in player (cpal: play/pause, seek bar, markers), export WAV/MP3 with progress + cancel, eject, delete all with typed DELETE — not run on hardware), Device page (rename, set clock, parameter scanner, About panel, firmware check/download/install with typed INSTALL — the app installer not run on hardware), window close blocked during a firmware update (asks during an export), changes seen within ~1 s (sysfs watch every 0.7 s; receiver read every 3 s, transmitters every 30 s or on demand so their power buttons keep working; settings shown at once), help (header button / F1: MicDock manual + RØDE guide as Markdown, chapter by chapter; guide fetched in the background once); next: translations, manual screenshots |

Desktop theme detection is not ported: libcosmic follows the COSMIC theme by itself.
