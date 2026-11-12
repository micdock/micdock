// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! A small built-in player for WAV recordings (one or more files played back to back, like a session).
//!
//! A worker thread owns the audio stream (cpal, through ALSA, so also PipeWire/PulseAudio) and reads the
//! PCM straight from the files in small chunks, so even hours of audio start at once and seeking only
//! means reading from another offset. The UI reads the position from shared atomics.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use micdock_core::recordings::{Container, Recording};

/// Frames per chunk handed from the reader to the audio callback (0.1 s at 48 kHz).
const CHUNK: u64 = 4800;

/// One WAV file's audio data.
#[derive(Clone)]
struct Segment {
    path: std::path::PathBuf,
    offset: u64,
    /// First frame of this file within the whole session.
    start: u64,
    frames: u64,
    frame_bytes: usize,
    width: usize,
}

struct Shared {
    /// The frame being heard now.
    pos: AtomicU64,
    paused: AtomicBool,
    /// Bumped on every seek: chunks of an older generation are dropped.
    generation: AtomicU64,
    finished: AtomicBool,
    /// Generation + 1 whose last chunk the reader has handed over (0: none).
    eof: AtomicU64,
    error: Mutex<Option<String>>,
}

enum Cmd {
    /// (frame, generation)
    Seek(u64, u64),
    Stop,
}

/// A chunk of mono samples: (generation, first frame, samples).
type Chunk = (u64, u64, Vec<f32>);

pub struct Player {
    shared: Arc<Shared>,
    cmd: mpsc::Sender<Cmd>,
    pub total: u64,
    pub rate: u32,
    /// Marker positions (frames from the start of the session).
    pub markers: Vec<u64>,
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
    }
}

impl Player {
    /// Start playing these WAV recordings back to back from the beginning.
    pub fn start(infos: &[Recording]) -> Result<Player, String> {
        let mut segs = Vec::new();
        let mut markers = Vec::new();
        let mut start = 0;
        for r in infos {
            let Container::Wav { data_offset, .. } = r.container else {
                return Err(format!("{} is not a WAV file", r.name));
            };
            if !matches!(r.width, 2..=4) {
                return Err(format!("{}: {}-bit audio is not supported", r.name, r.bits));
            }
            markers.extend(r.markers.iter().map(|&m| start + m as u64));
            segs.push(Segment {
                path: r.path.clone(),
                offset: data_offset,
                start,
                frames: r.frames,
                frame_bytes: r.frame_bytes,
                width: r.width,
            });
            start += r.frames;
        }
        let rate = infos.first().map(|r| r.rate).ok_or("nothing to play")?;
        let shared = Arc::new(Shared {
            pos: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            eof: AtomicU64::new(0),
            error: Mutex::new(None),
        });
        let (cmd, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let sh = shared.clone();
        thread::Builder::new()
            .name("micdock-player".into())
            .spawn(move || run(segs, rate, sh, cmd_rx, ready_tx))
            .map_err(|e| e.to_string())?;
        // wait until the audio device is open, so errors show up right away
        ready_rx
            .recv()
            .map_err(|_| "the player stopped".to_string())??;
        Ok(Player {
            shared,
            cmd,
            total: start,
            rate,
            markers,
        })
    }

    pub fn position(&self) -> u64 {
        self.shared.pos.load(Ordering::Relaxed).min(self.total)
    }

    pub fn seconds(&self, frames: u64) -> f64 {
        frames as f64 / self.rate as f64
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok()?.clone()
    }

    pub fn set_paused(&self, paused: bool) {
        if !paused && self.finished() {
            self.seek(0); // play again from the start
        }
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn seek(&self, frame: u64) {
        let frame = frame.min(self.total);
        self.shared.finished.store(false, Ordering::Relaxed);
        self.shared.pos.store(frame, Ordering::Relaxed);
        let generation = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.cmd.send(Cmd::Seek(frame, generation));
    }
}

/// Decode `n` frames of little-endian signed PCM (first channel only) to f32.
fn decode(buf: &[u8], frame_bytes: usize, width: usize, out: &mut Vec<f32>) {
    let scale = 1.0 / (1u64 << (width * 8 - 1)) as f32;
    for f in buf.chunks_exact(frame_bytes) {
        let s = &f[..width];
        let v = match width {
            2 => i16::from_le_bytes([s[0], s[1]]) as i32,
            3 => i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8,
            _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        };
        out.push(v as f32 * scale);
    }
}

/// Read up to `CHUNK` frames from `frame` on. None at the end.
fn read_chunk(
    segs: &[Segment],
    frame: u64,
    file: &mut Option<(usize, File)>,
) -> std::io::Result<Option<Vec<f32>>> {
    let Some(i) = segs.iter().position(|s| frame < s.start + s.frames) else {
        return Ok(None);
    };
    let s = &segs[i];
    if file.as_ref().is_none_or(|(j, _)| *j != i) {
        *file = Some((i, File::open(&s.path)?));
    }
    let f = &mut file.as_mut().unwrap().1;
    let n = CHUNK.min(s.start + s.frames - frame);
    f.seek(SeekFrom::Start(
        s.offset + (frame - s.start) * s.frame_bytes as u64,
    ))?;
    let mut buf = vec![0u8; n as usize * s.frame_bytes];
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..])? {
            0 => break,
            k => got += k,
        }
    }
    got -= got % s.frame_bytes;
    if got == 0 {
        return Ok(None); // the file is shorter than its header says
    }
    let mut out = Vec::with_capacity(got / s.frame_bytes);
    decode(&buf[..got], s.frame_bytes, s.width, &mut out);
    Ok(Some(out))
}

