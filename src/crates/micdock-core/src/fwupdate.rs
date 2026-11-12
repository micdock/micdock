// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Firmware installer for the RØDE Wireless GO II "2.5" units (USB PID 0x29 TX / 0x2a RX).
//!
//! Reverse-engineered from RØDE Central's rode_bridge.dll (WGo2FirmwareUpdater) by static analysis.
//! The Python version of this code updated two transmitters and a receiver (2.20 → 2.50, 2026-11-10).
//! A failed update can leave a unit in its update mode (PID 0x6e TX / 0x70 RX); this code can talk to
//! that mode again, but there is no guarantee. See PROTOCOL.md "Firmware update".
//!
//! Sequence (RØDE Central states 3 → 8 → 9 → 10 → 11 for these PIDs):
//!   1. report 1  'O' 01                 → [O,'A', BE u32 resume offset for the main image]
//!   2. report 1  'h'                    (Nordic radio phase; reply not checked)
//!      per HEX part: report 3 'A' addr size → [A,'A', crc32]; skip the chip if every CRC already matches
//!      report 3 'N' 0 0 00 01 00 (empty part) → 'A', then wait [N,'A',0]
//!      per part: report 3 'N' addr size 01 00 00 → 'A'; data via 'C' credits + report 5; wait
//!      `[N,'A',addr]`; verify again with 'A'
//!   3. main MCU: report 7 blocks of 0x600 from the resume offset; the unit answers 'A'/'C' + BE u32 next
//!      offset, or 'S' + status (D/O/R ok, F/I fail). Ends when < 0x600 bytes remain past the offset.
//!      Then report 1 'Z' (no reply) and wait 5 s.
//!   4. wait 4 s, report 1 '7' → 'A' (skipped in update mode), report 1 'Y' (no reply), wait for the unit.
//!
//! On failure RØDE Central sends report 1 'R'; we do the same.
//!
//! The update logic talks to a [`Transport`]: [`HidTransport`] for real units, a simulator in the tests.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::devices::{device_version, usb_dir};
use crate::hid::{self, Dev, ReportKind};
use crate::{firmware, Error, Result};

/// unit → (normal PID, update-mode PID)
pub fn unit_pids(unit: &str) -> Option<(u16, u16)> {
    match unit {
        "tx" => Some((0x0029, 0x006E)),
        "rx" => Some((0x002A, 0x0070)),
        _ => None,
    }
}
pub const UPDATE_MODE_PIDS: [u16; 2] = [0x006E, 0x0070];
pub const ACK: u8 = 0x41;

/// (USB PID, firmware version) of a unit after an update.
pub type UnitState = (Option<u16>, Option<(u32, u32, u32)>);
pub const MIN_BATTERY: u8 = 40;

fn update_err(msg: impl Into<String>) -> Error {
    Error::Update(msg.into())
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn be32(b: &[u8]) -> u32 {
    b.iter().take(4).fold(0u32, |acc, x| (acc << 8) | *x as u32)
}

fn le32(b: &[u8]) -> u32 {
    b.iter()
        .take(4)
        .rev()
        .fold(0u32, |acc, x| (acc << 8) | *x as u32)
}

// --------------------------------------------------------------------------
// firmware images
// --------------------------------------------------------------------------

/// Intel HEX → maximal contiguous byte runs [(start, bytes)] (as RØDE Central's f8610 does).
pub fn hex_runs(path: &Path) -> Result<Vec<(u32, Vec<u8>)>> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = std::fs::read_to_string(path)?;
    let mut mem: std::collections::BTreeMap<u32, u8> = Default::default();
    let mut base: u32 = 0;
    for (n, line) in text.lines().enumerate().map(|(i, l)| (i + 1, l.trim())) {
        if line.is_empty() {
            continue;
        }
        let Some(hexpart) = line.strip_prefix(':') else {
            return Err(update_err(format!("{name} line {n}: no ':'")));
        };
        if hexpart.len() % 2 != 0 {
            return Err(update_err(format!("{name} line {n}: odd length")));
        }
        let b: Vec<u8> = (0..hexpart.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hexpart[i..i + 2], 16))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| update_err(format!("{name} line {n}: not hex")))?;
        if b.iter().fold(0u8, |a, x| a.wrapping_add(*x)) != 0 {
            return Err(update_err(format!("{name} line {n}: bad checksum")));
        }
        if b.len() < 5 {
            return Err(update_err(format!("{name} line {n}: short record")));
        }
        let (cnt, addr, typ) = (b[0] as usize, ((b[1] as u32) << 8) | b[2] as u32, b[3]);
        let data = &b[4..(4 + cnt).min(b.len())];
        if data.len() != cnt {
            return Err(update_err(format!("{name} line {n}: short record")));
        }
        match typ {
            0 => {
                for (i, v) in data.iter().enumerate() {
                    mem.insert(base.wrapping_add(addr).wrapping_add(i as u32), *v);
                }
            }
            1 => break,
            2 => base = be32(data) << 4,
            4 => base = be32(data) << 16,
            _ => {}
        }
    }
    let mut runs: Vec<(u32, Vec<u8>)> = Vec::new();
    for (a, v) in mem {
        match runs.last_mut() {
            Some((start, d)) if *start + d.len() as u32 == a => d.push(v),
            _ => runs.push((a, vec![v])),
        }
    }
    Ok(runs)
}

