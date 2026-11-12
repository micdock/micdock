// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! The Wireless GO II's on-board recordings (`RECxxxxx.UGG` / `RECxxxxx.WAV`): reading, grouping, exporting.
//!
//! Firmware 2.50 records plain `RECxxxxx.WAV` files (PCM, 48 kHz 24-bit mono, with a `cue ` chunk for
//! markers and a `PAD ` chunk so the audio starts at 32768). Older firmware wrote `.UGG` files:
//! an Ogg container with an OggPCM-style mapping —
//!   packet 0: `"PCM     "` + u16 ver_major + u16 ver_minor + u32 format + u32 rate
//!             + u8 bits + u8 channels + u16 max_frames + u32 extra_headers   (all little-endian)
//!   packet 1: comment (`"vorbis"` + `"RODE Wireless GO II v..."`)
//!   rest:     raw interleaved PCM. Format 4 = signed 24-bit little-endian.
//! Pages may be followed by a few padding bytes, so we resync on `OggS`.
//! The granule position keeps counting across the TX's ~31-minute file splits, so a file whose first
//! granule equals the previous file's last granule continues the same recording session.
//! WAV files carry no session counter: a WAV continues the previous one when that one is a full
//! ~31-minute piece and this one started within a few seconds of its end.
//! `PEAxxxxx.EGG` files are waveform peak data for RØDE's app, not audio.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hid;
use crate::{Error, Result};

pub const TX_PID: u16 = 0x0029;
pub const CLOCK_PARAM: u8 = 0x01;
/// The TX splits recordings into ~31-minute files (1860 s).
pub const SPLIT_MIN_SECONDS: f64 = 1800.0;
/// Max seconds between one WAV piece's end and the next one's start (FAT times have 2 s resolution).
pub const SPLIT_GAP: f64 = 10.0;

/// OggPCM format id → (bytes per sample, needs byteswap).
fn ogg_format(id: u32) -> Option<(usize, bool)> {
    Some(match id {
        0x02 => (2, false), // S16LE
        0x03 => (2, true),  // S16BE
        0x04 => (3, false), // S24LE
        0x05 => (3, true),  // S24BE
        0x06 => (4, false), // S32LE
        0x07 => (4, true),  // S32BE
        _ => return None,
    })
}

// --------------------------------------------------------------------------
// Ogg packets
// --------------------------------------------------------------------------

/// Ogg packets from a reader, tolerating junk between pages.
pub struct OggPackets<R: Read> {
    reader: R,
    buf: Vec<u8>,
    cur: Vec<u8>,
    ready: std::collections::VecDeque<Vec<u8>>,
    done: bool,
}

impl<R: Read> OggPackets<R> {
    pub fn new(reader: R) -> Self {
        OggPackets {
            reader,
            buf: Vec::new(),
            cur: Vec::new(),
            ready: Default::default(),
            done: false,
        }
    }

    fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        if from >= hay.len() {
            return None;
        }
        hay[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| p + from)
    }

    /// Read one more chunk and cut every complete page in the buffer into packets.
    fn fill(&mut self) -> std::io::Result<()> {
        let mut chunk = vec![0u8; 1 << 20];
        let mut n = 0;
        while n < chunk.len() {
            let r = self.reader.read(&mut chunk[n..])?;
            if r == 0 {
                break;
            }
            n += r;
        }
        chunk.truncate(n);
        if chunk.is_empty() && self.buf.is_empty() {
            self.finish();
            return Ok(());
        }
        self.buf.extend_from_slice(&chunk);
        let buf = &self.buf;
        let mut pos = 0usize;
        while let Some(q) = Self::find(buf, b"OggS", pos) {
            if q + 27 > buf.len() {
                break;
            }
            let nseg = buf[q + 26] as usize;
            let hdr_end = q + 27 + nseg;
            if hdr_end > buf.len() {
                break;
            }
            let segs = &buf[q + 27..hdr_end];
            let end = hdr_end + segs.iter().map(|&s| s as usize).sum::<usize>();
            if end > buf.len() {
                break;
            }
            let mut p = hdr_end;
            for &s in segs {
                let s = s as usize;
                self.cur.extend_from_slice(&buf[p..p + s]);
                p += s;
                if s < 255 {
                    self.ready.push_back(std::mem::take(&mut self.cur));
                }
            }
            pos = end;
        }
        if chunk.is_empty() {
            self.finish();
            return Ok(());
        }
        if pos > 0 {
            self.buf.drain(..pos);
        } else if self.buf.len() > (1 << 22) {
            let keep = self.buf.len() - (1 << 16);
            self.buf.drain(..keep);
        }
        Ok(())
    }

    fn finish(&mut self) {
        if !self.cur.is_empty() {
            self.ready.push_back(std::mem::take(&mut self.cur));
        }
        self.done = true;
    }
}

