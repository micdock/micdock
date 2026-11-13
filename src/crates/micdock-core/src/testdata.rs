// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Synthetic recordings for tests (also used by the command-line tests): files laid out like the
//! transmitter writes them, small enough to create in a test.
//!
//! - WAV (firmware 2.50+): `RIFF`/`WAVE`, `fmt ` (PCM, 48 kHz, 24-bit, mono), `cue ` with the markers, a
//!   `PAD ` chunk up to offset 32768, then `data`.
//! - UGG (older firmware): Ogg pages; page 0 holds the OggPCM header packet and a comment packet, every
//!   following page one audio packet; the granule position counts samples and keeps counting across the
//!   ~31-minute pieces.

#![doc(hidden)]

use std::path::Path;

pub const RATE: u32 = 48000;

/// `n` samples of a quiet 24-bit test signal (different for each `seed`).
pub fn samples(n: usize, seed: i32) -> Vec<i32> {
    (0..n as i64)
        .map(|i| (((i * 7919 + seed as i64 * 104_729) % 600_000) - 300_000) as i32)
        .collect()
}

fn s24(samples: &[i32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|v| v.to_le_bytes()[..3].to_vec())
        .collect()
}

fn set_mtime(path: &Path, mtime: f64) {
    let t = filetime::FileTime::from_unix_time(mtime as i64, 0);
    filetime::set_file_times(path, t, t).expect("set mtime");
}

/// A WAV recording like firmware 2.50 writes it. `mtime` = when the recording ended.
pub fn write_wav(path: &Path, samples: &[i32], markers: &[u32], mtime: f64) {
    let data = s24(samples);
    let mut cue = Vec::new();
    cue.extend((markers.len() as u32).to_le_bytes());
    for (i, m) in markers.iter().enumerate() {
        cue.extend((i as u32 + 1).to_le_bytes()); // id
        cue.extend(m.to_le_bytes()); // position
        cue.extend(b"data");
        cue.extend(0u32.to_le_bytes()); // chunk start
        cue.extend(0u32.to_le_bytes()); // block start
        cue.extend(m.to_le_bytes()); // sample offset (what MicDock reads)
    }
    let mut f = Vec::new();
    f.extend(b"RIFF");
    f.extend(0u32.to_le_bytes()); // patched below
    f.extend(b"WAVEfmt ");
    f.extend(16u32.to_le_bytes());
    f.extend(1u16.to_le_bytes());
    f.extend(1u16.to_le_bytes());
    f.extend(RATE.to_le_bytes());
    f.extend((RATE * 3).to_le_bytes());
    f.extend(3u16.to_le_bytes());
    f.extend(24u16.to_le_bytes());
    f.extend(b"cue ");
    f.extend((cue.len() as u32).to_le_bytes());
    f.extend(&cue);
    let pad = 32768 - (f.len() + 8 + 8);
    f.extend(b"PAD ");
    f.extend((pad as u32).to_le_bytes());
    f.extend(vec![0u8; pad]);
    f.extend(b"data");
    f.extend((data.len() as u32).to_le_bytes());
    assert_eq!(f.len(), 32768);
    f.extend(&data);
    let riff = (f.len() - 8) as u32;
    f[4..8].copy_from_slice(&riff.to_le_bytes());
    std::fs::write(path, f).expect("write wav");
    set_mtime(path, mtime);
}

fn ogg_page(granule: i64, seq: u32, kind: u8, packets: &[&[u8]]) -> Vec<u8> {
    let mut lacing = Vec::new();
    for p in packets {
        lacing.extend(std::iter::repeat_n(255u8, p.len() / 255));
        lacing.push((p.len() % 255) as u8);
    }
    assert!(lacing.len() <= 255, "packet too big for one test page");
    let mut page = Vec::new();
    page.extend(b"OggS");
    page.push(0); // version
    page.push(kind);
    page.extend(granule.to_le_bytes());
    page.extend(0x5245_4331u32.to_le_bytes()); // stream serial
    page.extend(seq.to_le_bytes());
    page.extend(0u32.to_le_bytes()); // CRC: MicDock doesn't check it
    page.push(lacing.len() as u8);
    page.extend(lacing);
    for p in packets {
        page.extend_from_slice(p);
    }
    page
}

/// A UGG recording like older firmware writes it (OggPCM, S24LE, 48 kHz, mono). The first sample has
/// granule position `first_gp` (0 for a new recording, the previous piece's end for a continuation).
pub fn write_ugg(path: &Path, first_gp: i64, samples: &[i32], mtime: f64) {
    let mut head = Vec::new();
    head.extend(b"PCM     ");
    head.extend(0u16.to_le_bytes()); // version major
    head.extend(0u16.to_le_bytes()); // version minor
    head.extend(4u32.to_le_bytes()); // S24LE
    head.extend(RATE.to_le_bytes());
    head.push(24);
    head.push(1);
    head.extend([0u8; 6]);
    let comment = b"\x03vorbisRODE Wireless GO II test";
    let mut f = ogg_page(0, 0, 2, &[&head, comment]);
    let mut gp = first_gp;
    for (i, chunk) in samples.chunks(200).enumerate() {
        gp += chunk.len() as i64;
        let last = (i + 1) * 200 >= samples.len();
        f.extend(ogg_page(
            gp,
            i as u32 + 1,
            if last { 4 } else { 0 },
            &[&s24(chunk)],
        ));
    }
    std::fs::write(path, f).expect("write ugg");
    set_mtime(path, mtime);
}