/// One flash range. RØDE Central: id = start >> 8, addr = id << 8, data padded with 0xFF to 256.
#[derive(Debug, Clone)]
pub struct Part {
    pub addr: u32,
    pub raw: Vec<u8>,
    pub padded: Vec<u8>,
}

impl Part {
    pub fn new(start: u32, data: Vec<u8>) -> Result<Part> {
        if start & 0xFF != 0 {
            return Err(update_err(format!(
                "part at {start:#x} is not 256-byte aligned; not supported"
            )));
        }
        let mut padded = data.clone();
        padded.resize(data.len().div_ceil(256) * 256, 0xFF);
        Ok(Part {
            addr: start,
            raw: data,
            padded,
        })
    }
}

/// A verified firmware package, resolved for one unit ("tx" or "rx").
#[derive(Debug, Clone)]
pub struct Package {
    pub unit: String,
    pub version: String,
    pub main_path: PathBuf,
    pub nordic_path: PathBuf,
    pub main: Vec<u8>,
    pub parts: Vec<Part>,
}

impl Package {
    pub fn load(pkg_dir: &Path, unit: &str) -> Result<Package> {
        let parts = firmware::package_parts(pkg_dir, unit);
        let get = |k: &str| {
            parts
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, p)| p.clone())
        };
        let (Some(main_path), Some(nordic_path)) = (get("main"), get("nordic")) else {
            return Err(update_err(format!(
                "package {} has no main/nordic image for {unit}",
                pkg_dir.display()
            )));
        };
        let base = |p: &Path| {
            p.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let info = firmware::ota_info(&main_path);
        let want = format!("wgo25_{unit}");
        let Some((board, version)) = info.filter(|(b, _)| *b == want) else {
            let got = firmware::ota_info(&main_path)
                .map(|(b, _)| format!("'{b}'"))
                .unwrap_or("None".into());
            return Err(update_err(format!(
                "{} is for board {got}, expected '{want}'",
                base(&main_path)
            )));
        };
        let _ = board;
        if !base(&nordic_path).starts_with(&format!("wgo2.5_nordic_{unit}")) {
            return Err(update_err(format!(
                "unexpected Nordic file {}",
                nordic_path.display()
            )));
        }
        let main = std::fs::read(&main_path)?;
        let parts = hex_runs(&nordic_path)?
            .into_iter()
            .map(|(a, d)| Part::new(a, d))
            .collect::<Result<_>>()?;
        Ok(Package {
            unit: unit.into(),
            version,
            main_path,
            nordic_path,
            main,
            parts,
        })
    }
}

// --------------------------------------------------------------------------
// the link to the unit
// --------------------------------------------------------------------------

/// GO II units in normal or update mode: (hidraw info, USB serial, unit).
pub fn find_units(unit: Option<&str>) -> Vec<(hid::DevInfo, String, &'static str)> {
    let mut out = Vec::new();
    for d in hid::find_devices() {
        for u in ["tx", "rx"] {
            let (normal, update) = unit_pids(u).unwrap();
            if (d.pid == normal || d.pid == update) && unit.is_none_or(|x| x == u) {
                let serial =
                    std::fs::read_to_string(usb_dir(&d.node).join("serial")).unwrap_or_default();
                out.push((d.clone(), serial.trim().to_string(), u));
            }
        }
    }
    out
}

/// What the installer needs from a unit. The provided methods are RØDE Central's request helpers.
pub trait Transport {
    /// (Re)open the unit, waiting up to `timeout` s for it to appear (normal or update mode).
    fn open(&mut self, timeout: f64) -> Result<()>;
    fn close(&mut self);
    fn is_open(&self) -> bool;
    fn pid(&self) -> Option<u16>;
    /// Length of OUT report `rid` (payloads are zero-padded to it), or None if the unit has none.
    fn out_size(&self, rid: u8) -> Option<usize>;
    /// Send a padded report. Errors with [`Error::DeviceGone`] when the unit has gone away.
    fn send(&mut self, rid: u8, padded: &[u8]) -> Result<()>;
    /// The next input report's data (report id stripped), or None after `timeout`.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
    fn log(&mut self, line: &str);
    /// (PID, firmware version) of the unit after the update.
    fn after_update(&mut self) -> UnitState;

    fn write(&mut self, rid: u8, payload: &[u8]) -> Result<()> {
        let n = self
            .out_size(rid)
            .ok_or_else(|| update_err(format!("device has no OUT report {rid}")))?;
        if payload.len() > n {
            return Err(update_err(format!(
                "report {rid} payload too long ({} > {n})",
                payload.len()
            )));
        }
        let mut padded = payload.to_vec();
        padded.resize(n, 0);
        self.send(rid, &padded)?;
        let more = if payload.len() > 24 {
            format!(" …({} B)", payload.len())
        } else {
            String::new()
        };
        self.log(&format!(
            "-> {rid}: {}{more}",
            hex(&payload[..payload.len().min(24)])
        ));
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let r = self.recv(timeout)?;
        if let Some(d) = &r {
            self.log(&format!("<- : {}", hex(&d[..d.len().min(24)])));
        }
        Ok(r)
    }

