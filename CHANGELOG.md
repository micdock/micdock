# Changelog

## 0.11.0

- **Transmitter settings through the receiver:** record mode, Ø button, recording format (compressed or
  uncompressed in Backup mode), low-sensitivity pad and LED brightness (bright/dim) for each linked transmitter,
  without connecting it to the computer — on the receiver's Audio page, and from `rodego2 radio`.
- **LED brightness and recording format** are also on the transmitter's own page.
- **Receiver:** the Ø button's single click (power saver or marker) and the safety channel (Merged mode).
- Linked transmitters show their firmware, record mode, button, format and LED setting.
- A clock set by RØDE's app (local time) is recognised as such instead of showing as an hour off.
- Settings changes on the receiver are followed by its refresh command, as RØDE Central does.
- Protocol details found with the help of [Open Rode](https://github.com/areoseek/open-rode)'s capture of
  RØDE Central's traffic, each verified on hardware (some values turned out reversed).

## 0.10.0

- **Live audio from the receiver:** a new *Live* tab shows the receiver's USB audio with a level meter per
  channel (with peak marker and a *clip* warning) and a 24-band spectrum — two bars per band in split mode, or
  one channel of your choice, with optional peak markers to spot outliers — and records it to a WAV file
  (48 kHz, 24-bit, stereo). MicDock listens through
  PipeWire/PulseAudio, so other programs can use the receiver at the same time; the recording continues when you
  switch pages.
- **The receiver's on/off state:** MicDock now sees whether the receiver is switched on (it is a USB sound card
  only then) and says so in the sidebar and the header; transmitters no longer show as "off" just because the
  receiver is.
- Donation link (Buy Me a Coffee) in the About panel and the manual.

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
