// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! Wireless GO II HID access over Linux hidraw: devices, the settings protocol (report 9 GET/SET)
//! and the decoded settings (record mode, power-button function, merged/split output).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::{secs, Error, Result};

pub const RODE_VID: u16 = 0x19F7;
pub const SETTINGS_REPORT_ID: u8 = 9;
pub const ACK: u8 = 0x41;
/// RØDE Central uses 2000 ms for reads...
pub const GET_TIMEOUT: f64 = 2.0;
/// ...and 250 ms for writes; a little slack here.
pub const SET_TIMEOUT: f64 = 0.5;

// --------------------------------------------------------------------------
// device discovery
// --------------------------------------------------------------------------

/// Direction of a HID report, as declared by the report descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReportKind {
    Feat,
    In,
    Out,
}

impl ReportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::In => "in",
            ReportKind::Out => "out",
            ReportKind::Feat => "feat",
        }
    }
}

/// Report sizes in bytes (without the report-id byte), by (kind, report id).
pub type ReportSizes = BTreeMap<(ReportKind, u8), usize>;

fn read_bytes(path: impl AsRef<Path>) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

/// Parse a HID report descriptor: the size of every report, and whether report ids are used.
pub fn parse_report_sizes(desc: &[u8]) -> (ReportSizes, bool) {
    #[derive(Clone, Copy, Default)]
    struct Globals {
        size: u32,
        count: u32,
        id: u32,
    }
    let mut bits: BTreeMap<(ReportKind, u8), u64> = BTreeMap::new();
    let mut stack: Vec<Globals> = Vec::new();
    let mut g = Globals::default();
    let mut uses_ids = false;
    let mut i = 0usize;
    while i < desc.len() {
        let p = desc[i];
        if p == 0xFE {
            // long item
            i += 3 + desc.get(i + 1).copied().unwrap_or(0) as usize;
            continue;
        }
        let n = [0usize, 1, 2, 4][(p & 3) as usize];
        let mut val: u32 = 0;
        for (k, b) in desc.iter().skip(i + 1).take(n).enumerate() {
            val |= (*b as u32) << (8 * k);
        }
        let (typ, tag) = ((p >> 2) & 3, p >> 4);
        match typ {
            1 => match tag {
                // global items
                7 => g.size = val,
                9 => g.count = val,
                8 => {
                    g.id = val;
                    uses_ids = true;
                }
                10 => stack.push(g),
                11 => {
                    if let Some(top) = stack.pop() {
                        g = top;
                    }
                }
                _ => {}
            },
            0 if matches!(tag, 8 | 9 | 11) => {
                // main items: Input / Output / Feature
                let kind = match tag {
                    8 => ReportKind::In,
                    9 => ReportKind::Out,
                    _ => ReportKind::Feat,
                };
                *bits.entry((kind, g.id as u8)).or_insert(0) += g.size as u64 * g.count as u64;
            }
            _ => {}
        }
        i += 1 + n;
    }
    (
        bits.into_iter()
            .map(|(k, v)| (k, v.div_ceil(8) as usize))
            .collect(),
        uses_ids,
    )
}

/// A RØDE hidraw node, as described by sysfs (not opened yet).
#[derive(Debug, Clone)]
pub struct DevInfo {
    pub node: String,
    pub vid: u16,
    pub pid: u16,
    pub name: String,
    pub sizes: ReportSizes,
    pub uses_ids: bool,
}