impl<R: Read> Iterator for OggPackets<R> {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Some(Ok(p));
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.fill() {
                self.done = true;
                return Some(Err(e));
            }
        }
    }
}

/// Reverse the byte order of every `width`-byte sample.
pub fn swap(data: &[u8], width: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    for s in out.chunks_exact_mut(width) {
        s.reverse();
    }
    out
}

/// (rate, bits, channels, width, byteswap) from the OggPCM header packet.
fn parse_head(pk: &[u8]) -> Result<(u32, u16, u16, usize, bool)> {
    if !pk.starts_with(b"PCM     ") || pk.len() < 22 {
        return Err(Error::other(format!(
            "not an OggPCM stream (got {:?})",
            String::from_utf8_lossy(&pk[..pk.len().min(8)])
        )));
    }
    let fmt = u32::from_le_bytes(pk[12..16].try_into().unwrap());
    let rate = u32::from_le_bytes(pk[16..20].try_into().unwrap());
    let (bits, ch) = (pk[20] as u16, pk[21] as u16);
    let (width, byteswap) =
        ogg_format(fmt).ok_or_else(|| Error::other(format!("unsupported PCM format id {fmt}")))?;
    Ok((rate, bits, ch, width, byteswap))
}

// --------------------------------------------------------------------------
// recordings
// --------------------------------------------------------------------------

/// File-type specific details of a recording.
#[derive(Debug, Clone)]
pub enum Container {
    /// Older firmware: Ogg/OggPCM. The granule positions link the ~31-minute pieces.
    Ugg { first_gp: i64, last_gp: i64 },
    /// Firmware 2.50+: RIFF/WAVE with the audio in one data chunk.
    Wav { data_offset: u64, data_len: u64 },
}

/// One recording file on the transmitter (or a copy of it).
#[derive(Debug, Clone)]
pub struct Recording {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// When the TX closed the file (= the end of the recording), unix seconds, TX clock.
    pub mtime: f64,
    pub rate: u32,
    pub bits: u16,
    pub channels: u16,
    /// Bytes per sample.
    pub width: usize,
    pub byteswap: bool,
    pub frame_bytes: usize,
    pub frames: u64,
    /// Marker positions (sample offsets) from the WAV `cue ` chunk.
    pub markers: Vec<u32>,
    pub container: Container,
}

