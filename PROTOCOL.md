# RØDE Wireless GO II – settings protocol (from RØDE Central for Windows)

Source: `RODE_Central_WIN.msi` → `rode_bridge.dll` (x64, 32 MB). Static analysis only, nothing
captured from a live device yet, so everything marked *unverified* still needs a test on real hardware.

## What RØDE Central is made of

- **UI:** Flutter (`RODE Central.exe`, `flutter_windows.dll`, `data/app.so`). The UI has no device logic.
- **Device logic:** `rode_bridge.dll`, a C++/JUCE library that Flutter calls through Dart FFI.
  It has around 160 exports such as `setSafetyChannel` and `getBatteryLevel`.
- **Transport:** plain **USB HID**. It enumerates with SetupDi (`\\?\hid#`, `USB\VID_19F7&PID_`)
  and uses `CreateFile`/`WriteFile`/`ReadFile` on the HID handle, plus `HidD_SetFeature`.
  No custom driver is involved, so on Linux the stock `usbhid` driver plus `/dev/hidrawN` is enough.
- **Firmware:** OTA images for GO II are embedded (`wgo2_rx_2.50_ee7582e_ota.bin`, `wgo2_tx_…`,
  plus nRF52 hex files for GO II/2.5).

## Framing

The low-level writer is `fcn.180112cc0`, a `HidDevices` method. It allocates `OutputReportByteLength`,
puts the report ID in byte 0, copies the payload after it, zero-pads and calls `WriteFile`.
Incoming reports go into a queue, and the report ID byte is **stripped** before the payload is
handed back (`fcn.18010f350` copies from `buf+1`).

The generic request/response helper is `fcn.1800c0c90`. It takes `(reportId, expectFirstByte, tx, txLen, rx, rxLen, devId, hid, wantReply, timeoutMs)`.
It writes once, then polls the input queue until `rx[0] == expectFirstByte` or the timeout runs out.

### Settings: output report **9**

| op  | payload (after report ID 9)        | timeout | function       |
|-----|------------------------------------|---------|----------------|
| GET | `param, 0x01, sub, 00…`            | 2000 ms | `fcn.1800c30b0` |
| SET | `param, sub, data[len], 00…`       | 250 ms  | `fcn.1800c3ac0` |

Reply (input report, ID stripped): `param, 0x41 ('A' = ACK), data…`.
GET copies `len` bytes from reply offset 2. SET succeeds when reply byte 1 is `0x41`.
`sub` is 0 in almost every call.

Other reports seen: report 1 is used for a 16-byte request (`fcn.1800c3920`) and a `'Q'`-prefixed
command (`fcn.1800cae50`). Reports 5, 7 and 0x41 are firmware-transfer paths (`fcn.1800f96a0`,
`fcn.1800fd7d0`). The reply codes for those are `S`, `A` and `C`.

## Parameter IDs

Lengths and GET/SET usage are taken from the code. The names are inferred from where each ID is used.

| id   | len | used by | notes |
|------|-----|---------|-------|
| 0x00 | 1   | WirelessDevice init (GET) | probably status or protocol version |
| 0x01 | 4   | WirelessDevice (GET/SET)  | 32-bit value. Likely timestamp/clock (`setTimestamp` export) *unverified* |
| 0x03 | 22  | WirelessDevice (GET/SET)  | **device name** (`changeDeviceName` / `resetDeviceName`) |
| 0x04 | 1   | WGo2 + others             | GO II setting, toggled 0/1 |
| 0x05 | 2   | feature classes           | `[mode, value]` where mode is 0…4. Likely gain-mode/preset family |
| 0x06 | 1   | WGo2Tx vtable get/set     | TX setting (button mode / record mode?) |
| 0x07 | 1   | feature classes           | |
| 0x08 | 7   | feature classes           | `[x, 0xFF, …]`, camera preset? |
| 0x0a | 1   | WGo2 (bool, `== 1`)       | GO II toggle |
| 0x0b | 3   | WGo2 (6 setters)          | `[a, b, c]`, two cached bytes + new value. Likely gain or channel config |
| 0x18 | 1   | feature class             | |
| 0x19 | 1   | feature class             | |
| 0x20 | 5   | feature class (GET)       | |
| 0x21 | 1   | feature class             | |
| 0x23 | 1   | WGo2                      | GO II toggle |
| 0x25 | 1   | WGo2                      | GO II toggle |
| 0x12–0x14, 0x24, 0x27, 0x28, 0x3d, 0x3e, 0x41, 0x51 | | other models (Wireless PRO/ME/MICRO, TxThru) | |