impl DevInfo {
    /// Read what sysfs knows about a hidraw node such as `/dev/hidraw3`.
    pub fn from_node(node: &str) -> DevInfo {
        let base = format!(
            "/sys/class/hidraw/{}/device",
            Path::new(node)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let uevent = String::from_utf8_lossy(&read_bytes(format!("{base}/uevent"))).into_owned();
        let mut hid_id = "0:0:0".to_string();
        let mut name = "?".to_string();
        for line in uevent.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "HID_ID" => hid_id = v.to_string(),
                    "HID_NAME" => name = v.to_string(),
                    _ => {}
                }
            }
        }
        let mut parts = hid_id
            .split(':')
            .skip(1)
            .map(|s| u32::from_str_radix(s, 16).unwrap_or(0));
        let vid = parts.next().unwrap_or(0) as u16;
        let pid = parts.next().unwrap_or(0) as u16;
        let (sizes, uses_ids) =
            parse_report_sizes(&read_bytes(format!("{base}/report_descriptor")));
        DevInfo {
            node: node.to_string(),
            vid,
            pid,
            name,
            sizes,
            uses_ids,
        }
    }

    pub fn size(&self, kind: ReportKind, id: u8) -> Option<usize> {
        self.sizes.get(&(kind, id)).copied()
    }

    /// Payload length of OUT report 9 (excluding the report id byte).
    pub fn out_len(&self) -> usize {
        self.size(ReportKind::Out, SETTINGS_REPORT_ID)
            .filter(|&n| n > 0)
            .unwrap_or(63)
    }

    pub fn has_settings_report(&self) -> bool {
        self.sizes
            .contains_key(&(ReportKind::Out, SETTINGS_REPORT_ID))
    }

    /// Open the node for reading and writing (non-blocking).
    pub fn open(&self) -> Result<Dev> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.node)?;
        Ok(Dev {
            info: self.clone(),
            file,
        })
    }
}

/// Every RØDE (VID 19f7) hidraw node, sorted by node name.
pub fn find_devices() -> Vec<DevInfo> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("hidraw"))
                .map(|n| format!("/dev/{n}"))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes
        .into_iter()
        .map(|n| DevInfo::from_node(&n))
        .filter(|d| d.vid == RODE_VID)
        .collect()
}

/// An opened hidraw node.
pub struct Dev {
    pub info: DevInfo,
    file: File,
}

impl Dev {
    pub fn pid(&self) -> u16 {
        self.info.pid
    }

    fn wait_readable(&self, timeout: Duration) -> Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd for the duration of the call.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e.into());
        }
        Ok(r > 0)
    }

    /// Throw away input reports that are already waiting.
    pub fn drain(&mut self) {
        let mut buf = [0u8; 4096];
        while matches!(self.wait_readable(Duration::ZERO), Ok(true)) {
            if self.file.read(&mut buf).is_err() {
                break;
            }
        }
    }

    /// Send OUT report `id`, zero-padded to its length from the descriptor (`default_len` if unknown).
    pub fn write_report(&mut self, id: u8, payload: &[u8], default_len: usize) -> Result<()> {
        let n = self.info.size(ReportKind::Out, id).unwrap_or(default_len);
        let mut buf = Vec::with_capacity(n + 1);
        buf.push(id);
        buf.extend_from_slice(&payload[..payload.len().min(n)]);
        buf.resize(n + 1, 0);
        self.file.write_all(&buf).map_err(|e| self.gone_or(e))
    }

    pub fn write9(&mut self, payload: &[u8]) -> Result<()> {
        let n = self.info.out_len();
        let mut buf = Vec::with_capacity(n + 1);
        buf.push(SETTINGS_REPORT_ID);
        buf.extend_from_slice(&payload[..payload.len().min(n)]);
        buf.resize(n + 1, 0);
        self.file.write_all(&buf).map_err(|e| self.gone_or(e))
    }

    fn gone_or(&self, e: std::io::Error) -> Error {
        match e.raw_os_error() {
            Some(libc::EIO) | Some(libc::ENODEV) => Error::DeviceGone(format!(
                "{} disappeared (device reset or unplugged)",
                self.info.node
            )),
            _ => e.into(),
        }
    }

    /// The next input report as (report id, data), or None after `timeout`.
    pub fn read_report(&mut self, timeout: Duration) -> Result<Option<(u8, Vec<u8>)>> {
        if !self.wait_readable(timeout)? {
            return Ok(None);
        }
        let mut buf = vec![0u8; 4096];
        let n = match self.file.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(self.gone_or(e)),
        };
        buf.truncate(n);
        if self.info.uses_ids && !buf.is_empty() {
            let id = buf.remove(0);
            Ok(Some((id, buf)))
        } else {
            Ok(Some((0, buf)))
        }
    }
}

