// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! The Device page: name, clock, firmware (check, download, install) and the unit's details.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use cosmic::app::Task;
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget::{self, settings};
use micdock_core::devices::{self, DeviceStatus, Role};
use micdock_core::firmware::{self, Latest};
use micdock_core::fwupdate::{self, Package, Updater};

use crate::config::Config;
use crate::util::{local, Y2020};
use crate::{job, Message};

/// The name field holds at most 21 bytes (UTF-8).
pub const NAME_MAX: usize = 21;

pub enum Dialog {
    Clock {
        serial: String,
        title: String,
        body: String,
    },
    Install {
        serial: String,
        unit: String,
        title: String,
        plan: String,
        typed: String,
        show_plan: bool,
    },
    Info {
        title: String,
        body: String,
    },
}

/// A running firmware installation.
pub struct Install {
    pub serial: String,
    pub phase: String,
    pub frac: f64,
}

#[derive(Default)]
pub struct DevicePage {
    /// Name being typed, per serial (until it is saved).
    names: HashMap<String, String>,
    renaming: Option<String>,
    clocking: Option<String>,
    latest: Option<BTreeMap<String, Latest>>,
    fw_note: Option<String>,
    checking: bool,
    /// When the last check succeeded (unix seconds).
    checked_at: Option<f64>,
    /// Download progress (done, total), while downloading.
    download: Option<(u64, u64)>,
    pub install: Option<Install>,
    pub dialog: Option<Dialog>,
    pub status: Option<String>,
    /// Parameter scans per serial: (now, previous).
    scans: HashMap<String, (Scan, Option<Scan>)>,
    scanning: Option<String>,
}

/// Every parameter that answered a GET: id → value (trailing zeros removed).
pub type Scan = BTreeMap<u8, Vec<u8>>;

#[derive(Clone, Debug)]
pub enum DevMsg {
    NameEdit(String, String),
    Rename(String),
    Renamed(String, String, Result<(), String>),
    ClockAsk(String),
    ClockConfirm,
    ClockDone(Result<i64, String>),
    FwCheck,
    FwChecked(Result<BTreeMap<String, Latest>, String>),
    FwDownload,
    FwProgress(u64, u64),
    FwDownloaded(Result<PathBuf, String>),
    InstallAsk(String),
    InstallTyped(String),
    InstallPlan(bool),
    InstallConfirm,
    InstallProgress(String, f64),
    Installed(Result<String, String>, String),
    DialogClose,
    Scan(String),
    Scanned(String, Result<Scan, String>),
}

