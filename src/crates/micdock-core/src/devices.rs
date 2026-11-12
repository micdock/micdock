// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! Find, probe and control connected Wireless GO II units (HID + USB storage mode).
//! Everything here may block for seconds: an app runs it off its UI thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::hid::{self, ButtonMode, ChannelMode, Dev, DevInfo, Gain, RecMode};
use crate::{Error, Result};

pub const TX_PID: u16 = 0x0029;
pub const RX_PID: u16 = 0x002A;

/// Seconds the desktop gets to auto-mount a TX drive before we mount it ourselves.
pub const MOUNT_GRACE: f64 = 2.0;

fn sys(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// sysfs dir of the USB device behind a hidraw node (…/1-10/1-10:1.0/0003:… → …/1-10).
pub fn usb_dir(node: &str) -> PathBuf {
    let name = Path::new(node)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let p = std::fs::canonicalize(format!("/sys/class/hidraw/{name}/device")).unwrap_or_default();
    p.parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// True if the USB device also offers a mass-storage interface (the TX is in USB storage mode).
/// Whether the unit currently offers a USB audio interface (class 01). The receiver does that only while it
/// is switched on (found 2026-11-12: switched off and charging it has only its HID interface), so this is the
/// receiver's on/off state — read from sysfs, without touching the device.
pub fn has_audio_interface(udir: &Path) -> bool {
    let base = udir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(udir) else {
        return false;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{base}:"))
        })
        .any(|e| sys(e.path().join("bInterfaceClass")) == "01")
}

pub fn in_storage_mode(udir: &Path) -> bool {
    let base = udir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(udir) else {
        return false;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{base}:"))
        })
        .any(|e| sys(e.path().join("bInterfaceClass")) == "08")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Tx,
    Rx,
    Other,
}

impl Role {
    pub fn of(pid: u16) -> Role {
        match pid {
            TX_PID => Role::Tx,
            RX_PID => Role::Rx,
            _ => Role::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Tx => "TX",
            Role::Rx => "RX",
            Role::Other => "device",
        }
    }
}

/// Every RØDE unit that speaks the settings protocol, with its USB serial number.
pub fn hid_devices() -> Vec<(DevInfo, String)> {
    hid::find_devices()
        .into_iter()
        .filter(|d| d.has_settings_report())
        .map(|d| {
            let serial = sys(usb_dir(&d.node).join("serial"));
            (d, serial)
        })
        .collect()
}

pub fn open_by_serial(serial: &str) -> Result<Dev> {
    for (d, s) in hid_devices() {
        if s == serial {
            return d.open();
        }
    }
    Err(Error::other(format!("device {serial} is not connected")))
}

/// Firmware version from the USB bcdDevice, e.g. "0250" → (2, 5, 0).
pub fn device_version(bcd: &str) -> Option<(u32, u32, u32)> {
    if bcd.len() != 4 || !bcd.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let h = |s: &str| u32::from_str_radix(s, 16).ok();
    Some((h(&bcd[..2])?, h(&bcd[2..3])?, h(&bcd[3..4])?))
}

/// A transmitter the receiver is linked to right now, from the receiver's status record (param 0x0b for
/// TX1, 0x0c for TX2; refused while that slot has no linked TX). Found 2026-11-12 by reading the RX with
/// the TX on and off: the record and the TX's name (0x0f / 0x10) only exist while the TX is on and linked.
/// A TX's own USB replies don't change when it is switched off, and the receiver's on/off state isn't
/// visible at all, so "linked" is the only on/off signal there is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxLink {
    /// 1 or 2: the receiver's channel.
    pub slot: u8,
    /// The TX's USB serial number (record bytes 0–3, little-endian, as hex: "2288A001").
    pub serial: String,
    /// The TX's param 0x02 (record bytes 4–7), to match it with a TX on USB.
    pub id: u32,
    pub name: String,
    /// Record byte 21 = the TX's lav gain (0 = pad on; changed with the pad switched on the receiver).
    pub pad: Option<bool>,
    /// Record byte 22 (matched the TX's own battery reading, 100 %, in the one sample we have).
    pub battery: Option<u8>,
}

