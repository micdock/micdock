// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! micdock-firmware: check for, download and (soon) install Wireless GO II firmware.
//! Same output as the Python version.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use micdock_core::fwupdate::{self, Package, Updater};
use micdock_core::{devices, firmware, hid};

const USAGE: &str = "\
micdock-firmware: check for, download and install Wireless GO II firmware.

  micdock-firmware check                 latest version from RØDE vs. the connected units
  micdock-firmware download              download + MD5-verify the latest package
  micdock-firmware plan    tx|rx [DIR]   dry run of an install: print every step, touch nothing
  micdock-firmware install tx|rx [DIR]   install (asks for confirmation; keep the unit plugged in)";

/// Python's repr of a str (enough for file and board names).
fn repr(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `plan tx|rx [DIR]` (dry run) and `install tx|rx [DIR]`.
fn install(cmd: &str) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let Some(unit) = args.first().filter(|u| fwupdate::unit_pids(u).is_some()) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let fail = |msg: String| {
        eprintln!("{msg}");
        ExitCode::FAILURE
    };
    let dir = match args.get(1) {
        Some(d) => d.into(),
        None => match fwupdate::default_pkg_dir() {
            Ok(d) => d,
            Err(e) => return fail(e.to_string()),
        },
    };
    let pkg = match Package::load(&dir, unit) {
        Ok(p) => p,
        Err(e) => return fail(e.to_string()),
    };
    println!("{}", fwupdate::plan(&pkg));
    if cmd == "plan" {
        return ExitCode::SUCCESS;
    }
    let units = fwupdate::find_units(Some(unit));
    if units.len() != 1 {
        return fail(format!(
            "need exactly one {} connected, found {}",
            unit.to_uppercase(),
            units.len()
        ));
    }
    let (info, serial, _) = &units[0];
    let (normal, _) = fwupdate::unit_pids(unit).unwrap();
    if info.pid == normal {
        match fwupdate::battery(info) {
            Ok(b) if b < fwupdate::MIN_BATTERY => {
                return fail(format!(
                    "battery is {b} %; charge it to at least {} % first",
                    fwupdate::MIN_BATTERY
                ))
            }
            Ok(_) => {}
            Err(e) => return fail(format!("could not read the battery: {e}")),
        }
    }
    println!(
        "\nThis installs {} on {} {} (serial {serial}). Keep it plugged in until it is finished.",
        pkg.version,
        unit.to_uppercase(),
        info.node
    );
    print!("Type INSTALL to continue: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() || line.trim() != "INSTALL" {
        return fail("cancelled".into());
    }
    let (path, mut log) = match fwupdate::open_log(serial) {
        Ok(x) => x,
        Err(e) => return fail(format!("cannot open the log: {e}")),
    };
    println!("log: {}", path.display());
    let (log_a, log_b) = {
        // the transport and the CLI both log to the same file
        let shared = std::rc::Rc::new(std::cell::RefCell::new(move |s: &str| log(s)));
        let a = shared.clone();
        (
            Box::new(move |s: &str| (a.borrow_mut())(s)) as Box<dyn FnMut(&str)>,
            shared,
        )
    };
    let mut link = fwupdate::HidTransport::new(serial, unit, log_a);
    let result = {
        let mut up = Updater::new(&mut link, &pkg);
        up.progress = Box::new(|phase, frac| {
            print!("\r{phase:<6} {:5.1} %", frac * 100.0);
            let _ = std::io::stdout().flush();
        });
        up.run(false)
    };
    match result {
        Ok((pid, ver)) => {
            let pid = pid.map(|p| format!("{p:04x}")).unwrap_or("None".into());
            let ver = ver
                .map(|(a, b, c)| format!("{a}.{b}.{c}"))
                .unwrap_or("None".into());
            (log_b.borrow_mut())(&format!("done: PID {pid} version {ver}"));
            println!(
                "\ndone: unit is back as PID {pid}, version {ver}. log: {}",
                path.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            (log_b.borrow_mut())(&format!("FAILED: {e:?}"));
            fail(format!("\nFAILED: {e}\nlog: {}", path.display()))
        }
    }
}

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "check".into());
    match cmd.as_str() {
        "check" | "download" => {}
        "plan" | "install" => return install(&cmd),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    }
    let latest = match firmware::fetch_latest() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not read RØDE's update manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (key, info) in &latest {
        println!(
            "{key}: latest {} ({})",
            firmware::vstr(&info.version),
            info.url
        );
    }
    for d in hid::find_devices() {
        let Some((key, unit)) = firmware::product(d.pid) else {
            continue;
        };
        let bcd = std::fs::read_to_string(devices::usb_dir(&d.node).join("bcdDevice"))
            .unwrap_or_default();
        let cur = devices::device_version(bcd.trim()).map(|(a, b, c)| vec![a, b, c]);
        let new = latest.get(key).map(|l| l.version.clone());
        let state = match (&cur, &new) {
            (Some(c), Some(n)) if n > c => "update available",
            (Some(_), Some(_)) => "up to date",
            _ => "?",
        };
        let target = match &new {
            Some(n) => format!("{} ({state})", firmware::vstr(n)),
            None => state.to_string(),
        };
        println!(
            "{} {}: {} -> {}",
            unit.to_uppercase(),
            d.node,
            cur.as_deref()
                .map(firmware::vstr)
                .unwrap_or_else(|| "?".into()),
            target
        );
    }
    if cmd == "download" {
        for (key, info) in &latest {
            let dir = match firmware::download(
                key,
                info,
                |a, b| {
                    eprint!("\r{a}/{b}");
                    let _ = std::io::stderr().flush();
                },
                None,
            ) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("\n{key}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            println!("\n{key} verified in {}", dir.display());
            for unit in ["tx", "rx"] {
                let parts = firmware::package_parts(&dir, unit);
                let shown = parts
                    .iter()
                    .map(|(k, p)| {
                        let base = p
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        format!("{}: {}", repr(k), repr(&base))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ota = parts
                    .iter()
                    .find(|(k, _)| *k == "main")
                    .and_then(|(_, p)| firmware::ota_info(Path::new(p)))
                    .map(|(b, v)| format!("({}, {})", repr(&b), repr(&v)))
                    .unwrap_or_default();
                println!("  {unit}: {{{shown}}} {ota}");
            }
        }
    }
    ExitCode::SUCCESS
}
