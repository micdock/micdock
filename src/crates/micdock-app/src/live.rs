// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Live audio from the receiver, which is also a USB sound card (only while it is switched on).
//!
//! The audio is captured through the sound server (PipeWire or PulseAudio, with `parec`), never from the
//! ALSA device directly: the sound server shares the receiver, so other programs (OBS, Audacity, a video
//! call…) can record from it at the same time. A reader thread computes the level meters and a spectrum, and
//! writes the stream to a WAV file while recording.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const RATE: u32 = 48000;
pub const CHANNELS: usize = 2;
const FRAME_BYTES: usize = 3 * CHANNELS; // s24le
pub const BANDS: usize = 24;
const FFT: usize = 2048;
/// Meters show −60 … 0 dBFS; the spectrum −90 … −10 dB.
pub const FLOOR: f32 = -60.0;
const DECAY_DB_PER_S: f32 = 24.0;

/// The receiver's source in the sound server, e.g.
/// `alsa_input.usb-R__DE_Wireless_GO_II_RX_22481E5B-01.analog-stereo` (its name contains the USB serial).
pub fn find_source(serial: &str) -> Option<String> {
    let out = Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let tag = format!("_{serial}-");
    text.lines()
        .filter_map(|l| l.split('\t').nth(1))
        .find(|name| name.contains(&tag) && !name.ends_with(".monitor"))
        .map(str::to_string)
}

/// An open WAV file being written (24-bit stereo, 48 kHz).
struct Wav {
    file: BufWriter<File>,
    path: PathBuf,
    bytes: u64,
}

impl Wav {
    fn create(path: &Path) -> std::io::Result<Wav> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&header(0))?;
        Ok(Wav {
            file,
            path: path.to_path_buf(),
            bytes: 0,
        })
    }

    /// Write the final sizes into the header and close the file.
    fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush()?;
        let mut f = self.file.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header(self.bytes))?;
        f.sync_all()?;
        Ok(self.path)
    }
}

fn header(data: u64) -> Vec<u8> {
    let data = data.min(u32::MAX as u64 - 36) as u32;
    let mut h = Vec::with_capacity(44);
    h.extend(b"RIFF");
    h.extend((36 + data).to_le_bytes());
    h.extend(b"WAVEfmt ");
    h.extend(16u32.to_le_bytes());
    h.extend(1u16.to_le_bytes()); // PCM
    h.extend((CHANNELS as u16).to_le_bytes());
    h.extend(RATE.to_le_bytes());
    h.extend((RATE * FRAME_BYTES as u32).to_le_bytes());
    h.extend((FRAME_BYTES as u16).to_le_bytes());
    h.extend(24u16.to_le_bytes());
    h.extend(b"data");
    h.extend(data.to_le_bytes());
    h
}

/// What the reader thread shares with the window.
#[derive(Default)]
struct Shared {
    /// RMS level per channel, dBFS (with fall-back).
    level: [f32; CHANNELS],
    /// Peak per channel, dBFS (held, then falling).
    peak: [f32; CHANNELS],
    /// Spectrum of both channels together, dB per band.
    bands: Vec<f32>,
    recording: Option<Wav>,
    recorded_since: Option<Instant>,
    error: Option<String>,
    ended: bool,
}

/// A running capture. Dropping it stops the capture (and closes a recording).
pub struct Live {
    pub source: String,
    child: Child,
    shared: Arc<Mutex<Shared>>,
}

/// A snapshot for drawing.
pub struct Meters {
    pub level: [f32; CHANNELS],
    pub peak: [f32; CHANNELS],
    pub bands: Vec<f32>,
    /// (file, seconds, bytes) while recording.
    pub recording: Option<(PathBuf, f64, u64)>,
    pub error: Option<String>,
    pub ended: bool,
}

