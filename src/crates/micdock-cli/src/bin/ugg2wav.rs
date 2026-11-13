// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! ugg2wav — convert RØDE Wireless GO II on-board recordings (RECxxxxx.UGG / RECxxxxx.WAV) to WAV or MP3.
//! Same options and output as the Python version.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::TimeZone;
use clap::Parser;
use micdock_core::recordings::{self, Encoding, Progress, Recording};

const ABOUT: &str = "\
Convert RØDE Wireless GO II on-board recordings (RECxxxxx.UGG / RECxxxxx.WAV) to WAV or MP3.

--mp3 pipes the raw PCM into ffmpeg (libmp3lame), so ffmpeg must be installed.
--join writes one file per recording session instead of one per file.

Markers set with the transmitter's button are kept: as cue points in WAV files and as chapters in
MP3 files (shifted to the right place when pieces are joined). --labels also writes them to a .txt
next to each output file, which Audacity imports as a label track (File > Import > Labels).

Dates: the TX stamps files with its own clock (param 0x01, u32 seconds), which counts from
2000-01-01 if nobody ever set it. With --clock auto (the default) we read the TX clock over HID,
take offset = real_now - tx_now and add it to each file's date. That is only right if the clock
has not been reset (e.g. by a flat battery) since the recording was made. The offset is printed
so you can reuse it later with --clock SECONDS when the TX is not connected.";

#[derive(Parser)]
#[command(name = "ugg2wav", version, about = ABOUT)]
struct Cli {
    #[arg(short, long, default_value = ".")]
    outdir: PathBuf,
    /// overwrite existing output files
    #[arg(short, long)]
    force: bool,
    /// write MP3 instead of WAV (needs ffmpeg)
    #[arg(long)]
    mp3: bool,
    /// MP3 VBR quality, 0 = best (default 2, roughly 190 kbps)
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(0..=9), conflicts_with = "bitrate")]
    quality: Option<u8>,
    /// MP3 constant bitrate instead of VBR, e.g. 192k
    #[arg(short, long)]
    bitrate: Option<String>,
    /// one output file per recording session (joins the ~31-minute splits)
    #[arg(long)]
    join: bool,
    /// also write the markers to an Audacity label file (.txt) next to each output file
    #[arg(long)]
    labels: bool,
    /// date correction: 'auto' (read TX clock), 'none', or an offset in seconds
    #[arg(long, default_value = "auto", allow_hyphen_values = true)]
    clock: String,
    /// TX hidraw node for --clock auto (default: find it)
    #[arg(long)]
    dev: Option<String>,
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn have_ffmpeg() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("ffmpeg").is_file()))
        .unwrap_or(false)
}

fn main() -> ExitCode {
    let a = Cli::parse();
    let offset: i64 = match a.clock.as_str() {
        "none" => 0,
        "auto" => match recordings::tx_clock_offset(a.dev.as_deref()) {
            Ok(Some(o)) => o,
            Ok(None) => {
                eprintln!("TX not found for --clock auto. Connect it, or pass --clock SECONDS / --clock none.");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!(
                    "could not read the TX clock ({e}). Pass --clock SECONDS or --clock none."
                );
                return ExitCode::FAILURE;
            }
        },
        s => match s.parse() {
            Ok(o) => o,
            Err(_) => {
                eprintln!("--clock: expected auto, none or a number of seconds, got {s:?}");
                return ExitCode::FAILURE;
            }
        },
    };
    println!("date offset: {offset:+} s (reuse with --clock {offset})");
    let (encoding, ext) = if a.mp3 {
        if !have_ffmpeg() {
            eprintln!("--mp3 needs ffmpeg: sudo apt install ffmpeg");
            return ExitCode::FAILURE;
        }
        (
            Encoding::Mp3(recordings::mp3_args(
                a.quality.unwrap_or(2),
                a.bitrate.as_deref(),
            )),
            ".mp3",
        )
    } else {
        (Encoding::Wav, ".wav")
    };
    if let Err(e) = std::fs::create_dir_all(&a.outdir) {
        eprintln!("cannot create {}: {e}", a.outdir.display());
        return ExitCode::FAILURE;
    }
    let mut rc = ExitCode::SUCCESS;
    let mut infos: Vec<Recording> = Vec::new();
    for src in &a.files {
        match recordings::open_recording(src) {
            Ok(i) => infos.push(i),
            Err(e) => {
                eprintln!("FAIL {}: {e}", src.display());
                rc = ExitCode::FAILURE;
            }
        }
    }
    let groups: Vec<Vec<Recording>> = if a.join {
        recordings::group_sessions(infos)
    } else {
        infos.sort_by(|x, y| x.name.cmp(&y.name));
        infos.into_iter().map(|i| vec![i]).collect()
    };
    for grp in groups {
        let (first, last) = (&grp[0], &grp[grp.len() - 1]);
        let mut name = stem(&first.name).to_string();
        if grp.len() > 1 {
            name.push('-');
            name.push_str(stem(&last.name).get(3..).unwrap_or(""));
        }
        let dst: PathBuf = Path::new(&a.outdir).join(format!("{name}{ext}"));
        if dst.exists() && !a.force {
            println!("skip {} (exists)", dst.display());
            continue;
        }
        let end = last.mtime + offset as f64;
        let start = first.mtime + offset as f64 - first.seconds();
        let frames = match recordings::export(
            &grp,
            &dst,
            &encoding,
            Some(start),
            Some(end),
            Progress::default(),
        ) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("FAIL {}: {e}", dst.display());
                rc = ExitCode::FAILURE;
                continue;
            }
        };
        let marks = recordings::joined_markers(&grp).len();
        if a.labels {
            if let Err(e) = recordings::write_labels(&grp, &dst) {
                eprintln!("FAIL {}: {e}", recordings::label_path(&dst).display());
                rc = ExitCode::FAILURE;
            }
        }
        let s = frames as f64 / first.rate as f64;
        let when = match chrono::Local.timestamp_opt(start.floor() as i64, 0) {
            chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => {
                d.format("%a %Y-%m-%d %H:%M").to_string()
            }
            chrono::LocalResult::None => "?".into(),
        };
        let names = grp
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{names} -> {}  {when}  {}:{:02}:{:02}  {} Hz {}-bit {}ch{}",
            dst.display(),
            (s / 3600.0).floor() as u64,
            ((s % 3600.0) / 60.0).floor() as u64,
            (s % 60.0).floor() as u64,
            first.rate,
            first.bits,
            first.channels,
            match marks {
                0 => String::new(),
                1 => "  1 marker".into(),
                n => format!("  {n} markers"),
            }
        );
    }
    rc
}