/// Cut a name to what fits in the device's name field (at a character boundary).
fn fit_name(name: &str) -> String {
    let mut end = name.len().min(NAME_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

fn vtuple(v: (u32, u32, u32)) -> Vec<u32> {
    vec![v.0, v.1, v.2]
}

fn who(u: &DeviceStatus) -> String {
    u.name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| u.role.as_str().to_string())
}

impl DevicePage {
    /// A firmware update is running: nothing else may talk to the units.
    pub fn updating(&self) -> bool {
        self.install.is_some()
    }

    /// Demo mode: RØDE's latest firmware as "already checked" (no network).
    pub fn demo(&mut self) {
        let latest = Latest {
            version: vec![2, 5, 0],
            url: "https://update.rode.com/".into(),
            md5_url: None,
        };
        self.latest = Some(
            [("wgo2_5-manifest".to_string(), latest)]
                .into_iter()
                .collect(),
        );
        self.checked_at = Some(chrono::Utc::now().timestamp() as f64);
    }

    pub fn check_firmware(&mut self) -> Task<Message> {
        if self.checking {
            return Task::none();
        }
        self.checking = true;
        self.fw_note = Some("Checking update.rode.com…".into());
        job(|_| {
            // take at least a second, so "Checking…" can be read and the result is clearly new
            let t0 = std::time::Instant::now();
            let r = firmware::fetch_latest().map_err(|e| e.to_string());
            std::thread::sleep(std::time::Duration::from_secs(1).saturating_sub(t0.elapsed()));
            DevMsg::FwChecked(r)
        })
    }

    fn key(&self) -> Option<(String, Latest)> {
        self.latest
            .as_ref()?
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    /// The verified package folder of the latest firmware, if downloaded.
    fn package(&self) -> Option<PathBuf> {
        let (key, l) = self.key()?;
        let d = firmware::package_dir(&key, &l.version);
        d.join(".verified").exists().then_some(d)
    }

    /// The newer version RØDE offers for this unit, if any.
    fn newer(&self, u: &DeviceStatus) -> Option<Vec<u32>> {
        let (key, _) = firmware::product(u.pid)?;
        let new = &self.latest.as_ref()?.get(key)?.version;
        let cur = vtuple(u.fw_version?);
        (*new > cur).then(|| new.clone())
    }

    pub fn update(
        &mut self,
        msg: DevMsg,
        units: &[DeviceStatus],
        cfg: &mut Config,
        live_offset: Option<i64>,
    ) -> Task<Message> {
        let unit = |serial: &str| units.iter().find(|u| u.serial == serial);
        match msg {
            DevMsg::NameEdit(serial, text) => {
                self.names.insert(serial, fit_name(&text));
            }
            DevMsg::Rename(serial) => {
                let Some(name) = self.names.get(&serial).cloned() else {
                    return Task::none();
                };
                self.renaming = Some(serial.clone());
                return job(move |_| {
                    let r = devices::set_name(&serial, &name).map_err(|e| e.to_string());
                    DevMsg::Renamed(serial, name, r)
                });
            }
            DevMsg::Renamed(serial, name, r) => {
                self.renaming = None;
                match r {
                    Ok(()) => {
                        self.names.remove(&serial);
                        self.status = Some(if name.is_empty() {
                            "Name removed".into()
                        } else {
                            format!("Name set to “{name}”")
                        });
                    }
                    Err(e) => self.status = Some(format!("Rename failed: {e}")),
                }
            }
            DevMsg::ClockAsk(serial) => {
                let Some(u) = unit(&serial) else {
                    return Task::none();
                };
                let role = u.role.as_str();
                let mut body = String::new();
                if u.role == Role::Tx {
                    body.push_str("New recordings will then get correct dates.");
                    let old = live_offset.or(cfg.int("legacy_offset"));
                    if let (Some(c), Some(o)) = (u.clock, old) {
                        if (c as f64) < Y2020 {
                            body.push_str(&format!(
                                "\n\nThe current offset ({o:+} s) is saved, so the dates of existing recordings stay correct in this app."
                            ));
                        }
                    }
                } else {
                    body.push_str("The receiver's clock isn't used for recordings.");
                }
                self.dialog = Some(Dialog::Clock {
                    serial,
                    title: format!("Set the {role} clock to the current time?"),
                    body,
                });
            }
            DevMsg::ClockConfirm => {
                let Some(Dialog::Clock { serial, .. }) = self.dialog.take() else {
                    return Task::none();
                };
                if unit(&serial).is_some_and(|u| u.role == Role::Tx) {
                    // only the TX clock dates recordings: keep the old offset for the existing ones
                    if let Some(o) = live_offset.or(cfg.int("legacy_offset")) {
                        cfg.set("legacy_offset", o);
                    }
                }
                self.clocking = Some(serial.clone());
                return job(move |_| {
                    DevMsg::ClockDone(devices::set_clock_now(&serial).map_err(|e| e.to_string()))
                });
            }
            DevMsg::ClockDone(r) => {
                self.clocking = None;
                self.status = Some(match r {
                    Ok(d) if d.abs() <= 2 => "Clock set to the current time".into(),
                    Ok(d) => format!("Clock set; it reads {d:+} s off"),
                    Err(e) => format!("Could not set the clock: {e}"),
                });
            }
            DevMsg::FwCheck => return self.check_firmware(),
            DevMsg::FwChecked(r) => {
                self.checking = false;
                match r {
                    Ok(l) => {
                        self.latest = Some(l);
                        self.fw_note = None;
                        self.checked_at = Some(chrono::Utc::now().timestamp() as f64);
                    }
                    Err(e) => self.fw_note = Some(format!("Could not check for updates: {e}")),
                }
            }
            DevMsg::FwDownload => {
                let Some((key, latest)) = self.key() else {
                    return Task::none();
                };
                self.download = Some((0, 0));
                return job(move |send| {
                    let r = firmware::download(
                        &key,
                        &latest,
                        |a, b| send(DevMsg::FwProgress(a, b)),
                        None,
                    );
                    DevMsg::FwDownloaded(r.map_err(|e| e.to_string()))
                });
            }
            DevMsg::FwProgress(a, b) => {
                if self.download.is_some() {
                    self.download = Some((a, b));
                }
            }
            DevMsg::FwDownloaded(r) => {
                self.download = None;
                match r {
                    Ok(d) => self.status = Some(format!("Firmware downloaded to {}", d.display())),
                    Err(e) => {
                        self.dialog = Some(Dialog::Info {
                            title: "Firmware download failed".into(),
                            body: e,
                        })
                    }
                }
            }
            DevMsg::InstallAsk(serial) => {
                let Some(u) = unit(&serial) else {
                    return Task::none();
                };
                let (Some(dir), Some((_, unit_kind))) = (self.package(), firmware::product(u.pid))
                else {
                    return Task::none();
                };
                if let Some(b) = u.battery.filter(|b| *b < fwupdate::MIN_BATTERY) {
                    self.dialog = Some(Dialog::Info {
                        title: "Charge the battery first".into(),
                        body: format!(
                            "The {} battery is at {b} %. Charge it to at least {} % before updating.",
                            u.role.as_str(),
                            fwupdate::MIN_BATTERY
                        ),
                    });
                    return Task::none();
                }
                match Package::load(&dir, unit_kind) {
                    Ok(pkg) => {
                        let cur = u
                            .fw_version
                            .map(|v| firmware::vstr(&vtuple(v)))
                            .unwrap_or_else(|| u.fw.clone());
                        self.dialog = Some(Dialog::Install {
                            title: format!(
                                "Install firmware {} on {} (serial {}, now {cur})?",
                                pkg.version,
                                who(u),
                                u.serial
                            ),
                            plan: fwupdate::plan(&pkg),
                            serial,
                            unit: unit_kind.into(),
                            typed: String::new(),
                            show_plan: false,
                        });
                    }
                    Err(e) => {
                        self.dialog = Some(Dialog::Info {
                            title: "Firmware package problem".into(),
                            body: e.to_string(),
                        })
                    }
                }
            }
            DevMsg::InstallTyped(t) => {
                if let Some(Dialog::Install { typed, .. }) = &mut self.dialog {
                    *typed = t;
                }
            }
            DevMsg::InstallPlan(v) => {
                if let Some(Dialog::Install { show_plan, .. }) = &mut self.dialog {
                    *show_plan = v;
                }
            }
            DevMsg::InstallConfirm => {
                let Some(Dialog::Install {
                    serial,
                    unit,
                    typed,
                    ..
                }) = self.dialog.take()
                else {
                    return Task::none();
                };
                let Some(dir) = self.package() else {
                    return Task::none();
                };
                if typed.trim() != "INSTALL" {
                    return Task::none();
                }
                self.install = Some(Install {
                    serial: serial.clone(),
                    phase: "Starting".into(),
                    frac: 0.0,
                });
                return job(move |send| install(&serial, &unit, dir, send));
            }
            DevMsg::InstallProgress(phase, frac) => {
                if let Some(i) = &mut self.install {
                    i.phase = phase;
                    i.frac = frac;
                }
            }
            DevMsg::Installed(r, log) => {
                self.install = None;
                self.dialog = Some(match r {
                    Ok(v) => Dialog::Info {
                        title: "Update finished".into(),
                        body: format!("The unit reports version {v}.\n\nLog: {log}"),
                    },
                    Err(e) => Dialog::Info {
                        title: "The firmware update failed".into(),
                        body: format!(
                            "{e}\n\nLog: {log}\n\nLeave the unit connected and try again. The main firmware transfer \
                             resumes where it stopped."
                        ),
                    },
                });
            }
            DevMsg::DialogClose => self.dialog = None,
            DevMsg::Scan(serial) => {
                if self.scanning.is_some() || self.updating() {
                    return Task::none();
                }
                self.scanning = Some(serial.clone());
                // read-only: a GET of every id; ids the unit doesn't know are refused at once
                return job(move |_| {
                    let r = devices::scan_params(&serial, 0x60).map_err(|e| e.to_string());
                    DevMsg::Scanned(serial, r)
                });
            }
            DevMsg::Scanned(serial, r) => {
                self.scanning = None;
                match r {
                    Ok(now) => {
                        let prev = self.scans.remove(&serial).map(|(p, _)| p);
                        self.scans.insert(serial, (now, prev));
                    }
                    Err(e) => self.status = Some(format!("Scan failed: {e}")),
                }
            }
        }
        Task::none()
    }

    pub fn view<'a>(
        &'a self,
        u: &'a DeviceStatus,
        cfg: &Config,
        live_offset: Option<i64>,
    ) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();
        let serial = u.serial.clone();
        let ok = u.error.is_none() && !self.updating();
        let m = |d: DevMsg| Message::Dev(d);

        // name
        let current = u.name.clone().unwrap_or_default();
        let typed = self
            .names
            .get(&u.serial)
            .cloned()
            .unwrap_or_else(|| current.clone());
        let renaming = self.renaming.as_deref() == Some(u.serial.as_str());
        let s1 = serial.clone();
        let name = settings::section()
            .title("Name")
            .add(settings::item(
                "Device name",
                widget::row::with_capacity(2)
                    .spacing(space.space_xs)
                    .align_y(Alignment::Center)
                    .push(
                        widget::text_input("(no name)", typed.clone())
                            .on_input(move |t| Message::Dev(DevMsg::NameEdit(s1.clone(), t)))
                            .on_submit(move |_| Message::Dev(DevMsg::Rename(serial.clone())))
                            .width(Length::Fixed(220.0)),
                    )
                    .push(
                        widget::button::standard(if renaming { "Renaming…" } else { "Rename" })
                            .on_press_maybe(
                                (ok && !renaming && typed != current)
                                    .then(|| m(DevMsg::Rename(u.serial.clone()))),
                            ),
                    ),
            ))
            .add(widget::text::caption(format!(
                "Shown in RØDE apps and here. Up to {NAME_MAX} characters."
            )));

        // clock
        let clock_text = match (u.clock, u.clock_at) {
            (None, _) | (_, None) => match &u.error {
                Some(e) => format!("Could not read: {e}"),
                None => "—".into(),
            },
            (Some(c), Some(at)) if (c as f64) < Y2020 => {
                let mut t = format!(
                    "Not set — the device thinks it is {}.",
                    local(c as f64, "%Y-%m-%d %H:%M")
                );
                if u.role == Role::Tx {
                    let off = live_offset
                        .or(cfg.int("legacy_offset"))
                        .map(|o| o as f64)
                        .unwrap_or(at - c as f64);
                    t.push_str(&format!(
                        " Recording dates are corrected by {:+.0} days.",
                        off / 86400.0
                    ));
                }
                t
            }
            (Some(c), Some(at)) => {
                let drift = c as f64 - at;
                let state = if drift.abs() < 5.0 {
                    "in sync".to_string()
                } else {
                    format!("{drift:+.0} s off")
                };
                format!("{}  ({state})", local(c as f64, "%-d %B %Y, %H:%M:%S"))
            }
        };
        let clocking = self.clocking.as_deref() == Some(u.serial.as_str());
        let mut clock = settings::section().title("Clock").add(settings::item(
            "Device time",
            widget::button::standard(if clocking {
                "Setting…"
            } else {
                "Set clock to now"
            })
            .on_press_maybe((ok && !clocking).then(|| m(DevMsg::ClockAsk(u.serial.clone())))),
        ));
        clock = clock.add(widget::text::body(clock_text));
        if u.role == Role::Tx {
            clock = clock.add(widget::text::caption(
                "The transmitter stamps its recordings with this clock.",
            ));
        }

        // firmware
        let mut lines = vec![format!(
            "Installed: {}",
            u.fw_version
                .map(|v| firmware::vstr(&vtuple(v)))
                .unwrap_or_else(|| u.fw.clone())
        )];
        let latest = firmware::product(u.pid).and_then(|(k, _)| self.latest.as_ref()?.get(k));
        // the result of the last check, as a sentence that stays until the next check
        let mut result = None;
        if let (Some(l), Some(cur)) = (latest, u.fw_version) {
            let new = firmware::vstr(&l.version);
            let when = self
                .checked_at
                .map(|t| format!("Checked at {}: ", local(t, "%H:%M")))
                .unwrap_or_default();
            result = Some(if l.version > vtuple(cur) {
                format!(
                    "{when}firmware {new} is available for this {}.",
                    u.role.as_str()
                )
            } else {
                format!(
                    "{when}this {} has the latest firmware ({new}).",
                    u.role.as_str()
                )
            });
        }
        if let Some(n) = &self.fw_note {
            lines.push(n.clone());
        }
        let have = self.package().is_some();
        if have {
            lines.push("Package downloaded and checksum-verified.".into());
        }
        let newer = self.newer(u).is_some();
        let mut buttons = widget::row::with_capacity(3).spacing(space.space_xs).push(
            widget::button::standard(if self.checking {
                "Checking…"
            } else {
                "Check for updates"
            })
            .on_press_maybe((!self.checking && !self.updating()).then_some(m(DevMsg::FwCheck))),
        );
        if self.key().is_some() && !have {
            buttons = buttons.push(
                widget::button::standard(if self.download.is_some() {
                    "Downloading…"
                } else {
                    "Download"
                })
                .on_press_maybe(self.download.is_none().then_some(m(DevMsg::FwDownload))),
            );
        }
        if newer {
            buttons = buttons.push(widget::button::suggested("Install update…").on_press_maybe(
                (have && ok && !u.storage).then(|| m(DevMsg::InstallAsk(u.serial.clone()))),
            ));
            if u.storage {
                lines.push("Unplug the transmitter and plug it in again (leaving USB storage mode) to update it.".into());
            }
        }
        let mut fw = settings::section()
            .title("Firmware")
            .add(widget::text::body(lines.join("\n")));
        if let Some(r) = result.filter(|_| !self.checking) {
            fw = fw.add(widget::text::heading(r));
        }
        fw = fw.add(buttons);
        if let Some((a, b)) = self.download {
            let frac = if b > 0 { a as f32 / b as f32 } else { 0.0 };
            fw = fw.add(widget::progress_bar::determinate_linear(frac));
        }
        if let Some(i) = self.install.as_ref().filter(|i| i.serial == u.serial) {
            let (name, total) = match i.phase.as_str() {
                "radio" => ("Radio chip", i.frac * 0.15),
                "main" => ("Main firmware", 0.15 + i.frac * 0.85),
                p => (p, 0.0),
            };
            fw = fw
                .add(widget::text::body(format!(
                    "Updating: {name} — {:.0} %. Keep the unit connected, don't touch it and don't close MicDock.",
                    total * 100.0
                )))
                .add(widget::progress_bar::determinate_linear(total as f32));
        }

        // about
        let about = settings::section()
            .title("About this device")
            .add(settings::item(
                "Product",
                widget::text::body(u.product.clone()),
            ))
            .add(settings::item(
                "Serial number",
                widget::text::body(u.serial.clone()),
            ))
            .add(settings::item(
                "Connected at",
                widget::text::body(u.node.clone()),
            ));

        // USB storage mode (transmitters only: the receiver has no recordings and no other mode)
        let usb_mode: Option<Element<'a, Message>> = (u.role == Role::Tx).then(|| {
            let (state, note) = if u.storage {
                (
                    "On: the recordings drive is available, wireless is off",
                    "To return to normal, switch the transmitter off and on (hold its power button) or unplug it. \
                     MicDock can't switch it back: the only command for that erased recordings in a test.",
                )
            } else {
                (
                    "Off: normal use, transmitting to the receiver",
                    "Switch to USB storage mode to copy the recordings (the Recordings tab does this too). The \
                     transmitter stops transmitting until you switch it off and on or unplug it.",
                )
            };
            let mut sec = settings::section()
                .title("USB storage mode")
                .add(settings::item("State", widget::text::body(state)));
            if !u.storage {
                sec = sec.add(settings::item(
                    "Switch",
                    widget::button::standard("Switch to USB storage…").on_press_maybe(
                        ok.then(|| Message::Rec(crate::recordings::RecMsg::Connect(u.serial.clone()))),
                    ),
                ));
            }
            sec.add(widget::text::caption(note)).into()
        });

        // parameter scanner (read-only)
        let scanning = self.scanning.as_deref() == Some(u.serial.as_str());
        let mut scanner = settings::section()
            .title("Parameter scanner")
            .add(settings::item(
                "Read every setting",
                widget::button::standard(if scanning { "Scanning…" } else { "Scan" })
                    .on_press_maybe(
                        (ok && self.scanning.is_none()).then(|| m(DevMsg::Scan(u.serial.clone()))),
                    ),
            ));
        scanner = scanner.add(widget::text::caption(
            "For the curious: reads every setting ID (read-only). Change one thing on the unit, scan again, and \
             the values that changed are marked with ●.",
        ));
        if let Some((now, prev)) = self.scans.get(&u.serial) {
            let hex = |v: &[u8]| {
                v.iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let mut ids: Vec<u8> = now.keys().copied().collect();
            if let Some(p) = prev {
                ids.extend(p.keys().copied().filter(|k| !now.contains_key(k)));
            }
            ids.sort_unstable();
            let mut rows = widget::column::with_capacity(ids.len());
            for id in ids {
                let a = now.get(&id).map(|v| hex(v)).unwrap_or_else(|| "—".into());
                let line = match prev {
                    Some(p) => {
                        let b = p.get(&id).map(|v| hex(v)).unwrap_or_else(|| "—".into());
                        if a != b {
                            format!("● 0x{id:02x}  {a}   (was {b})")
                        } else {
                            format!("  0x{id:02x}  {a}")
                        }
                    }
                    None => format!("  0x{id:02x}  {a}"),
                };
                rows = rows.push(widget::text::monotext(line));
            }
            scanner = scanner.add(rows);
        }

        let mut col = widget::column::with_capacity(6)
            .spacing(space.space_m)
            .push(name)
            .push(clock)
            .push(fw)
            .push(about)
            .push_maybe(usb_mode)
            .push(scanner);
        if let Some(s) = &self.status {
            col = col.push(widget::text::caption(s.clone()));
        }
        col.into()
    }

    pub fn dialog_view(&self) -> Option<Element<'_, Message>> {
        let close =
            || widget::button::standard("Cancel").on_press(Message::Dev(DevMsg::DialogClose));
        Some(match self.dialog.as_ref()? {
            Dialog::Clock { title, body, .. } => widget::dialog()
                .title(title.clone())
                .body(body.clone())
                .primary_action(
                    widget::button::suggested("Set clock")
                        .on_press(Message::Dev(DevMsg::ClockConfirm)),
                )
                .secondary_action(close())
                .into(),
            Dialog::Info { title, body } => widget::dialog()
                .title(title.clone())
                .body(body.clone())
                .primary_action(
                    widget::button::suggested("OK").on_press(Message::Dev(DevMsg::DialogClose)),
                )
                .into(),
            Dialog::Install {
                title,
                plan,
                typed,
                show_plan,
                ..
            } => {
                let mut control = widget::column::with_capacity(3)
                    .spacing(cosmic::theme::spacing().space_s)
                    .push(
                        widget::text_input("INSTALL", typed.as_str())
                            .on_input(|t| Message::Dev(DevMsg::InstallTyped(t))),
                    )
                    .push(
                        widget::checkbox(*show_plan)
                            .label("Show every step")
                            .on_toggle(|v| Message::Dev(DevMsg::InstallPlan(v))),
                    );
                if *show_plan {
                    control = control.push(
                        widget::scrollable(widget::text::monotext(plan.clone()))
                            .height(Length::Fixed(220.0))
                            .width(Length::Fill),
                    );
                }
                widget::dialog()
                    .title(title.clone())
                    .body(
                        "This installer was reverse-engineered from RØDE's own software. It has updated two \
                         transmitters and a receiver successfully, but it is not an official tool.\n\nKeep the unit \
                         connected by USB, don't touch it, and don't close MicDock until it says it is done. Every \
                         packet is logged, so a failure can be analysed.\n\nTo start, type INSTALL:",
                    )
                    .control(control)
                    .primary_action(
                        widget::button::destructive("Install")
                            .on_press_maybe((typed.trim() == "INSTALL").then_some(Message::Dev(DevMsg::InstallConfirm))),
                    )
                    .secondary_action(close())
                    .into()
            }
        })
    }
}