fn run(
    segs: Vec<Segment>,
    rate: u32,
    shared: Arc<Shared>,
    cmd: Receiver<Cmd>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let (tx, rx) = mpsc::sync_channel::<Chunk>(6);
    let stream = match open_stream(rate, shared.clone(), rx) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    feed(&segs, &shared, &cmd, &tx);
    drop(stream);
}

/// The reader loop: keep the audio callback supplied, follow seeks, stop on Stop or when the UI is gone.
fn feed(segs: &[Segment], shared: &Shared, cmd: &Receiver<Cmd>, tx: &SyncSender<Chunk>) {
    let mut next = 0u64;
    let mut generation = 0u64;
    let mut file = None;
    let mut pending: Option<Chunk> = None;
    let mut at_end = false;
    loop {
        // commands first; wait a little when there is nothing to do
        let wait = if pending.is_some() || at_end {
            Duration::from_millis(20)
        } else {
            Duration::ZERO
        };
        match cmd.recv_timeout(wait) {
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Cmd::Seek(f, g)) => {
                generation = g;
                next = f;
                pending = None;
                at_end = false;
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if pending.is_none() && !at_end {
            match read_chunk(segs, next, &mut file) {
                Ok(Some(samples)) => {
                    let n = samples.len() as u64;
                    pending = Some((generation, next, samples));
                    next += n;
                }
                Ok(None) => at_end = true,
                Err(e) => {
                    if let Ok(mut err) = shared.error.lock() {
                        *err = Some(e.to_string());
                    }
                    at_end = true;
                }
            }
        }
        if at_end && pending.is_none() {
            shared.eof.store(generation + 1, Ordering::Relaxed);
        }
        if let Some(c) = pending.take() {
            match tx.try_send(c) {
                Ok(()) => {}
                Err(TrySendError::Full(c)) => pending = Some(c),
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

fn open_stream(
    rate: u32,
    shared: Arc<Shared>,
    rx: Receiver<Chunk>,
) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no audio output device found")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let mut config: cpal::StreamConfig = supported.into();
    // Ask for the recording's own rate (PipeWire and PulseAudio convert it); resample ourselves if refused.
    config.sample_rate = rate;
    let stream = match build::<f32>(&device, config, rate, shared.clone(), rx) {
        Ok(s) => s,
        Err((e, rx)) => {
            let Some(rx) = rx else { return Err(e) };
            let config: cpal::StreamConfig = device
                .default_output_config()
                .map_err(|e| e.to_string())?
                .into();
            match format {
                cpal::SampleFormat::I16 => build::<i16>(&device, config, rate, shared, rx),
                cpal::SampleFormat::I32 => build::<i32>(&device, config, rate, shared, rx),
                _ => build::<f32>(&device, config, rate, shared, rx),
            }
            .map_err(|(e, _)| e)?
        }
    };
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Build the output stream. On failure the receiver is handed back, to try another configuration.
fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    rate: u32,
    shared: Arc<Shared>,
    rx: Receiver<Chunk>,
) -> Result<cpal::Stream, (String, Option<Receiver<Chunk>>)> {
    let channels = config.channels as usize;
    // source frames per output frame (1.0 when the device runs at the recording's rate)
    let step = rate as f64 / config.sample_rate as f64;
    let rx = Arc::new(Mutex::new(rx));
    let rx_cb = rx.clone();
    let mut cur: Option<Chunk> = None;
    let mut frac = 0.0f64; // position inside the current chunk, in source frames
    let callback = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        let Ok(rx) = rx_cb.try_lock() else {
            data.fill(T::from_sample(0.0));
            return;
        };
        for frame in data.chunks_mut(channels) {
            let mut v = 0.0f32;
            if !shared.paused.load(Ordering::Relaxed) {
                loop {
                    let generation = shared.generation.load(Ordering::Relaxed);
                    if let Some((g, first, s)) = &cur {
                        if *g == generation && (frac as usize) < s.len() {
                            let i = frac as usize;
                            let a = s[i];
                            let b = s.get(i + 1).copied().unwrap_or(a);
                            v = a + (b - a) * (frac - i as f64) as f32;
                            shared.pos.store(first + i as u64, Ordering::Relaxed);
                            frac += step;
                            break;
                        }
                        if *g == generation {
                            frac -= s.len() as f64;
                        } else {
                            frac = 0.0;
                        }
                    }
                    match rx.try_recv() {
                        Ok(c) => cur = Some(c),
                        Err(_) => {
                            // nothing buffered: either the end, or the reader is behind (play silence)
                            if let Some((g, first, s)) = &cur {
                                if *g == generation {
                                    shared.pos.store(first + s.len() as u64, Ordering::Relaxed);
                                }
                            }
                            cur = None;
                            frac = 0.0;
                            if shared.eof.load(Ordering::Relaxed) == generation + 1 {
                                shared.finished.store(true, Ordering::Relaxed);
                                shared.paused.store(true, Ordering::Relaxed);
                            }
                            break;
                        }
                    }
                }
            }
            frame.fill(T::from_sample(v));
        }
    };
    match device.build_output_stream(config, callback, |e| eprintln!("audio: {e}"), None) {
        Ok(s) => Ok(s),
        Err(e) => {
            // the callback (holding one reference) is gone, so the receiver can be taken back
            let rx = Arc::try_unwrap(rx).ok().and_then(|m| m.into_inner().ok());
            Err((e.to_string(), rx))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays a generated 1 s tone on the real output device: run with `--ignored` on a desktop.
    #[test]
    #[ignore]
    fn plays_and_seeks() {
        let dir = std::env::temp_dir().join(format!("micdock-player-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("REC00001.WAV");
        let frames = 48000u32;
        let mut w = Vec::new();
        w.extend(b"RIFF");
        w.extend((36 + frames * 3).to_le_bytes());
        w.extend(b"WAVEfmt ");
        w.extend(16u32.to_le_bytes());
        w.extend(1u16.to_le_bytes());
        w.extend(1u16.to_le_bytes());
        w.extend(48000u32.to_le_bytes());
        w.extend((48000u32 * 3).to_le_bytes());
        w.extend(3u16.to_le_bytes());
        w.extend(24u16.to_le_bytes());
        w.extend(b"data");
        w.extend((frames * 3).to_le_bytes());
        for i in 0..frames {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.1 * 8_388_607.0)
                as i32;
            w.extend(&v.to_le_bytes()[..3]);
        }
        std::fs::write(&path, w).unwrap();
        let rec = micdock_core::recordings::open_recording(&path).unwrap();
        let p = Player::start(&[rec]).unwrap();
        thread::sleep(Duration::from_millis(400));
        let a = p.position();
        assert!(a > 4800 && a < 30000, "position after 0.4 s: {a}");
        p.seek(40000);
        thread::sleep(Duration::from_millis(400));
        assert!(
            p.finished(),
            "should have reached the end, at {}",
            p.position()
        );
        assert_eq!(p.position(), 48000);
        assert!(p.error().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