/// What a quick look at a unit tells us (one entry per connected unit).
#[derive(Debug, Clone)]
pub struct DeviceStatus {
    pub node: String,
    pub pid: u16,
    pub role: Role,
    pub product: String,
    pub serial: String,
    /// Firmware as shown, e.g. "2.50".
    pub fw: String,
    pub fw_version: Option<(u32, u32, u32)>,
    /// In USB storage mode (its wireless link is off until unplugged or switched off and on).
    pub storage: bool,
    pub battery: Option<u8>,
    pub name: Option<String>,
    /// The unit's clock (unix seconds) and when it was read.
    pub clock: Option<u32>,
    pub clock_at: Option<f64>,
    pub error: Option<String>,
    pub rec: Option<RecMode>,
    pub button: Option<ButtonMode>,
    pub channel: Option<ChannelMode>,
    /// Transmitter: low-sensitivity pad on (lav gain 0).
    pub pad: Option<bool>,
    /// Receiver: switched on (it offers its USB sound card only then). `None` for transmitters.
    pub powered: Option<bool>,
    /// Receiver: power saver on (backlight off after 10 s).
    pub power_saver: Option<bool>,
    /// Receiver: output gain.
    pub gain: Option<Gain>,
    /// Param 0x02 (a fixed per-unit number), to match a TX on USB with the receiver's links.
    pub id: Option<u32>,
    /// Receiver: the transmitters linked to it now (= switched on and in range).
    pub links: Vec<TxLink>,
}

/// Read battery, name, clock and the mode settings of every connected unit.
pub fn probe_devices() -> Vec<DeviceStatus> {
    probe_devices_ex(true, true)
}

/// Like [`probe_devices`], but without the reads that disturb the units' buttons, for frequent polling.
///
/// Found 2026-11-12: while a TX's button-mode param 0x0a is read every 3 s, a long press on its power
/// button doesn't switch it off (reading battery or name at the same rate is harmless). 0x04 is the other
/// half of the button setting (and the RX's power saver) and is left out too. So `button` (TX) and
/// `power_saver` (RX) stay `None` here: callers keep the values from their last full read.
pub fn probe_devices_light() -> Vec<DeviceStatus> {
    probe_devices_ex(false, true)
}

/// Only the receivers are read; transmitters are listed (node, serial, firmware, storage mode from sysfs) but
/// not opened, and their other fields stay `None`.
///
/// Found 2026-11-12: any HID traffic during a long press (2–3 s) on a TX's power button cancels it, and
/// reading a TX every 3 s blocks switching it off by hand. The receiver tells whether transmitters are on
/// (its link records), so frequent polling only needs the receiver.
pub fn probe_devices_rx_only() -> Vec<DeviceStatus> {
    probe_devices_ex(false, false)
}

fn probe_devices_ex(full: bool, read_tx: bool) -> Vec<DeviceStatus> {
    let mut out = Vec::new();
    for (info, serial) in hid_devices() {
        let udir = usb_dir(&info.node);
        let bcd = sys(udir.join("bcdDevice"));
        let fw = if bcd.len() == 4 && bcd.chars().all(|c| c.is_ascii_digit()) {
            format!(
                "{}.{:02}",
                bcd[..2].parse::<u32>().unwrap_or(0),
                bcd[2..].parse::<u32>().unwrap_or(0)
            )
        } else {
            bcd.clone()
        };
        let mut st = DeviceStatus {
            node: info.node.clone(),
            pid: info.pid,
            role: Role::of(info.pid),
            product: info.name.clone(),
            serial,
            fw,
            fw_version: device_version(&bcd),
            storage: in_storage_mode(&udir),
            battery: None,
            name: None,
            clock: None,
            clock_at: None,
            error: None,
            rec: None,
            button: None,
            channel: None,
            id: None,
            links: Vec::new(),
            power_saver: None,
            gain: None,
            pad: None,
            powered: (Role::of(info.pid) == Role::Rx).then(|| has_audio_interface(&udir)),
        };
        if !read_tx && st.role == Role::Tx {
            out.push(st);
            continue;
        }
        if let Err(e) = probe_one(&info, &mut st, full) {
            // device busy, switching modes, unplugged, no permission...
            st.error = Some(e.to_string());
        }
        out.push(st);
    }
    out
}