/// The installation itself (on a worker thread): same steps and log as `micdock-firmware install`.
fn install(serial: &str, unit: &str, dir: PathBuf, send: &dyn Fn(DevMsg)) -> DevMsg {
    let pkg = match Package::load(&dir, unit) {
        Ok(p) => p,
        Err(e) => return DevMsg::Installed(Err(e.to_string()), "—".into()),
    };
    let (path, log) = match fwupdate::open_log(serial) {
        Ok(x) => x,
        Err(e) => return DevMsg::Installed(Err(format!("cannot open the log: {e}")), "—".into()),
    };
    let shown = path.display().to_string();
    // the transport and this function both write to the same log
    let shared = Rc::new(RefCell::new(log));
    let a = shared.clone();
    let log_line = |s: &str| (shared.borrow_mut())(s);
    log_line(&format!(
        "app install {unit} {} serial {serial}",
        pkg.version
    ));
    let mut link =
        fwupdate::HidTransport::new(serial, unit, Box::new(move |s: &str| (a.borrow_mut())(s)));
    let result = {
        let mut up = Updater::new(&mut link, &pkg);
        up.progress =
            Box::new(|phase, frac| send(DevMsg::InstallProgress(phase.to_string(), frac)));
        up.run(false)
    };
    match result {
        Ok((pid, ver)) => {
            let v = ver
                .map(|(a, b, c)| firmware::vstr(&[a, b, c]))
                .unwrap_or_else(|| "?".into());
            log_line(&format!(
                "done: PID {} version {v}",
                pid.map(|p| format!("{p:04x}")).unwrap_or("None".into())
            ));
            DevMsg::Installed(Ok(v), shown)
        }
        Err(e) => {
            log_line(&format!("FAILED: {e:?}"));
            DevMsg::Installed(Err(e.to_string()), shown)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_the_device_field() {
        assert_eq!(fit_name("Presenter"), "Presenter");
        assert_eq!(
            fit_name("A very long transmitter name"),
            "A very long transmitt"
        ); // 21 bytes
           // never cut inside a character: "ø" is 2 bytes
        let n = fit_name("øøøøøøøøøøøø");
        assert!(n.len() <= NAME_MAX && n.chars().all(|c| c == 'ø'));
        assert_eq!(n.chars().count(), 10);
    }
}