    fn drain(&mut self) -> Result<()> {
        while self.recv(Duration::ZERO)?.is_some() {}
        Ok(())
    }

    /// The next input report for which `pred` is true, or None on timeout.
    fn wait_for(&mut self, pred: &dyn Fn(&[u8]) -> bool, timeout: f64) -> Result<Option<Vec<u8>>> {
        let end = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
        loop {
            let now = Instant::now();
            if now >= end {
                return Ok(None);
            }
            if let Some(d) = self.read(end - now)? {
                if pred(&d) {
                    return Ok(Some(d));
                }
            }
        }
    }

    /// RØDE Central's request helper (fcn 0x1800c0c90): write, then wait for a reply whose first byte is
    /// `expect` (other reports are skipped). If the unit vanished (it re-enumerates when it switches
    /// modes), reopen it and resend once.
    fn cmd(
        &mut self,
        rid: u8,
        payload: &[u8],
        expect: Option<u8>,
        timeout: f64,
    ) -> Result<Option<Vec<u8>>> {
        match self.cmd_once(rid, payload, expect, timeout) {
            Err(e) if gone(&e) => {
                self.log(&format!("unit went away ({e}), reopening"));
                self.open(30.0)?;
                self.cmd_once(rid, payload, expect, timeout)
            }
            r => r,
        }
    }

    fn cmd_once(
        &mut self,
        rid: u8,
        payload: &[u8],
        expect: Option<u8>,
        timeout: f64,
    ) -> Result<Option<Vec<u8>>> {
        self.drain()?;
        self.write(rid, payload)?;
        match expect {
            None => Ok(None),
            Some(x) => self.wait_for(&|d: &[u8]| d.first() == Some(&x), timeout),
        }
    }
}

fn gone(e: &Error) -> bool {
    match e {
        Error::DeviceGone(_) => true,
        Error::Io(io) => matches!(
            io.raw_os_error(),
            Some(libc::EIO) | Some(libc::ENODEV) | Some(libc::EPIPE)
        ),
        _ => false,
    }
}

/// A real unit over hidraw; re-finds it by USB serial when it re-enumerates.
pub struct HidTransport {
    pub serial: String,
    pub unit: String,
    dev: Option<Dev>,
    log: Box<dyn FnMut(&str)>,
}

impl HidTransport {
    pub fn new(serial: &str, unit: &str, log: Box<dyn FnMut(&str)>) -> Self {
        HidTransport {
            serial: serial.into(),
            unit: unit.into(),
            dev: None,
            log,
        }
    }
}