impl Live {
    pub fn start(source: &str) -> Result<Live, String> {
        let mut child = Command::new("parec")
            .args([
                "-d",
                source,
                "--format=s24le",
                "--rate=48000",
                "--channels=2",
                "--raw",
                "--latency-msec=30",
                "--client-name=MicDock",
                "--stream-name=MicDock live",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start parec (PipeWire or PulseAudio is needed): {e}"))?;
        let stdout = child.stdout.take().ok_or("parec has no output")?;
        let shared = Arc::new(Mutex::new(Shared {
            level: [FLOOR; CHANNELS],
            peak: [FLOOR; CHANNELS],
            bands: vec![-90.0; BANDS],
            ..Default::default()
        }));
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("micdock-live".into())
            .spawn(move || read_loop(stdout, sh))
            .map_err(|e| e.to_string())?;
        Ok(Live {
            source: source.to_string(),
            child,
            shared,
        })
    }

    pub fn meters(&self) -> Meters {
        let Ok(s) = self.shared.lock() else {
            return Meters {
                level: [FLOOR; 2],
                peak: [FLOOR; 2],
                bands: vec![],
                recording: None,
                error: None,
                ended: true,
            };
        };
        Meters {
            level: s.level,
            peak: s.peak,
            bands: s.bands.clone(),
            recording: s.recording.as_ref().map(|w| {
                let secs = w.bytes as f64 / (RATE as f64 * FRAME_BYTES as f64);
                (w.path.clone(), secs, w.bytes)
            }),
            error: s.error.clone(),
            ended: s.ended,
        }
    }

    pub fn recording(&self) -> bool {
        self.shared
            .lock()
            .map(|s| s.recording.is_some())
            .unwrap_or(false)
    }

    /// Start writing the stream to this file.
    pub fn start_recording(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let wav = Wav::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut s = self
            .shared
            .lock()
            .map_err(|_| "the capture stopped".to_string())?;
        s.recording = Some(wav);
        s.recorded_since = Some(Instant::now());
        Ok(())
    }

    /// Stop recording; returns the finished file.
    pub fn stop_recording(&self) -> Option<Result<PathBuf, String>> {
        let wav = self.shared.lock().ok()?.recording.take()?;
        Some(wav.finish().map_err(|e| e.to_string()))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.stop_recording();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn db(x: f32) -> f32 {
    if x > 0.0 {
        20.0 * x.log10()
    } else {
        -200.0
    }
}

fn read_loop(mut input: impl Read, shared: Arc<Mutex<Shared>>) {
    let mut buf = vec![0u8; 1024 * FRAME_BYTES];
    let mut carry: Vec<u8> = Vec::new();
    let mut mono: Vec<f32> = Vec::with_capacity(FFT);
    let window: Vec<f32> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT - 1) as f32).cos())
        .collect();
    let mut last = Instant::now();
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        let whole = carry.len() - carry.len() % FRAME_BYTES;
        let chunk: Vec<u8> = carry.drain(..whole).collect();
        if chunk.is_empty() {
            continue;
        }
        // levels of this chunk
        let mut sum = [0f64; CHANNELS];
        let mut pk = [0f32; CHANNELS];
        for f in chunk.chunks_exact(FRAME_BYTES) {
            let mut m = 0.0;
            for (c, s) in f.chunks_exact(3).enumerate() {
                let v = (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0;
                sum[c] += (v * v) as f64;
                pk[c] = pk[c].max(v.abs());
                m += v;
            }
            mono.push(m / CHANNELS as f32);
        }
        let frames = (chunk.len() / FRAME_BYTES) as f64;
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32();
        last = now;
        let bands = (mono.len() >= FFT).then(|| {
            let start = mono.len() - FFT;
            let b = spectrum(&mono[start..], &window);
            mono.drain(..start + FFT / 2); // half overlap
            b
        });
        let Ok(mut s) = shared.lock() else { break };
        for c in 0..CHANNELS {
            let rms = db((sum[c] / frames).sqrt() as f32).max(FLOOR);
            let peak = db(pk[c]).max(FLOOR);
            s.level[c] = rms.max(s.level[c] - DECAY_DB_PER_S * dt);
            s.peak[c] = peak.max(s.peak[c] - DECAY_DB_PER_S / 2.0 * dt);
        }
        if let Some(b) = bands {
            for (old, new) in s.bands.iter_mut().zip(b) {
                *old = new.max(*old - DECAY_DB_PER_S * 2.0 * dt);
            }
        }
        if let Some(w) = s.recording.as_mut() {
            if let Err(e) = w.file.write_all(&chunk) {
                s.error = Some(format!("recording stopped: {e}"));
                if let Some(w) = s.recording.take() {
                    let _ = w.finish();
                }
            } else {
                w.bytes += chunk.len() as u64;
            }
        }
    }
    if let Ok(mut s) = shared.lock() {
        s.ended = true;
        if let Some(w) = s.recording.take() {
            match w.finish() {
                Ok(p) => {
                    s.error = Some(format!(
                        "the receiver's audio stopped; recording saved as {}",
                        p.display()
                    ))
                }
                Err(e) => s.error = Some(format!("recording stopped: {e}")),
            }
        }
    }
}

/// `BANDS` log-spaced bands from 40 Hz to 20 kHz, in dB (0 dB = full-scale sine).
fn spectrum(x: &[f32], window: &[f32]) -> Vec<f32> {
    let mut re: Vec<f32> = x.iter().zip(window).map(|(a, w)| a * w).collect();
    let mut im = vec![0f32; FFT];
    fft(&mut re, &mut im);
    let mag: Vec<f32> = (0..FFT / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() * 4.0 / FFT as f32)
        .collect();
    let hz = |k: usize| k as f32 * RATE as f32 / FFT as f32;
    let (lo, hi) = (40f32, 20000f32);
    (0..BANDS)
        .map(|b| {
            let f0 = lo * (hi / lo).powf(b as f32 / BANDS as f32);
            let f1 = lo * (hi / lo).powf((b + 1) as f32 / BANDS as f32);
            let peak = (1..FFT / 2)
                .filter(|&k| hz(k) >= f0 && hz(k) < f1)
                .map(|k| mag[k])
                .fold(0f32, f32::max);
            // narrow low bands may hold no FFT bin: use the nearest one
            let peak = if peak > 0.0 {
                peak
            } else {
                mag[((f0 + f1) / 2.0 / hz(1)).round() as usize]
            };
            db(peak).max(-90.0)
        })
        .collect()
}

/// In-place iterative radix-2 FFT.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1f32, 0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectrum_finds_a_1khz_tone() {
        let x: Vec<f32> = (0..FFT)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE as f32).sin())
            .collect();
        let w: Vec<f32> = (0..FFT)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT - 1) as f32).cos())
            .collect();
        let b = spectrum(&x, &w);
        let top = (0..BANDS).max_by(|&a, &c| b[a].total_cmp(&b[c])).unwrap();
        let f0 = 40f32 * (500f32).powf(top as f32 / BANDS as f32);
        let f1 = 40f32 * (500f32).powf((top + 1) as f32 / BANDS as f32);
        assert!(
            f0 <= 1100.0 && f1 >= 900.0,
            "loudest band {f0:.0}–{f1:.0} Hz"
        );
        assert!(b[top] > -12.0 && b[top] < 0.0, "level {}", b[top]); // a −6 dBFS sine, minus window loss
    }

    #[test]
    fn wav_header_sizes() {
        let h = header(6000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 6036);
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 6000);
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 24);
    }
}

