// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Formatting and small helpers (same output as the Python app's util.py).

use chrono::TimeZone;

/// Clock values before 2020 mean the TX clock was never set.
pub const Y2020: f64 = 1_577_836_800.0;

/// 1:23 / 2:49:02
pub fn fmt_dur(s: f64) -> String {
    let s = s.round() as i64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// 613.0 MB
pub fn fmt_size(b: u64) -> String {
    let mut v = b as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 || unit == "GB" {
            return if unit == "B" || unit == "KB" {
                format!("{v:.0} {unit}")
            } else {
                format!("{v:.1} {unit}")
            };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// Local time formatted with a strftime pattern.
pub fn local(t: f64, fmt: &str) -> String {
    match chrono::Local.timestamp_opt(t.floor() as i64, 0) {
        chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => {
            d.format(fmt).to_string()
        }
        chrono::LocalResult::None => "?".into(),
    }
}

/// Open a file, folder or URL with the default app, detached and silenced: its own messages must not end
/// up in MicDock's terminal, and it keeps running when MicDock closes.
pub fn open_external(target: &str) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_and_sizes() {
        assert_eq!(fmt_dur(83.2), "1:23");
        assert_eq!(fmt_dur(10142.0), "2:49:02");
        assert_eq!(fmt_size(500), "500 B");
        assert_eq!(fmt_size(642_779_136), "613.0 MB");
        assert_eq!(fmt_size(1_460_470_364), "1.4 GB");
    }
}
