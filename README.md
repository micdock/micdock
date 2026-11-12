# MicDock – manage a RØDE Wireless GO II on Linux

MicDock does on Linux what RØDE's own apps do for the Wireless GO II:

- copy the transmitters' on-board recordings (`.WAV` and older `.UGG`) with correct dates, play them, and
  export them to WAV or MP3, joining the ~31-minute pieces of a long recording;
- set the on-board record mode (Always / Manual / Backup / Off), the Ø button (Mute / Marker / Record / None)
  and the transmitters' low-sensitivity pad;
- set the receiver's output (merged / split), gain mode and level (coarse, fine, camera preset) and power saver;
- show which transmitters are switched on (through the receiver), with their battery level;
- rename the units, set their clocks, and check for, download and install firmware updates.

It talks to the units over `/dev/hidraw*` and needs no RØDE software. The app is built with libcosmic and follows
the desktop theme; the manual and RØDE's user guide are available offline inside the app.

*MicDock is an unofficial project, not affiliated with or endorsed by RØDE Microphones. RØDE and
Wireless GO are trademarks of their owner and are used here only to name the hardware MicDock works with.*

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

## Contributing and reporting problems

Bug reports and pull requests are welcome at <https://github.com/micdock/micdock/issues>. Please mention the
firmware version of your units (Device page, or `micdock-firmware check`) and, for device problems, the output
of `rodego2 list` and `rodego2 --dev /dev/hidrawN scan` (read-only).

**Never test unknown device commands on units with recordings you want to keep.** One undocumented report-1
command (`'Q' 01`) erased the recordings of two transmitters during development; see [PROTOCOL.md](PROTOCOL.md).
New settings are found by changing them on the unit itself and comparing read-only scans before and after.

Before sending code: `cargo fmt`, `cargo clippy --all-targets` (no warnings) and `cargo test` in `src/`.

## License

MicDock is free software: GNU General Public License, version 3 or later (see [LICENSE](LICENSE)).
How the device protocol was worked out is documented in [PROTOCOL.md](PROTOCOL.md).