impl Transport for HidTransport {
    fn open(&mut self, timeout: f64) -> Result<()> {
        self.close();
        let end = Instant::now() + Duration::from_secs_f64(timeout);
        loop {
            for (d, s, _) in find_units(Some(&self.unit)) {
                if s == self.serial {
                    if let Ok(dev) = d.open() {
                        (self.log)(&format!("opened {} PID {:04x} ({})", d.node, d.pid, d.name));
                        self.dev = Some(dev);
                        return Ok(());
                    }
                }
            }
            if Instant::now() > end {
                return Err(update_err(format!(
                    "unit {} did not come back within {timeout:.0} s",
                    self.serial
                )));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn close(&mut self) {
        self.dev = None;
    }

    fn is_open(&self) -> bool {
        self.dev.is_some()
    }

    fn pid(&self) -> Option<u16> {
        self.dev.as_ref().map(|d| d.pid())
    }

    fn out_size(&self, rid: u8) -> Option<usize> {
        self.dev.as_ref()?.info.size(ReportKind::Out, rid)
    }

    fn send(&mut self, rid: u8, padded: &[u8]) -> Result<()> {
        let dev = self
            .dev
            .as_mut()
            .ok_or_else(|| Error::DeviceGone("unit is not open".into()))?;
        dev.write_report(rid, padded, padded.len())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let dev = self
            .dev
            .as_mut()
            .ok_or_else(|| Error::DeviceGone("unit is not open".into()))?;
        Ok(dev.read_report(timeout)?.map(|(_, d)| d))
    }

    fn log(&mut self, line: &str) {
        (self.log)(line)
    }

    fn after_update(&mut self) -> UnitState {
        for (d, s, _) in find_units(Some(&self.unit)) {
            if s == self.serial {
                let bcd =
                    std::fs::read_to_string(usb_dir(&d.node).join("bcdDevice")).unwrap_or_default();
                return (Some(d.pid), device_version(bcd.trim()));
            }
        }
        (None, None)
    }
}

// --------------------------------------------------------------------------
// the update
// --------------------------------------------------------------------------

/// Receives (phase "radio" | "main", fraction done).
pub type ProgressFn<'a> = Box<dyn FnMut(&str, f64) + 'a>;
/// Writes one line to the update log.
pub type Logger = Box<dyn FnMut(&str)>;

/// Runs an update over a [`Transport`]. `sleep` is injectable so the simulator runs at full speed.
pub struct Updater<'a, T: Transport> {
    pub link: &'a mut T,
    pub pkg: &'a Package,
    pub progress: ProgressFn<'a>,
    pub sleep: Box<dyn FnMut(f64) + 'a>,
}

/// RØDE Central runs the update from a 1 s timer and sleeps 1 s after the radio phase, so there are
/// always >= 1–2 s between steps. The RX needs that: sending the first main block 2 ms after the radio
/// verify got no answer (log 2026-11-10 11:10).
pub const STEP_PAUSE: f64 = 1.0;
pub const AFTER_RADIO_PAUSE: f64 = 2.0;
/// RØDE Central retries a failed state up to 5 times, restarting with 'R' → 'O'.
pub const ATTEMPTS: u32 = 3;

impl<'a, T: Transport> Updater<'a, T> {
    pub fn new(link: &'a mut T, pkg: &'a Package) -> Self {
        Updater {
            link,
            pkg,
            progress: Box::new(|_, _| {}),
            sleep: Box::new(|s| std::thread::sleep(Duration::from_secs_f64(s))),
        }
    }

    fn ok(reply: Option<Vec<u8>>, what: &str) -> Result<Vec<u8>> {
        let r = reply.ok_or_else(|| update_err(format!("{what}: no reply")))?;
        if r.len() < 2 || r[1] != ACK {
            return Err(update_err(format!(
                "{what}: device refused ({})",
                hex(&r[..r.len().min(8)])
            )));
        }
        Ok(r)
    }

    /// Phase 1: enter update mode. Returns the resume offset for the main image.
    pub fn start(&mut self) -> Result<usize> {
        let r = Self::ok(
            self.link.cmd(1, b"O\x01", Some(b'O'), 5.0)?,
            "'O' (start update)",
        )?;
        let mut offset = be32(r.get(2..).unwrap_or(&[])) as usize;
        self.link
            .log(&format!("resume offset for the main image: {offset:#x}"));
        if offset >= self.pkg.main.len() {
            self.link.log("offset beyond image size, starting from 0");
            offset = 0;
        }
        Ok(offset)
    }

    fn part_crc(&mut self, addr: u32, len: usize) -> Result<u32> {
        let mut q = b"A".to_vec();
        q.extend_from_slice(&addr.to_le_bytes());
        q.extend_from_slice(&(len as u32).to_le_bytes());
        let r = Self::ok(
            self.link.cmd(3, &q, Some(b'A'), 2.0)?,
            &format!("CRC query at {addr:#x}"),
        )?;
        Ok(le32(r.get(2..).unwrap_or(&[])))
    }

    fn send_part(&mut self, part: Option<&Part>) -> Result<()> {
        let empty: Vec<u8> = Vec::new();
        let (data, addr) = match part {
            Some(p) => (&p.padded, p.addr),
            None => (&empty, 0),
        };
        let mut hdr = b"N".to_vec();
        hdr.extend_from_slice(&addr.to_le_bytes());
        hdr.extend_from_slice(&(data.len() as u32).to_le_bytes());
        hdr.extend_from_slice(&[part.is_some() as u8, part.is_none() as u8, 0]);
        Self::ok(
            self.link.cmd(3, &hdr, Some(b'N'), 2.0)?,
            &format!("part header {addr:#x}"),
        )?;
        let mut sent = 0usize;
        while sent < data.len() {
            let credit = self.wait_credit(addr, sent)?;
            let n = credit.min(data.len() - sent);
            let mut off = sent;
            while off < sent + n {
                let chunk = &data[off..(off + 1070).min(sent + n)];
                let mut pk = (chunk.len() as u16).to_le_bytes().to_vec();
                pk.extend_from_slice(chunk);
                self.link.write(5, &pk)?;
                off += 1070;
            }
            sent += n;
            (self.progress)("radio", sent as f64 / data.len() as f64);
        }
        let done =
            move |d: &[u8]| d.len() >= 6 && d[0] == b'N' && d[1] == ACK && le32(&d[2..6]) == addr;
        if self.link.wait_for(&done, 5.0)?.is_none() {
            return Err(update_err(format!("no completion for part at {addr:#x}")));
        }
        Ok(())
    }

    /// Wait for a 'C' report and return how many bytes we may send (RØDE Central fcn f9470).
    fn wait_credit(&mut self, addr: u32, sent: usize) -> Result<usize> {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            let left = (end - Instant::now()).as_secs_f64();
            let Some(d) = self
                .link
                .wait_for(&|d: &[u8]| d.first() == Some(&b'C') && d.len() >= 14, left)?
            else {
                break;
            };
            let (a, b, tag) = (
                le32(&d[2..6]) as i64,
                le32(&d[6..10]) as i64,
                le32(&d[10..14]),
            );
            if tag != addr && tag != 0 {
                continue;
            }
            if !(0..15000).contains(&(a - b)) {
                continue;
            }
            let room = b - sent as i64 + 15000;
            if (0..=15000).contains(&room) && room - 1000 > 0 {
                return Ok((room - 1000) as usize);
            }
        }
        Err(update_err(format!(
            "device stopped asking for data at {addr:#x} (+{sent})"
        )))
    }

    /// Phase 2: the Nordic radio chip. Returns false when it already had this image.
    pub fn radio(&mut self, force: bool) -> Result<bool> {
        self.link.cmd(1, b"h", Some(b'h'), 5.0)?; // RØDE Central ignores this reply
        let parts = self.pkg.parts.clone();
        let mut same = true;
        for p in &parts {
            if self.part_crc(p.addr, p.raw.len())? != crc32fast::hash(&p.raw) {
                same = false;
                break;
            }
        }
        if same && !force {
            self.link
                .log("Nordic chip already has this image, skipping");
            (self.progress)("radio", 1.0);
            return Ok(false);
        }
        self.send_part(None)?;
        for p in &parts {
            self.send_part(Some(p))?;
            let got = self.part_crc(p.addr, p.padded.len())?;
            let want = crc32fast::hash(&p.padded);
            if got != want {
                return Err(update_err(format!(
                    "Nordic part {:#x} CRC mismatch after writing ({got:08x} != {want:08x})",
                    p.addr
                )));
            }
        }
        Ok(true)
    }

    /// Phase 3: the main MCU image, from `offset`.
    pub fn main(&mut self, mut offset: usize) -> Result<()> {
        let img = &self.pkg.main;
        while img.len() - offset >= 0x600 {
            self.link.write(7, &img[offset..offset + 0x600])?;
            let r = self
                .link
                .wait_for(
                    &|d: &[u8]| matches!(d.first(), Some(b'A' | b'C' | b'S')),
                    4.0,
                )?
                .ok_or_else(|| {
                    update_err(format!("main image: no answer at offset {offset:#x}"))
                })?;
            if r[0] == b'S' {
                let status = r.get(1).map(|&c| c as char).unwrap_or('?');
                if "DOR".contains(status) {
                    self.link
                        .log(&format!("main image: device reports done ('{status}')"));
                    break;
                }
                return Err(update_err(format!(
                    "main image: device reports failure '{status}' at {offset:#x}"
                )));
            }
            let new = be32(r.get(2..).unwrap_or(&[])) as usize;
            if new > img.len() {
                return Err(update_err(format!(
                    "main image: device asked for offset {new:#x} beyond the image"
                )));
            }
            offset = new;
            (self.progress)("main", offset as f64 / img.len() as f64);
        }
        (self.progress)("main", 1.0);
        self.link.cmd(1, b"Z", None, 2.0)?;
        (self.sleep)(5.0);
        Ok(())
    }

    /// Phase 4: let the unit restart into the new firmware.
    pub fn finish(&mut self) -> Result<()> {
        self.link.open(30.0)?;
        (self.sleep)(4.0);
        if !self
            .link
            .pid()
            .is_some_and(|p| UPDATE_MODE_PIDS.contains(&p))
        {
            Self::ok(self.link.cmd(1, b"7", Some(b'7'), 2.0)?, "'7' (finish)")?;
        }
        self.link.cmd(1, b"Y", None, 2.0)?;
        (self.sleep)(1.0);
        self.link.close();
        (self.sleep)(3.0);
        self.link.open(60.0)
    }

    fn abort(&mut self) {
        if let Err(e) = self.link.cmd(1, b"R", Some(b'R'), 2.0) {
            self.link.log(&format!("'R' failed: {e}"));
        }
    }

    fn attempt(&mut self, force_radio: bool) -> Result<()> {
        self.link.open(30.0)?;
        let offset = self.start()?;
        (self.sleep)(STEP_PAUSE);
        self.radio(force_radio)?; // cmd() reopens if 'O' re-enumerated it
        (self.sleep)(AFTER_RADIO_PAUSE);
        self.main(offset)?;
        self.finish()
    }

    /// The whole update, retried up to [`ATTEMPTS`] times. Returns (PID, version) afterwards.
    pub fn run(&mut self, force_radio: bool) -> Result<UnitState> {
        let mut last = None;
        for attempt in 1..=ATTEMPTS {
            self.link.log(&format!("attempt {attempt}/{ATTEMPTS}"));
            match self.attempt(force_radio && attempt == 1) {
                Ok(()) => {
                    let v = self.link.after_update();
                    self.link.close();
                    return Ok(v);
                }
                Err(Error::Update(msg)) => {
                    self.link.log(&format!("attempt {attempt} failed: {msg}"));
                    if self.link.is_open() {
                        self.abort();
                    }
                    self.link.close();
                    (self.sleep)(3.0);
                    last = Some(Error::Update(msg));
                }
                Err(e) => {
                    // not a step failure (e.g. the unit vanished mid-transfer): stop, like the Python version
                    if self.link.is_open() {
                        self.abort();
                    }
                    self.link.close();
                    return Err(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| update_err("update failed")))
    }
}

// --------------------------------------------------------------------------
// checks, dry run, logging
// --------------------------------------------------------------------------

pub fn battery(info: &hid::DevInfo) -> Result<u8> {
    let mut d = info.open()?;
    Ok(hid::get_param(&mut d, 0x00)?.first().copied().unwrap_or(0))
}

fn le_hex(v: u32) -> String {
    v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// The dry run: every step and packet header, without touching anything.
pub fn plan(pkg: &Package) -> String {
    let base = |p: &Path| {
        p.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut lines = vec![format!(
        "Package: {} {} (main {}, {} B; Nordic {})",
        pkg.unit.to_uppercase(),
        pkg.version,
        base(&pkg.main_path),
        pkg.main.len(),
        base(&pkg.nordic_path)
    )];
    lines.push("1. report 1 'O' 01 → expect [O,'A', BE resume offset]".into());
    lines.push("2. report 1 'h'".into());
    for p in &pkg.parts {
        lines.push(format!(
            "   report 3 'A' {} {} → compare with CRC32 {:08x}",
            le_hex(p.addr),
            le_hex(p.raw.len() as u32),
            crc32fast::hash(&p.raw)
        ));
    }
    lines.push(
        "   if any differ: report 3 'N' 00000000 00000000 00 01 00 → 'A', wait [N,'A',0]".into(),
    );
    for p in &pkg.parts {
        lines.push(format!(
            "   report 3 'N' {} {} 01 00 00, {} report-5 packets on 'C' credit, wait [N,'A',{:#x}], verify CRC32 {:08x}",
            le_hex(p.addr),
            le_hex(p.padded.len() as u32),
            p.padded.len().div_ceil(1070),
            p.addr,
            crc32fast::hash(&p.padded)
        ));
    }
    lines.push(format!(
        "3. report 7: up to {} blocks of 0x600 from the resume offset, then report 1 'Z', wait 5 s",
        pkg.main.len() / 0x600
    ));
    lines.push("4. wait 4 s, report 1 '7' → 'A', report 1 'Y', wait for the unit to return".into());
    lines.join("\n")
}

pub fn log_dir() -> PathBuf {
    firmware::fw_root().join("logs")
}

/// Open a log file for an update of the unit with this serial. Returns (path, logger).
pub fn open_log(serial: &str) -> Result<(PathBuf, Logger)> {
    std::fs::create_dir_all(log_dir())?;
    let path = log_dir().join(format!(
        "update-{serial}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let log = Box::new(move |s: &str| {
        let _ = writeln!(f, "{} {s}", chrono::Local::now().format("%H:%M:%S%.3f"));
    });
    Ok((path, log))
}

/// The newest verified wgo2_5 package in the cache.
pub fn default_pkg_dir() -> Result<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(firmware::fw_root())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("wgo2_5-"))
                })
                .filter(|p| p.join(".verified").exists())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs.pop().ok_or_else(|| {
        update_err("no verified wgo2_5 package; run 'micdock-firmware download' (or the app's Download button) first")
    })
}

// --------------------------------------------------------------------------
// simulator tests (a model of the unit's update protocol, ported from the Python sim_test.py)
// --------------------------------------------------------------------------
#[cfg(test)]
mod sim {
    use super::*;
    use std::collections::{BTreeSet, HashMap, VecDeque};