impl Recording {
    /// "UGG" or "WAV".
    pub fn kind(&self) -> &'static str {
        match self.container {
            Container::Ugg { .. } => "UGG",
            Container::Wav { .. } => "WAV",
        }
    }

    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.rate as f64
    }

    pub fn same_format(&self, other: &Recording) -> bool {
        (self.rate, self.width, self.channels, self.byteswap)
            == (other.rate, other.width, other.channels, other.byteswap)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn mtime_of(meta: &std::fs::Metadata) -> f64 {
    meta.mtime() as f64 + meta.mtime_nsec() as f64 / 1e9
}

/// What we can learn about a .UGG file from its first and last 64 KiB.
pub fn open_ugg(path: &Path) -> Result<Recording> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mut f = File::open(path)?;
    let mut head = Vec::new();
    (&mut f).take(1 << 16).read_to_end(&mut head)?;
    f.seek(SeekFrom::Start(size.saturating_sub(1 << 16)))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let first = OggPackets::new(&head[..])
        .next()
        .transpose()?
        .ok_or_else(|| Error::other("not an OggPCM stream (no packets)"))?;
    let (rate, bits, channels, width, byteswap) = parse_head(&first)?;
    let frame_bytes = width * channels as usize;
    let i64_at = |b: &[u8], at: usize| i64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let last_gp = match tail.windows(4).rposition(|w| w == b"OggS") {
        Some(q) if q + 14 <= tail.len() => i64_at(&tail, q + 6),
        _ => 0,
    };
    // page 0 holds both header packets; page 1 is the first audio page
    let first_gp = match OggPackets::<&[u8]>::find(&head, b"OggS", 4) {
        Some(q) if q + 27 <= head.len() => {
            let gp = i64_at(&head, q + 6);
            let nseg = head[q + 26] as usize;
            let sum: i64 = head[q + 27..(q + 27 + nseg).min(head.len())]
                .iter()
                .map(|&s| s as i64)
                .sum();
            gp - sum.div_euclid(frame_bytes.max(1) as i64)
        }
        _ => last_gp,
    };
    Ok(Recording {
        path: path.to_path_buf(),
        name: file_name(path),
        size,
        mtime: mtime_of(&meta),
        rate,
        bits,
        channels,
        width,
        byteswap,
        frame_bytes,
        frames: (last_gp - first_gp).max(0) as u64,
        markers: Vec::new(),
        container: Container::Ugg { first_gp, last_gp },
    })
}

/// A RIFF/WAVE recording (firmware 2.50+): format, data chunk position and markers.
pub fn open_wav(path: &Path) -> Result<Recording> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mut f = File::open(path)?;
    let mut head = [0u8; 12];
    if f.read(&mut head)? < 12 || &head[..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err(Error::other("not a RIFF/WAVE file"));
    }
    let mut fmt: Option<Vec<u8>> = None;
    let mut data: Option<(u64, u64)> = None;
    let mut markers = Vec::new();
    let mut pos = 12u64;
    while pos + 8 <= size {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h)?;
        let ln = u32::from_le_bytes(h[4..8].try_into().unwrap()) as u64;
        match &h[..4] {
            b"fmt " => {
                let mut b = Vec::new();
                (&mut f).take(ln.min(40)).read_to_end(&mut b)?;
                fmt = Some(b);
            }
            b"cue " => {
                let mut b = Vec::new();
                (&mut f).take(ln.min(4 + 24 * 4096)).read_to_end(&mut b)?;
                if b.len() >= 4 {
                    let n = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
                    for k in 0..n.min((b.len() - 4) / 24) {
                        let at = 4 + 24 * k + 20;
                        markers.push(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()));
                    }
                }
            }
            b"data" => {
                let off = pos + 8;
                data = Some((off, ln.min(size - off))); // an unfinished file may be short
                break;
            }
            _ => {}
        }
        pos += 8 + ln + (ln & 1);
    }
    let (Some(fmt), Some((data_offset, mut data_len))) = (fmt, data) else {
        return Err(Error::other("WAV file without fmt/data chunk"));
    };
    if fmt.len() < 16 {
        return Err(Error::other("WAV fmt chunk too short"));
    }
    let u16_at = |at: usize| u16::from_le_bytes(fmt[at..at + 2].try_into().unwrap());
    let mut tag = u16_at(0);
    let channels = u16_at(2);
    let rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
    let bits = u16_at(14);
    if tag == 0xFFFE && fmt.len() >= 26 {
        tag = u16_at(24); // WAVE_FORMAT_EXTENSIBLE: real tag = first 2 bytes of the GUID
    }
    if tag != 1 {
        return Err(Error::other(format!(
            "unsupported WAV format tag {tag:#x} (only PCM)"
        )));
    }
    let width = (bits as usize).div_ceil(8);
    let frame_bytes = width * channels as usize;
    if frame_bytes == 0 {
        return Err(Error::other("WAV file with no channels"));
    }
    data_len -= data_len % frame_bytes as u64;
    Ok(Recording {
        path: path.to_path_buf(),
        name: file_name(path),
        size,
        mtime: mtime_of(&meta),
        rate,
        bits,
        channels,
        width,
        byteswap: false,
        frame_bytes,
        frames: data_len / frame_bytes as u64,
        markers,
        container: Container::Wav {
            data_offset,
            data_len,
        },
    })
}