// --------------------------------------------------------------------------
// protocol
// --------------------------------------------------------------------------

/// Receives every input report seen while waiting for a reply (for `-v`).
pub type Trace<'a> = Option<&'a mut dyn FnMut(u8, &[u8])>;

/// Send a report-9 payload and wait for the reply whose first byte echoes `payload[0]`; return its data.
/// `trace` receives every input report seen (for `-v`).
pub fn transact(dev: &mut Dev, payload: &[u8], timeout: f64, trace: Trace) -> Result<Vec<u8>> {
    let mut trace = trace;
    dev.drain();
    dev.write9(payload)?;
    let end = Instant::now() + secs(timeout);
    loop {
        let now = Instant::now();
        if now >= end {
            return Err(Error::Timeout(format!(
                "no reply to param 0x{:02x}",
                payload[0]
            )));
        }
        let Some((rid, data)) = dev.read_report(end - now)? else {
            continue;
        };
        if let Some(t) = trace.as_mut() {
            t(rid, &data);
        }
        if data.first() == Some(&payload[0]) {
            if data.len() < 2 || data[1] != ACK {
                let status = data.get(1).map(|b| *b as i32).unwrap_or(-1);
                return Err(Error::NoAck(format!(
                    "param 0x{:02x}: status 0x{:02x}",
                    payload[0], status
                )));
            }
            return Ok(data[2..].to_vec());
        }
    }
}

pub fn get_param(dev: &mut Dev, param: u8) -> Result<Vec<u8>> {
    get_param_ex(dev, param, 0, GET_TIMEOUT, None)
}

pub fn get_param_ex(
    dev: &mut Dev,
    param: u8,
    sub: u8,
    timeout: f64,
    trace: Trace,
) -> Result<Vec<u8>> {
    transact(dev, &[param, 0x01, sub], timeout, trace)
}

pub fn set_param(dev: &mut Dev, param: u8, data: &[u8]) -> Result<Vec<u8>> {
    set_param_ex(dev, param, data, 0, None)
}

pub fn set_param_ex(
    dev: &mut Dev,
    param: u8,
    data: &[u8],
    sub: u8,
    trace: Trace,
) -> Result<Vec<u8>> {
    let mut payload = vec![param, sub];
    payload.extend_from_slice(data);
    transact(dev, &payload, SET_TIMEOUT, trace)
}

fn first_byte(dev: &mut Dev, param: u8) -> Result<u8> {
    Ok(get_param(dev, param)?.first().copied().unwrap_or(0))
}

// TX on-board record mode = RØDE Central's WirelessRecMode (refresh 0x1800cb710):
//   0x0f != 0 → button (start/stop with the power button; the button's function becomes "Record")
//   0x0d != 0 → always (records whenever the TX is on — from the next power-on)
//   0x05 != 0 → backup (records while linked to the receiver)
//   else      → off

/// TX on-board record mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecMode {
    Off,
    Backup,
    Always,
    Button,
}

impl RecMode {
    pub const ALL: [RecMode; 4] = [
        RecMode::Off,
        RecMode::Backup,
        RecMode::Always,
        RecMode::Button,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RecMode::Off => "off",
            RecMode::Backup => "backup",
            RecMode::Always => "always",
            RecMode::Button => "button",
        }
    }

    pub fn parse(s: &str) -> Option<RecMode> {
        RecMode::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// Values of params (0x05 enabled, 0x0d always, 0x0f button).
    fn flags(self) -> (u8, u8, u8) {
        match self {
            RecMode::Off => (0, 0, 0),
            RecMode::Backup => (1, 0, 0),
            RecMode::Always => (1, 1, 0),
            RecMode::Button => (1, 0, 1),
        }
    }
}

