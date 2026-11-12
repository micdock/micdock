// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Firmware check and download for the RØDE Wireless GO II.
//!
//! RØDE's app reads <https://update.rode.com/rode-devices-manifest.json>. Units with USB PID 0x29/0x2a
//! (the "GO II 2.5" hardware revision) use `wgo2_5-manifest` → `main-version`:
//! `{"update-version": "2.5.0", "firmware-URL": "update.rode.com/wgo2_5/firmware.zip", "firmware-md5": …}`.
//! That firmware.zip holds, per unit (tx / rx):
//!   `wgo25_<tx|rx>_<ver>_<hash>_ota.bin`  main MCU image, Actions Semi OTA ("ACTTEST0" header,
//!                                         XML at 0x200 with board_name + version_name)
//!   `wgo2.5_nordic_<tx|rx>_<ver>.hex`     Nordic radio chip (Intel HEX)
//! Packages are kept in `~/.cache/micdock/firmware/<package>-<version>/`, shared with the Python version.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use md5::{Digest, Md5};

use crate::{paths, Error, Result};

pub const MANIFEST_URL: &str = "https://update.rode.com/rode-devices-manifest.json";
pub const CHANNEL: &str = "main-version";

/// USB PID → (manifest key, unit). RØDE's app treats PIDs 0x29/0x2a as the "GO II 2.5" hardware
/// (WGo2FirmwareUpdater flag 0x3b2): it flashes wgo25_<u>_*_ota.bin + wgo2.5_nordic_<u>_*.hex, which only
/// the wgo2_5 package contains. Older GO II units (PIDs 0x1e/0x1f/0x2d/0x2e) use "wgo2-manifest".
pub fn product(pid: u16) -> Option<(&'static str, &'static str)> {
    match pid {
        0x0029 => Some(("wgo2_5-manifest", "tx")),
        0x002A => Some(("wgo2_5-manifest", "rx")),
        _ => None,
    }
}

pub const MANIFEST_KEYS: [&str; 1] = ["wgo2_5-manifest"];

pub type Version = Vec<u32>;

/// "2.5.0" → [2, 5, 0] (every run of digits).
pub fn vtuple(s: &str) -> Version {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect()
}

pub fn vstr(v: &[u32]) -> String {
    v.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// The newest firmware RØDE offers for one product.
#[derive(Debug, Clone)]
pub struct Latest {
    pub version: Version,
    pub url: String,
    pub md5_url: Option<String>,
}

fn full_url(u: &str) -> String {
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{u}")
    }
}

fn agent(timeout: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout))
        .user_agent(paths::CLIENT_NAME)
        .build()
}

fn net(e: ureq::Error) -> Error {
    Error::other(e.to_string())
}

/// {manifest key: latest firmware} for the GO II products.
pub fn fetch_latest() -> Result<BTreeMap<String, Latest>> {
    let text = agent(30)
        .get(MANIFEST_URL)
        .call()
        .map_err(net)?
        .into_string()?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::other(format!("bad manifest: {e}")))?;
    let mut out = BTreeMap::new();
    for key in MANIFEST_KEYS {
        let entry = &manifest[key][CHANNEL];
        let (Some(ver), Some(url)) = (
            entry["update-version"].as_str(),
            entry["firmware-URL"].as_str(),
        ) else {
            continue;
        };
        if ver.is_empty() || url.is_empty() {
            continue;
        }
        out.insert(
            key.to_string(),
            Latest {
                version: vtuple(ver),
                url: full_url(url),
                md5_url: entry["firmware-md5"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(full_url),
            },
        );
    }
    Ok(out)
}

pub fn fw_root() -> PathBuf {
    paths::cache_dir().join("firmware")
}

/// `~/.cache/micdock/firmware/wgo2_5-2.5.0`
pub fn package_dir(key: &str, version: &[u32]) -> PathBuf {
    fw_root().join(format!(
        "{}-{}",
        key.replace("-manifest", ""),
        vstr(version)
    ))
}