#[cfg(test)]
mod hardware {
    use super::*;

    /// Records 2 s from a connected, switched-on receiver: `MICDOCK_TEST_RX=<serial> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn records_from_the_receiver() {
        let Ok(serial) = std::env::var("MICDOCK_TEST_RX") else {
            return;
        };
        let src = find_source(&serial).expect("receiver source (is it switched on?)");
        let live = Live::start(&src).unwrap();
        let path = std::env::temp_dir().join(format!("micdock-live-{}.wav", std::process::id()));
        live.start_recording(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let m = live.meters();
        let (_, secs, _) = m.recording.clone().unwrap();
        let out = live.stop_recording().unwrap().unwrap();
        let len = std::fs::metadata(&out).unwrap().len();
        eprintln!(
            "recorded {secs:.2} s, {len} bytes; level {:?} peak {:?}; bands {:?}",
            m.level,
            m.peak,
            &m.bands[..6]
        );
        assert!(secs > 1.5 && secs < 2.5, "{secs}");
        assert_eq!((len - 44) % 6, 0);
        let h = std::fs::read(&out).unwrap();
        assert_eq!(
            u32::from_le_bytes([h[40], h[41], h[42], h[43]]) as u64,
            len - 44
        );
        std::fs::remove_file(out).unwrap();
    }
}