pub fn rec_mode(d: &mut Dev) -> Result<RecMode> {
    let enabled = first_byte(d, 0x05)?;
    let always = first_byte(d, 0x0D)?;
    let button = first_byte(d, 0x0F)?;
    Ok(if button != 0 {
        RecMode::Button
    } else if always != 0 {
        RecMode::Always
    } else if enabled != 0 {
        RecMode::Backup
    } else {
        RecMode::Off
    })
}

pub fn set_rec_mode(d: &mut Dev, mode: RecMode) -> Result<RecMode> {
    let (enabled, always, button) = mode.flags();
    if button != 0 {
        // the power button must be in its plain state for Record (WirelessButtonMode type 3)
        set_param(d, 0x0A, &[0])?;
        set_param(d, 0x04, &[1])?;
    }
    // clear the flags we're leaving first, then set the new ones
    for (p, v) in [(0x0F, button), (0x0D, always)] {
        if v == 0 {
            set_param(d, p, &[0])?;
        }
    }
    set_param(d, 0x05, &[enabled])?;
    for (p, v) in [(0x0D, always), (0x0F, button)] {
        if v != 0 {
            set_param(d, p, &[v])?;
        }
    }
    rec_mode(d)
}

// TX power (Ø) button function = RØDE Central's WirelessButtonMode (params 0x0a, 0x04; decoder 0x1800cb8a0):
//   record: record mode is "button"   marker: 0x0a = 1   none: 0x04 = 0   mute (default): 0x0a = 0, 0x04 = 1

/// TX power-button function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonMode {
    Mute,
    Marker,
    None,
    Record,
}

impl ButtonMode {
    pub const ALL: [ButtonMode; 4] = [
        ButtonMode::Mute,
        ButtonMode::Marker,
        ButtonMode::None,
        ButtonMode::Record,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ButtonMode::Mute => "mute",
            ButtonMode::Marker => "marker",
            ButtonMode::None => "none",
            ButtonMode::Record => "record",
        }
    }

    pub fn parse(s: &str) -> Option<ButtonMode> {
        ButtonMode::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

pub fn button_mode(d: &mut Dev) -> Result<ButtonMode> {
    if first_byte(d, 0x0F)? != 0 {
        return Ok(ButtonMode::Record);
    }
    if first_byte(d, 0x0A)? == 1 {
        return Ok(ButtonMode::Marker);
    }
    Ok(if first_byte(d, 0x04)? != 0 {
        ButtonMode::Mute
    } else {
        ButtonMode::None
    })
}

pub fn set_button_mode(d: &mut Dev, mode: ButtonMode) -> Result<ButtonMode> {
    if mode == ButtonMode::Record {
        set_rec_mode(d, RecMode::Button)?;
        return button_mode(d);
    }
    set_param(d, 0x0A, &[(mode == ButtonMode::Marker) as u8])?;
    set_param(d, 0x04, &[(mode != ButtonMode::None) as u8])?;
    if rec_mode(d)? == RecMode::Button {
        // like RØDE Central: leaving 'Record' turns Button recording off
        set_rec_mode(d, RecMode::Off)?;
    }
    button_mode(d)
}

// RX output mode (verified on hardware 2026-11-10, firmware 2.50): param 0x06 = 1 merged, 0 split.
// Same as holding both receiver buttons for 3 s. RØDE Central has no such setting for the GO II.

/// Receiver output: both transmitters mixed, or one per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    Merged,
    Split,
}

impl ChannelMode {
    pub const ALL: [ChannelMode; 2] = [ChannelMode::Merged, ChannelMode::Split];

