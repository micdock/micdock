// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! The Recordings page: connect a transmitter's drive, list its recordings (one list per transmitter),
//! play, export to WAV/MP3, eject, delete all.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cosmic::app::Task;
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget::{self, settings};
use micdock_core::devices::{self, ConnectPhase, DeviceStatus, ErasePhase};
use micdock_core::recordings::{self as rec, Encoding, Progress, Recording};

use crate::config::Config;
use crate::player::Player;
use crate::util::{fmt_dur, fmt_size, local, open_external, Y2020};
use crate::{job, Message};

/// One transmitter's listed recordings.
pub struct RecList {
    pub folder: PathBuf,
    pub sessions: Vec<Vec<Recording>>,
    /// Checked sessions, by the name of their first file.
    pub checked: HashSet<String>,
}

/// One output file of an export: (pieces, destination, real start, real end).
type OutFile = (Vec<Recording>, PathBuf, Option<f64>, Option<f64>);

/// A running export (one at a time).
pub struct ExportJob {
    pub done: f32,
    pub label: String,
    pub cancel: Arc<AtomicBool>,
}

pub enum Dialog {
    /// The window is being closed while an export runs.
    Quit,
    /// Confirm switching a transmitter to USB storage mode (it stops transmitting until unplugged).
    Connect {
        serial: String,
        name: String,
        dont_ask: bool,
    },
    /// Delete every recording on a transmitter: type DELETE to confirm.
    Erase {
        serial: String,
        name: String,
        what: String,
        typed: String,
    },
}

#[derive(Default)]
pub struct Recordings {
    lists: HashMap<String, RecList>,
    connecting: HashSet<String>,
    loading: HashSet<String>,
    erasing: HashSet<String>,
    /// Drives the user ejected: not mounted again automatically.
    ejected: HashSet<String>,
    /// Drives that could not be mounted automatically: not retried.
    auto_failed: HashSet<String>,
    phase: HashMap<String, String>,
    pub export: Option<ExportJob>,
    pub dialog: Option<Dialog>,
    pub status: Option<String>,
    /// TX clock offset read live from a connected TX (for recordings dated before 2020).
    pub live_offset: Option<i64>,
    /// The built-in player: (serial, session key, player).
    pub player: Option<(String, String, Player)>,
    /// Slider position (frames) while the user drags it.
    drag: Option<f64>,
    /// A session being converted for playback: (serial, key).
    preparing: Option<(String, String)>,
}

#[derive(Clone, Debug)]
pub enum RecMsg {
    Connect(String),
    ConnectConfirmed,
    DontAsk(bool),
    DialogCancel,
    Phase(String, String),
    Connected(String, bool, Result<String, String>),
    Loaded(
        String,
        PathBuf,
        Result<(Vec<Recording>, Vec<String>), String>,
    ),
    Eject(String),
    Ejected(String, Result<(), String>),
    Toggle(String, String, bool),
    All(String, bool),
    Play(String, String),
    /// A converted preview is ready (serial, key, WAV path) or failed.
    Prepared(String, String, Result<PathBuf, String>),
    PlayPause,
    Stop,
    SeekDrag(f64),
    SeekRelease,
    /// Time to redraw the player's position.
    Tick,
    /// A worker failed unexpectedly.
    Failed(String),
    OpenFolder,
    ExportDir(String),
    Format(usize),
    Preset(usize),
    Join(bool),
    Export(String),
    Progress(f32, String),
    Exported(Result<(usize, usize, bool, String), String>),
    Cancel,
    EraseAsk(String),
    EraseTyped(String),
    EraseConfirm,
    EraseDone(String, Result<(), String>),
}

pub const FORMATS: [&str; 2] = ["WAV", "MP3"];
pub const MP3_PRESETS: [&str; 6] = [
    "VBR high (V0, ~245 kbps)",
    "VBR good (V2, ~190 kbps)",
    "VBR small (V5, ~130 kbps)",
    "CBR 320 kbps",
    "CBR 192 kbps",
    "CBR 128 kbps",
];

fn mp3_preset_args(i: usize) -> Vec<String> {
    match i {
        0 => rec::mp3_args(0, None),
        2 => rec::mp3_args(5, None),
        3 => rec::mp3_args(2, Some("320k")),
        4 => rec::mp3_args(2, Some("192k")),
        5 => rec::mp3_args(2, Some("128k")),
        _ => rec::mp3_args(2, None),
    }
}

fn stem(name: &str) -> &str {
    name.rfind('.')
        .filter(|&i| i > 0)
        .map(|i| &name[..i])
        .unwrap_or(name)
}