Several feature classes keep their param ID in a member variable that is set per device model,
so the exact GO II IDs for features like safety channel, backlight, LED brightness and
gain-assist are filled in at runtime. `rodego2 scan` followed by a diff is the quickest way to pin them down:

1. `rodego2 scan > before.txt`
2. change one setting on the device (or in RØDE Central on another machine)
3. `rodego2 scan > after.txt && diff before.txt after.txt`

## Not covered yet

- Firmware update (separate report-5/7 bulk path with `S`/`A`/`C` handshakes). Not implemented. The plan is to
  capture one real update made by RØDE Central with usbmon, then build the installer from that trace.
  What is known so far:
  - Update check: `https://update.rode.com/rode-devices-manifest.json` → `wgo2-manifest.main-version`
    = `{"update-version": "2.5.0", "firmware-URL": "update.rode.com/wgo2/firmware.zip", "firmware-md5": …}`
    (as of 2026-11-10). The installed version is in USB `bcdDevice` (`0220` = 2.2.0).
  - firmware.zip (1.5 MB, MD5 71c8ca119a3d17ecf2dabe54ee8a49fb) contains, for each of tx and rx:
    `wgo2_<u>_2.50_ee7582e_ota.bin` (main MCU, Actions Semi OTA: `ACTTEST0` header, XML at 0x200 with
    board_name/version and two partitions, `mbrec.bin` boot 0x1000 + `zephyr.bin` system),
    `wgo2.5_nordic_<u>_0.26_a0a1f06.hex` (Nordic radio, Intel HEX) and `Betty<U>.spihex` (third chip, Intel HEX).
  - The same images are also embedded in rode_bridge.dll (plus older 1.93 images). The DLL has the `WGo2FirmwareUpdater` class.
- (done) TX on-board recordings: OUT report 1 `51 00` ('Q', 0) switches the TX to USB mass storage
  until it is replugged. The `.UGG` files are OggPCM S24LE 48 kHz mono, and `ugg2wav` (micdock.core.recordings) converts them.
  The `.EGG` files are peak data only.

## Firmware update (static analysis, work in progress, NOT verified on hardware)

Class `WGo2FirmwareUpdater`: vtable `0x18046ebb8`. The update runs as a timer-driven state machine at
`0x1800fa760`: state in `[this+0x48]`, jump table `0x1800fb6f4` for states 2..13. States 2/6/10/11 fall
into error state 12 after 16.5 s (`0x4074` ms).

| state | code | what it does |
|---|---|---|
| 2 | `fa81a` | find the device (`f67a0`: VID 19f7, looks for PIDs 0x5f/0x96), `'R'` (`f7a50`) |
| 3 | `fa907` | report 1 `'B'` (2 s, reply `[B, 'A', flag]`). If flag → `f7f60`, else report 1 `'Y'` (no reply) + 4 s sleep, then `f7f60` |
| 5 | `fad60` | `f8160`: report 1 `'U'` |
| 6 | `fad8d` | report 1 `'F'` |
| 7 | `fae0d` | radio chip: file selector 5 (BettyRx/WilmaFp) or 7 (BettyTx/WilmaPp) → parts → `f6620` (`'V'`) → `f6eb0` |
| 8 | `fae0d` | Nordic: selector 0/1, report 1 `'h'` (5 s) first, then `f6eb0` with flag |
| 9 | `fb30b` | main MCU: `f7570` (report 7, 0x600-byte packets, replies S/D/F/I/O/R), then `f7c50` (`'Z'`, waits for PID 0x1f) |
| 10 | `fb380` | `f8320` (`'7'`), then `'Y'` |
| 12/13 | `fb4c2` | finished / error |