    pub fn as_str(self) -> &'static str {
        match self {
            ChannelMode::Merged => "merged",
            ChannelMode::Split => "split",
        }
    }

    pub fn parse(s: &str) -> Option<ChannelMode> {
        ChannelMode::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

pub fn channel_mode(d: &mut Dev) -> Result<ChannelMode> {
    Ok(if first_byte(d, 0x06)? != 0 {
        ChannelMode::Merged
    } else {
        ChannelMode::Split
    })
}

pub fn set_channel_mode(d: &mut Dev, mode: ChannelMode) -> Result<ChannelMode> {
    set_param(d, 0x06, &[(mode == ChannelMode::Merged) as u8])?;
    refresh(d)?;
    channel_mode(d)
}

/// RX power saver (param 0x04, verified 2026-11-12): 0 = on (the backlight goes off after 10 s),
/// 1 = off (backlight always on). The receiver's power button toggles the same value.
pub fn power_saver(d: &mut Dev) -> Result<bool> {
    Ok(first_byte(d, 0x04)? == 0)
}

/// Set the RX power saver. The first write of a test was ACKed but did not stick (the screen was dark), so
/// the value is read back and the write repeated once if needed.
pub fn set_power_saver(d: &mut Dev, on: bool) -> Result<bool> {
    for _ in 0..2 {
        set_param(d, 0x04, &[(!on) as u8])?;
        refresh(d)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        if power_saver(d)? == on {
            break;
        }
    }
    power_saver(d)
}

/// RX output gain (param 0x05, verified 2026-11-12 on the receiver's screen): `[mode, step]`.
/// - mode 0 = coarse: step 0, 1, 2 = −24, −12, 0 dB (the screen shows a level triangle);
/// - mode 1 = fine: dB = −30 + 3 × step, step 0–10;
/// - mode 2 = camera preset: step = preset, 0 = RØDE's default "WIGO2" (the screen shows the preset name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gain {
    pub mode: u8,
    pub step: u8,
}

pub const COARSE_GAIN: u8 = 0;
pub const FINE_GAIN: u8 = 1;
pub const PRESET_GAIN: u8 = 2;
pub const COARSE_GAIN_STEPS: u8 = 2;
pub const FINE_GAIN_STEPS: u8 = 10;

impl Gain {
    /// The output level in dB (coarse and fine mode).
    pub fn db(self) -> Option<i32> {
        match self.mode {
            COARSE_GAIN if self.step <= COARSE_GAIN_STEPS => Some(-24 + 12 * self.step as i32),
            FINE_GAIN if self.step <= FINE_GAIN_STEPS => Some(-30 + 3 * self.step as i32),
            _ => None,
        }
    }

    /// The highest step of this mode (None: unknown mode).
    pub fn max_step(mode: u8) -> Option<u8> {
        match mode {
            COARSE_GAIN => Some(COARSE_GAIN_STEPS),
            FINE_GAIN => Some(FINE_GAIN_STEPS),
            PRESET_GAIN => Some(0), // only the default preset is known
            _ => None,
        }
    }

    /// The same level in another mode (nearest step), for switching modes.
    pub fn converted(self, mode: u8) -> Gain {
        let db = self.db().unwrap_or(-12);
        let step = match mode {
            COARSE_GAIN => ((db + 24 + 6).div_euclid(12)).clamp(0, 2) as u8,
            FINE_GAIN => ((db + 30 + 1).div_euclid(3)).clamp(0, 10) as u8,
            _ => 0,
        };
        Gain { mode, step }
    }
}

pub fn gain(d: &mut Dev) -> Result<Gain> {
    let v = get_param(d, 0x05)?;
    Ok(Gain {
        mode: v.first().copied().unwrap_or(0),
        step: v.get(1).copied().unwrap_or(0),
    })
}

/// Set the output gain (mode and step), only with the values listed at [`Gain`].
pub fn set_gain(d: &mut Dev, g: Gain) -> Result<Gain> {
    match Gain::max_step(g.mode) {
        Some(max) if g.step <= max => {}
        _ => return Err(Error::other("gain value out of range")),
    }
    set_param(d, 0x05, &[g.mode, g.step])?;
    refresh(d)?;
    gain(d)
}

/// TX lav gain / low-sensitivity pad (param 0x0e, verified 2026-11-12: the receiver showed the pad symbol):
/// 1 = normal, 0 = pad. The receiver's long press on dB (with the TX selected) switches the same setting.
pub fn pad(d: &mut Dev) -> Result<bool> {
    Ok(first_byte(d, 0x0e)? == 0)
}