/// `RECxxxxx.UGG` / `RECxxxxx.WAV` (not the `PEAxxxxx.EGG` peak files, not macOS `._` files).
pub fn is_recording(name: &str) -> bool {
    let up = name.to_uppercase();
    (up.ends_with(".UGG") || up.ends_with(".WAV")) && !name.starts_with('.')
}

/// Open a recording; the file's signature decides between UGG and WAV.
pub fn open_recording(path: impl AsRef<Path>) -> Result<Recording> {
    let path = path.as_ref();
    let mut magic = [0u8; 4];
    let n = File::open(path)?.read(&mut magic)?;
    match &magic[..n] {
        b"RIFF" => open_wav(path),
        b"OggS" => open_ugg(path),
        other => Err(Error::other(format!(
            "unknown file type (starts with {other:?})"
        ))),
    }
}

/// True if `info` is the next ~31-minute piece of the same recording as `prev`.
pub fn continues(prev: &Recording, info: &Recording) -> bool {
    if prev.kind() != info.kind() || !info.same_format(prev) {
        return false;
    }
    match (&prev.container, &info.container) {
        // the granule keeps counting across splits
        (Container::Ugg { last_gp, .. }, Container::Ugg { first_gp, .. }) => {
            *last_gp != 0 && first_gp == last_gp
        }
        // WAV: the previous piece must be full length and this one must start where it ended
        _ => {
            prev.seconds() >= SPLIT_MIN_SECONDS
                && ((info.mtime - info.seconds()) - prev.mtime).abs() <= SPLIT_GAP
        }
    }
}

/// Group recordings (any order) into sessions: runs of consecutive files that continue each other.
pub fn group_sessions(mut infos: Vec<Recording>) -> Vec<Vec<Recording>> {
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    let mut sessions: Vec<Vec<Recording>> = Vec::new();
    for info in infos {
        match sessions.last_mut() {
            Some(s) if continues(s.last().unwrap(), &info) => s.push(info),
            _ => sessions.push(vec![info]),
        }
    }
    sessions
}

// --------------------------------------------------------------------------
// export
// --------------------------------------------------------------------------

/// Output format of [`export`].
#[derive(Debug, Clone)]
pub enum Encoding {
    Wav,
    /// MP3 through ffmpeg/libmp3lame with these encoder options (see [`mp3_args`]).
    Mp3(Vec<String>),
}

/// libmp3lame options: VBR quality 0–9 (0 = best), or a constant bitrate such as "192k".
pub fn mp3_args(quality: u8, bitrate: Option<&str>) -> Vec<String> {
    match bitrate {
        Some(b) => vec!["-b:a".into(), b.into()],
        None => vec!["-q:a".into(), quality.to_string()],
    }
}

/// Progress and cancellation for long exports.
#[derive(Default)]
pub struct Progress<'a> {
    /// Called with (bytes done, bytes total) about every MiB.
    pub report: Option<&'a mut dyn FnMut(u64, u64)>,
    pub cancel: Option<&'a AtomicBool>,
}

pub const CANCELLED: &str = "cancelled";

