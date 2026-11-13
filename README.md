# MicDock – manage a RØDE Wireless GO II on Linux

[![build](https://github.com/micdock/micdock/actions/workflows/build.yml/badge.svg)](https://github.com/micdock/micdock/actions/workflows/build.yml)

MicDock does on Linux what RØDE's own apps do for the Wireless GO II:

- copy the transmitters' on-board recordings (`.WAV` and older `.UGG`) with correct dates, play them, and
  export them to WAV or MP3, joining the ~31-minute pieces of a long recording;
- set the on-board record mode (Always / Manual / Backup / Off), the Ø button (Mute / Marker / Record / None),
  the recording format, the LED brightness and the low-sensitivity pad — over USB or through the receiver, so a
  transmitter doesn't even have to be connected;
- set the receiver's output (merged / split), gain mode and level (coarse, fine, camera preset), power saver,
  safety channel and what its Ø button does;
- show which transmitters are switched on (through the receiver), with their battery level, and whether the
  receiver is on;
- show the receiver's USB audio live (level meters, spectrum) and record it to WAV — shared with other programs;
- rename the units, set their clocks, and check for, download and install firmware updates.

It talks to the units over `/dev/hidraw*` and needs no RØDE software. The app is built with libcosmic and follows
the desktop theme; the manual and RØDE's user guide are available offline inside the app.

*MicDock is an unofficial project, not affiliated with or endorsed by RØDE Microphones. RØDE and
Wireless GO are trademarks of their owner and are used here only to name the hardware MicDock works with.*

## Download

Ready-made builds for Linux on x86-64 and 64-bit ARM (glibc 2.34 or newer: Ubuntu 22.04+, Debian 12+, Fedora,
RHEL/Alma/Rocky 9+, openSUSE, Arch, …) are on the [releases page](https://github.com/micdock/micdock/releases):
unpack `micdock-<version>-<x86_64 or aarch64>-linux.tar.gz` and run `./install.sh` (for your user; it prints the one `sudo`
command for the udev rule) or `sudo ./install.sh --system` (for everyone). `./install.sh --remove` uninstalls.
What changed: [CHANGELOG.md](CHANGELOG.md).

## Build and install

```bash
git clone https://github.com/micdock/micdock.git
cd micdock
```

Needs Rust (1.93 or newer, e.g. via rustup), the ALSA development files (`sudo apt install libasound2-dev`) and libcosmic's build dependencies (see its README; on Pop!_OS they are usually present); `ffmpeg` for MP3
export and `udisks2` for mounting the transmitters' drives.

```bash
cd src
cargo build --release          # target/release/{micdock,rodego2,ugg2wav,micdock-firmware}
```

Install for your user (from this folder):

```bash
for b in micdock rodego2 ugg2wav micdock-firmware; do ln -sf "$PWD/src/target/release/$b" ~/.local/bin/; done
sed "s|^Exec=.*|Exec=$HOME/.local/bin/micdock|" micdock.desktop > ~/.local/share/applications/micdock.desktop
```

Device access needs the udev rule once:
`sudo cp 70-rode.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger`.

The app finds its manual in `docs/` next to the source folder, in `<prefix>/share/micdock/docs`, or wherever
`MICDOCK_DOCS` points.

## Use

Start **MicDock** from the app menu, or run `micdock`. The user manual is in the app (help button or F1) and in
[docs/en/manual.md](docs/en/manual.md). RØDE's own user guide opens in the same place: MicDock keeps an offline copy
and leaves out its chapters about RØDE's app (the MicDock manual covers those tasks). It is never bundled: it is
downloaded from rode.com.

Command-line tools (each has `--help`):

```bash
rodego2 list                                  # connected units
rodego2 --dev /dev/hidrawN rec [MODE]         # TX record mode: off, backup, always, button
rodego2 --dev /dev/hidrawN button [MODE]      # TX Ø button: mute, marker, none, record
rodego2 --dev /dev/hidrawN pad [on|off]       # TX low-sensitivity pad
rodego2 --dev /dev/hidrawN mode [MODE]        # RX output: merged, split
rodego2 --dev /dev/hidrawN gain [DB] [--mode coarse|fine|preset]   # RX output gain
rodego2 --dev /dev/hidrawN powersaver [on|off]                     # RX backlight power saver
rodego2 --dev /dev/hidrawN safety [on|off]                         # RX safety channel (merged mode)
rodego2 --dev /dev/hidrawN rxbutton [powersaver|marker]            # RX Ø button, single click
rodego2 --dev /dev/hidrawN radio 1 [--led dim] [--pad on] [--rec backup] [--format compressed]  # a TX via the RX
rodego2 --dev /dev/hidrawN scan               # read-only dump of all parameters
rodego2 --dev /dev/hidrawN erase              # TX: delete ALL recordings (asks first; irreversible)
ugg2wav [--mp3] [--join] -o ~/go2-recordings /media/$USER/<drive>/REC*
micdock-firmware check|download|plan tx|install tx
```

`scan`, `get`, `name`, `list` and `listen` only read; `set`, `raw` and `out` change the device (see
[PROTOCOL.md](PROTOCOL.md) before using them).

## Project layout

```
src/                       Cargo workspace
  crates/micdock-core/     device logic, no GUI: hid (protocol), devices, recordings, firmware,
                           fwupdate (installer), guide (RØDE's guide, offline), paths
  crates/micdock-cli/      rodego2, ugg2wav, micdock-firmware
  crates/micdock-app/      the desktop app (libcosmic): main, recordings, player, device, help, config, util
docs/<lang>/manual.md      the user manual, shown in the app
PROTOCOL.md                how the device protocol was worked out
packaging/                 release archive (make-release.sh) and its installer (install.sh)
.github/workflows/         CI: build, checks, tests; releases on v* tags
70-rode.rules              udev rule for device access
micdock.desktop            app-menu entry
```

## Screenshots for the manual

The manual's pictures are made by the app itself, in demo mode (made-up units and recordings, no hardware, no
network, your settings untouched):

```bash
MICDOCK_SCREENSHOTS=docs/en/img src/target/release/micdock   # opens, saves 5 PNGs, quits
MICDOCK_DEMO=1 src/target/release/micdock                     # just look around in demo mode
```

Run it again after changing the interface. The pictures follow your desktop theme (light/dark, accent colour).

## Support MicDock

MicDock is free and made in spare time. If it saves you a trip to a Windows or Mac computer, you can
[buy me a coffee](https://buymeacoffee.com/ilom) ☕. Thank you!

## Contributing and reporting problems

Bug reports and pull requests are welcome at <https://github.com/micdock/micdock/issues>. Please mention the
firmware version of your units (Device page, or `micdock-firmware check`) and, for device problems, the output
of `rodego2 list` and `rodego2 --dev /dev/hidrawN scan` (read-only).

**Never test unknown device commands on units with recordings you want to keep.** One undocumented report-1
command (`'Q' 01`) erased the recordings of two transmitters during development; see [PROTOCOL.md](PROTOCOL.md).
New settings are found by changing them on the unit itself and comparing read-only scans before and after.

Before sending code, run the same checks as CI locally: `scripts/check.sh` (formatting, clippy without warnings,
all tests; `--quick` skips the tests). `scripts/install-hooks.sh` makes `git push` run it automatically.

CI runs on pull requests (required before merging into `main`), on `main` and on release tags, for x86-64 and
ARM64 — not on every branch push. A newer push to a pull request cancels the older run, and a pull request that
only changes documentation skips the build. To build a branch without a pull request: Actions → build →
*Run workflow*. CodeQL code scanning runs only on pushes to `main` (`.github/workflows/codeql.yml`). The tests need no hardware: they use synthetic
recordings (`micdock_core::testdata`, laid out like the transmitter writes them), run the command-line tools as a
user would, and simulate the firmware installer. A few tests use real hardware and only run on request:

```bash
cargo test -- --ignored                                   # player: plays a test tone
MICDOCK_TEST_RX=<receiver serial> cargo test -- --ignored # records 2 s from a switched-on receiver
```

## License

MicDock is free software: GNU General Public License, version 3 or later (see [LICENSE](LICENSE)).
How the device protocol was worked out is documented in [PROTOCOL.md](PROTOCOL.md).
