// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Demo mode: made-up units and recordings, no device access and no network. Used for the manual's
//! screenshots (`MICDOCK_SCREENSHOTS=<folder> micdock` takes them and quits) and for trying the app
//! without hardware (`MICDOCK_DEMO=1 micdock`).

use std::path::PathBuf;

use chrono::TimeZone;
use micdock_core::devices::{DeviceStatus, Role, TxLink};
use micdock_core::hid::{ButtonMode, ChannelMode, Gain, RecMode, FINE_GAIN};
use micdock_core::recordings::{Container, Recording};

pub const RX: &str = "2248DE40";
pub const TX1: &str = "2288A001";
pub const TX2: &str = "2288A002";

/// Where to save screenshots (implies demo mode).
pub fn screenshot_dir() -> Option<PathBuf> {
    std::env::var_os("MICDOCK_SCREENSHOTS").map(PathBuf::from)
}

pub fn active() -> bool {
    std::env::var_os("MICDOCK_DEMO").is_some() || screenshot_dir().is_some()
}

fn now() -> f64 {
    chrono::Utc::now().timestamp() as f64
}

fn unit(serial: &str, pid: u16, role: Role, node: &str, name: &str, battery: u8) -> DeviceStatus {
    let product = if role == Role::Tx {
        "RØDE Wireless GO II TX"
    } else {
        "RØDE Wireless GO II RX"
    };
    DeviceStatus {
        node: node.into(),
        pid,
        role,
        product: product.into(),
        serial: serial.into(),
        fw: "2.50".into(),
        fw_version: Some((2, 5, 0)),
        storage: false,
        battery: Some(battery),
        name: Some(name.into()),
        clock: Some(now() as u32),
        clock_at: Some(now()),
        error: None,
        rec: None,
        button: None,
        channel: None,
        pad: None,
        power_saver: None,
        powered: None,
        safety: None,
        rx_button: None,
        gain: None,
        id: None,
        links: Vec::new(),
    }
}

/// A receiver with two linked transmitters, both connected by USB.
pub fn units() -> Vec<DeviceStatus> {
    let mut rx = unit(RX, 0x002A, Role::Rx, "/dev/hidraw8", "Camera A", 100);
    rx.channel = Some(ChannelMode::Split);
    rx.power_saver = Some(true);
    rx.powered = Some(true);
    rx.safety = Some(false);
    rx.rx_button = Some(micdock_core::hid::RxButton::PowerSaver);
    rx.gain = Some(Gain {
        mode: FINE_GAIN,
        step: 7,
    });
    rx.links = vec![
        TxLink {
            slot: 1,
            serial: TX1.into(),
            id: 1,
            name: "Presenter".into(),
            battery: Some(86),
            pad: Some(false),
            firmware: "2.5.0".into(),
            rec: Some(RecMode::Always),
            button: Some(ButtonMode::Marker),
            uncompressed: Some(true),
            led_dim: Some(false),
        },
        TxLink {
            slot: 2,
            serial: TX2.into(),
            id: 2,
            name: "Guest".into(),
            battery: Some(64),
            pad: Some(true),
            firmware: "2.5.0".into(),
            rec: Some(RecMode::Backup),
            button: Some(ButtonMode::Mute),
            uncompressed: Some(false),
            led_dim: Some(true),
        },
    ];
    let mut tx1 = unit(TX1, 0x0029, Role::Tx, "/dev/hidraw9", "Presenter", 86);
    tx1.rec = Some(RecMode::Always);
    tx1.button = Some(ButtonMode::Marker);
    tx1.pad = Some(false);
    let mut tx2 = unit(TX2, 0x0029, Role::Tx, "/dev/hidraw10", "Guest", 64);
    tx2.rec = Some(RecMode::Backup);
    tx2.button = Some(ButtonMode::Mute);
    tx2.pad = Some(true);
    vec![rx, tx1, tx2]
}

fn wav(n: u32, end: f64, secs: f64, markers: Vec<u32>) -> Recording {
    let frames = (secs * 48000.0) as u64;
    Recording {
        path: PathBuf::from(format!("/media/demo/2288-A001/REC{n:05}.WAV")),
        name: format!("REC{n:05}.WAV"),
        size: 32768 + frames * 3,
        mtime: end,
        rate: 48000,
        bits: 24,
        channels: 1,
        width: 3,
        byteswap: false,
        frame_bytes: 3,
        frames,
        markers,
        container: Container::Wav {
            data_offset: 32768,
            data_len: frames * 3,
        },
    }
}

fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
    chrono::Local
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .single()
        .map(|t| t.timestamp() as f64)
        .unwrap_or(0.0)
}

/// The first transmitter's recordings: a long interview (three 31-minute pieces), a talk with markers and
/// a short sound check.
pub fn sessions() -> Vec<Vec<Recording>> {
    let start = local(2026, 11, 7, 9, 30);
    let a = wav(51, start + 1800.0, 1800.0, vec![]);
    let b = wav(52, start + 3602.0, 1800.0, vec![48000 * 600]);
    let c = wav(53, start + 4224.0, 620.0, vec![]);
    let talk_end = local(2026, 11, 8, 14, 12) + 754.0;
    let talk = wav(54, talk_end, 754.0, vec![48000 * 95, 48000 * 410]);
    let check_end = local(2026, 11, 10, 11, 2) + 42.0;
    let check = wav(55, check_end, 42.0, vec![]);
    vec![vec![a, b, c], vec![talk], vec![check]]
}

/// The folder shown for the demo drive.
pub fn folder() -> PathBuf {
    PathBuf::from("/media/demo/2288-A001")
}