fn probe_one(info: &DevInfo, st: &mut DeviceStatus, full: bool) -> Result<()> {
    let mut d = info.open()?;
    st.battery = hid::get_param(&mut d, 0x00)?.first().copied();
    let raw = hid::get_param(&mut d, 0x03)?;
    let raw = &raw[..raw.len().min(0x16)];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    st.name = Some(String::from_utf8_lossy(&raw[..end]).into_owned());
    let t0 = now();
    let c = hid::get_param(&mut d, 0x01)?;
    let t1 = now();
    if c.len() >= 4 {
        st.clock = Some(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        st.clock_at = Some((t0 + t1) / 2.0);
    }
    st.id = hid::get_param(&mut d, 0x02)
        .ok()
        .filter(|v| v.len() >= 4)
        .map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
    if st.role == Role::Rx {
        st.links = read_links(&mut d);
    }
    match st.role {
        Role::Tx => {
            // a refused or unanswered mode read is not an error for the whole unit
            if let Ok(m) = hid::rec_mode(&mut d) {
                st.rec = Some(m);
                if full {
                    st.button = hid::button_mode(&mut d).ok();
                }
                st.pad = hid::pad(&mut d).ok();
            }
        }
        Role::Rx => {
            st.channel = hid::channel_mode(&mut d).ok();
            if full {
                st.power_saver = hid::power_saver(&mut d).ok();
            }
            st.gain = hid::gain(&mut d).ok();
        }
        Role::Other => {}
    }
    Ok(())
}

/// The receiver's linked transmitters (read-only GETs; an empty slot is refused at once).
fn read_links(d: &mut Dev) -> Vec<TxLink> {
    let mut out = Vec::new();
    for (slot, record, name_param) in [(1u8, 0x0bu8, 0x0fu8), (2, 0x0c, 0x10)] {
        let Ok(v) = hid::get_param_ex(d, record, 0, 0.5, None) else {
            continue;
        };
        if v.len() < 23 {
            continue;
        }
        let name = hid::get_param_ex(d, name_param, 0, 0.5, None)
            .map(|raw| {
                let raw = &raw[..raw.len().min(0x16)];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                String::from_utf8_lossy(&raw[..end]).into_owned()
            })
            .unwrap_or_default();
        out.push(TxLink {
            slot,
            serial: format!("{:08X}", u32::from_le_bytes([v[0], v[1], v[2], v[3]])),
            id: u32::from_le_bytes([v[4], v[5], v[6], v[7]]),
            name,
            battery: Some(v[22]).filter(|b| *b <= 100),
            pad: (v[21] <= 1).then_some(v[21] == 0),
        });
    }
    out
}

// --------------------------------------------------------------------------
// the transmitter's drive (USB storage mode)
// --------------------------------------------------------------------------

/// Undo the octal escapes of /proc/mounts (`\040` = space).
pub fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = oct.iter().fold(0u32, |acc, c| acc * 8 + (c - b'0') as u32);
                out.push(v as u8);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// USB serial number of the device a /sys/block/sdX disk belongs to: the TX's serial (same as its HID).
fn block_serial(sysdir: &Path) -> String {
    let mut d = std::fs::canonicalize(sysdir.join("device")).unwrap_or_default();
    while d.as_os_str().len() > 1 {
        if d.join("idVendor").exists() && d.join("serial").exists() {
            return sys(d.join("serial"));
        }
        if !d.pop() {
            break;
        }
    }
    String::new()
}

/// [(/dev/sdX, TX serial)] for every RØDE storage device.
pub fn rode_disks() -> Vec<(String, String)> {
    let mut blocks: Vec<PathBuf> = std::fs::read_dir("/sys/block")
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    blocks.retain(|b| {
        b.file_name()
            .map(|n| n.to_string_lossy().starts_with("sd"))
            .unwrap_or(false)
    });
    blocks.sort();
    blocks
        .into_iter()
        .filter(|b| {
            sys(b.join("device/vendor"))
                .to_uppercase()
                .starts_with("RODE")
        })
        .map(|b| {
            (
                format!("/dev/{}", b.file_name().unwrap().to_string_lossy()),
                block_serial(&b),
            )
        })
        .collect()
}

pub fn rode_block_devices(serial: Option<&str>) -> Vec<String> {
    rode_disks()
        .into_iter()
        .filter(|(_, s)| serial.is_none_or(|x| x == s))
        .map(|(d, _)| d)
        .collect()
}

/// Partitions of a whole-disk device node, e.g. /dev/sdd → [/dev/sdd1], sorted.
fn partitions(dev: &str) -> Vec<String> {
    let base = Path::new(dev)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out: Vec<String> = std::fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    n.len() > base.len()
                        && n.starts_with(&base)
                        && n[base.len()..].starts_with(|c: char| c.is_ascii_digit())
                })
                .map(|n| format!("/dev/{n}"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// {TX serial: (block device, mount point)} for every mounted TX drive.
pub fn tx_mounts() -> BTreeMap<String, (String, String)> {
    let mut mounts: BTreeMap<String, String> = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/mounts") else {
        return BTreeMap::new();
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(src), Some(dst)) = (parts.next(), parts.next()) {
            mounts
                .entry(src.to_string())
                .or_insert_with(|| unescape_mount(dst));
        }
    }
    let mut out = BTreeMap::new();
    for (dev, serial) in rode_disks() {
        let mut cands = vec![dev.clone()];
        cands.extend(partitions(&dev));
        if let Some(c) = cands.into_iter().find(|c| mounts.contains_key(c)) {
            let m = mounts[&c].clone();
            out.insert(serial, (c, m));
        }
    }
    out
}