    /// A simulated GO II update target.
    struct Sim {
        flash: HashMap<u32, u8>,
        q: VecDeque<Vec<u8>>,
        resume: usize,
        main_fail: bool,
        reenumerate: bool,
        gone_once: bool,
        present: bool,
        main_rx: Vec<u8>,
        main_seen: BTreeSet<usize>,
        main_next: Option<usize>,
        cur: Option<(u32, usize, Vec<u8>)>,
        sent_cmds: String,
        r3: String,
    }

    impl Sim {
        fn new(
            pkg: &Package,
            nordic_has_new: bool,
            resume: usize,
            main_fail: bool,
            reenumerate: bool,
        ) -> Sim {
            let mut flash = HashMap::new();
            if nordic_has_new {
                for p in &pkg.parts {
                    for (i, b) in p.padded.iter().enumerate() {
                        flash.insert(p.addr + i as u32, *b);
                    }
                }
            }
            Sim {
                flash,
                q: VecDeque::new(),
                resume,
                main_fail,
                reenumerate,
                gone_once: false,
                present: false,
                main_rx: vec![0; pkg.main.len()],
                main_seen: BTreeSet::new(),
                main_next: None,
                cur: None,
                sent_cmds: String::new(),
                r3: String::new(),
            }
        }

        fn reply(&mut self, parts: &[&[u8]]) {
            self.q.push_back(parts.concat());
        }