Report-1 commands are 16-byte payloads `[letter, args…]`. The reply echoes the letter, with `'A'` at byte 1.
`f7f60`: `'S'` then `'O' 01` (5 s). `f6620`: `'V' ff`. `f7c50`: `'Z'`.

File selection (`f8fa0`, selector → vtable getter): 0 nordic rx, 1 nordic tx, 2 main rx (`wgo2_rx`/`wgo25_rx` + `_ota`),
3 main tx, 4 `WilmaFp`+`_NvsSector_`, 5 betty rx, 6 `WilmaPp`+`_NvsSector_`, 7 betty tx. Flags `[this+0x3b0/0x3b2/0x3b4]`
choose between the GO II / GO II 2.5 variants (0x3b2 = 2.5 hardware → `wgo25_*`, `wgo2.5_nordic_*`; Betty is used when
0x3b4 is set and 0x3b0 is clear, otherwise Wilma).

### Report-3/5 part transfer (`f6eb0`)
A part is 40 bytes: `{u32 id; u32 addr @+8; u32 size @+0xc; vector<u8> data @+0x10}`. If the address isn't
256-aligned, it is aligned down and the gap is filled with 0xFF. The data is padded with 0xFF to a multiple of 256.
1. OUT report 3, 16 bytes: `[cmd, u32 addr, u32 size, a, b, c, 0…]`, where cmd = `'N'` (flag=1, Nordic) or `'P'` (flag=0).
   The reply (12 bytes) must have `'A'` at byte 1.
2. Loop: wait (up to 15 s) for a 60-byte input `'C'`: bytes 2..5 = u32 A, 6..9 = u32 B (received), 10..13 = u32 addr (or 0).
   Credit = B + 15000 − sent − 1000. Send that many bytes as OUT report 5 (1072 bytes: u16 LE length + up to 1070 data).
3. Final (5 s): input `[cmd, 'A', u32 == id<<8]`.

### Hardware flags and the path for our units (PID 0x29 TX / 0x2a RX)
Set in state 2 by `vt[22]` (`f34d0`) from the USB device info:
- `0x3b2` ("GO II 2.5") = PID is 0x29 or 0x2a → **true for our units**.
- `0x3b3` = PID in {0x1e, 0x1f, 0x2d, 0x2e} and version < 2.0.0. `0x3b4` = same PIDs and version > 1.9.3 (older GO II).
- So for our units: main file `wgo25_<u>` + `_ota`, Nordic file `wgo2.5_nordic_<u>`, no Betty. These are in
  **`wgo2_5-manifest`** → `update.rode.com/wgo2_5/firmware.zip` (MD5 aa896183a69c26f1fb389d55228d52a2 on 2026-11-10),
  not `wgo2-manifest`.

The sequence when `0x3b2` is set (as far as it is understood):
1. state 3: `'O' 01` on report 1 (5 s). Reply `[O, 'A', BE u32 resume offset]` → `[+0x31c]`. (`'S'` is only sent for older units.)
   If the device then shows up with PID 0x6e → state 4 → 8, otherwise state 8.
2. state 8 (Nordic): report 1 `'h'` (5 s). For each part (Intel HEX → one segment at 0, ~55 KB):
   report 3 `['A', u32 addr, u32 size]` → reply `[A, 'A', u32 crc32]` (zlib CRC32 of the part data). If they match, skip it.
   Otherwise report 3 `'N'` with an empty part (addr 0, size 0, b=1), then per part `'N' addr size 01 00 00` + report-5 data
   (credit flow, see above) + final `[N, 'A', id<<8]`, then verify again.
3. state 9 (main MCU): file `wgo25_<u>_*_ota.bin`. Loop: seek to `[+0x31c]`, send 0x600 bytes as OUT report 7, read an
   8-byte input (4 s): `'A'`/`'C'` + BE u32 next offset → continue; `'S'` + status (`D`/`O`/`R` = ok, `F`/`I` = fail) → stop.
   The loop also ends OK when less than 0x600 bytes remain past the offset. Then `'Z'` (`f7c50`).