pub fn set_pad(d: &mut Dev, on: bool) -> Result<bool> {
    set_param(d, 0x0e, &[(!on) as u8])?;
    pad(d)
}

/// RX safety channel (param 0x07, 0/1): in Merged mode a second channel 20 dB quieter, for the camera's
/// 3.5 mm input. Verified 2026-11-13 that the RX takes and reports it; on the USB audio both channels stay
/// identical, so its effect is on the 3.5 mm output only.
pub fn safety_channel(d: &mut Dev) -> Result<bool> {
    Ok(first_byte(d, 0x07)? != 0)
}

pub fn set_safety_channel(d: &mut Dev, on: bool) -> Result<bool> {
    set_param(d, 0x07, &[on as u8])?;
    refresh(d)?;
    safety_channel(d)
}

/// What a single click on the receiver's power button does (param 0x0a, verified 2026-11-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxButton {
    /// Switch the power saver (backlight) on or off — the default.
    PowerSaver,
    /// Drop a marker into the transmitters' recordings.
    Marker,
}

impl RxButton {
    pub fn as_str(self) -> &'static str {
        match self {
            RxButton::PowerSaver => "powersaver",
            RxButton::Marker => "marker",
        }
    }

    pub fn parse(s: &str) -> Option<RxButton> {
        [RxButton::PowerSaver, RxButton::Marker]
            .into_iter()
            .find(|b| b.as_str() == s)
    }
}

pub fn rx_button(d: &mut Dev) -> Result<RxButton> {
    Ok(if first_byte(d, 0x0a)? != 0 {
        RxButton::Marker
    } else {
        RxButton::PowerSaver
    })
}

pub fn set_rx_button(d: &mut Dev, b: RxButton) -> Result<RxButton> {
    set_param(d, 0x0a, &[(b == RxButton::Marker) as u8])?;
    refresh(d)?;
    rx_button(d)
}

/// RØDE Central's "refresh" (`09 1a 00`): sent after every change and periodically. A setting written over
/// the radio link (see [`radio_write`]) only reaches the transmitter after it.
pub fn refresh(d: &mut Dev) -> Result<()> {
    set_param(d, 0x1a, &[])?;
    Ok(())
}

// --------------------------------------------------------------------------
// Transmitter settings over the receiver's radio link (verified on hardware 2026-11-13).
//
// Report 9 `[0x0d, reg, value]` writes a register of the TX linked as TX1, `[0x0e, reg, value]` of TX2
// (0x0e is not in RØDE Central's traffic — its capture had one TX — but tested: it reached TX2 only). Then
// `09 1a 00` (refresh). It works with or without the TX on USB. Registers (values as on our units, several
// are the reverse of what an earlier reverse-engineering project assumed):
//   0x08 pad: 1 = normal, 0 = low-sensitivity pad      0x1a LEDs: 0xDF bright, 0xFE dim
//   record mode + button, always written together, closed by 0x1e = 1 ("apply"):
//   0x1b Always · 0x04 recording on (Always/Backup) · 0x21 Manual (button records) · 0x1d button = marker ·
//   0x17 button active (Mute unless marker/record) · 0x18 1 = uncompressed (only used in Backup) · 0x1e 1
// The TX clock register (0x06) didn't take on our units; clocks are set over USB.
// --------------------------------------------------------------------------

/// The report-9 opcode that writes to the TX in `slot` (1 or 2).
pub fn radio_op(slot: u8) -> Option<u8> {
    match slot {
        1 => Some(0x0d),
        2 => Some(0x0e),
        _ => None,
    }
}

/// Write one register of the TX linked in `slot` (without the refresh).
pub fn radio_write(d: &mut Dev, slot: u8, reg: u8, value: u8) -> Result<()> {
    let op = radio_op(slot).ok_or_else(|| Error::other("TX slot must be 1 or 2"))?;
    transact(d, &[op, reg, value], SET_TIMEOUT, None)?;
    Ok(())
}