/// "REC00037 – 00042"
fn session_name(s: &[Recording]) -> String {
    let first = stem(&s[0].name);
    if s.len() == 1 {
        first.to_string()
    } else {
        format!(
            "{first} – {}",
            stem(&s[s.len() - 1].name).get(3..).unwrap_or("")
        )
    }
}

fn load_folder(folder: &Path) -> Result<(Vec<Recording>, Vec<String>), String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .map_err(|e| format!("Cannot read {}: {e}", folder.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| rec::is_recording(n))
        .collect();
    names.sort();
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for n in names {
        match rec::open_recording(folder.join(&n)) {
            Ok(r) => ok.push(r),
            Err(e) => bad.push(format!("{n}: {e}")),
        }
    }
    Ok((ok, bad))
}

impl Recordings {
    /// Real (start, end) of back-to-back recordings, or None when the TX clock offset is unknown.
    pub fn times(&self, infos: &[Recording], cfg: &Config) -> Option<(f64, f64)> {
        let offset = |r: &Recording| -> Option<f64> {
            if r.mtime >= Y2020 {
                Some(0.0)
            } else {
                self.live_offset
                    .or(cfg.int("legacy_offset"))
                    .map(|o| o as f64)
            }
        };
        let (first, last) = (&infos[0], &infos[infos.len() - 1]);
        Some((
            first.mtime + offset(first)? - first.seconds(),
            last.mtime + offset(last)?,
        ))
    }

    /// Called after every device poll: pick up drives that appeared, forget drives that went away.
    pub fn after_poll(&mut self, units: &[DeviceStatus], cfg: &mut Config) -> Task<Message> {
        // The TX clock offset, for recordings made while its clock was unset (dated in 2000). The last one
        // seen is kept in the config, so their dates stay right after the clock is set or the TX unplugged.
        if let Some(c) = units
            .iter()
            .find(|u| u.role == devices::Role::Tx && u.clock.is_some())
        {
            let (clock, at) = (
                c.clock.unwrap_or_default() as f64,
                c.clock_at.unwrap_or_default(),
            );
            let off = (clock < Y2020).then(|| (at - clock).round() as i64);
            self.live_offset = off;
            if let Some(o) = off {
                if cfg
                    .int("legacy_offset")
                    .is_none_or(|old| (o - old).abs() > 30)
                {
                    cfg.set("legacy_offset", o);
                }
            }
        }
        let mounts = devices::tx_mounts();
        let mut tasks = Vec::new();
        for u in units.iter().filter(|u| u.role == devices::Role::Tx) {
            let s = &u.serial;
            let busy =
                self.connecting.contains(s) || self.loading.contains(s) || self.erasing.contains(s);
            let want = mounts.contains_key(s)
                || (u.storage && !self.ejected.contains(s) && !self.auto_failed.contains(s));
            if want && !busy && !self.lists.contains_key(s) {
                tasks.push(self.start_connect(s.clone(), true));
            }
        }
        let gone: Vec<String> = self
            .lists
            .iter()
            .filter(|(s, l)| {
                !mounts.contains_key(*s) && !l.folder.is_dir() && !self.connecting.contains(*s)
            })
            .map(|(s, _)| s.clone())
            .collect();
        for s in gone {
            self.stop_player(&s);
            self.lists.remove(&s);
        }
        Task::batch(tasks)
    }

    fn start_connect(&mut self, serial: String, auto: bool) -> Task<Message> {
        if self.connecting.contains(&serial) || self.loading.contains(&serial) {
            return Task::none();
        }
        if !auto {
            self.ejected.remove(&serial);
            self.auto_failed.remove(&serial);
        }
        self.connecting.insert(serial.clone());
        self.phase
            .insert(serial.clone(), "Connecting to the transmitter…".into());
        job(move |send| {
            let s = serial.clone();
            let r = devices::connect_tx_storage(Some(&serial), |p| {
                let text = match p {
                    ConnectPhase::Switching => {
                        "Switching the transmitter to USB storage mode. This takes a few seconds…"
                    }
                    ConnectPhase::Waiting => "Waiting for the transmitter's drive…",
                    ConnectPhase::Mounting => "Mounting the transmitter's drive (read-only)…",
                };
                send(RecMsg::Phase(s.clone(), text.into()));
            });
            RecMsg::Connected(
                serial,
                auto,
                r.map(|(_, mount)| mount).map_err(|e| e.to_string()),
            )
        })
    }

    fn start_load(&mut self, serial: String, folder: PathBuf) -> Task<Message> {
        self.loading.insert(serial.clone());
        self.phase
            .insert(serial.clone(), "Reading the recordings…".into());
        job(move |_| {
            let r = load_folder(&folder);
            RecMsg::Loaded(serial, folder, r)
        })
    }

