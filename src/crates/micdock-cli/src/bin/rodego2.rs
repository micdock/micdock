// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! rodego2 — talk to a RØDE Wireless GO II over Linux hidraw, using the settings protocol recovered from
//! RØDE Central's rode_bridge.dll. Same commands and output as the Python version.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use micdock_core::hid::{self, ButtonMode, ChannelMode, Dev, DevInfo, RecMode, ERASE_PIDS};
use micdock_core::{devices, Error};

const ABOUT: &str = "\
rodego2 - talk to a RØDE Wireless GO II (and related RØDE wireless devices)
over Linux hidraw, using the settings protocol recovered from RØDE Central's
rode_bridge.dll.

Protocol (see PROTOCOL.md):
  OUT report 9:  GET  [param, 0x01, sub, 0...]
                 SET  [param, sub,  data..., 0...]
  IN  report  :  [param-echo, 0x41 ('A' = ACK), data...]";

#[derive(Parser)]
#[command(name = "rodego2", version, about = ABOUT, after_help = "Use --dev /dev/hidrawN to pick a device when more than one is attached.")]
struct Cli {
    /// hidraw node, e.g. /dev/hidraw3
    #[arg(long, global = true)]
    dev: Option<String>,
    /// print every reply received
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// show RØDE hidraw devices and report sizes
    List,
    /// read a parameter (hex or decimal id)
    Get {
        #[arg(value_parser = num)]
        param: u32,
        #[arg(short = 'n', long = "len", value_parser = num)]
        len: Option<u32>,
        #[arg(long, value_parser = num, default_value = "0")]
        sub: u32,
    },
    /// write a parameter, e.g. set 0x0a 01
    Set {
        #[arg(value_parser = num)]
        param: u32,
        #[arg(required = true)]
        data: Vec<String>,
        #[arg(long, value_parser = num, default_value = "0")]
        sub: u32,
    },
    /// read every param id and print the ACKed ones (read-only)
    Scan {
        #[arg(long, value_parser = num, default_value = "0x60")]
        max: u32,
        /// per-param wait (s)
        #[arg(short, long, default_value_t = 0.4)]
        timeout: f64,
    },
    /// read or set the device name (param 3)
    Name { new: Option<String> },
    /// TX power-button function: mute, marker, none or record
    Button {
        #[arg(value_parser = ["mute", "marker", "none", "record"])]
        mode: Option<String>,
    },
    /// TX: delete ALL on-board recordings (asks for confirmation; irreversible)
    Erase {
        /// don't ask (for scripts)
        #[arg(long)]
        yes: bool,
    },
    /// RX output: merged (both TXs on both channels) or split (TX1 left, TX2 right)
    Mode {
        #[arg(value_parser = ["merged", "split"])]
        mode: Option<String>,
    },
    /// RX power saver (param 0x04): on = backlight off after 10 s, off = always lit
    Powersaver {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// RX output gain (param 0x05): coarse -24/-12/0 dB, fine -30..0 dB in 3 dB steps, or camera preset
    Gain {
        /// level in dB, e.g. -15 (coarse and fine mode)
        #[arg(allow_hyphen_values = true)]
        db: Option<i32>,
        /// switch the gain mode
        #[arg(long, value_parser = ["coarse", "fine", "preset"])]
        mode: Option<String>,
    },
    /// RX safety channel (param 0x07): a 20 dB quieter copy on the 3.5 mm output in merged mode
    Safety {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// RX power button, single click (param 0x0a): powersaver (toggle the backlight) or marker
    Rxbutton {
        #[arg(value_parser = ["powersaver", "marker"])]
        mode: Option<String>,
    },
    /// a transmitter's settings through the receiver (pick the RX with --dev): show, or change some
    Radio {
        /// the receiver's TX slot
        #[arg(value_parser = ["1", "2"])]
        slot: String,
        #[arg(long, value_parser = ["on", "off"])]
        pad: Option<String>,
        #[arg(long, value_parser = ["bright", "dim"])]
        led: Option<String>,
        #[arg(long, value_parser = ["off", "backup", "always", "button"])]
        rec: Option<String>,
        #[arg(long, value_parser = ["mute", "marker", "none", "record"])]
        button: Option<String>,
        /// recording format (used in backup mode)
        #[arg(long, value_parser = ["compressed", "uncompressed"])]
        format: Option<String>,
    },
    /// TX low-sensitivity pad / lav gain (param 0x0e): on = pad, off = normal
    Pad {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// TX on-board record mode: params 0x05 (enabled), 0x0d (always), 0x0f (button)
    Rec {
        #[arg(value_parser = ["off", "backup", "always", "button"])]
        mode: Option<String>,
    },
    /// send a raw report-9 payload, print replies
    Raw {
        #[arg(required = true)]
        data: Vec<String>,
        #[arg(short, long, default_value_t = 1.0)]
        timeout: f64,
    },
    /// send any OUT report (e.g. out 1 51 00); known-destructive commands are refused
    Out {
        #[arg(value_parser = num)]
        report: u32,
        #[arg(required = true)]
        data: Vec<String>,
        #[arg(short, long, default_value_t = 2.0)]
        timeout: f64,
    },
    /// print every input report the device sends
    Listen {
        #[arg(short, long, default_value_t = 0.0)]
        timeout: f64,
    },
}

fn num(s: &str) -> Result<u32, String> {
    let r = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16)
    } else if let Some(o) = s.strip_prefix("0o") {
        u32::from_str_radix(o, 8)
    } else if let Some(b) = s.strip_prefix("0b") {
        u32::from_str_radix(b, 2)
    } else {
        s.parse()
    };
    r.map_err(|e| e.to_string())
}

/// Hex bytes from one or more arguments: "01 02", "01:02", "0102" are all fine.
fn hexbytes(args: &[String]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for a in args {
        let clean: String = a
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ':' && *c != ',')
            .collect();
        if !clean.len().is_multiple_of(2) {
            return Err(format!("odd number of hex digits in {a:?}"));
        }
        for i in (0..clean.len()).step_by(2) {
            out.push(u8::from_str_radix(&clean[i..i + 2], 16).map_err(|e| format!("{a:?}: {e}"))?);
        }
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn monotonic() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn pick(dev_arg: Option<&str>) -> Result<DevInfo, String> {
    if let Some(n) = dev_arg {
        return Ok(DevInfo::from_node(n));
    }
    let all = hid::find_devices();
    let with_settings: Vec<DevInfo> = all
        .iter()
        .filter(|d| d.has_settings_report())
        .cloned()
        .collect();
    let devs = if with_settings.is_empty() {
        all
    } else {
        with_settings
    };
    if devs.is_empty() {
        return Err(
            "No RØDE (VID 19f7) hidraw device found. Is the GO II plugged in \
                    and switched on? See README for the udev rule."
                .into(),
        );
    }
    if devs.len() > 1 {
        eprintln!(
            "Several RØDE devices found, using {} (pick one with --dev):",
            devs[0].node
        );
        for d in &devs {
            eprintln!("   {}  {:04x}:{:04x}  {}", d.node, d.vid, d.pid, d.name);
        }
    }
    Ok(devs[0].clone())
}

/// Report-1 commands that `out` refuses (see PROTOCOL.md).
fn refused(report: u32, payload: &[u8]) -> Option<&'static str> {
    if report != 1 {
        return None;
    }
    match payload {
        [0x51, 0x01, ..] => Some(
            "report 1 'Q' 01 is refused: on 2026-11-10 it erased the recordings of two transmitters and reset \
             their settings. Storage mode ends by unplugging the TX or switching it off and on.",
        ),
        [0x4A, 0x01, ..] => Some("report 1 'J' 01 deletes all recordings; use `rodego2 erase`, which asks first."),
        _ => None,
    }
}

fn run(cli: Cli) -> Result<(), Error> {
    let verbose = cli.verbose;
    let mut printer = |rid: u8, data: &[u8]| println!("  <- id {rid}: {}", hex(data));
    let open = |dev: &Option<String>| -> Result<Dev, Error> {
        let info = pick(dev.as_deref()).map_err(Error::Other)?;
        info.open()
    };
    match cli.cmd {
        Cmd::List => {
            let devs = hid::find_devices();
            if devs.is_empty() {
                println!("No RØDE hidraw devices found.");
            }
            for d in devs {
                let path = std::ffi::CString::new(d.node.clone()).unwrap();
                // SAFETY: valid C string.
                let ok = unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
                let sizes = d
                    .sizes
                    .iter()
                    .map(|((k, id), v)| format!("{}{}:{}B", k.as_str(), id, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{}  {:04x}:{:04x}  {:<28} {}{}",
                    d.node,
                    d.vid,
                    d.pid,
                    d.name,
                    sizes,
                    if ok {
                        ""
                    } else {
                        "   [no permission - install the udev rule]"
                    }
                );
            }
        }
        Cmd::Get { param, len, sub } => {
            let mut d = open(&cli.dev)?;
            let trace: hid::Trace = if verbose { Some(&mut printer) } else { None };
            let mut data =
                hid::get_param_ex(&mut d, param as u8, sub as u8, hid::GET_TIMEOUT, trace)?;
            if let Some(n) = len.filter(|&n| n > 0) {
                data.truncate(n as usize);
            }
            println!("0x{:02x}: {}", param, hex(&data));
        }
        Cmd::Set { param, data, sub } => {
            let data = hexbytes(&data).map_err(Error::Other)?;
            let mut d = open(&cli.dev)?;
            println!("param 0x{:02x} <- {}", param, hex(&data));
            let trace: hid::Trace = if verbose { Some(&mut printer) } else { None };
            hid::set_param_ex(&mut d, param as u8, &data, sub as u8, trace)?;
            println!("ACK");
        }
        Cmd::Scan { max, timeout } => {
            let mut d = open(&cli.dev)?;
            println!(
                "Reading params 0x00..0x{:02x} on {} ({}). Read-only.",
                max, d.info.node, d.info.name
            );
            for p in 0..=max.min(0xFF) {
                match hid::get_param_ex(&mut d, p as u8, 0, timeout, None) {
                    Ok(mut data) => {
                        while data.last() == Some(&0) {
                            data.pop();
                        }
                        println!(
                            "0x{:02x}: {}",
                            p,
                            if data.is_empty() {
                                "00".into()
                            } else {
                                hex(&data)
                            }
                        );
                        let _ = std::io::stdout().flush();
                    }
                    Err(Error::Timeout(_)) => {}
                    Err(Error::NoAck(e)) => {
                        if verbose {
                            println!("0x{p:02x}: {e}");
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Cmd::Name { new } => {
            let mut d = open(&cli.dev)?;
            match new {
                None => {
                    let raw = hid::get_param(&mut d, 3)?;
                    let raw = &raw[..raw.len().min(0x16)];
                    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                    println!("{}", String::from_utf8_lossy(&raw[..end]));
                }
                Some(name) => {
                    let payload = micdock_core::devices::name_payload(&name);
                    hid::set_param(&mut d, 3, &payload)?;
                    let end = payload
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(payload.len());
                    println!("Name set to '{}'", String::from_utf8_lossy(&payload[..end]));
                }
            }
        }
        Cmd::Mode { mode } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x002A {
                return Err(Error::Other(
                    "merged/split is a receiver setting; pick the RX with --dev".into(),
                ));
            }
            let m = match mode.as_deref().and_then(ChannelMode::parse) {
                Some(m) => hid::set_channel_mode(&mut d, m)?,
                None => hid::channel_mode(&mut d)?,
            };
            println!("output: {}", m.as_str());
        }
        Cmd::Powersaver { state } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x002A {
                return Err(Error::Other(
                    "power saver is a receiver setting; pick the RX with --dev".into(),
                ));
            }
            let on = match state.as_deref() {
                Some(s) => hid::set_power_saver(&mut d, s == "on")?,
                None => hid::power_saver(&mut d)?,
            };
            println!("power saver: {}", if on { "on" } else { "off" });
        }
        Cmd::Gain { db, mode } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x002A {
                return Err(Error::Other(
                    "gain is a receiver setting; pick the RX with --dev".into(),
                ));
            }
            let cur = hid::gain(&mut d)?;
            let m = match mode.as_deref() {
                Some("coarse") => hid::COARSE_GAIN,
                Some("fine") => hid::FINE_GAIN,
                Some("preset") => hid::PRESET_GAIN,
                _ => cur.mode,
            };
            let g = match (db, mode.is_some()) {
                (Some(db), _) => {
                    let target = (0..=hid::Gain::max_step(m).unwrap_or(0))
                        .map(|step| hid::Gain { mode: m, step })
                        .find(|g| g.db() == Some(db))
                        .ok_or_else(|| {
                            Error::Other(format!("{db} dB is not a level of this gain mode"))
                        })?;
                    hid::set_gain(&mut d, target)?
                }
                (None, true) => hid::set_gain(&mut d, cur.converted(m))?,
                (None, false) => cur,
            };
            match (g.mode, g.db()) {
                (hid::COARSE_GAIN, Some(db)) => println!("gain: {db} dB (coarse)"),
                (hid::FINE_GAIN, Some(db)) => println!("gain: {db} dB (fine)"),
                (hid::PRESET_GAIN, _) => println!("gain: camera preset {}", g.step),
                _ => println!("gain: mode {} step {}", g.mode, g.step),
            }
        }
        Cmd::Safety { state } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x002A {
                return Err(Error::Other(
                    "the safety channel is a receiver setting; pick the RX with --dev".into(),
                ));
            }
            let on = match state.as_deref() {
                Some(s) => hid::set_safety_channel(&mut d, s == "on")?,
                None => hid::safety_channel(&mut d)?,
            };
            println!("safety channel: {}", if on { "on" } else { "off" });
        }
        Cmd::Rxbutton { mode } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x002A {
                return Err(Error::Other(
                    "this is the receiver's button; pick the RX with --dev".into(),
                ));
            }
            let b = match mode.as_deref().and_then(hid::RxButton::parse) {
                Some(b) => hid::set_rx_button(&mut d, b)?,
                None => hid::rx_button(&mut d)?,
            };
            println!("receiver button: {}", b.as_str());
        }
        Cmd::Radio {
            slot,
            pad,
            led,
            rec,
            button,
            format,
        } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x002A {
                return Err(Error::Other(
                    "transmitter settings go through the receiver; pick the RX with --dev".into(),
                ));
            }
            let slot: u8 = slot.parse().unwrap_or(1);
            let record_param = if slot == 1 { 0x0b } else { 0x0c };
            let read = |d: &mut Dev| -> Result<devices::TxLink, Error> {
                let v = hid::get_param(d, record_param)
                    .map_err(|_| Error::Other(format!("no transmitter linked as TX{slot}")))?;
                Ok(devices::decode_record(&v))
            };
            let cur = read(&mut d)?;
            if let Some(p) = pad {
                hid::radio_set_pad(&mut d, slot, p == "on")?;
            }
            if let Some(l) = led {
                hid::radio_set_led(&mut d, slot, l == "dim")?;
            }
            if rec.is_some() || button.is_some() || format.is_some() {
                let r = rec
                    .as_deref()
                    .and_then(RecMode::parse)
                    .or(cur.rec)
                    .unwrap_or(RecMode::Off);
                let b = button
                    .as_deref()
                    .and_then(ButtonMode::parse)
                    .or(cur.button)
                    .unwrap_or(ButtonMode::Mute);
                let unc = format
                    .map(|f| f == "uncompressed")
                    .or(cur.uncompressed)
                    .unwrap_or(true);
                hid::radio_set_settings(&mut d, slot, r, b, unc)?;
            }
            std::thread::sleep(Duration::from_millis(1500)); // the record catches up over the radio link
            let t = read(&mut d)?;
            let name = |o: Option<&str>| o.unwrap_or("?").to_string();
            println!(
                "TX{slot}: firmware {}  record {}  button {}  format {}  pad {}  LEDs {}",
                t.firmware,
                name(t.rec.map(|m| m.as_str())),
                name(t.button.map(|m| m.as_str())),
                name(
                    t.uncompressed
                        .map(|u| if u { "uncompressed" } else { "compressed" })
                ),
                name(t.pad.map(|p| if p { "on" } else { "off" })),
                name(t.led_dim.map(|d| if d { "dim" } else { "bright" })),
            );
        }
        Cmd::Pad { state } => {
            let mut d = open(&cli.dev)?;
            if d.pid() != 0x0029 {
                return Err(Error::Other(
                    "the pad is a transmitter setting; pick a TX with --dev".into(),
                ));
            }
            let on = match state.as_deref() {
                Some(s) => hid::set_pad(&mut d, s == "on")?,
                None => hid::pad(&mut d)?,
            };
            println!("low-sensitivity pad: {}", if on { "on" } else { "off" });
        }
        Cmd::Button { mode } => {
            let mut d = open(&cli.dev)?;
            let m = match mode.as_deref().and_then(ButtonMode::parse) {
                Some(m) => hid::set_button_mode(&mut d, m)?,
                None => hid::button_mode(&mut d)?,
            };
            println!("power button: {}", m.as_str());
        }
        Cmd::Rec { mode } => {
            let mut d = open(&cli.dev)?;
            let m = match mode.as_deref().and_then(RecMode::parse) {
                Some(m) => hid::set_rec_mode(&mut d, m)?,
                None => hid::rec_mode(&mut d)?,
            };
            println!("on-board recording: {}", m.as_str());
        }
        Cmd::Raw { data, timeout } => {
            let data = hexbytes(&data).map_err(Error::Other)?;
            let mut d = open(&cli.dev)?;
            d.drain();
            d.write9(&data)?;
            let end = Instant::now() + Duration::from_secs_f64(timeout);
            while Instant::now() < end {
                if let Some((rid, rep)) = d.read_report(end - Instant::now())? {
                    println!("<- id {rid}: {}", hex(&rep));
                }
            }
        }
        Cmd::Out {
            report,
            data,
            timeout,
        } => {
            let data = hexbytes(&data).map_err(Error::Other)?;
            if let Some(why) = refused(report, &data) {
                return Err(Error::Other(why.into()));
            }
            let mut d = open(&cli.dev)?;
            let n = d
                .info
                .size(hid::ReportKind::Out, report as u8)
                .ok_or_else(|| Error::Other(format!("device has no OUT report {report}")))?;
            let payload = &data[..data.len().min(n)];
            d.drain();
            d.write_report(report as u8, payload, n)?;
            println!("-> report {report}: {}", hex(payload));
            let end = Instant::now() + Duration::from_secs_f64(timeout);
            while Instant::now() < end {
                if let Some((rid, rep)) = d.read_report(end - Instant::now())? {
                    println!("<- id {rid}: {}", hex(&rep));
                }
            }
        }
        Cmd::Listen { timeout } => {
            let mut d = open(&cli.dev)?;
            let end = (timeout > 0.0).then(|| Instant::now() + Duration::from_secs_f64(timeout));
            while end.is_none_or(|e| Instant::now() < e) {
                if let Some((rid, rep)) = d.read_report(Duration::from_millis(500))? {
                    println!("{:.3} id {rid}: {}", monotonic(), hex(&rep));
                }
            }
        }
        Cmd::Erase { yes } => {
            let mut d = open(&cli.dev)?;
            if !ERASE_PIDS.contains(&d.pid()) {
                return Err(Error::Other(
                    "only a Wireless GO II transmitter has recordings to delete; pick the TX with --dev".into(),
                ));
            }
            if !yes {
                println!(
                    "This permanently deletes EVERY recording on {} ({}).",
                    d.info.name, d.info.node
                );
                println!(
                    "Copy what you want to keep first. If its drive is mounted, unmount it first."
                );
                print!("Type DELETE to continue: ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if line.trim() != "DELETE" {
                    return Err(Error::Other("cancelled".into()));
                }
            }
            let reply = hid::erase_recordings(&mut d)?;
            println!(
                "deleted (TX replied {}). The transmitter may restart for a moment.",
                hex(&reply[..reply.len().min(2)])
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "Permission denied on the hidraw node. Install 70-rode.rules (see README) or run once with sudo to test."
            );
            ExitCode::FAILURE
        }
        Err(e @ (Error::Timeout(_) | Error::NoAck(_) | Error::DeviceGone(_))) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