/// The first 32-digit hex string in a text (RØDE's .md5 files).
fn find_md5(text: &[u8]) -> Option<String> {
    text.windows(32)
        .find(|w| w.iter().all(u8::is_ascii_hexdigit))
        .map(|w| String::from_utf8_lossy(w).to_lowercase())
}

/// Download and MD5-verify the package, extract it. Returns its folder. Reuses a verified copy.
/// `progress(done, total)` (total 0 = unknown); `cancel` stops it.
pub fn download(
    key: &str,
    latest: &Latest,
    mut progress: impl FnMut(u64, u64),
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf> {
    let d = package_dir(key, &latest.version);
    if d.join(".verified").exists() {
        return Ok(d);
    }
    std::fs::create_dir_all(&d)?;
    let want = match &latest.md5_url {
        Some(u) => {
            let mut body = Vec::new();
            agent(30)
                .get(u)
                .call()
                .map_err(net)?
                .into_reader()
                .read_to_end(&mut body)?;
            find_md5(&body)
        }
        None => None,
    }
    .ok_or_else(|| {
        Error::other("RØDE did not publish a checksum for this firmware, refusing to use it")
    })?;
    let zpath = d.join("firmware.zip");
    let part = d.join("firmware.zip.part");
    let resp = agent(60).get(&latest.url).call().map_err(net)?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut f = File::create(&part)?;
    let mut md5 = Md5::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            drop(f);
            let _ = std::fs::remove_file(&part);
            return Err(Error::other("cancelled"));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        md5.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    drop(f);
    let got: String = md5.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != want {
        let _ = std::fs::remove_file(&part);
        return Err(Error::other(format!(
            "checksum mismatch (got {got}, expected {want})"
        )));
    }
    std::fs::rename(&part, &zpath)?;
    extract_package(&zpath, &d)?;
    std::fs::write(d.join(".verified"), format!("{want}\n"))?;
    Ok(d)
}

/// Unpack a firmware package into `dir`. Paths inside the archive are never trusted: every file is written
/// under its bare file name (no folders, no `..`). Returns the names written.
pub fn extract_package(zip_path: &Path, dir: &Path) -> Result<Vec<String>> {
    let bad = |e: zip::result::ZipError| Error::other(format!("bad zip: {e}"));
    let mut z = zip::ZipArchive::new(File::open(zip_path)?).map_err(bad)?;
    let mut written = Vec::new();
    for i in 0..z.len() {
        let mut entry = z.by_index(i).map_err(bad)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().map_err(bad)?.into_owned();
        let Some(base) = Path::new(&name).file_name().map(|s| s.to_owned()) else {
            continue;
        };
        if base.is_empty() || base == ".." {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        std::fs::write(dir.join(&base), data)?;
        written.push(base.to_string_lossy().into_owned());
    }
    Ok(written)
}

fn between<'a>(hay: &'a [u8], open: &[u8], close: &[u8]) -> Option<(&'a [u8], usize)> {
    let s = hay.windows(open.len()).position(|w| w == open)? + open.len();
    let e = hay[s..].windows(close.len()).position(|w| w == close)? + s;
    Some((&hay[s..e], e + close.len()))
}

/// (board_name, version_name) from an Actions OTA image, or None.
pub fn ota_info(path: &Path) -> Option<(String, String)> {
    let mut head = Vec::new();
    File::open(path)
        .ok()?
        .take(8192)
        .read_to_end(&mut head)
        .ok()?;
    if !head.starts_with(b"ACTTEST0") {
        return None;
    }
    let (board, after) = between(&head, b"<board_name>", b"</board_name>")?;
    let rest = &head[after..];
    let skip = rest.iter().take_while(|b| b.is_ascii_whitespace()).count();
    if !rest[skip..].starts_with(b"<version_name>") {
        return None;
    }
    let (ver, _) = between(&rest[skip..], b"<version_name>", b"</version_name>")?;
    Some((
        String::from_utf8_lossy(board).into_owned(),
        String::from_utf8_lossy(ver).into_owned(),
    ))
}

/// Files in an extracted package that belong to one unit ("tx" or "rx"): "main", "nordic", "betty",
/// in the order their file names sort (as the Python version lists them).
pub fn package_parts(d: &Path, unit: &str) -> Vec<(&'static str, PathBuf)> {
    let mut names: Vec<String> = std::fs::read_dir(d)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut parts: Vec<(&'static str, PathBuf)> = Vec::new();
    let mut put = |k: &'static str, p: PathBuf| match parts.iter_mut().find(|(key, _)| *key == k) {
        Some(slot) => slot.1 = p,
        None => parts.push((k, p)),
    };
    for name in names {
        let low = name.to_lowercase();
        let p = d.join(&name);
        if low.ends_with("_ota.bin")
            && (low.starts_with(&format!("wgo2_{unit}_"))
                || low.starts_with(&format!("wgo25_{unit}_")))
        {
            put("main", p);
        } else if low.ends_with(".hex") && low.contains(&format!("nordic_{unit}")) {
            put("nordic", p);
        } else if low.ends_with(".spihex") && low.starts_with(&format!("betty{unit}")) {
            put("betty", p);
        }
    }
    parts
}

#[cfg(test)]
mod zip_tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("micdock-zip-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A package with a folder, a nested file and a path that tries to escape the target folder.
    #[test]
    fn extracts_files_flat_and_safely() {
        let d = tmp("pkg");
        let zpath = d.join("firmware.zip");
        let mut w = zip::ZipWriter::new(File::create(&zpath).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.add_directory("pkg/", opts).unwrap();
        w.start_file("pkg/wgo25_tx_2.5.0_ota.bin", opts).unwrap();
        w.write_all(&[0xA5; 5000]).unwrap();
        w.start_file("readme.txt", opts).unwrap();
        w.write_all(b"hello").unwrap();
        w.start_file("../escape.txt", opts).unwrap();
        w.write_all(b"must stay inside").unwrap();
        w.finish().unwrap();

        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let mut names = extract_package(&zpath, &out).unwrap();
        names.sort();
        assert_eq!(
            names,
            ["escape.txt", "readme.txt", "wgo25_tx_2.5.0_ota.bin"]
        );
        assert_eq!(
            std::fs::read(out.join("wgo25_tx_2.5.0_ota.bin")).unwrap(),
            vec![0xA5; 5000]
        );
        assert_eq!(
            std::fs::read(out.join("escape.txt")).unwrap(),
            b"must stay inside"
        );
        assert!(
            !d.join("escape.txt").exists(),
            "nothing written outside the target folder"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rejects_a_broken_package() {
        let d = tmp("broken");
        std::fs::write(d.join("firmware.zip"), b"PK not really").unwrap();
        assert!(extract_package(&d.join("firmware.zip"), &d).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    /// RØDE's real package, when it is in the cache (not in CI): unpacked again it must give exactly the
    /// files that are already there (unpacked earlier, with the previous zip version).
    #[test]
    fn real_package_unpacks_identically_if_cached() {
        let cached = fw_root().join("wgo2_5-2.5.0");
        if !cached.join("firmware.zip").is_file() {
            return;
        }
        let d = tmp("real");
        let names = extract_package(&cached.join("firmware.zip"), &d).unwrap();
        assert!(names.len() >= 4, "{names:?}");
        for n in &names {
            assert_eq!(
                std::fs::read(d.join(n)).unwrap(),
                std::fs::read(cached.join(n)).unwrap(),
                "{n}"
            );
        }
        std::fs::remove_dir_all(d).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(vtuple("2.5.0"), vec![2, 5, 0]);
        assert_eq!(vstr(&vtuple("v2.10")), "2.10");
        assert!(vtuple("2.5.0") > vtuple("2.2.0"));
        assert!(vtuple("2.10.0") > vtuple("2.9.9"));
    }

    #[test]
    fn checksum_text() {
        assert_eq!(
            find_md5(b"MD5 (x) = 0123456789ABCDEF0123456789abcdef\n").unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
        assert!(find_md5(b"nope").is_none());
    }

    #[test]
    fn urls() {
        assert_eq!(
            full_url("update.rode.com/x.zip"),
            "https://update.rode.com/x.zip"
        );
        assert_eq!(full_url("https://a/b"), "https://a/b");
    }
}