    pub fn update(
        &mut self,
        msg: RecMsg,
        units: &[DeviceStatus],
        cfg: &mut Config,
    ) -> Task<Message> {
        let name_of = |serial: &str| {
            units
                .iter()
                .find(|u| u.serial == serial)
                .map(|u| {
                    u.name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "the transmitter".into())
                })
                .unwrap_or_else(|| "the transmitter".into())
        };
        match msg {
            RecMsg::Connect(serial) => {
                let storage = units.iter().any(|u| u.serial == serial && u.storage);
                if !storage && cfg.bool("warn_storage") {
                    self.dialog = Some(Dialog::Connect {
                        name: name_of(&serial),
                        serial,
                        dont_ask: false,
                    });
                    return Task::none();
                }
                return self.start_connect(serial, false);
            }
            RecMsg::DontAsk(v) => {
                if let Some(Dialog::Connect { dont_ask, .. }) = &mut self.dialog {
                    *dont_ask = v;
                }
            }
            RecMsg::ConnectConfirmed => {
                if let Some(Dialog::Connect {
                    serial, dont_ask, ..
                }) = self.dialog.take()
                {
                    if dont_ask {
                        cfg.set("warn_storage", false);
                    }
                    return self.start_connect(serial, false);
                }
            }
            RecMsg::DialogCancel => self.dialog = None,
            RecMsg::Phase(serial, text) => {
                self.phase.insert(serial, text);
            }
            RecMsg::Connected(serial, auto, r) => {
                self.connecting.remove(&serial);
                match r {
                    Ok(mount) => return self.start_load(serial, PathBuf::from(mount)),
                    Err(e) => {
                        self.phase.remove(&serial);
                        if auto {
                            self.auto_failed.insert(serial);
                        }
                        self.status =
                            Some(format!("Could not connect the transmitter's drive: {e}"));
                    }
                }
            }
            RecMsg::Loaded(serial, folder, r) => {
                self.loading.remove(&serial);
                self.phase.remove(&serial);
                match r {
                    Ok((infos, bad)) => {
                        let sessions = rec::group_sessions(infos);
                        self.lists.insert(
                            serial,
                            RecList {
                                folder,
                                sessions,
                                checked: HashSet::new(),
                            },
                        );
                        if !bad.is_empty() {
                            self.status =
                                Some(format!("Some files could not be read: {}", bad.join("; ")));
                        }
                    }
                    Err(e) => self.status = Some(e),
                }
            }
            RecMsg::Eject(serial) => {
                self.stop_player(&serial); // an open file would keep the drive busy
                return job(move |_| {
                    let r = devices::eject_tx_storage(Some(&serial)).map_err(|e| e.to_string());
                    RecMsg::Ejected(serial, r)
                });
            }
            RecMsg::Ejected(serial, r) => match r {
                Ok(()) => {
                    self.lists.remove(&serial);
                    self.ejected.insert(serial);
                    self.status = Some(
                        "Drive disconnected. The transmitter stays in USB storage mode (wireless off) until you unplug it or switch it off and on.".into(),
                    );
                }
                Err(e) => self.status = Some(format!("Could not eject: {e}")),
            },
            RecMsg::Toggle(serial, key, on) => {
                if let Some(l) = self.lists.get_mut(&serial) {
                    if on {
                        l.checked.insert(key);
                    } else {
                        l.checked.remove(&key);
                    }
                }
            }
            RecMsg::All(serial, on) => {
                if let Some(l) = self.lists.get_mut(&serial) {
                    l.checked = if on {
                        l.sessions.iter().map(|s| s[0].name.clone()).collect()
                    } else {
                        HashSet::new()
                    };
                }
            }
            RecMsg::Play(serial, key) => {
                if let Some((ps, pk, p)) = &self.player {
                    if *ps == serial && *pk == key {
                        p.set_paused(!p.paused());
                        return Task::none();
                    }
                }
                let Some(session) = self.session(&serial, &key) else {
                    return Task::none();
                };
                self.player = None;
                if session.iter().all(|r| r.kind() == "WAV") {
                    // WAV recordings play straight from the drive
                    self.start_player(serial, key, &session);
                    return Task::none();
                }
                // older UGG recordings: convert to a WAV in the cache first
                let dst = micdock_core::paths::cache_dir()
                    .join(format!("{}.wav", stem(&session[0].name)));
                self.preparing = Some((serial.clone(), key.clone()));
                return job(move |_| {
                    if let Ok(rd) = std::fs::read_dir(micdock_core::paths::cache_dir()) {
                        for e in rd.flatten() {
                            if e.path().extension().is_some_and(|x| x == "wav") && e.path() != dst {
                                let _ = std::fs::remove_file(e.path()); // keep only one preview around
                            }
                        }
                    }
                    let _ = std::fs::create_dir_all(micdock_core::paths::cache_dir());
                    let r = if dst.exists() {
                        Ok(0)
                    } else {
                        rec::export(
                            &session,
                            &dst,
                            &Encoding::Wav,
                            None,
                            None,
                            Progress::default(),
                        )
                    };
                    RecMsg::Prepared(serial, key, r.map(|_| dst).map_err(|e| e.to_string()))
                });
            }
            RecMsg::Prepared(serial, key, r) => {
                if self.preparing.as_ref() != Some(&(serial.clone(), key.clone())) {
                    return Task::none(); // another recording was chosen meanwhile
                }
                self.preparing = None;
                match r.and_then(|p| rec::open_recording(p).map_err(|e| e.to_string())) {
                    Ok(wav) => self.start_player(serial, key, &[wav]),
                    Err(e) => self.status = Some(format!("Could not play: {e}")),
                }
            }
            RecMsg::PlayPause => {
                if let Some((_, _, p)) = &self.player {
                    p.set_paused(!p.paused());
                }
            }
            RecMsg::Stop => self.player = None,
            RecMsg::SeekDrag(v) => self.drag = Some(v),
            RecMsg::SeekRelease => {
                if let (Some(v), Some((_, _, p))) = (self.drag.take(), &self.player) {
                    p.seek(v as u64);
                }
            }
            RecMsg::Tick => {
                if let Some(e) = self.player.as_ref().and_then(|(_, _, p)| p.error()) {
                    self.player = None;
                    self.status = Some(format!("Playback stopped: {e}"));
                }
            }
            RecMsg::Failed(e) => self.status = Some(e),
            RecMsg::OpenFolder => {
                let dir = cfg.str("export_dir");
                let _ = std::fs::create_dir_all(&dir);
                open_external(&dir);
            }
            RecMsg::ExportDir(d) => cfg.set("export_dir", d),
            RecMsg::Format(i) => cfg.set("format", if i == 1 { "mp3" } else { "wav" }),
            RecMsg::Preset(i) => cfg.set("mp3_preset", i as i64),
            RecMsg::Join(v) => cfg.set("join", v),
            RecMsg::Export(serial) => return self.start_export(&serial, cfg),
            RecMsg::Progress(done, label) => {
                if let Some(j) = &mut self.export {
                    j.done = done;
                    j.label = label;
                }
            }
            RecMsg::Cancel => {
                if let Some(j) = &self.export {
                    j.cancel.store(true, Ordering::Relaxed);
                }
            }
            RecMsg::Exported(r) => {
                self.export = None;
                self.status = Some(match r {
                    Ok((written, skipped, cancelled, folder)) => {
                        let mut s = format!(
                            "Exported {written} file{} to {folder}.",
                            if written == 1 { "" } else { "s" }
                        );
                        if skipped > 0 {
                            s.push_str(&format!(" Skipped {skipped} that already existed."));
                        }
                        if cancelled {
                            s = format!("Export cancelled. {s}");
                        }
                        s
                    }
                    Err(e) => format!("Export failed: {e}"),
                });
            }
            RecMsg::EraseAsk(serial) => {
                let what = match self.lists.get(&serial) {
                    Some(l) => {
                        let secs: f64 = l.sessions.iter().flatten().map(|r| r.seconds()).sum();
                        let n = l.sessions.len();
                        format!(
                            "{n} recording{} ({})",
                            if n == 1 { "" } else { "s" },
                            fmt_dur(secs)
                        )
                    }
                    None => "every recording".into(),
                };
                self.dialog = Some(Dialog::Erase {
                    name: name_of(&serial),
                    serial,
                    what,
                    typed: String::new(),
                });
            }
            RecMsg::EraseTyped(t) => {
                if let Some(Dialog::Erase { typed, .. }) = &mut self.dialog {
                    *typed = t;
                }
            }
            RecMsg::EraseConfirm => {
                let Some(Dialog::Erase { serial, typed, .. }) = self.dialog.take() else {
                    return Task::none();
                };
                if typed.trim() != "DELETE" {
                    self.status = Some("Nothing was deleted.".into());
                    return Task::none();
                }
                self.stop_player(&serial);
                self.erasing.insert(serial.clone());
                return job(move |send| {
                    let s = serial.clone();
                    let r = devices::erase_tx_recordings(&serial, |p| {
                        let text = match p {
                            ErasePhase::Unmounting => "Unmounting the transmitter's drive…",
                            ErasePhase::Erasing => "Deleting all recordings on the transmitter…",
                            ErasePhase::Restarting => "Waiting for the transmitter to restart…",
                        };
                        send(RecMsg::Phase(s.clone(), text.into()));
                    });
                    RecMsg::EraseDone(serial, r.map(|_| ()).map_err(|e| e.to_string()))
                });
            }
            RecMsg::EraseDone(serial, r) => {
                self.erasing.remove(&serial);
                self.phase.remove(&serial);
                self.lists.remove(&serial);
                self.status = Some(match r {
                    Ok(()) => format!("All recordings on {} were deleted.", name_of(&serial)),
                    Err(e) => format!("Could not delete the recordings: {e}"),
                });
            }
        }
        Task::none()
    }

    fn start_player(&mut self, serial: String, key: String, infos: &[Recording]) {
        self.drag = None;
        match Player::start(infos) {
            Ok(p) => self.player = Some((serial, key, p)),
            Err(e) => self.status = Some(format!("Could not play: {e}")),
        }
    }

    fn stop_player(&mut self, serial: &str) {
        if self.player.as_ref().is_some_and(|(s, _, _)| s == serial) {
            self.player = None;
        }
        if self.preparing.as_ref().is_some_and(|(s, _)| s == serial) {
            self.preparing = None;
        }
    }

    /// Whether the position needs redrawing regularly.
    pub fn playing(&self) -> bool {
        self.player.as_ref().is_some_and(|(_, _, p)| !p.paused())
    }

    /// The player bar for this transmitter's recording being played.
    fn player_bar(&self, serial: &str, list: &RecList) -> Option<Element<'_, Message>> {
        let space = cosmic::theme::spacing();
        if let Some((s, key)) = &self.preparing {
            if s == serial {
                let name = list
                    .sessions
                    .iter()
                    .find(|x| x[0].name == *key)
                    .map(|x| session_name(x))
                    .unwrap_or_default();
                return Some(
                    widget::row::with_capacity(2)
                        .spacing(space.space_s)
                        .align_y(Alignment::Center)
                        .push(widget::progress_bar::indeterminate_circular().size(20.0))
                        .push(widget::text::body(format!(
                            "Preparing {name} for playback…"
                        )))
                        .into(),
                );
            }
        }
        let (s, key, p) = self.player.as_ref()?;
        if s != serial {
            return None;
        }
        let name = list
            .sessions
            .iter()
            .find(|x| x[0].name == *key)
            .map(|x| session_name(x))
            .unwrap_or_default();
        let pos = self.drag.unwrap_or(p.position() as f64);
        let icon = if p.paused() {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        };
        let mut marks = String::new();
        if !p.markers.is_empty() {
            let n = p.markers.len();
            marks = format!(
                "  ·  {n} marker{} at {}",
                if n == 1 { "" } else { "s" },
                p.markers
                    .iter()
                    .map(|&m| fmt_dur(p.seconds(m)))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let controls = widget::row::with_capacity(4)
            .spacing(space.space_s)
            .align_y(Alignment::Center)
            .push(
                widget::button::icon(widget::icon::from_name(icon))
                    .tooltip(if p.paused() { "Play" } else { "Pause" })
                    .on_press(Message::Rec(RecMsg::PlayPause)),
            )
            .push(
                widget::slider(0.0..=p.total.max(1) as f64, pos, |v| {
                    Message::Rec(RecMsg::SeekDrag(v))
                })
                .on_release(Message::Rec(RecMsg::SeekRelease))
                .width(Length::Fill),
            )
            .push(widget::text::body(format!(
                "{} / {}",
                fmt_dur(p.seconds(pos as u64)),
                fmt_dur(p.seconds(p.total))
            )))
            .push(
                widget::button::icon(widget::icon::from_name("media-playback-stop-symbolic"))
                    .tooltip("Stop")
                    .on_press(Message::Rec(RecMsg::Stop)),
            );
        Some(
            widget::column::with_capacity(2)
                .spacing(space.space_xxs)
                .push(widget::text::caption(format!("Playing {name}{marks}")))
                .push(controls)
                .into(),
        )
    }

    /// Demo mode: show these recordings for this transmitter (the first one ticked).
    pub fn demo(&mut self, serial: &str, folder: PathBuf, sessions: Vec<Vec<Recording>>) {
        let checked = sessions
            .first()
            .map(|s| s[0].name.clone())
            .into_iter()
            .collect();
        self.lists.insert(
            serial.to_string(),
            RecList {
                folder,
                sessions,
                checked,
            },
        );
    }

    fn session(&self, serial: &str, key: &str) -> Option<Vec<Recording>> {
        self.lists
            .get(serial)?
            .sessions
            .iter()
            .find(|s| s[0].name == key)
            .cloned()
    }

    fn start_export(&mut self, serial: &str, cfg: &Config) -> Task<Message> {
        let Some(list) = self.lists.get(serial) else {
            return Task::none();
        };
        let join = cfg.bool("join");
        let mp3 = cfg.str("format") == "mp3";
        let (encoding, ext) = if mp3 {
            (
                Encoding::Mp3(mp3_preset_args(cfg.int("mp3_preset").unwrap_or(1) as usize)),
                "mp3",
            )
        } else {
            (Encoding::Wav, "wav")
        };
        let folder = PathBuf::from(cfg.str("export_dir"));
        let mut jobs: Vec<OutFile> = Vec::new();
        for s in list
            .sessions
            .iter()
            .filter(|s| list.checked.contains(&s[0].name))
        {
            let groups: Vec<Vec<Recording>> = if join {
                vec![s.clone()]
            } else {
                s.iter().map(|r| vec![r.clone()]).collect()
            };
            for g in groups {
                let mut recs = stem(&g[0].name).to_string();
                if g.len() > 1 {
                    recs = format!("{recs}-{}", stem(&g[g.len() - 1].name));
                }
                let times = self.times(&g, cfg);
                let name = match times {
                    Some((start, _)) => format!("{} {recs}.{ext}", local(start, "%Y-%m-%d %H.%M")),
                    None => format!("{recs}.{ext}"),
                };
                jobs.push((g, folder.join(name), times.map(|t| t.0), times.map(|t| t.1)));
            }
        }
        if jobs.is_empty() {
            return Task::none();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.export = Some(ExportJob {
            done: 0.0,
            label: "Exporting…".into(),
            cancel: cancel.clone(),
        });
        job(move |send| {
            if let Err(e) = std::fs::create_dir_all(&folder) {
                return RecMsg::Exported(Err(format!("Cannot create {}: {e}", folder.display())));
            }
            let total: u64 = jobs
                .iter()
                .flat_map(|j| j.0.iter())
                .map(|r| r.frames)
                .sum::<u64>()
                .max(1);
            let (mut before, mut written, mut skipped) = (0u64, 0usize, 0usize);
            let n = jobs.len();
            for (k, (g, dst, start, end)) in jobs.into_iter().enumerate() {
                let frames: u64 = g.iter().map(|r| r.frames).sum();
                if dst.exists() {
                    skipped += 1;
                    before += frames;
                    continue;
                }
                let label = format!(
                    "Exporting {}/{n}: {}",
                    k + 1,
                    dst.file_name().unwrap_or_default().to_string_lossy()
                );
                let mut report = |done: u64, all: u64| {
                    let part = done as f64 / all.max(1) as f64 * frames as f64;
                    send(RecMsg::Progress(
                        ((before as f64 + part) / total as f64) as f32,
                        label.clone(),
                    ));
                };
                let progress = Progress {
                    report: Some(&mut report),
                    cancel: Some(&cancel),
                };
                match rec::export(&g, &dst, &encoding, start, end, progress) {
                    Ok(_) => written += 1,
                    Err(e) if e.to_string() == rec::CANCELLED => {
                        return RecMsg::Exported(Ok((
                            written,
                            skipped,
                            true,
                            folder.display().to_string(),
                        )))
                    }
                    Err(e) => return RecMsg::Exported(Err(format!("{}: {e}", dst.display()))),
                }
                before += frames;
            }
            RecMsg::Exported(Ok((written, skipped, false, folder.display().to_string())))
        })
    }

    pub fn view<'a>(&'a self, unit: &'a DeviceStatus, cfg: &Config) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();
        let serial = unit.serial.clone();
        let list = self.lists.get(&unit.serial);
        let working = self.connecting.contains(&unit.serial) || self.loading.contains(&unit.serial);
        let erasing = self.erasing.contains(&unit.serial);
        let msg = |m: RecMsg| Message::Rec(m);

        // top: where the recordings come from, and the drive actions
        let source = match (self.phase.get(&unit.serial), list) {
            (Some(p), _) => p.clone(),
            (None, Some(l)) => {
                let files: usize = l.sessions.iter().map(Vec::len).sum();
                let secs: f64 = l.sessions.iter().flatten().map(|r| r.seconds()).sum();
                format!(
                    "TX drive: {} — {files} file{}, {}",
                    l.folder.display(),
                    if files == 1 { "" } else { "s" },
                    fmt_dur(secs)
                )
            }
            (None, None) => {
                "No recordings loaded. Connect the transmitter's storage to list them.".into()
            }
        };
        let connect_label = if working {
            "Connecting…"
        } else if list.is_some() {
            "TX storage connected"
        } else {
            "Connect TX storage"
        };
        let connect = widget::button::suggested(connect_label).on_press_maybe(
            (!working && list.is_none() && !erasing).then(|| msg(RecMsg::Connect(serial.clone()))),
        );
        let mut actions = widget::row::with_capacity(3)
            .spacing(space.space_xs)
            .push(connect);
        if list.is_some() {
            actions = actions.push(
                widget::button::standard("Eject").on_press(msg(RecMsg::Eject(serial.clone()))),
            );
        }
        actions = actions.push(
            widget::button::destructive(if erasing {
                "Deleting…"
            } else {
                "Delete all…"
            })
            .on_press_maybe((!erasing && !working).then(|| msg(RecMsg::EraseAsk(serial.clone())))),
        );
        let mut col = widget::column::with_capacity(6)
            .spacing(space.space_s)
            .push(actions)
            .push(widget::text::caption(source));
        if unit.storage {
            col = col.push(widget::text::body(
                "Wireless is off: while this transmitter is in USB storage mode it doesn't transmit to the receiver. \
                 Unplug it or switch it off and on to return to normal (Eject only disconnects the drive).",
            ));
        }

        // the recordings
        if let Some(l) = list {
            if l.sessions.is_empty() {
                col = col.push(widget::text::body(
                    "There are no recordings on this transmitter.",
                ));
            } else {
                let mut items = widget::list_column();
                for s in l.sessions.iter().rev() {
                    let key = s[0].name.clone();
                    let checked = l.checked.contains(&key);
                    let (k1, k2, s1, s2) =
                        (key.clone(), key.clone(), serial.clone(), serial.clone());
                    let date = self
                        .times(s, cfg)
                        .map(|(start, _)| local(start, "%-d %b %Y  %H:%M"))
                        .unwrap_or_else(|| "unknown date".into());
                    let secs: f64 = s.iter().map(|r| r.seconds()).sum();
                    let size: u64 = s.iter().map(|r| r.size).sum();
                    let marks: usize = s.iter().map(|r| r.markers.len()).sum();
                    let mut details = format!(
                        "{date}  ·  {}  ·  {} file{}, {}  ·  {}",
                        fmt_dur(secs),
                        s.len(),
                        if s.len() == 1 { "" } else { "s" },
                        fmt_size(size),
                        s[0].kind()
                    );
                    if marks > 0 {
                        details.push_str(&format!(
                            "  ·  {marks} marker{}",
                            if marks == 1 { "" } else { "s" }
                        ));
                    }
                    let is_playing = self
                        .player
                        .as_ref()
                        .is_some_and(|(ps, pk, p)| *ps == unit.serial && *pk == key && !p.paused());
                    let row = widget::row::with_capacity(3)
                        .spacing(space.space_s)
                        .align_y(Alignment::Center)
                        .push(widget::checkbox(checked).on_toggle(move |on| {
                            Message::Rec(RecMsg::Toggle(s1.clone(), k1.clone(), on))
                        }))
                        .push(
                            widget::column::with_capacity(2)
                                .push(widget::text::body(session_name(s)))
                                .push(widget::text::caption(details))
                                .width(Length::Fill),
                        )
                        .push(
                            widget::button::icon(widget::icon::from_name(if is_playing {
                                "media-playback-pause-symbolic"
                            } else {
                                "media-playback-start-symbolic"
                            }))
                            .tooltip(if is_playing { "Pause" } else { "Play" })
                            .on_press(Message::Rec(RecMsg::Play(s2, k2))),
                        );
                    items = items.add(row);
                }
                let all = l.checked.len() == l.sessions.len();
                col = col.push(
                    widget::row::with_capacity(2).spacing(space.space_xs).push(
                        widget::button::text(if all { "Select none" } else { "Select all" })
                            .on_press(msg(RecMsg::All(serial.clone(), !all))),
                    ),
                );
                if let Some(bar) = self.player_bar(&unit.serial, l) {
                    col = col.push(bar);
                }
                col = col.push(items);
                col = col.push(self.export_section(&serial, l, cfg));
            }
        }
        if let Some(s) = &self.status {
            col = col.push(widget::text::caption(s.clone()));
        }
        col.into()
    }

    fn export_section<'a>(
        &'a self,
        serial: &str,
        l: &RecList,
        cfg: &Config,
    ) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();
        let mp3 = cfg.str("format") == "mp3";
        let mut sec = settings::section()
            .title("Export")
            .add(settings::item(
                "Folder",
                widget::row::with_capacity(2)
                    .spacing(space.space_xs)
                    .align_y(Alignment::Center)
                    .push(
                        widget::text_input("", cfg.str("export_dir"))
                            .on_input(|v| Message::Rec(RecMsg::ExportDir(v)))
                            .width(Length::Fixed(280.0)),
                    )
                    .push(
                        widget::button::icon(widget::icon::from_name("folder-open-symbolic"))
                            .tooltip("Open the folder")
                            .on_press(Message::Rec(RecMsg::OpenFolder)),
                    ),
            ))
            .add(settings::item(
                "Format",
                widget::dropdown(&FORMATS[..], Some(mp3 as usize), |i| {
                    Message::Rec(RecMsg::Format(i))
                }),
            ));
        if mp3 {
            sec = sec.add(settings::item(
                "Quality",
                widget::dropdown(
                    &MP3_PRESETS[..],
                    Some(cfg.int("mp3_preset").unwrap_or(1) as usize),
                    |i| Message::Rec(RecMsg::Preset(i)),
                ),
            ));
        }
        sec = sec.add(settings::item(
            "Join split files",
            widget::toggler(cfg.bool("join")).on_toggle(|v| Message::Rec(RecMsg::Join(v))),
        ));
        let bottom: Element<'a, Message> = match &self.export {
            Some(j) => widget::row::with_capacity(3)
                .spacing(space.space_s)
                .align_y(Alignment::Center)
                .push(widget::text::caption(j.label.clone()).width(Length::Fill))
                .push(widget::progress_bar::determinate_linear(j.done).width(Length::Fixed(160.0)))
                .push(widget::button::standard("Cancel").on_press(Message::Rec(RecMsg::Cancel)))
                .into(),
            None => {
                let n = l.checked.len();
                let label = if n == 0 {
                    "Tick recordings to export them".to_string()
                } else {
                    format!("{n} selected")
                };
                widget::row::with_capacity(2)
                    .spacing(space.space_s)
                    .align_y(Alignment::Center)
                    .push(widget::text::caption(label).width(Length::Fill))
                    .push(widget::button::suggested("Export").on_press_maybe(
                        (n > 0).then(|| Message::Rec(RecMsg::Export(serial.to_string()))),
                    ))
                    .into()
            }
        };
        widget::column::with_capacity(2)
            .spacing(space.space_s)
            .push(sec)
            .push(bottom)
            .into()
    }

    /// The dialog on top of the window, if any.
    pub fn dialog_view(&self) -> Option<Element<'_, Message>> {
        match self.dialog.as_ref()? {
            Dialog::Quit => Some(
                widget::dialog()
                    .title("An export is running")
                    .body("Cancel it and quit? The file being written now is removed; files already exported are kept.")
                    .primary_action(
                        widget::button::destructive("Cancel export and quit").on_press(Message::ForceQuit),
                    )
                    .secondary_action(
                        widget::button::standard("Keep exporting")
                            .on_press(Message::Rec(RecMsg::DialogCancel)),
                    )
                    .into(),
            ),
            Dialog::Connect { name, dont_ask, .. } => Some(
                widget::dialog()
                    .title(format!("Switch {name} to USB storage mode?"))
                    .body(
                        "To read its recordings, the transmitter becomes a USB drive. Meanwhile it doesn't transmit to the \
                         receiver, until you unplug its USB cable or switch it off and on. Don't do this during a take.",
                    )
                    .control(widget::checkbox(*dont_ask).label("Don't ask again").on_toggle(|v| Message::Rec(RecMsg::DontAsk(v))))
                    .primary_action(widget::button::suggested("Connect TX storage").on_press(Message::Rec(RecMsg::ConnectConfirmed)))
                    .secondary_action(widget::button::standard("Cancel").on_press(Message::Rec(RecMsg::DialogCancel)))
                    .into(),
            ),
            Dialog::Erase { name, what, typed, .. } => Some(
                widget::dialog()
                    .title(format!("Delete all recordings on {name}?"))
                    .body(format!(
                        "This permanently deletes {what} from the transmitter's memory. It can't be undone. Export the \
                         recordings you want to keep first. (Single recordings can't be deleted: the transmitter only \
                         allows deleting all of them.)\n\nTo confirm, type DELETE:"
                    ))
                    .control(widget::text_input("DELETE", typed.as_str()).on_input(|t| Message::Rec(RecMsg::EraseTyped(t))))
                    .primary_action(
                        widget::button::destructive("Delete all recordings")
                            .on_press_maybe((typed.trim() == "DELETE").then_some(Message::Rec(RecMsg::EraseConfirm))),
                    )
                    .secondary_action(widget::button::standard("Cancel").on_press(Message::Rec(RecMsg::DialogCancel)))
                    .into(),
            ),
        }
    }
}