/// The registers RØDE Central writes for a record mode, button function and recording format.
pub fn radio_settings(rec: RecMode, button: ButtonMode, uncompressed: bool) -> [(u8, u8); 7] {
    // Manual recording and the button's Record function go together
    let manual = rec == RecMode::Button || button == ButtonMode::Record;
    let rec = if manual { RecMode::Button } else { rec };
    let button = match (manual, button) {
        (true, _) => ButtonMode::Record,
        (false, ButtonMode::Record) => ButtonMode::Mute,
        (false, b) => b,
    };
    [
        (0x1b, (rec == RecMode::Always) as u8),
        (0x04, matches!(rec, RecMode::Always | RecMode::Backup) as u8),
        (0x21, manual as u8),
        (0x1d, (button == ButtonMode::Marker) as u8),
        (0x17, (button != ButtonMode::None) as u8),
        (0x18, uncompressed as u8),
        (0x1e, 1),
    ]
}

/// Set record mode, button function and recording format of the TX in `slot`, then refresh.
pub fn radio_set_settings(
    d: &mut Dev,
    slot: u8,
    rec: RecMode,
    button: ButtonMode,
    uncompressed: bool,
) -> Result<()> {
    for (reg, value) in radio_settings(rec, button, uncompressed) {
        radio_write(d, slot, reg, value)?;
    }
    refresh(d)
}

pub fn radio_set_pad(d: &mut Dev, slot: u8, pad: bool) -> Result<()> {
    radio_write(d, slot, 0x08, (!pad) as u8)?;
    refresh(d)
}

pub const LED_BRIGHT: u8 = 0xDF;
pub const LED_DIM: u8 = 0xFE;

pub fn radio_set_led(d: &mut Dev, slot: u8, dim: bool) -> Result<()> {
    radio_write(d, slot, 0x1a, if dim { LED_DIM } else { LED_BRIGHT })?;
    refresh(d)
}

// Report 1 commands. Only the proven ones are here.
//
// 'Q' 00 switches a TX to USB storage mode (it re-enumerates with a write-protected mass-storage drive).
// DANGER: never send 'Q' 01 — on 2026-11-10 it erased the recordings of two transmitters and reset their
// settings (see PROTOCOL.md). Storage mode ends by unplugging the TX or switching it off and on.

/// Put a TX into USB storage mode (report 1 'Q' 00). The node disappears and comes back.
pub fn enter_storage_mode(d: &mut Dev) -> Result<()> {
    match d.write_report(1, &[0x51, 0x00], 16) {
        Err(Error::DeviceGone(_)) => Ok(()), // already re-enumerating
        r => r,
    }
}

// Delete every recording on a transmitter = RØDE Central's "Delete All" (export formatTxMemory, class
// WGo2DiskFormatter, state machine at rode_bridge 0x1800d6250): OUT report 1 'J' 01, the TX answers with an
// input report starting with 'J' within 4 s, and may then restart. Only for GO II transmitters. The TX drive
// is write-protected for the computer, so single recordings cannot be deleted at all.
pub const ERASE_COMMAND: [u8; 2] = [b'J', 0x01];
pub const ERASE_PIDS: [u16; 3] = [0x001E, 0x0029, 0x002D];

