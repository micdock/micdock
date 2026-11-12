# Changelog

## 0.9.0 — first public release

MicDock manages a RØDE Wireless GO II on Linux, without RØDE's software.

- **Recordings:** connect a transmitter's storage, list its recordings (one list per transmitter, split
  recordings shown as one), play them inside the app, export to WAV or MP3 (joined or per file, with the real
  recording dates), eject, and delete all recordings.
- **Transmitters:** on-board record mode (Always / Manual / Backup / Off), Ø button (Mute / Marker / Record /
  None), low-sensitivity pad.
- **Receiver:** merged / split output, gain mode (coarse, fine, camera preset) and output gain, power saver,
  and which transmitters are on (with their battery level).
- **Device:** rename, set the clock, firmware check, download (MD5-verified) and install, USB storage mode,
  a read-only parameter scanner.
- **Help:** the MicDock manual and RØDE's Wireless GO II user guide (downloaded once, then offline) inside the app.
- **Command-line tools:** `rodego2`, `ugg2wav`, `micdock-firmware`.

Known limitations: the firmware installer has updated units during development, but this version's installer
has not yet run on hardware; LED brightness, the safety channel and editing camera presets are not available.
Builds for x86-64 and 64-bit ARM Linux (glibc 2.34 or newer). Tested on Pop!_OS 24.04 (COSMIC) with firmware 2.5.0.