/// Feed the raw PCM of all recordings, back to back, to `sink`.
fn pump(
    infos: &[Recording],
    progress: &mut Progress,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let total = infos
        .iter()
        .map(|i| i.frames * i.frame_bytes as u64)
        .sum::<u64>()
        .max(1);
    let (mut done, mut last) = (0u64, 0u64);
    let mut tick = |n: usize, progress: &mut Progress| -> Result<()> {
        done += n as u64;
        if done - last >= 1 << 20 {
            last = done;
            if progress.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(Error::other(CANCELLED));
            }
            if let Some(r) = progress.report.as_mut() {
                r(done.min(total), total);
            }
        }
        Ok(())
    };
    let mut written = 0u64;
    for info in infos {
        match info.container {
            Container::Wav {
                data_offset,
                data_len,
            } => {
                let mut f = File::open(&info.path)?;
                f.seek(SeekFrom::Start(data_offset))?;
                let mut left = data_len;
                let step = info.frame_bytes << 16; // whole frames per read
                let mut buf = vec![0u8; step];
                while left > 0 {
                    let want = (step as u64).min(left) as usize;
                    let n = f.read(&mut buf[..want])?;
                    if n == 0 {
                        break;
                    }
                    left -= n as u64;
                    sink(&buf[..n])?;
                    written += n as u64;
                    tick(n, progress)?;
                }
            }
            Container::Ugg { .. } => {
                let mut packets = OggPackets::new(File::open(&info.path)?);
                packets.next().transpose()?; // header
                packets.next().transpose()?; // comment
                for pk in packets {
                    let pk = pk?;
                    if pk.is_empty() {
                        continue;
                    }
                    sink(&pk)?;
                    written += pk.len() as u64;
                    tick(pk.len(), progress)?;
                }
            }
        }
    }
    if let Some(r) = progress.report.as_mut() {
        r(total, total);
    }
    Ok(written)
}

/// A canonical 44-byte PCM WAV header (the same layout Python's `wave` module writes).
fn wav_header(first: &Recording, data_len: u64) -> Vec<u8> {
    let ch = first.channels as u32;
    let width = first.width as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&(ch as u16).to_le_bytes());
    h.extend_from_slice(&first.rate.to_le_bytes());
    h.extend_from_slice(&(ch * first.rate * width).to_le_bytes());
    h.extend_from_slice(&((ch * width) as u16).to_le_bytes());
    h.extend_from_slice(&((width * 8) as u16).to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(data_len as u32).to_le_bytes());
    h
}

fn write_wav(tmp: &Path, infos: &[Recording], progress: &mut Progress) -> Result<u64> {
    let first = &infos[0];
    let mut out = BufWriter::with_capacity(1 << 20, File::create(tmp)?);
    out.write_all(&wav_header(first, 0))?;
    let n = pump(infos, progress, &mut |pk| {
        if first.byteswap {
            out.write_all(&swap(pk, first.width))?;
        } else {
            out.write_all(pk)?;
        }
        Ok(())
    })?;
    let mut f = out.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&wav_header(first, n))?;
    Ok(n)
}

fn write_mp3(
    tmp: &Path,
    infos: &[Recording],
    args: &[String],
    progress: &mut Progress,
) -> Result<u64> {
    let first = &infos[0];
    // ffmpeg reads the PCM as-is, so tell it the byte order instead of swapping
    let pcm_fmt = format!(
        "s{}{}",
        first.width * 8,
        if first.byteswap { "be" } else { "le" }
    );
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            &pcm_fmt,
        ])
        .args([
            "-ar",
            &first.rate.to_string(),
            "-ac",
            &first.channels.to_string(),
            "-i",
            "pipe:0",
        ])
        .args(["-c:a", "libmp3lame"])
        .args(args)
        .args(["-f", "mp3"])
        .arg(tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::other(format!("cannot start ffmpeg: {e}")))?;
    let mut stdin = child.stdin.take().unwrap();
    let result = pump(infos, progress, &mut |pk| match stdin.write_all(pk) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(Error::other("ffmpeg stopped")),
        r => Ok(r?),
    });
    drop(stdin);
    let n = match result {
        Ok(n) => n,
        Err(Error::Other(m)) if m == "ffmpeg stopped" => 0, // its exit status below says why
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let tail: String = err
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(Error::other(format!(
            "ffmpeg exited with code {}: {tail}",
            out.status.code().unwrap_or(-1)
        )));
    }
    Ok(n)
}