/// Erase all on-board recordings of a TX. Irreversible. Returns the TX's reply.
pub fn erase_recordings(dev: &mut Dev) -> Result<Vec<u8>> {
    if !ERASE_PIDS.contains(&dev.pid()) {
        return Err(Error::other(
            "only a Wireless GO II transmitter has recordings to delete",
        ));
    }
    dev.drain();
    dev.write_report(1, &ERASE_COMMAND, 16)?;
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        if let Some((_, data)) = dev.read_report(end - Instant::now())? {
            if data.first() == Some(&ERASE_COMMAND[0]) {
                return Ok(data);
            }
        }
    }
    Err(Error::Timeout(
        "the transmitter did not confirm deleting its recordings".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_sizes_of_a_simple_descriptor() {
        // Usage page (vendor), collection, report id 9, size 8, count 27, output; report id 2, count 15, input
        let desc = [
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, //
            0x85, 0x09, 0x75, 0x08, 0x95, 0x1B, 0x91, 0x02, //
            0x85, 0x02, 0x95, 0x0F, 0x81, 0x02, 0xC0,
        ];
        let (sizes, uses_ids) = parse_report_sizes(&desc);
        assert!(uses_ids);
        assert_eq!(sizes.get(&(ReportKind::Out, 9)), Some(&27));
        assert_eq!(sizes.get(&(ReportKind::In, 2)), Some(&15));
    }

    #[test]
    fn mode_names_round_trip() {
        for m in RecMode::ALL {
            assert_eq!(RecMode::parse(m.as_str()), Some(m));
        }
        for m in ButtonMode::ALL {
            assert_eq!(ButtonMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(ChannelMode::parse("split"), Some(ChannelMode::Split));
    }
}

#[cfg(test)]
mod gain_tests {
    use super::*;

    #[test]
    fn gain_levels_and_mode_switches() {
        let g = |mode, step| Gain { mode, step };
        assert_eq!(g(COARSE_GAIN, 0).db(), Some(-24));
        assert_eq!(g(COARSE_GAIN, 2).db(), Some(0));
        assert_eq!(g(FINE_GAIN, 0).db(), Some(-30));
        assert_eq!(g(FINE_GAIN, 9).db(), Some(-3));
        assert_eq!(g(PRESET_GAIN, 0).db(), None);
        assert_eq!(g(FINE_GAIN, 9).converted(COARSE_GAIN), g(COARSE_GAIN, 2)); // -3 dB → 0 dB
        assert_eq!(g(FINE_GAIN, 5).converted(COARSE_GAIN), g(COARSE_GAIN, 1)); // -15 → -12
        assert_eq!(g(COARSE_GAIN, 1).converted(FINE_GAIN), g(FINE_GAIN, 6)); // -12 → -12
        assert_eq!(g(COARSE_GAIN, 0).converted(FINE_GAIN), g(FINE_GAIN, 2)); // -24 → -24
        assert_eq!(g(FINE_GAIN, 3).converted(PRESET_GAIN), g(PRESET_GAIN, 0));
    }
}

#[cfg(test)]
mod radio_tests {
    use super::*;

    fn regs(rec: RecMode, button: ButtonMode, unc: bool) -> Vec<(u8, u8)> {
        radio_settings(rec, button, unc).to_vec()
    }

    /// The sets as tested on the units on 2026-11-13 (registers 1b 04 21 1d 17 18 1e).
    #[test]
    fn settings_sets() {
        use ButtonMode::*;
        use RecMode::*;
        let vals = |v: &[(u8, u8)]| v.iter().map(|(_, x)| *x).collect::<Vec<_>>();
        assert_eq!(
            regs(Off, Mute, true)
                .iter()
                .map(|(r, _)| *r)
                .collect::<Vec<_>>(),
            [0x1b, 0x04, 0x21, 0x1d, 0x17, 0x18, 0x1e]
        );
        assert_eq!(vals(&regs(Off, Mute, true)), [0, 0, 0, 0, 1, 1, 1]);
        assert_eq!(vals(&regs(Off, None, true)), [0, 0, 0, 0, 0, 1, 1]);
        assert_eq!(vals(&regs(Off, Marker, true)), [0, 0, 0, 1, 1, 1, 1]);
        assert_eq!(vals(&regs(Always, Mute, true)), [1, 1, 0, 0, 1, 1, 1]);
        assert_eq!(vals(&regs(Backup, Mute, false)), [0, 1, 0, 0, 1, 0, 1]);
        // Manual and the Record button imply each other
        assert_eq!(vals(&regs(Button, Mute, true)), [0, 0, 1, 0, 1, 1, 1]);
        assert_eq!(vals(&regs(Always, Record, true)), [0, 0, 1, 0, 1, 1, 1]);
    }

    #[test]
    fn radio_slots() {
        assert_eq!(radio_op(1), Some(0x0d));
        assert_eq!(radio_op(2), Some(0x0e));
        assert_eq!(radio_op(3), None);
    }
}