        fn credit(&mut self) {
            let (addr, _, buf) = self.cur.as_ref().unwrap();
            let received = buf.len() as u32;
            let mut r = b"C\x00".to_vec();
            for v in [received, received, *addr] {
                r.extend_from_slice(&v.to_le_bytes());
            }
            r.extend_from_slice(&[0; 46]);
            self.q.push_back(r);
        }
    }

    impl Transport for Sim {
        fn open(&mut self, _timeout: f64) -> Result<()> {
            self.present = true;
            Ok(())
        }
        fn close(&mut self) {
            self.present = false;
        }
        fn is_open(&self) -> bool {
            self.present
        }
        fn pid(&self) -> Option<u16> {
            self.present.then_some(0x29)
        }
        fn out_size(&self, rid: u8) -> Option<usize> {
            match rid {
                1 | 3 => Some(16),
                5 => Some(1072),
                7 => Some(1536),
                _ => None,
            }
        }
        fn send(&mut self, rid: u8, p: &[u8]) -> Result<()> {
            if !self.present {
                return Err(Error::DeviceGone("sim re-enumerated".into()));
            }
            match rid {
                1 => {
                    let c = p[0] as char;
                    self.sent_cmds.push(c);
                    match c {
                        'O' => {
                            assert_eq!(p[1], 1);
                            let r = (self.resume as u32).to_be_bytes();
                            self.reply(&[b"OA", &r]);
                            if self.reenumerate && !self.gone_once {
                                self.gone_once = true;
                                self.present = false; // next write fails until reopened
                            }
                        }
                        'h' => self.reply(&[b"hA"]),
                        '7' => self.reply(&[b"7A"]),
                        'R' => self.reply(&[b"RA"]),
                        _ => {}
                    }
                }
                3 => {
                    let cmd = p[0];
                    let addr = le32(&p[1..5]);
                    let size = le32(&p[5..9]) as usize;
                    let (a, b) = (p[9], p[10]);
                    self.r3.push(cmd as char);
                    match cmd {
                        b'A' => {
                            let data: Vec<u8> = (0..size)
                                .map(|i| *self.flash.get(&(addr + i as u32)).unwrap_or(&0))
                                .collect();
                            let crc = crc32fast::hash(&data).to_le_bytes();
                            self.reply(&[b"AA", &crc]);
                        }
                        b'N' => {
                            assert!(size.is_multiple_of(256) && addr.is_multiple_of(256));
                            self.reply(&[b"NA", &[0; 10]]);
                            if b == 1 {
                                assert_eq!((addr, size, a), (0, 0, 0));
                                self.flash.clear();
                                self.reply(&[b"NA", &0u32.to_le_bytes()]);
                            } else {
                                assert_eq!(a, 1);
                                self.cur = Some((addr, size, Vec::new()));
                                self.credit();
                            }
                        }
                        _ => {}
                    }
                }
                5 => {
                    let ln = u16::from_le_bytes([p[0], p[1]]) as usize;
                    assert!(ln > 0 && ln <= 1070);
                    let (addr, size, buf) = self.cur.as_mut().expect("data without header");
                    let before = buf.len();
                    buf.extend_from_slice(&p[2..2 + ln]);
                    assert!(buf.len() <= *size);
                    let (addr, done, crossed) =
                        (*addr, buf.len() == *size, buf.len() / 4096 != before / 4096);
                    if done {
                        let (a, _, buf) = self.cur.take().unwrap();
                        for (i, v) in buf.iter().enumerate() {
                            self.flash.insert(a + i as u32, *v);
                        }
                        self.reply(&[b"NA", &addr.to_le_bytes()]);
                    } else if crossed {
                        self.credit();
                    }
                }
                7 => {
                    let off = self.main_next.unwrap_or(self.resume);
                    let end = (off + 0x600).min(self.main_rx.len());
                    self.main_rx[off..end].copy_from_slice(&p[..end - off]);
                    self.main_seen.insert(off);
                    let nxt = off + 0x600;
                    if self.main_fail && nxt > self.main_rx.len() / 2 {
                        self.reply(&[b"SF"]);
                    } else if self.main_rx.len() - nxt < 0x600 {
                        self.reply(&[b"SD"]);
                    } else {
                        self.main_next = Some(nxt);
                        self.reply(&[b"A", &[0], &(nxt as u32).to_be_bytes()]);
                    }
                }
                _ => {}
            }
            Ok(())
        }
        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>> {
            Ok(self.q.pop_front())
        }
        fn log(&mut self, _line: &str) {}
        fn after_update(&mut self) -> UnitState {
            (Some(0x29), Some((2, 5, 0)))
        }
    }

