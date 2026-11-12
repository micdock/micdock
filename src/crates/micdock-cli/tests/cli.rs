// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! The command-line tools as a user runs them (no device needed).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use micdock_core::testdata::{samples, write_ugg, write_wav};

fn run(bin: &str, args: &[&str]) -> Output {
    let exe = match bin {
        "rodego2" => env!("CARGO_BIN_EXE_rodego2"),
        "ugg2wav" => env!("CARGO_BIN_EXE_ugg2wav"),
        _ => env!("CARGO_BIN_EXE_micdock-firmware"),
    };
    Command::new(exe).args(args).output().expect("run")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("micdock-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn wav_samples(path: &Path) -> Vec<i32> {
    let b = std::fs::read(path).unwrap();
    b[44..]
        .as_chunks::<3>()
        .0
        .iter()
        .map(|s| i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8)
        .collect()
}

#[test]
fn rodego2_refuses_the_destructive_commands() {
    // refused before any device is opened, so this is safe anywhere
    for payload in [["51", "01"], ["4a", "01"]] {
        let o = run("rodego2", &["out", "1", payload[0], payload[1]]);
        assert!(!o.status.success(), "{payload:?} must be refused");
        assert!(
            text(&o).contains("refused") || text(&o).contains("deletes all"),
            "{}",
            text(&o)
        );
    }
}

#[test]
fn rodego2_lists_its_commands() {
    let o = run("rodego2", &["--help"]);
    assert!(o.status.success());
    let t = text(&o);
    for cmd in [
        "list",
        "scan",
        "rec",
        "button",
        "pad",
        "mode",
        "gain",
        "powersaver",
        "erase",
    ] {
        assert!(t.contains(cmd), "--help lacks {cmd}");
    }
}

#[test]
fn rodego2_rejects_bad_values() {
    for args in [
        vec!["gain", "-5"],
        vec!["rec", "sometimes"],
        vec!["out", "1", "zz"],
    ] {
        let o = run("rodego2", &args);
        assert!(!o.status.success(), "{args:?} should fail");
    }
}

#[test]
fn ugg2wav_converts_wav_and_ugg() {
    let d = tmp("convert");
    let (w, u) = (d.join("REC00051.WAV"), d.join("REC00007.UGG"));
    let (sw, su) = (samples(4800, 1), samples(2000, 2));
    write_wav(&w, &sw, &[100], 1_700_000_000.0);
    write_ugg(&u, 0, &su, 1_600_000_000.0);
    let out = d.join("out");
    let o = run(
        "ugg2wav",
        &[
            "--clock",
            "none",
            "-o",
            out.to_str().unwrap(),
            w.to_str().unwrap(),
            u.to_str().unwrap(),
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(wav_samples(&out.join("REC00051.wav")), sw);
    assert_eq!(wav_samples(&out.join("REC00007.wav")), su);
    // a second run skips existing files
    let o = run(
        "ugg2wav",
        &[
            "--clock",
            "none",
            "-o",
            out.to_str().unwrap(),
            w.to_str().unwrap(),
        ],
    );
    assert!(text(&o).contains("skip"), "{}", text(&o));
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn ugg2wav_joins_split_pieces() {
    let d = tmp("join");
    let (a, b) = (d.join("REC00001.UGG"), d.join("REC00002.UGG"));
    let (sa, sb) = (samples(1200, 3), samples(600, 4));
    write_ugg(&a, 0, &sa, 100.0);
    write_ugg(&b, 1200, &sb, 200.0);
    let out = d.join("out");
    let o = run(
        "ugg2wav",
        &[
            "--clock",
            "none",
            "--join",
            "-o",
            out.to_str().unwrap(),
            b.to_str().unwrap(),
            a.to_str().unwrap(),
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    let mut want = sa;
    want.extend(sb);
    assert_eq!(wav_samples(&out.join("REC00001-00002.wav")), want);
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn ugg2wav_reports_unreadable_files() {
    let d = tmp("junk");
    let p = d.join("REC00009.WAV");
    std::fs::write(&p, b"not audio").unwrap();
    let o = run(
        "ugg2wav",
        &[
            "--clock",
            "none",
            "-o",
            d.to_str().unwrap(),
            p.to_str().unwrap(),
        ],
    );
    assert!(text(&o).contains("FAIL"), "{}", text(&o));
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn micdock_firmware_usage_and_missing_package() {
    let o = run("micdock-firmware", &["frobnicate"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("micdock-firmware check"), "{}", text(&o));
    let o = run(
        "micdock-firmware",
        &["plan", "tx", "/nonexistent/micdock-test-package"],
    );
    assert!(!o.status.success(), "{}", text(&o));
}