4. state 10: wait 4 s, `'7'` (`f8320`), then `'Y'` (reboot). state 11: wait for the device → done.
The input reader isn't filtered by report ID: it takes the next input report and uses its first N bytes.

Still unknown: `f89b0` (how HEX segments become parts, and the part `id`), and the exact behaviour of `'Z'`, `'7'`, `'h'`, `'R'`,
PIDs 0x1f/0x5f/0x6e/0x96 (bootloader modes?), and `[+0x354]` (unit type 0/1/2, set in `f7e80`).

### Completed analysis (2026-11-10), implemented in `src/crates/micdock-core/src/fwupdate.rs`
- Parts (`f89b0`/`f8610`): Intel HEX → maximal contiguous byte runs `[start, end)`. Part = `{id = start>>8, end>>8,
  addr = id<<8, end, data}`. The final confirm value `id<<8` is the aligned start address. The Nordic HEX is one run
  at 0 (TX 0xd21c bytes, RX 0xddf8), so it becomes one part at addr 0.
- Nordic CRC query uses command `'A'` (`'V'` is for Betty); size = data length, CRC32 = zlib. The reply CRC is LE.
- Report-5 packet: `u16 LE length` + up to 1070 data bytes (flag=1 for Nordic).
- Unit type `[+0x354]` from the PID (`f7e80`): TX = {0x1e,0x29,0x2d,…,0x6e,…} → 1, RX = {0x1f,0x2a,0x2e,…,0x70,0x61} → 0.
  So the update-mode PIDs are **0x6e (TX) and 0x70 (RX)**.
- `'Z'` (report 1, no reply) after the main image, then a 5 s sleep (old-PID units re-open as 0x2d/0x2e). `'7'` (reply `'A'`, 2 s)
  is skipped when the PID is 0x6e/0x70. `'Y'` (no reply) is then sent (reboot). On error, Central sends `'R'`.
- `[+0x361]` (`'B'` pre-check) is only ever cleared → for our units state 3 = `'O' 01` straight away.
- The OTA images are device-driven: the device requests offsets, and Central stops when < 0x600 bytes remain after the
  requested offset (the RX image has meaningful data in its last partial block, so the device must request it).

Tested against a simulator that follows this reading of the protocol (full TX/RX update, Nordic skip, resume,
device-reported failure → `'R'`, re-enumeration after `'O'`). **Not yet run against a real unit.**

### Real-hardware results (2026-11-10)
- **TX A and TX B: updated 2.2.0 → 2.5.0 successfully** with the installer (now `src/crates/micdock-core/src/fwupdate.rs`). The protocol above is confirmed:
  `'O' 01` → `[O,'A',0]`; Nordic CRC query → mismatch → empty `'N'` + `'N'` part → `[N,'A',0]` → CRC 52711e8b ✓;
  260 report-7 blocks, each answered within ~5 ms by `[41 52 BE-offset]` ('A','R' + next offset); `'Z'`;
  the unit re-enumerated with the same PID 0x29 (not 0x6e); `'7'` → `'A'`; `'Y'`; back as bcdDevice 0x0250.
- Nordic 'C' credit reports in practice: `[43 41, u32 A, u32 B, …]` where B = bytes received; the device asks for ~14000 bytes at a time.
- The RX, first try: the Nordic update succeeded (CRC verified), but the first report-7 block, sent 2 ms after
  the verify, got no answer in 4 s → `'R'` → `[R,'A']`, and the unit stayed in normal mode on main 2.20. Fix: pause like
  Central (1 s between steps, 2 s after the radio phase) and retry the whole sequence up to 3 times.

