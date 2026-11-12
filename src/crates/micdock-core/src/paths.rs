// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Where MicDock keeps its files: the XDG folders, the same ones the Python version uses.

use std::path::PathBuf;

pub const APP_ID: &str = "micdock";
/// Client name in HTTP requests (to RØDE's update and guide servers).
pub const CLIENT_NAME: &str = "micdock";

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback),
    }
}

/// `~/.config/micdock`
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join(APP_ID)
}

/// `~/.cache/micdock`
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join(APP_ID)
}

/// `~/.config/micdock/config.json`
pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}