/// (block device, mount point) of that TX's drive (any TX's when `serial` is None).
pub fn find_tx_mount(serial: Option<&str>) -> Option<(String, String)> {
    let found = tx_mounts();
    match serial {
        Some(s) => found.get(s).cloned(),
        None => found.into_values().next(),
    }
}

/// Steps reported by [`connect_tx_storage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPhase {
    /// The storage-mode switch was sent.
    Switching,
    /// The drive is there, but not mounted yet.
    Waiting,
    /// We mount it ourselves (read-only), because the desktop did not.
    Mounting,
}

fn have(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Put a TX (the one with this serial, or the first one) into USB storage mode and wait until its drive
/// is mounted. Returns (block device, mount point). Its wireless link is off until it is unplugged or
/// switched off and on.
pub fn connect_tx_storage(
    serial: Option<&str>,
    mut status: impl FnMut(ConnectPhase),
) -> Result<(String, String)> {
    if let Some(found) = find_tx_mount(serial) {
        return Ok(found);
    }
    // The desktop auto-mounts a drive when it appears. If the drive was already there (e.g. after Eject),
    // it won't do that again, so we mount it ourselves right away instead of waiting.
    let already = !rode_block_devices(serial).is_empty();
    let grace = if already { 0.0 } else { MOUNT_GRACE };
    if !already {
        let tx = hid_devices()
            .into_iter()
            .find(|(d, s)| d.pid == TX_PID && serial.is_none_or(|x| x == s))
            .ok_or_else(|| {
                Error::other(
                    "No TX found. Connect the Wireless GO II TX with USB and switch it on.",
                )
            })?;
        status(ConnectPhase::Switching);
        let mut d = tx.0.open()?;
        hid::enter_storage_mode(&mut d)?;
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut seen: Option<Instant> = None;
    let mut tried = false;
    while Instant::now() < deadline {
        if let Some(found) = find_tx_mount(serial) {
            return Ok(found);
        }
        let blks = rode_block_devices(serial);
        if let Some(blk) = blks.first() {
            let since = *seen.get_or_insert_with(|| {
                status(ConnectPhase::Waiting);
                Instant::now()
            });
            if !tried && since.elapsed().as_secs_f64() >= grace && have("udisksctl") {
                tried = true;
                status(ConnectPhase::Mounting);
                let target = partitions(blk)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| blk.clone());
                let r = Command::new("udisksctl")
                    .args(["mount", "--no-user-interaction", "-o", "ro", "-b", &target])
                    .output()?;
                if let Some(found) = find_tx_mount(serial) {
                    return Ok(found); // the desktop may have won the race: that is fine too
                }
                if !r.status.success() {
                    return Err(Error::other(format!(
                        "Could not mount {target}: {}",
                        String::from_utf8_lossy(&r.stderr).trim()
                    )));
                }
            }
        }
        sleep(Duration::from_millis(250));
    }
    Err(Error::other(
        "The TX drive did not show up. Unplug and replug the TX, then try again.",
    ))
}

/// Unmount the TX's drive. The TX stays in storage mode (wireless off) until unplugged or switched off and on.
pub fn eject_tx_storage(serial: Option<&str>) -> Result<()> {
    let Some((blk, _)) = find_tx_mount(serial) else {
        return Ok(());
    };
    let r = Command::new("udisksctl")
        .args(["unmount", "--no-user-interaction", "-b", &blk])
        .output()?;
    if !r.status.success() {
        let msg = String::from_utf8_lossy(&r.stderr).trim().to_string();
        return Err(Error::other(if msg.is_empty() {
            "unmount failed".into()
        } else {
            msg
        }));
    }
    Ok(())
}

// --------------------------------------------------------------------------
// settings by serial number (what an app uses)
// --------------------------------------------------------------------------

/// Read every parameter id up to `last` (read-only).
pub fn scan_params(serial: &str, last: u8) -> Result<BTreeMap<u8, Vec<u8>>> {
    let mut d = open_by_serial(serial)?;
    let mut out = BTreeMap::new();
    for p in 0..=last {
        match hid::get_param_ex(&mut d, p, 0, 0.3, None) {
            Ok(mut v) => {
                while v.last() == Some(&0) {
                    v.pop();
                }
                out.insert(p, v);
            }
            Err(e) if e.is_unanswered() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Set the unit's clock to now. Returns the difference read back (seconds).
pub fn set_clock_now(serial: &str) -> Result<i64> {
    let mut d = open_by_serial(serial)?;
    let t = now() as u32;
    hid::set_param(&mut d, 0x01, &t.to_le_bytes())?;
    let c = hid::get_param(&mut d, 0x01)?;
    if c.len() < 4 {
        return Err(Error::other("short clock reply"));
    }
    let c = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    Ok(c as i64 - now() as i64)
}

pub fn set_rec_mode(serial: &str, mode: RecMode) -> Result<RecMode> {
    hid::set_rec_mode(&mut open_by_serial(serial)?, mode)
}

pub fn set_button_mode(serial: &str, mode: ButtonMode) -> Result<ButtonMode> {
    hid::set_button_mode(&mut open_by_serial(serial)?, mode)
}

pub fn set_channel_mode(serial: &str, mode: ChannelMode) -> Result<ChannelMode> {
    hid::set_channel_mode(&mut open_by_serial(serial)?, mode)
}

/// Name a unit (up to 21 bytes of UTF-8).
pub fn set_power_saver(serial: &str, on: bool) -> Result<bool> {
    hid::set_power_saver(&mut open_by_serial(serial)?, on)
}

pub fn set_gain(serial: &str, g: Gain) -> Result<Gain> {
    hid::set_gain(&mut open_by_serial(serial)?, g)
}

pub fn set_pad(serial: &str, on: bool) -> Result<bool> {
    hid::set_pad(&mut open_by_serial(serial)?, on)
}

pub fn set_name(serial: &str, name: &str) -> Result<()> {
    hid::set_param(&mut open_by_serial(serial)?, 0x03, &name_payload(name))?;
    Ok(())
}

/// The 22-byte name field: at most 21 bytes of UTF-8 (cut at a character boundary), zero-padded.
pub fn name_payload(name: &str) -> Vec<u8> {
    let mut end = name.len().min(0x15);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let mut raw = name.as_bytes()[..end].to_vec();
    raw.resize(0x16, 0);
    raw
}

/// Steps reported by [`erase_tx_recordings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasePhase {
    Unmounting,
    Erasing,
    Restarting,
}

/// Delete all recordings on the TX with this serial (RØDE Central's "Delete All"). Irreversible.
/// Its drive is unmounted first, so no stale file list stays behind. Returns the TX's reply.
pub fn erase_tx_recordings(serial: &str, mut status: impl FnMut(ErasePhase)) -> Result<Vec<u8>> {
    if find_tx_mount(Some(serial)).is_some() {
        status(ErasePhase::Unmounting);
        eject_tx_storage(Some(serial))?;
    }
    status(ErasePhase::Erasing);
    let reply = {
        let mut d = open_by_serial(serial)?;
        hid::erase_recordings(&mut d)?
    };
    // like RØDE Central: give the TX 4 s; if it restarts, wait (up to 20 s) until it is back
    status(ErasePhase::Restarting);
    let t0 = Instant::now();
    let mut gone = false;
    while t0.elapsed() < Duration::from_secs(20) {
        let present = hid_devices().iter().any(|(_, s)| s == serial);
        if !present {
            gone = true;
        } else if gone || t0.elapsed() >= Duration::from_secs(4) {
            break;
        }
        sleep(Duration::from_millis(250));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_escapes() {
        assert_eq!(
            unescape_mount(r"/media/me/My\040Drive"),
            "/media/me/My Drive"
        );
        assert_eq!(unescape_mount("/media/me/ABCD-1234"), "/media/me/ABCD-1234");
        assert_eq!(unescape_mount(r"/odd\"), r"/odd\");
    }

    #[test]
    fn versions() {
        assert_eq!(device_version("0250"), Some((2, 5, 0)));
        assert_eq!(device_version("x"), None);
    }

    #[test]
    fn names_are_cut_at_character_boundaries() {
        let p = name_payload("ØØØØØØØØØØØ"); // 22 bytes of UTF-8
        assert_eq!(p.len(), 0x16);
        assert_eq!(&p[..20], "ØØØØØØØØØØ".as_bytes());
        assert_eq!(p[20], 0);
    }
}