## On-board record mode (2026-11-10, firmware 2.50) — set directly on the TX
Class `WirelessRecMode`, a mix-in of `WGo2Tx` at offset 0x508. The constructor `0x1800c6c50` is called from the
WGo2Tx constructor `0x1800bf100` with param IDs 0x05, 0x0d, 0x0f. Refresh `0x1800cb710` (1 byte each):
- **0x0f** ≠ 0 → mode 3 = **Button** (start/stop with the power button)
- else **0x0d** ≠ 0 → mode 1 = **Always**
- else **0x05** ≠ 0 → mode 2 = **Backup** (name from RØDE's support article; probably "record while linked to the RX")
- else → mode 0 = **Off**.
RØDE's support article names the modes "Always, Backup, Button". The beta-firmware announcement says Button mode needs
"Record → Button Mode" *and* the Ø (power) button assigned to "Record".

Power button = `WirelessButtonMode` (mix-in at 0x590, constructor `0x1800c4ed0`, params **0x0a** and **0x04**, linked
to the RecMode object). Its decoder `0x1800cb8a0` (GO II variant): record mode 3 (+ a feature flag) → button type 3 = Record;
else 0x0a == 1 → type 1; else 0x04 == 0 → type 2; else type 0. The app names Mute and Marker (and presumably None) for types 0–2;
which is which has not been mapped yet. Default on our TXs: 0x0a = 0, 0x04 = 1 → type 0.

Button-mode setter (`0x1800c97f0`, arg = type): starts from 0x0a = 0, 0x04 = 1; Marker → 0x0a = 1; None → 0x04 = 0; Record →
record mode 3; any other type while the record mode is 3 → record mode 0 (Off). The decoder also gives Mute = the default (0x0a 0, 0x04 1),
which matches the user guide ("Mute … is the default function").
User guide (rode.com/user-guides/wireless-go-ii): "To activate this mode, configure the 'Ø Button' setting … to 'Record' – this
automatically enables 'Button' mode recording." The LEDs cycle dim→strong on start and flash every 5 s while recording.
Always and Button modes always record uncompressed; compressed recording is only available in Backup mode.

**Verified on hardware (2026-11-10):** Off (the RX indicator changed); Button (0x05 = 1, 0x0f = 1, 0x0a = 0, 0x04 = 1): the power button
starts and stops recording on TX mic2.

Correction: an earlier guess said the switch was RX params 0x06/0x14. That was wrong. Those writes were made on the RX
(0x06 → 0, 0x14 → 1) and then restored to 0x06 = 01, 0x14 = 00. A TX NAKs changes to its own 0x06; that param is something else.

## Receiver Merged / Split output (2026-11-10, firmware 2.50) — RX param 0x06
**RX param 0x06 (1 byte): 1 = Merged, 0 = Split.** Found with scan diffs while switching on the receiver itself (both
buttons held 3 s): Split → 0x06 = 00, Merged → 0x06 = 01, nothing else changed. Writing it switches the receiver at once
(verified: `set 0x06 00` → two level meters; also round-tripped from the app). This explains the "RX indicator changed" seen
during the record-mode guess above: that write had switched the receiver to Split.
RØDE Central has no such setting for the GO II: its `setChannelMode` export (`0x180146d80`) writes a per-device model value
(+0x2230) that is only bound to the AI-Micro (`AiMicro::ChannelModeType`, binding at `0x1801274e8`); the Wireless PRO RX
has a separate `RoutingType`. The GO II user guide only describes the button combination.
CLI: `rodego2 --dev <RX> mode [merged|split]`. App: receiver → Audio → Merged & Split Mode.

## On-board recording files, firmware 2.50 (2026-11-10)
After the update to 2.50, new recordings are `RECnnnnn.WAV` (numbering continues from the UGG files; old UGGs stay).
RIFF/WAVE, PCM tag 1, 1 ch, 48000 Hz, 24-bit (block align 3). Chunk order: `fmt ` (16), `cue ` (4 + 24·n; n = 0 when no
markers were set), `PAD ` (zeros, so `data` starts at offset 32768), `data`. No date/time or session data in the file;
the FAT mtime is the end of the recording. Full split pieces hold 89 304 112 frames (1860.5 s, data 267 912 336 bytes).
`PEAnnnnn.EGG` peak files are still written alongside.

## Deleting recordings (2026-11-10)
The TX drive is **write-protected towards the host**: in storage mode `/sys/block/sdX/ro` = 1 and udisks reports
`ReadOnly: true`, so no OS can delete single files. RØDE Central offers only "Delete All" ("This Will Erase
Everything"): export `formatTxMemory` → class `WGo2DiskFormatter`, state machine `0x1800d6250` (500 ms ticks,
20 s overall timeout):
1. check `0x1800d60a0`: the unit is connected and its USB PID is a TX (0x1e, 0x29, 0x2d, …); else fail;
2. **OUT report 1 `4A 01` ('J' 01)**, 8 bytes; read input reports for up to 4 s until one starts with `'J'`;
3. wait 4 s, or until the unit has dropped off USB and come back (it may restart); done.
MicDock: `hid.erase_recordings` / `devices.erase_tx_recordings` (unmounts the drive first), `rodego2 erase`,
Recordings page → *Delete all…*. **Not yet run on hardware** (it erases the user's recordings): the first real
run is the test.

## DANGER: report 1 'Q' 01 erased both transmitters (2026-11-10)
RØDE Central's `setMassStorageState(state)` (`0x1800cae50`) sends report 1 `'Q', state ^ 1`: **'Q' 00 = storage mode
on**. 'Q' 01 ("off") was sent to both TXs while they were in storage mode, to try to end it without unplugging. Both
ACKed (`51 41`), stayed in storage mode, and **afterwards their memory was empty** (FAT freshly formatted: 4 KB used
of 3.7 GB) and their record mode read *off* (TX A was Always, TX B Button) — consistent with a factory reset.
TX A lost 19 files, TX B its recordings since 09:51. (Earlier, 'Q' 01 sent in *normal* mode did nothing visible.)
**Never send 'Q' 01.** Storage mode only ends when the TX is unplugged; `udisksctl power-off` detaches the USB
device but the TX stays in storage mode.

## Is a unit on? (2026-11-12, read-only test)

Scans (GET 0x00–0x60) with everything off, with RX + TX1 on and linked, and with only the RX on:

- A unit that is switched off still answers every GET over USB with the same values: a TX's and the RX's
  own replies don't show power at all (only the clock moves). Nothing is sent unasked either.
- While a TX is on and linked, the RX answers **0x0b** (TX1) / **0x0c** (TX2) with a 28-byte record and the
  TX's name in **0x0f** / **0x10**; 0x0d, 0x16 and 0x1e appear as well. With no TX linked these are refused
  at once (status 0x4e). Record: bytes 0–3 = the TX's USB serial (u32 LE, printed `%08X`), 4–7 = the TX's
  param 0x02, byte 22 = battery % (0x64 when the TX read 100 %), bytes 15/17 possibly "recording now".
- So "linked to the receiver" is the only on/off signal; while the RX is off no TX looks linked.


## No power on/off command (2026-11-12, static analysis of rode_bridge.dll)

- None of the DLL's 198 exports switches a unit on or off. `setSuspended` checks an app-wide flag;
  `diagnosticsCheckPowerButton` is a diagnostics step (press the button); `setTimeout` sets the feature class
  `WirelessTimeoutEnabled`, which only `WirelessTxThruDevice` has (at offset 0x960), not the GO II.
- The GO II classes, from RTTI (feature mix-ins with their offsets):
  - `WGo2Tx`: WirelessRecMode@0x508, WirelessButtonMode@0x590, WirelessLavGain@0x640, WirelessLEDsBrightness@0x6c8
  - `WGo2Rx`: WirelessGainMode@0x3d0, WirelessBackLight@0x848, WirelessButtonMode@0x9c0, WirelessGainPresets@0xa70,
    WirelessSafetyChannel@0xc48
- So RØDE Central (desktop) can't power a GO II on or off; the guide's "manually powering on and off" is
  probably the mobile app or the hardware button. Not yet in MicDock and possible later: TX LED brightness and
  lav gain, RX backlight (power saver), gain presets, safety channel.
- Work files: `~/.cache/micdock-re/` (extracted MSI, `bridge.dll`, `bridge.asm` from objdump).

## Receiver power saver and output gain (verified 2026-11-12)

Found by changing them on the receiver itself and diffing read-only scans, then written and read back:

- **RX 0x04**: 0 = power saver on (backlight off after 10 s), 1 = backlight always on. The power button's
  single click toggles it. A write while the screen was dark was ACKed but read back 0 once; the second write
  stuck, so the setter reads back and retries once.
- **RX 0x05** = `[mode, step]`. Mode 1 = fine gain: dB = −30 + 3 × step (0 = −30, 1 = −27, 5 = −15 dB, seen on
  screen; 10 = 0 dB inferred). Written `01 05` → screen showed −15 dB. Other modes (coarse 0/−12/−24 dB?) not
  mapped: MicDock only writes in fine mode.
- From the DLL (constructor arguments in `WGo2Tx` at 0x1800bf1da): **TX 0x0c** = LED brightness (RØDE's default
  0xdf, also what the TX reads), **TX 0x0e** = lav gain; value meanings unknown (they come from the Flutter UI).
  RX features without fixed IDs in their constructors: backlight, gain presets, safety channel; SET handlers
  with fixed IDs exist for 0x05 (`[1|2, value]`), 0x07, 0x18 and 0x19 (value 2 = not sent).


## Every report 1 command in rode_bridge.dll (2026-11-12, static; nothing sent)

All callers of the request helper `0x1800c0c90` and the HID write `0x180112cc0`, with the class that owns them
(walking callers up to a vtable):

| cmd | where | owner | what it looks like |
|---|---|---|---|
| 'Q' 00/01 | 0x1800cae50 | WirelessDevice | storage mode on/off — **'Q' 01 erased recordings, never send** |
| 'J' 01 | 0x1800d6250 | WGo2DiskFormatter | delete all recordings |
| (report write) | 0x1800f0bc0 | FactoryResetDesktop | factory reset — not analysed, never send |
| 'B' | 0x1800f6b70 | WirelessDevice | query (bootloader/flag), also used by the updater |
| 'N' | 0x1800f7930 | WirelessDevice (all models) | query, reply checked for 'A' |
| 'w', 'v', '8', '6' | 0x1800f64b0 / 6950 / 6a60 | WGo2Checker (diagnostics), storage-mode lambda | queries: reply `[cmd, 'A', flag]` |
| 'O' 'S' 'U' 'R' 'Z' 'Y' 'F' 'h' '7' 'T' '5' 'V' | 0x1800f7…–0x1800fd… | *FirmwareUpdater classes | firmware update steps |
| 'a', 'b' | 0x1800f6320 | no GO II owner found | — |
| 'E' 'P' 'I' 'W' 'U' 'R' (9-byte reports) | 0x180100e30… | no GO II owner found (other products) | — |

No command switches a unit on or off. Report 1 commands are the dangerous class (see 'Q' 01): none of the
unexplained ones may be sent without the user's explicit go-ahead.

## RØDE Central Mobile (Android 2.2.0) — no power command either (2026-11-12, static)

APK from a public mirror (XAPK, armeabi-v7a split), kept in `~/.cache/micdock-re/apk/`. Nothing was run.

- `librode_bridge.so` is the same bridge as the desktop DLL. Its only extra exports are Bluetooth ones for the
  NTH-Wireless headphones (`bluetooth*`, `setBluetooth*`, `nthWireless*`, `enterNthwPairing`, `classic*`) and
  `dartSend`. Power-related exports: only `setSuspended` and `setTimeout`, like on the desktop.
- Itanium RTTI (`_ZTI` symbols + relocations): `WGo2Tx` = WirelessTxDevice + RecMode, ButtonMode, LavGain,
  LEDsBrightness; `WGo2Rx` = WirelessRxDevice + ButtonMode, GainPresets, BackLight, SafetyChannel. Identical to
  the desktop: no `WirelessTimeoutEnabled` ("Enable or disable automatic power-off."), no plug-in detect
  ("the receiver will automatically turn on and off along with the camera" — other receivers).
  `WGo2BackUp` (mobile only) is a `juce::TimeSliceClient` (background job, probably copying recordings).
- The Dart UI's device controls (`package:rode_central/device_controls/controls/…`): backlight_status,
  button_mode, lav_gain, led_brightness, onboard_recording, output_gain_mode, temporary_mass_storage, timeout,
  plug_in_detect, tx_pairing, … — none switches power. "PowerOff"/"WakeUp"/"Sleep"/"Standby" strings exist but
  belong to no GO II class.
- Conclusion: neither RØDE app can switch a Wireless GO II on or off; the guide's "manually powering on and off"
  most likely means the power button itself.

## Features of RØDE Central Mobile tested on the GO II (2026-11-12, with the user watching the units)

- **RX 0x05 gain modes** `[mode, step]`: 0 = coarse, steps 0/1/2 = −24/−12/0 dB (screen: level triangle, fuller =
  louder, read back after button presses); 1 = fine, dB = −30 + 3·step (all 3 dB steps exist, user checked);
  2 = camera preset, step 0 showed the default preset name ("WIGO2", also in RX 0x08 = `00 06 'WIGO2'`).
- **TX 0x0e lav gain / low-sensitivity pad**: 1 = normal, 0 = pad. Written over USB → the receiver showed the pad
  symbol for that TX at once, and it went away when 1 was written back. Switching the pad on the receiver
  (pair button + long press on dB) changed **byte 21** of that TX's RX record (01 → 00); a USB write doesn't
  update the record until the TX links again.
- **RX records mirror TX settings**: byte 14 = TX 0x04, 15 = TX 0x05 (record enable), 17 = TX 0x0d (Always),
  18 = TX 0x0c (LED value), 21 = TX 0x0e (lav gain), 22 = battery. (Bytes 15/17 are not "recording now".)
  Linking a TX changed none of the TX's own params.
- **TX 0x0c LED brightness — not solved.** 0xdf is RØDE's "default/unknown" marker (the DLL maps 0/1/2/3 of
  another model's 0x3d to 0xdf/0xfe/0x01/0x02). Written values persist and show in RX record byte 18. 0xff switched
  the LEDs off at once; writing other values didn't relight them until a power cycle; 0x00, 0x01, 0x02 all looked
  normal after a power cycle. RØDE's "Dim" value is unknown. Restored to 0xdf.
- **RX 0x07 safety channel — not confirmed.** ACKed, no screen change. Measured over the RX's USB audio in Merged
  mode: L−R 6.5 dB with 0x07 = 0, 8.5 dB with 0x07 = 1 (expected ~20 dB): it may only affect the 3.5 mm output.
  Restored to 0. Not in MicDock.
- **Unexplained**: TX A's record mode and button changed (Always/None → Off/Mute) some time after it had been in
  USB storage mode, without a write from us or the user. Watch for it.

## Reading a TX over USB blocks its power button (2026-11-12, tested with the user)

With MicDock open, the TX's Ø/power button didn't work over USB (it did with MicDock closed, and on a charger).
Tests with MicDock closed, reading the TX from the command line every 3 s while the user pressed the button:

| reads every 3 s | short press | long press (off) |
|---|---|---|
| battery 0x00 only | works | works |
| name 0x03 only | — | works |
| full probe (≈10 GETs, 0.26 s) | works | **fails** |
| 0x0a + 0x04 + 0x0e | — | **fails** |
| 0x0a only | — | **fails** |
| 0x0a + 0x0e | — | **fails** |
| 0x00 + 0x01 + 0x02 + 0x03 | works | **fails** |

So it is not one parameter: HID traffic during the 2–3 s hold cancels a long press (single short reads only
rarely hit a hold). MicDock now reads transmitters only every 30 s and when needed (unit/tab selected, after a
write, new unit); the receiver (every 3 s) supplies on/off and battery of linked TXs. With that, short and long
presses work while MicDock is open. The receiver's buttons work while it is read every 3 s.
The receiver's own on/off state still can't be read: with no TX linked at all, MicDock says "not linked"
instead of "off".