    /// A small synthetic package: an OTA image with the right header and a two-part Nordic HEX.
    fn synthetic_package(unit: &str) -> (tempdir::Dir, Package) {
        let dir = tempdir::Dir::new();
        let mut main = b"ACTTEST0".to_vec();
        main.resize(0x200, 0);
        main.extend_from_slice(
            format!("<board_name>wgo25_{unit}</board_name>\n<version_name>2.50</version_name>")
                .as_bytes(),
        );
        let mut x: u32 = 12345;
        while main.len() < 0x600 * 300 + 0x123 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            main.push((x >> 16) as u8);
        }
        std::fs::write(dir.0.join(format!("wgo25_{unit}_2.50_test_ota.bin")), &main).unwrap();
        let mut hexfile = String::new();
        let rec = |hexfile: &mut String, addr: u16, typ: u8, data: &[u8]| {
            let mut b = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, typ];
            b.extend_from_slice(data);
            let ck = 0u8.wrapping_sub(b.iter().fold(0u8, |a, v| a.wrapping_add(*v)));
            b.push(ck);
            hexfile.push(':');
            hexfile.push_str(&b.iter().map(|v| format!("{v:02X}")).collect::<String>());
            hexfile.push('\n');
        };
        // part 1 at 0x0000: 0x1234 bytes; part 2 at 0x1_0000 (extended address): 0x2000 + 7 bytes
        for (base, len) in [(0u32, 0x1234usize), (0x1_0000, 0x2007)] {
            rec(
                &mut hexfile,
                0,
                4,
                &[(base >> 24) as u8, (base >> 16) as u8],
            );
            let mut i = 0;
            while i < len {
                let n = 16.min(len - i);
                let data: Vec<u8> = (0..n)
                    .map(|k| ((i + k) * 7 + base as usize) as u8)
                    .collect();
                rec(&mut hexfile, i as u16, 0, &data);
                i += n;
            }
        }
        rec(&mut hexfile, 0, 1, &[]);
        std::fs::write(
            dir.0.join(format!("wgo2.5_nordic_{unit}_0.26_test.hex")),
            hexfile,
        )
        .unwrap();
        let pkg = Package::load(&dir.0, unit).unwrap();
        (dir, pkg)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "micdock-fwtest-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn run(sim: &mut Sim, pkg: &Package) -> Result<UnitState> {
        let mut up = Updater::new(sim, pkg);
        up.sleep = Box::new(|_| {});
        up.run(false)
    }

    fn check_full_update(pkg: &Package) {
        let mut sim = Sim::new(pkg, false, 0, false, false);
        run(&mut sim, pkg).expect("update failed");
        for p in &pkg.parts {
            let got: Vec<u8> = (0..p.padded.len())
                .map(|i| sim.flash[&(p.addr + i as u32)])
                .collect();
            assert_eq!(got, p.padded, "nordic data mismatch at {:#x}", p.addr);
        }
        let covered = sim.main_seen.len() * 0x600;
        assert_eq!(
            &sim.main_rx[..covered],
            &pkg.main[..covered],
            "main image mismatch"
        );
        assert_eq!(sim.sent_cmds, "OhZ7Y");
        assert!(sim.r3.starts_with('A') && sim.r3.contains('N'));
    }

    #[test]
    fn full_update_tx_and_rx() {
        for unit in ["tx", "rx"] {
            let (_dir, pkg) = synthetic_package(unit);
            assert_eq!(pkg.parts.len(), 2);
            assert_eq!(pkg.parts[1].addr, 0x1_0000);
            assert_eq!(pkg.parts[1].padded.len(), 0x2100);
            check_full_update(&pkg);
        }
    }

    #[test]
    fn radio_already_current_is_skipped() {
        let (_dir, pkg) = synthetic_package("tx");
        let mut sim = Sim::new(&pkg, true, 0, false, false);
        run(&mut sim, &pkg).unwrap();
        assert!(!sim.r3.contains('N') && sim.r3.contains('A'), "{}", sim.r3);
    }

    #[test]
    fn resumes_main_image_at_offset() {
        let (_dir, pkg) = synthetic_package("tx");
        let mut sim = Sim::new(&pkg, false, 0x600 * 100, false, false);
        run(&mut sim, &pkg).unwrap();
        assert_eq!(*sim.main_seen.iter().next().unwrap(), 0x600 * 100);
    }

    #[test]
    fn device_failure_gives_error_and_abort() {
        let (_dir, pkg) = synthetic_package("tx");
        let mut sim = Sim::new(&pkg, false, 0, true, false);
        let err = run(&mut sim, &pkg).unwrap_err();
        assert!(matches!(err, Error::Update(_)), "{err}");
        assert!(sim.sent_cmds.ends_with('R'), "{}", sim.sent_cmds);
        assert_eq!(sim.sent_cmds.matches('O').count(), ATTEMPTS as usize);
    }

    #[test]
    fn reenumeration_after_start_is_handled() {
        let (_dir, pkg) = synthetic_package("tx");
        let mut sim = Sim::new(&pkg, false, 0, false, true);
        run(&mut sim, &pkg).unwrap();
    }

    /// The same full update with RØDE's real 2.5.0 package, if it is in the cache.
    #[test]
    fn full_update_with_real_package_if_cached() {
        let Ok(dir) = default_pkg_dir() else { return };
        for unit in ["tx", "rx"] {
            let pkg = Package::load(&dir, unit).unwrap();
            check_full_update(&pkg);
        }
    }

    #[test]
    fn intel_hex_errors() {
        let d = tempdir::Dir::new();
        let p = d.0.join("x.hex");
        std::fs::write(&p, ":0100000001FF\n").unwrap();
        assert!(hex_runs(&p)
            .unwrap_err()
            .to_string()
            .contains("bad checksum"));
        std::fs::write(&p, "0100000001FE\n").unwrap();
        assert!(hex_runs(&p).unwrap_err().to_string().contains("no ':'"));
    }
}