/// Write one WAV or MP3 from one or more recordings, joined back to back. `start`/`end` are real unix
/// times: the MP3 date tag and the output file's modification time. Returns the sample frames written.
pub fn export(
    infos: &[Recording],
    dst: &Path,
    encoding: &Encoding,
    start: Option<f64>,
    end: Option<f64>,
    mut progress: Progress,
) -> Result<u64> {
    let first = infos
        .first()
        .ok_or_else(|| Error::other("nothing to export"))?;
    if let Some(odd) = infos.iter().find(|i| !i.same_format(first)) {
        return Err(Error::other(format!(
            "{} has a different audio format than {}",
            odd.name, first.name
        )));
    }
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    let result = match encoding {
        Encoding::Wav => write_wav(&tmp, infos, &mut progress),
        Encoding::Mp3(args) => {
            let mut args = args.clone();
            if let Some(start) = start {
                let iso = local_iso(start);
                args.extend(["-metadata".into(), format!("date={}", &iso[..10])]);
                args.extend(["-metadata".into(), format!("creation_time={iso}")]);
            }
            write_mp3(&tmp, infos, &args, &mut progress)
        }
    };
    let n = match result {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, dst)?;
    if let Some(end) = end {
        let t = filetime::FileTime::from_unix_time(
            end.floor() as i64,
            ((end - end.floor()) * 1e9) as u32,
        );
        filetime::set_file_times(dst, t, t)?;
    }
    Ok(n / first.frame_bytes as u64)
}

/// Local time with offset, seconds precision: "2026-11-10T09:47:00+01:00".
pub fn local_iso(t: f64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(t.floor() as i64, 0) {
        chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => {
            d.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
        }
        chrono::LocalResult::None => String::new(),
    }
}

/// Seconds to add to TX timestamps to get real time (read from the TX clock), or None if no TX is found.
pub fn tx_clock_offset(dev_node: Option<&str>) -> Result<Option<i64>> {
    let info = match dev_node {
        Some(n) => hid::DevInfo::from_node(n),
        None => match hid::find_devices().into_iter().find(|d| d.pid == TX_PID) {
            Some(d) => d,
            None => return Ok(None),
        },
    };
    let mut d = info.open()?;
    let now = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    };
    let t0 = now();
    let data = hid::get_param(&mut d, CLOCK_PARAM)?;
    let t1 = now();
    if data.len() < 4 {
        return Err(Error::other("short clock reply"));
    }
    let clock = u32::from_le_bytes(data[..4].try_into().unwrap()) as i64;
    Ok(Some(((t0 + t1) / 2.0).round_ties_even() as i64 - clock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(granule: i64, payload: &[u8]) -> Vec<u8> {
        let mut segs = Vec::new();
        let mut left = payload.len();
        loop {
            let s = left.min(255);
            segs.push(s as u8);
            left -= s;
            if s < 255 {
                break;
            }
        }
        let mut p = b"OggS\0\0".to_vec();
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&[0; 12]); // serial, sequence, crc (not checked)
        p.push(segs.len() as u8);
        p.extend_from_slice(&segs);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn ogg_packets_survive_junk_between_pages() {
        let mut data = page(0, b"first");
        data.extend_from_slice(b"\0\0junk");
        data.extend(page(5, &[7u8; 300]));
        let packets: Vec<Vec<u8>> = OggPackets::new(&data[..]).map(|p| p.unwrap()).collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], b"first");
        assert_eq!(packets[1].len(), 300);
    }

    #[test]
    fn swapping_samples() {
        assert_eq!(swap(&[1, 2, 3, 4, 5, 6], 3), vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn recording_names() {
        assert!(is_recording("REC00042.WAV"));
        assert!(is_recording("rec00001.ugg"));
        assert!(!is_recording("PEA00042.EGG"));
        assert!(!is_recording("._REC00042.WAV"));
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use crate::testdata::{samples, write_ugg, write_wav};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("micdock-rec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The PCM data of a 44-byte-header WAV as samples.
    fn wav_samples(path: &Path) -> Vec<i32> {
        let b = std::fs::read(path).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        let len = u32::from_le_bytes(b[40..44].try_into().unwrap()) as usize;
        assert_eq!(len, b.len() - 44, "data size in the header");
        b[44..]
            .as_chunks::<3>()
            .0
            .iter()
            .map(|s| i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8)
            .collect()
    }

    #[test]
    fn reads_a_wav_recording() {
        let d = tmp("wav");
        let p = d.join("REC00051.WAV");
        write_wav(&p, &samples(4800, 1), &[480, 2400], 1_800_000_000.0);
        let r = open_recording(&p).unwrap();
        assert_eq!(r.kind(), "WAV");
        assert_eq!((r.rate, r.bits, r.channels, r.width), (48000, 24, 1, 3));
        assert_eq!(r.frames, 4800);
        assert!((r.seconds() - 0.1).abs() < 1e-9);
        assert_eq!(r.markers, vec![480, 2400]);
        assert_eq!(r.mtime, 1_800_000_000.0);
        assert!(matches!(
            r.container,
            Container::Wav {
                data_offset: 32768,
                ..
            }
        ));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn reads_a_ugg_recording_with_its_granule() {
        let d = tmp("ugg");
        let p = d.join("REC00042.UGG");
        write_ugg(&p, 1000, &samples(1000, 2), 1_000_000_000.0);
        let r = open_recording(&p).unwrap();
        assert_eq!(r.kind(), "UGG");
        assert_eq!((r.rate, r.bits, r.channels, r.width), (48000, 24, 1, 3));
        assert_eq!(r.frames, 1000);
        match r.container {
            Container::Ugg { first_gp, last_gp } => assert_eq!((first_gp, last_gp), (1000, 2000)),
            _ => panic!("not UGG"),
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rejects_other_files() {
        let d = tmp("junk");
        let p = d.join("PEA00001.EGG");
        std::fs::write(&p, b"RODEWgo2P peaks").unwrap();
        assert!(open_recording(&p).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn ugg_pieces_join_by_granule() {
        let d = tmp("join-ugg");
        let (a, b, c) = (
            d.join("REC00001.UGG"),
            d.join("REC00002.UGG"),
            d.join("REC00003.UGG"),
        );
        write_ugg(&a, 0, &samples(600, 1), 100.0);
        write_ugg(&b, 600, &samples(400, 2), 200.0); // continues a
        write_ugg(&c, 0, &samples(200, 3), 300.0); // a new recording
        let infos = [&c, &a, &b].map(|p| open_recording(p).unwrap()).to_vec();
        let sessions = group_sessions(infos);
        let names: Vec<Vec<&str>> = sessions
            .iter()
            .map(|s| s.iter().map(|r| r.name.as_str()).collect())
            .collect();
        assert_eq!(
            names,
            [vec!["REC00001.UGG", "REC00002.UGG"], vec!["REC00003.UGG"]]
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    /// WAV pieces have no granule: a full-length piece (>= 30 min) followed within 10 s by the next.
    #[test]
    fn wav_pieces_join_by_length_and_time() {
        let piece = |name: &str, secs: f64, end: f64| Recording {
            path: PathBuf::from(name),
            name: name.into(),
            size: 0,
            mtime: end,
            rate: 48000,
            bits: 24,
            channels: 1,
            width: 3,
            byteswap: false,
            frame_bytes: 3,
            frames: (secs * 48000.0) as u64,
            markers: vec![],
            container: Container::Wav {
                data_offset: 32768,
                data_len: 0,
            },
        };
        let a = piece("REC00010.WAV", 1860.0, 10_000.0);
        let b = piece("REC00011.WAV", 500.0, 10_000.0 + 2.0 + 500.0); // starts 2 s after a ended
        let c = piece("REC00012.WAV", 60.0, 20_000.0); // much later
        let d = piece("REC00013.WAV", 30.0, 20_000.0 + 30.0); // right after c, but c was short
        assert!(continues(&a, &b));
        assert!(!continues(&b, &c));
        assert!(!continues(&c, &d), "only a full-length piece is continued");
        let late = piece("REC00011.WAV", 500.0, 10_000.0 + 30.0 + 500.0);
        assert!(!continues(&a, &late), "a gap of 30 s is a new recording");
        let s = group_sessions(vec![d, c, b, a]);
        assert_eq!(s.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1, 1]);
    }

    #[test]
    fn exports_joined_wav_pieces_sample_exact() {
        let d = tmp("export-wav");
        let (a, b) = (d.join("REC00001.WAV"), d.join("REC00002.WAV"));
        let (sa, sb) = (samples(3000, 1), samples(1500, 2));
        write_wav(&a, &sa, &[], 100.0);
        write_wav(&b, &sb, &[], 200.0);
        let infos = [&a, &b].map(|p| open_recording(p).unwrap());
        let out = d.join("joined.wav");
        let mut reports = 0;
        let mut report = |_: u64, _: u64| reports += 1;
        let progress = Progress {
            report: Some(&mut report),
            cancel: None,
        };
        let frames = export(
            &infos,
            &out,
            &Encoding::Wav,
            Some(50.0),
            Some(1_700_000_000.5),
            progress,
        )
        .unwrap();
        assert_eq!(frames, 4500);
        assert!(reports >= 1, "the final progress report");
        let mut want = sa.clone();
        want.extend(&sb);
        assert_eq!(wav_samples(&out), want);
        let mtime = std::fs::metadata(&out).unwrap().modified().unwrap();
        let secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!(
            (secs - 1_700_000_000.5).abs() < 0.01,
            "file date = end of the recording"
        );
        assert!(!d.join("joined.wav.part").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn exports_ugg_as_wav_sample_exact() {
        let d = tmp("export-ugg");
        let p = d.join("REC00007.UGG");
        let s = samples(2500, 7);
        write_ugg(&p, 0, &s, 100.0);
        let r = open_recording(&p).unwrap();
        let out = d.join("out.wav");
        export(&[r], &out, &Encoding::Wav, None, None, Progress::default()).unwrap();
        assert_eq!(wav_samples(&out), s);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn refuses_mixed_formats() {
        let d = tmp("mixed");
        let (a, b) = (d.join("REC00001.WAV"), d.join("REC00002.UGG"));
        write_wav(&a, &samples(100, 1), &[], 1.0);
        write_ugg(&b, 0, &samples(100, 2), 2.0);
        let mut infos = [&a, &b].map(|p| open_recording(p).unwrap());
        infos[1].channels = 2; // a different format
        let err = export(
            &infos,
            &d.join("x.wav"),
            &Encoding::Wav,
            None,
            None,
            Progress::default(),
        );
        assert!(err.is_err());
        assert!(!d.join("x.wav").exists() && !d.join("x.wav.part").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn cancelled_export_leaves_no_file() {
        let d = tmp("cancel");
        let p = d.join("REC00001.WAV");
        write_wav(&p, &samples(800_000, 1), &[], 1.0); // 2.4 MB: progress is checked every MiB
        let r = open_recording(&p).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let progress = Progress {
            report: None,
            cancel: Some(&cancel),
        };
        let out = d.join("out.wav");
        let err = export(&[r], &out, &Encoding::Wav, None, None, progress).unwrap_err();
        assert_eq!(err.to_string(), CANCELLED);
        assert!(!out.exists() && !d.join("out.wav.part").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn mp3_options() {
        assert_eq!(mp3_args(2, None), ["-q:a", "2"]);
        assert_eq!(mp3_args(0, Some("320k")), ["-b:a", "320k"]);
    }
}
