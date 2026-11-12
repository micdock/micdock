// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! MicDock — manage a RØDE Wireless GO II on Linux. The libcosmic desktop app.
//!
//! All device work goes through `micdock-core` on a blocking worker thread, so the window never waits
//! on a unit. The units are polled every few seconds and listed in the navigation sidebar.

use std::collections::BTreeMap;
use std::time::Duration;

use cosmic::app::{Settings, Task};
use cosmic::iced::{Alignment, Length, Size};
use cosmic::prelude::*;
use cosmic::widget::{self, nav_bar, segmented_button, settings};
use cosmic::{executor, iced, Core};
use micdock_core::devices::{self, DeviceStatus, Role, TxLink};
use micdock_core::hid::{
    ButtonMode, ChannelMode, Gain, RecMode, COARSE_GAIN, FINE_GAIN, PRESET_GAIN,
};

mod config;
mod demo;
mod device;
mod help;
mod player;
mod recordings;
mod util;

use device::{DevMsg, DevicePage};
use help::{Doc, Help, HelpMsg};
use recordings::{RecMsg, Recordings};

/// Reverse-DNS app id (placeholder until the project has a home page).
const APP_ID: &str = "io.github.micdock.MicDock";
/// Full read of every unit (battery, name, clock, settings).
const POLL: Duration = Duration::from_secs(3);
/// Transmitters are read this seldom (and when needed): any read during a long press on a transmitter's power
/// button cancels it. The receiver (polled every `POLL`) tells which transmitters are on, and their battery.
const TX_POLL: Duration = Duration::from_secs(30);
/// Cheap check of what is plugged in and in which mode (sysfs only): a change triggers a full read at once.
const WATCH: Duration = Duration::from_millis(700);

fn main() -> cosmic::iced::Result {
    // MicDock decides itself when the window may close: never during a firmware update
    let size = if demo::screenshot_dir().is_some() {
        Size::new(1000.0, 720.0)
    } else {
        Size::new(900.0, 680.0)
    };
    let settings = Settings::default()
        .exit_on_close(false)
        .size(size)
        .size_limits(
            cosmic::iced::Limits::NONE
                .min_width(420.0)
                .min_height(360.0),
        );
    cosmic::app::run::<App>(settings, ())
}

/// The pages of a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Audio,
    Recordings,
    Device,
}

#[derive(Clone, Debug)]
pub enum Message {
    Poll,
    Watch,
    /// (generation, units): results of a read that started before the last write are dropped.
    /// (generation, units, what was read).
    Probed(u64, Vec<DeviceStatus>, Read),
    TxTab(segmented_button::Entity),
    RxTab(segmented_button::Entity),
    SetRec(String, RecMode),
    SetButton(String, ButtonMode),
    SetChannel(String, ChannelMode),
    SetPowerSaver(String, bool),
    /// Receiver output gain (mode + step).
    SetGain(String, Gain),
    /// Transmitter low-sensitivity pad.
    SetPad(String, bool),
    /// A setting was written (or failed): text for the status line.
    Done(Result<String, String>),
    Rec(RecMsg),
    Dev(DevMsg),
    Help(HelpMsg),
    /// Show or hide the About panel.
    About,
    OpenUrl(String),
    /// Screenshot mode: show step n, then capture it.
    Shot(usize),
    ShotCapture(usize),
    ShotTaken(usize, iced::window::Screenshot),
    /// The window's close button, Alt+F4, the compositor…
    Quit,
    /// Quit even though an export is running (it is cancelled).
    ForceQuit,
}

struct App {
    core: Core,
    /// The connected units, one sidebar entry each (entry data = serial number).
    nav: nav_bar::Model,
    units: BTreeMap<String, DeviceStatus>,
    /// Tabs of a transmitter and of the receiver (no recordings); each remembers its own last tab.
    tx_tabs: segmented_button::SingleSelectModel,
    rx_tabs: segmented_button::SingleSelectModel,
    polling: bool,
    /// A write is in progress: settings are disabled meanwhile.
    busy: bool,
    status: Option<String>,
    cfg: config::Config,
    rec: Recordings,
    dev: DevicePage,
    /// Bumped by every write, so a read that started earlier can't show the old value again.
    generation: u64,
    /// Another read was asked for while one was running.
    repoll: bool,
    /// What `fingerprint()` returned last.
    seen: String,
    /// The next read should be a full one (button mode and power saver too). Those reads disturb the
    /// units' power buttons, so they're only done when something may have changed them.
    full_read: bool,
    /// When the transmitters were last read.
    tx_read_at: Option<std::time::Instant>,
    /// Quit as soon as the cancelled export has stopped.
    quitting: bool,
    help: Help,
    /// Demo mode: made-up units, no device access, no network.
    demo: bool,
    about: widget::about::About,
    /// The page's scroll area, to jump back to the top.
    scroll: widget::Id,
}

/// Scroll the page back to the top.
fn to_top(id: &widget::Id) -> Task<Message> {
    cosmic::iced::widget::scrollable::snap_to(
        id.clone(),
        cosmic::iced::widget::scrollable::RelativeOffset {
            x: None,
            y: Some(0.0),
        },
    )
}

/// Which units are plugged in, in which mode, and which TX drives are mounted: sysfs and /proc only, so it
/// can run often. (Settings changes need a full read; that runs every few seconds.)
fn fingerprint() -> String {
    let mut out = String::new();
    for d in micdock_core::hid::find_devices() {
        let storage = devices::in_storage_mode(&devices::usb_dir(&d.node));
        out.push_str(&format!("{} {:04x} {storage};", d.node, d.pid));
    }
    for (serial, (_, mount)) in devices::tx_mounts() {
        out.push_str(&format!("{serial}@{mount};"));
    }
    out
}

/// Run blocking core work on tokio's blocking pool and turn the result into a message.
fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    then: fn(T) -> Message,
) -> Task<Message> {
    cosmic::task::future(async move {
        match tokio::task::spawn_blocking(work).await {
            Ok(v) => then(v),
            Err(e) => Message::Done(Err(e.to_string())),
        }
    })
}

/// Run blocking work on tokio's blocking pool; `send` delivers progress messages while it runs and the
/// returned message comes last.
pub fn job<M, F>(f: F) -> Task<Message>
where
    M: Into<Message> + Send + 'static,
    F: FnOnce(&dyn Fn(M)) -> M + Send + 'static,
{
    let s = cosmic::iced::stream::channel(
        64,
        async move |mut tx: futures::channel::mpsc::Sender<Message>| {
            let progress_tx = tx.clone();
            let res = tokio::task::spawn_blocking(move || {
                let send = move |m: M| {
                    let _ = progress_tx.clone().try_send(m.into());
                };
                f(&send)
            })
            .await;
            let last = res
                .map(Into::into)
                .unwrap_or_else(|e| Message::Done(Err(e.to_string())));
            let _ = futures::SinkExt::send(&mut tx, last).await;
        },
    );
    cosmic::task::stream(s)
}

impl From<RecMsg> for Message {
    fn from(m: RecMsg) -> Message {
        Message::Rec(m)
    }
}

impl From<HelpMsg> for Message {
    fn from(m: HelpMsg) -> Message {
        Message::Help(m)
    }
}

impl From<DevMsg> for Message {
    fn from(m: DevMsg) -> Message {
        Message::Dev(m)
    }
}

const REC_CHOICES: [(RecMode, &str); 4] = [
    (RecMode::Off, "Off"),
    (RecMode::Always, "Always"),
    (RecMode::Button, "Manual (Ø button)"),
    (RecMode::Backup, "Backup"),
];
const BUTTON_CHOICES: [(ButtonMode, &str); 4] = [
    (ButtonMode::Mute, "Mute"),
    (ButtonMode::Marker, "Marker"),
    (ButtonMode::Record, "Record"),
    (ButtonMode::None, "None"),
];
const CHANNEL_CHOICES: [(ChannelMode, &str); 2] = [
    (ChannelMode::Merged, "Merged"),
    (ChannelMode::Split, "Split"),
];

/// Fine-gain steps 0–10 = −30 to 0 dB.
const GAIN_MODES: [&str; 3] = ["Coarse", "Fine", "Camera preset"];
const COARSE_LABELS: [&str; 3] = ["−24 dB", "−12 dB", "0 dB"];
const GAIN_LABELS: [&str; 11] = [
    "−30 dB", "−27 dB", "−24 dB", "−21 dB", "−18 dB", "−15 dB", "−12 dB", "−9 dB", "−6 dB",
    "−3 dB", "0 dB",
];

fn labels<T>(choices: &[(T, &'static str)]) -> Vec<&'static str> {
    choices.iter().map(|(_, l)| *l).collect()
}

fn index_of<T: PartialEq>(choices: &[(T, &str)], v: Option<T>) -> Option<usize> {
    v.and_then(|v| choices.iter().position(|(c, _)| *c == v))
}

fn unit_title(u: &DeviceStatus) -> String {
    match u.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("Wireless GO II {}", u.role.as_str()),
    }
}

/// What a poll reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// Everything, button mode and power saver included (those reads disturb the power buttons).
    Full,
    /// All units, without button mode and power saver.
    Light,
    /// Only the receivers; transmitters keep their last values.
    RxOnly,
}

/// Whether a transmitter is on, as far as MicDock can tell: only the receiver knows (its link records), so
/// this needs a receiver on USB. A TX's own USB replies are the same whether it is on or off.
enum Power<'a> {
    /// Linked to the receiver: switched on and in range.
    On(&'a TxLink),
    /// Not linked while the receiver has another transmitter linked (so the receiver is on): this one is
    /// switched off or out of range.
    NotLinked,
    /// The receiver has no transmitter linked at all: this one is off, or the receiver is (MicDock can't
    /// read the receiver's own on/off state).
    NoLinks,
    /// In USB storage mode: its wireless is off.
    Storage,
    /// No receiver on USB to ask.
    Unknown,
}

fn tx_power<'a>(units: &'a BTreeMap<String, DeviceStatus>, tx: &DeviceStatus) -> Power<'a> {
    if tx.storage {
        return Power::Storage;
    }
    let mut rxs = units
        .values()
        .filter(|u| u.role == Role::Rx && u.error.is_none())
        .peekable();
    if rxs.peek().is_none() {
        return Power::Unknown;
    }
    let links: Vec<&TxLink> = rxs.flat_map(|rx| rx.links.iter()).collect();
    match links.iter().find(|l| l.serial == tx.serial) {
        Some(l) => Power::On(l),
        None if links.is_empty() => Power::NoLinks,
        None => Power::NotLinked,
    }
}

/// The screenshots for the manual: (file name, what to show).
const SHOTS: [(&str, Shot); 5] = [
    ("tx-audio", Shot::Unit(demo::TX1, 0)),
    ("recordings", Shot::Unit(demo::TX1, 1)),
    ("device", Shot::Unit(demo::TX1, 2)),
    ("rx-audio", Shot::Unit(demo::RX, 0)),
    ("help", Shot::Help("#on-board-recording")),
];

#[derive(Clone, Copy)]
enum Shot {
    /// (serial, tab position)
    Unit(&'static str, u16),
    /// The help page at this link.
    Help(&'static str),
}

fn save_png(path: &std::path::Path, shot: &iced::window::Screenshot) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, shot.size.width, shot.size.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(std::io::Error::other)?;
    w.write_image_data(&shot.rgba)
        .map_err(std::io::Error::other)?;
    Ok(())
}

impl App {
    /// Screenshot mode: show step `i`, wait for it to be drawn, then capture it; quit after the last.
    fn shot(&mut self, i: usize) -> Task<Message> {
        let Some((_, what)) = SHOTS.get(i) else {
            return iced::exit();
        };
        let task = match *what {
            Shot::Unit(serial, tab) => {
                self.help.open = false;
                let entity = self
                    .nav
                    .iter()
                    .find(|&e| self.nav.data::<String>(e).map(String::as_str) == Some(serial));
                if let Some(e) = entity {
                    self.nav.activate(e);
                }
                if serial == demo::RX {
                    self.rx_tabs.activate_position(tab);
                } else {
                    self.tx_tabs.activate_position(tab);
                }
                to_top(&self.scroll)
            }
            Shot::Help(link) => {
                let (t1, _) = self.help.update(HelpMsg::Open(Doc::Manual));
                let (t2, _) = self.help.update(HelpMsg::Link(link.into()));
                Task::batch([t1, t2, to_top(&self.scroll)])
            }
        };
        // the first shot waits longer: the window has to appear and the units to be listed
        let wait = if i == 0 { 2500 } else { 1200 };
        let capture = cosmic::task::future(async move {
            let _ = tokio::task::spawn_blocking(move || {
                std::thread::sleep(Duration::from_millis(wait))
            })
            .await;
            Message::ShotCapture(i)
        });
        Task::batch([task, capture])
    }

    fn selected(&self) -> Option<&DeviceStatus> {
        self.nav
            .active_data::<String>()
            .and_then(|s| self.units.get(s))
    }

    fn tab(&self, role: Role) -> Tab {
        let tabs = if role == Role::Tx {
            &self.tx_tabs
        } else {
            &self.rx_tabs
        };
        tabs.active_data::<Tab>().copied().unwrap_or(Tab::Audio)
    }

    /// Rebuild the sidebar from the latest probe, keeping the selected unit selected.
    fn set_units(&mut self, list: Vec<DeviceStatus>) {
        let keep = self.nav.active_data::<String>().cloned();
        self.units = list.into_iter().map(|u| (u.serial.clone(), u)).collect();
        self.nav.clear();
        let mut order: Vec<&DeviceStatus> = self.units.values().collect();
        order.sort_by_key(|u| (u.role != Role::Tx, u.serial.clone()));
        for u in order {
            let icon = if u.role == Role::Tx {
                "audio-input-microphone-symbolic"
            } else {
                "audio-card-symbolic"
            };
            let state = match (u.role, tx_power(&self.units, u)) {
                (Role::Tx, Power::On(_)) => " · on",
                (Role::Tx, Power::NotLinked) => " · off",
                (Role::Tx, Power::NoLinks) => " · not linked",
                (Role::Tx, Power::Storage) => " · USB drive",
                _ => "",
            };
            let mut entry = self
                .nav
                .insert()
                .text(format!("{}{state}", unit_title(u)))
                .icon(widget::icon::from_name(icon))
                .data(u.serial.clone());
            if keep.as_deref() == Some(u.serial.as_str()) {
                entry = entry.activate();
            }
            let _ = entry;
        }
        if self.nav.active_data::<String>().is_none() {
            self.nav.activate_position(0);
        }
    }

    fn header(&self, u: &DeviceStatus) -> Element<'_, Message> {
        let mut info = vec![format!("Wireless GO II {}", u.role.as_str())];
        if let Some(b) = u.battery {
            info.push(format!("battery {b} %"));
        }
        if u.role == Role::Tx {
            match tx_power(&self.units, u) {
                Power::On(l) => info.push(format!("on · linked to the receiver as TX{}", l.slot)),
                Power::NotLinked => {
                    info.push("off · not linked to the receiver (switched off or out of range)".into())
                }
                Power::NoLinks => info.push(
                    "not linked · the receiver has no transmitter linked: this one or the receiver is off".into(),
                ),
                Power::Storage => info.push("USB storage · wireless off".into()),
                Power::Unknown => {}
            }
        } else if u.storage {
            info.push("USB storage · wireless off".into());
        }
        widget::column::with_capacity(2)
            .push(widget::text::title3(unit_title(u)))
            .push(widget::text::caption(info.join("  ·  ")))
            .spacing(2)
            .into()
    }

    fn audio_page(&self, u: &DeviceStatus) -> Element<'_, Message> {
        let serial = u.serial.clone();
        let mut col = widget::column::with_capacity(2).spacing(cosmic::theme::spacing().space_m);
        match u.role {
            Role::Tx => {
                let s1 = serial.clone();
                let rec = widget::dropdown(
                    labels(&REC_CHOICES),
                    index_of(&REC_CHOICES, u.rec),
                    move |i| Message::SetRec(s1.clone(), REC_CHOICES[i].0),
                );
                let note = match u.rec {
                    Some(RecMode::Always) => "Records from power-on to power-off. If the transmitter is on now, switch it off and on to start.",
                    Some(RecMode::Button) => "Press the Ø button to start a recording and again to stop it.",
                    Some(RecMode::Backup) => "Records while the transmitter is linked to the receiver.",
                    Some(RecMode::Off) => "The transmitter does not record. Existing recordings stay on it.",
                    None => "The setting could not be read.",
                };
                col = col.push(
                    settings::section()
                        .title("On-board recording")
                        .add(settings::item("Record", rec))
                        .add(widget::text::caption(note)),
                );
                let s2 = serial.clone();
                let button = widget::dropdown(
                    labels(&BUTTON_CHOICES),
                    index_of(&BUTTON_CHOICES, u.button),
                    move |i| Message::SetButton(s2.clone(), BUTTON_CHOICES[i].0),
                );
                col = col.push(
                    settings::section()
                        .title("Ø button")
                        .add(settings::item("Short press", button))
                        .add(widget::text::caption(
                            "Record also switches on-board recording to Manual.",
                        )),
                );
                if let Some(pad) = u.pad {
                    let s3 = serial.clone();
                    col = col.push(
                        settings::section()
                            .title("Input")
                            .add(settings::item(
                                "Low-sensitivity pad",
                                widget::toggler(pad).on_toggle(move |v| Message::SetPad(s3.clone(), v)),
                            ))
                            .add(widget::text::caption(
                                "Lowers the sensitivity of the built-in microphone and the 3.5 mm input, e.g. when \
                                 the transmitter is close to the mouth. The receiver shows a pad symbol for it.",
                            )),
                    );
                }
            }
            _ => {
                let s = serial.clone();
                let ch = widget::dropdown(
                    labels(&CHANNEL_CHOICES),
                    index_of(&CHANNEL_CHOICES, u.channel),
                    move |i| Message::SetChannel(s.clone(), CHANNEL_CHOICES[i].0),
                );
                col = col.push(
                    settings::section().title("Output").add(settings::item("Merged & split", ch)).add(widget::text::caption(
                        "Merged: both transmitters on both channels. Split: transmitter 1 left, transmitter 2 right.",
                    )),
                );
                // output gain and power saver (both also on the receiver's own buttons)
                let mut out = settings::section().title("Receiver");
                if let Some(g) = u.gain {
                    let s = serial.clone();
                    let mode_idx = match g.mode {
                        COARSE_GAIN => Some(0),
                        FINE_GAIN => Some(1),
                        PRESET_GAIN => Some(2),
                        _ => None,
                    };
                    out = out.add(settings::item(
                        "Gain mode",
                        widget::dropdown(&GAIN_MODES[..], mode_idx, move |i| {
                            let m = [COARSE_GAIN, FINE_GAIN, PRESET_GAIN][i];
                            Message::SetGain(s.clone(), g.converted(m))
                        }),
                    ));
                    let s = serial.clone();
                    let level: Element<'_, Message> = match g.mode {
                        COARSE_GAIN | FINE_GAIN => {
                            let labels: &[&str] = if g.mode == COARSE_GAIN {
                                &COARSE_LABELS
                            } else {
                                &GAIN_LABELS
                            };
                            let mode = g.mode;
                            widget::dropdown(
                                labels,
                                (g.db().is_some()).then_some(g.step as usize),
                                move |i| {
                                    Message::SetGain(
                                        s.clone(),
                                        Gain {
                                            mode,
                                            step: i as u8,
                                        },
                                    )
                                },
                            )
                            .into()
                        }
                        PRESET_GAIN => widget::text::body(if g.step == 0 {
                            "WIGO2 (default preset)".to_string()
                        } else {
                            format!("preset {} (choose it on the receiver)", g.step)
                        })
                        .into(),
                        _ => widget::text::body(format!("mode {}, step {}", g.mode, g.step)).into(),
                    };
                    out = out.add(settings::item("Output gain", level));
                }
                if let Some(on) = u.power_saver {
                    let s = serial.clone();
                    out = out.add(settings::item(
                        "Power saver",
                        widget::toggler(on)
                            .on_toggle(move |v| Message::SetPowerSaver(s.clone(), v)),
                    ));
                }
                col = col.push(out.add(widget::text::caption(
                    "Output gain affects the 3.5 mm output only, not USB. Coarse: three steps; fine: 3 dB steps; \
                     camera preset: presets loaded with RØDE's app, picked with the receiver's gain button. \
                     Power saver turns the screen's backlight off \
                     after 10 seconds; a single click on the receiver's power button switches it too.",
                )));
                // which transmitters are on: the receiver's link records
                let mut txs = settings::section().title("Transmitters");
                for slot in [1u8, 2] {
                    let value = match u.links.iter().find(|l| l.slot == slot) {
                        Some(l) => {
                            let name = if l.name.is_empty() {
                                "(no name)".to_string()
                            } else {
                                l.name.clone()
                            };
                            let mut v = match l.battery {
                                Some(b) => format!("{name} — on, battery {b} %"),
                                None => format!("{name} — on"),
                            };
                            if l.pad == Some(true) {
                                v.push_str(", pad on");
                            }
                            v
                        }
                        None => "not linked (off or out of range)".into(),
                    };
                    txs = txs.add(settings::item(
                        format!("TX{slot}"),
                        widget::text::body(value),
                    ));
                }
                col = col.push(txs.add(widget::text::caption(
                    "A transmitter shows as on while it is linked to this receiver. The receiver's own on/off state \
                     can't be read over USB: while it is off, both transmitters show as not linked.",
                )));
            }
        }
        col.into()
    }
}

impl cosmic::Application for App {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, _flags: ()) -> (Self, Task<Message>) {
        core.nav_bar_toggle_condensed();
        let rx_tabs = segmented_button::ModelBuilder::default()
            .insert(|b| b.text("Audio").data(Tab::Audio).activate())
            .insert(|b| b.text("Device").data(Tab::Device))
            .build();
        let tx_tabs = segmented_button::ModelBuilder::default()
            .insert(|b| b.text("Audio").data(Tab::Audio).activate())
            .insert(|b| b.text("Recordings").data(Tab::Recordings))
            .insert(|b| b.text("Device").data(Tab::Device))
            .build();
        let mut app = App {
            core,
            nav: nav_bar::Model::default(),
            units: BTreeMap::new(),
            tx_tabs,
            rx_tabs,
            polling: false,
            busy: false,
            status: None,
            cfg: if demo::active() { config::Config::demo() } else { config::Config::load() },
            rec: Recordings::default(),
            dev: DevicePage::default(),
            generation: 0,
            repoll: false,
            seen: String::new(),
            full_read: true,
            tx_read_at: None,
            quitting: false,
            help: Help::default(),
            demo: demo::active(),
            about: widget::about::About::default()
                .name("MicDock")
                .icon(widget::icon::from_name("audio-input-microphone"))
                .version(env!("CARGO_PKG_VERSION"))
                .comments(
                    "Manage a RØDE Wireless GO II on Linux: recordings, record mode, Ø button, gain, power saver, \
                     merged/split output, names, clocks and firmware.\n\nUnofficial: not affiliated with or \
                     endorsed by RØDE Microphones. RØDE and Wireless GO are trademarks of their owner. Firmware and \
                     the user guide are downloaded from rode.com; firmware installs are at your own risk.",
                )
                .license("GPL-3.0-or-later")
                .license_url("https://www.gnu.org/licenses/gpl-3.0.html")
                .links([
                    ("Project page and issues", "https://github.com/micdock/micdock"),
                    ("RØDE's Wireless GO II user guide", micdock_core::guide::GUIDE_URL),
                ]),
            scroll: widget::Id::unique(),
        };
        let title = match app.core.main_window_id() {
            Some(id) => app.set_window_title("MicDock".into(), id),
            None => Task::none(),
        };
        let (fw, guide) = if app.demo {
            app.dev.demo();
            app.rec.demo(demo::TX1, demo::folder(), demo::sessions());
            let shots = if demo::screenshot_dir().is_some() {
                cosmic::task::message(Message::Shot(0))
            } else {
                Task::none()
            };
            (shots, Task::none())
        } else {
            // RØDE's guide can't be bundled: fetch it once in the background, so it also opens offline later
            (
                app.dev.check_firmware(),
                cosmic::task::message(Message::Help(HelpMsg::Prefetch)),
            )
        };
        (
            app,
            Task::batch([title, cosmic::task::message(Message::Poll), fw, guide]),
        )
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        Some(&self.nav)
    }

    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<Message> {
        self.nav.activate(id);
        self.help.open = false;
        self.full_read = true;
        to_top(&self.scroll)
    }

    fn context_drawer(&self) -> Option<cosmic::app::context_drawer::ContextDrawer<'_, Message>> {
        if !self.core.window.show_context {
            return None;
        }
        Some(
            cosmic::app::context_drawer::context_drawer(
                widget::about(&self.about, |url| Message::OpenUrl(url.to_string())),
                Message::About,
            )
            .title("About"),
        )
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        vec![
            widget::button::icon(widget::icon::from_name("help-about-symbolic"))
                .tooltip("About MicDock")
                .on_press(Message::About)
                .into(),
            widget::button::icon(widget::icon::from_name("help-browser-symbolic"))
                .tooltip("Help (F1)")
                .on_press(Message::Help(HelpMsg::Open(Doc::Manual)))
                .into(),
        ]
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        let poll = iced::Subscription::batch([
            iced::time::every(POLL).map(|_| Message::Poll),
            iced::time::every(WATCH).map(|_| Message::Watch),
            iced::window::close_requests().map(|_| Message::Quit),
            iced::keyboard::listen().filter_map(|e| match e {
                iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::F1),
                    ..
                } => Some(Message::Help(HelpMsg::Open(Doc::Manual))),
                _ => None,
            }),
        ]);
        if self.rec.playing() {
            // redraw the player's position
            let tick =
                iced::time::every(Duration::from_millis(250)).map(|_| Message::Rec(RecMsg::Tick));
            return iced::Subscription::batch([poll, tick]);
        }
        poll
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Poll if self.demo => {
                if !self.polling {
                    self.polling = true;
                    let full = self.units.is_empty();
                    return cosmic::task::message(Message::Probed(
                        self.generation,
                        demo::units(),
                        if full { Read::Full } else { Read::Light },
                    ));
                }
            }
            Message::Watch if self.demo => {}
            Message::Shot(i) => return self.shot(i),
            Message::ShotCapture(i) => {
                let Some(id) = self.core.main_window_id() else {
                    return Task::none();
                };
                return iced::window::screenshot(id)
                    .map(move |s| cosmic::Action::App(Message::ShotTaken(i, s)));
            }
            Message::ShotTaken(i, shot) => {
                let name = SHOTS.get(i).map(|s| s.0).unwrap_or("shot");
                if let Some(dir) = demo::screenshot_dir() {
                    if let Err(e) = save_png(&dir.join(format!("{name}.png")), &shot) {
                        eprintln!("{name}: {e}");
                    } else {
                        eprintln!(
                            "saved {name}.png ({}×{})",
                            shot.size.width, shot.size.height
                        );
                    }
                }
                return cosmic::task::message(Message::Shot(i + 1));
            }
            Message::Poll => {
                if self.dev.updating() {
                    return Task::none(); // the firmware installer owns the unit
                }
                if self.polling || self.busy {
                    self.repoll = true;
                    return Task::none();
                }
                self.polling = true;
                self.repoll = false;
                self.seen = fingerprint();
                let generation = self.generation;
                let kind = if std::mem::take(&mut self.full_read) {
                    Read::Full
                } else if self.tx_read_at.is_none_or(|t| t.elapsed() >= TX_POLL) {
                    Read::Light
                } else {
                    Read::RxOnly
                };
                if std::env::var_os("MICDOCK_DEBUG").is_some() {
                    eprintln!(
                        "{} poll {kind:?}",
                        chrono::Local::now().format("%H:%M:%S%.3f")
                    );
                }
                return cosmic::task::future(async move {
                    let probe = match kind {
                        Read::Full => devices::probe_devices,
                        Read::Light => devices::probe_devices_light,
                        Read::RxOnly => devices::probe_devices_rx_only,
                    };
                    let list = tokio::task::spawn_blocking(probe).await.unwrap_or_default();
                    Message::Probed(generation, list, kind)
                });
            }
            Message::Watch => {
                if !self.polling && !self.dev.updating() && fingerprint() != self.seen {
                    return cosmic::task::message(Message::Poll);
                }
            }
            Message::Probed(generation, mut list, kind) => {
                self.polling = false;
                if generation != self.generation || self.repoll {
                    // a write happened meanwhile (or another read was asked for): read again
                    self.full_read |= kind == Read::Full;
                    return cosmic::task::message(Message::Poll);
                }
                if kind != Read::RxOnly {
                    self.tx_read_at = Some(std::time::Instant::now());
                }
                if kind != Read::Full {
                    // keep what this read left out: button mode and power saver, and (receiver-only reads)
                    // everything a transmitter reports about itself
                    let mut new_unit = false;
                    for u in &mut list {
                        match self.units.get(&u.serial) {
                            Some(old)
                                if kind == Read::RxOnly
                                    && u.role == Role::Tx
                                    && u.error.is_none() =>
                            {
                                let (node, storage) = (u.node.clone(), u.storage);
                                *u = old.clone();
                                u.node = node;
                                u.storage = storage;
                            }
                            Some(old) => {
                                u.button = old.button;
                                u.power_saver = old.power_saver;
                            }
                            None => new_unit = true,
                        }
                    }
                    if new_unit {
                        self.full_read = true;
                    }
                }
                // a linked transmitter's battery, from the receiver (fresher than its own last read)
                let links: Vec<TxLink> = list.iter().flat_map(|u| u.links.clone()).collect();
                for u in list.iter_mut().filter(|u| u.role == Role::Tx) {
                    if let Some(b) = links
                        .iter()
                        .find(|l| l.serial == u.serial)
                        .and_then(|l| l.battery)
                    {
                        u.battery = Some(b);
                    }
                }
                self.set_units(list);
                if self.demo {
                    return Task::none();
                }
                let units: Vec<DeviceStatus> = self.units.values().cloned().collect();
                let task = self.rec.after_poll(&units, &mut self.cfg);
                if self.full_read {
                    // a new unit (or a tab switch meanwhile): read everything now
                    return Task::batch([task, cosmic::task::message(Message::Poll)]);
                }
                return task;
            }
            Message::TxTab(id) => {
                self.tx_tabs.activate(id);
                self.full_read = true;
                return to_top(&self.scroll);
            }
            Message::RxTab(id) => {
                self.rx_tabs.activate(id);
                self.full_read = true;
                return to_top(&self.scroll);
            }
            Message::About => {
                self.core.window.show_context = !self.core.window.show_context;
            }
            Message::OpenUrl(url) => {
                util::open_external(&url);
            }
            Message::Help(m) => {
                let (task, top) = self.help.update(m);
                if top {
                    return Task::batch([task, to_top(&self.scroll)]);
                }
                return task;
            }
            Message::SetRec(serial, mode) => {
                self.busy = true;
                self.generation += 1;
                if let Some(u) = self.units.get_mut(&serial) {
                    u.rec = Some(mode); // show it at once; the next read confirms it
                }
                return blocking(
                    move || {
                        devices::set_rec_mode(&serial, mode)
                            .map(|m| format!("On-board recording: {}", m.as_str()))
                    },
                    |r| Message::Done(r.map_err(|e| e.to_string())),
                );
            }
            Message::SetButton(serial, mode) => {
                self.busy = true;
                self.generation += 1;
                if let Some(u) = self.units.get_mut(&serial) {
                    u.button = Some(mode); // show it at once; the next read confirms it
                }
                return blocking(
                    move || {
                        devices::set_button_mode(&serial, mode)
                            .map(|m| format!("Ø button: {}", m.as_str()))
                    },
                    |r| Message::Done(r.map_err(|e| e.to_string())),
                );
            }
            Message::SetChannel(serial, mode) => {
                self.busy = true;
                self.generation += 1;
                if let Some(u) = self.units.get_mut(&serial) {
                    u.channel = Some(mode); // show it at once; the next read confirms it
                }
                return blocking(
                    move || {
                        devices::set_channel_mode(&serial, mode)
                            .map(|m| format!("Receiver output: {}", m.as_str()))
                    },
                    |r| Message::Done(r.map_err(|e| e.to_string())),
                );
            }
            Message::SetPowerSaver(serial, on) => {
                self.busy = true;
                self.generation += 1;
                if let Some(u) = self.units.get_mut(&serial) {
                    u.power_saver = Some(on);
                }
                return blocking(
                    move || {
                        devices::set_power_saver(&serial, on)
                            .map(|on| format!("Power saver: {}", if on { "on" } else { "off" }))
                    },
                    |r| Message::Done(r.map_err(|e| e.to_string())),
                );
            }
            Message::SetGain(serial, g) => {
                self.busy = true;
                self.generation += 1;
                if let Some(u) = self.units.get_mut(&serial) {
                    u.gain = Some(g);
                }
                return blocking(
                    move || {
                        devices::set_gain(&serial, g).map(|g| match g.db() {
                            Some(db) => format!("Output gain: {db} dB"),
                            None => "Output gain: camera preset".into(),
                        })
                    },
                    |r| Message::Done(r.map_err(|e| e.to_string())),
                );
            }
            Message::SetPad(serial, on) => {
                self.busy = true;
                self.generation += 1;
                if let Some(u) = self.units.get_mut(&serial) {
                    u.pad = Some(on);
                }
                return blocking(
                    move || {
                        devices::set_pad(&serial, on).map(|on| {
                            format!("Low-sensitivity pad: {}", if on { "on" } else { "off" })
                        })
                    },
                    |r| Message::Done(r.map_err(|e| e.to_string())),
                );
            }
            Message::Quit => {
                if self.dev.updating() {
                    self.dev.dialog = Some(device::Dialog::Info {
                        title: "A firmware update is running".into(),
                        body: "MicDock can't close until the update has finished: stopping it halfway could leave \
                               the unit unusable until it is updated again. Keep the unit connected and wait."
                            .into(),
                    });
                    return Task::none();
                }
                if self.rec.export.is_some() {
                    self.rec.dialog = Some(recordings::Dialog::Quit);
                    return Task::none();
                }
                return iced::exit();
            }
            Message::ForceQuit => {
                if self.dev.updating() {
                    return Task::none();
                }
                match &self.rec.export {
                    Some(j) => {
                        // cancel, and quit once the export has removed its unfinished file
                        j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        self.rec.dialog = None;
                        self.quitting = true;
                    }
                    None => return iced::exit(),
                }
            }
            Message::Done(r) => {
                self.busy = false;
                self.full_read = true; // confirm the new value (button mode included)
                self.status = Some(match r {
                    Ok(s) => s,
                    Err(e) => format!("Could not change the setting: {e}"),
                });
                return cosmic::task::message(Message::Poll);
            }
            Message::Dev(m) => {
                // writes: drop reads that started before, and read again when done
                let write = matches!(
                    m,
                    DevMsg::Rename(..) | DevMsg::ClockConfirm | DevMsg::InstallConfirm
                );
                let done = matches!(
                    m,
                    DevMsg::Renamed(..) | DevMsg::ClockDone(..) | DevMsg::Installed(..)
                );
                if write || done {
                    self.generation += 1;
                }
                if let DevMsg::Renamed(serial, name, Ok(())) = &m {
                    if let Some(u) = self.units.get_mut(serial) {
                        u.name = Some(name.clone());
                    }
                    self.set_units(self.units.values().cloned().collect());
                }
                let units: Vec<DeviceStatus> = self.units.values().cloned().collect();
                let task = self
                    .dev
                    .update(m, &units, &mut self.cfg, self.rec.live_offset);
                if done {
                    return Task::batch([task, cosmic::task::message(Message::Poll)]);
                }
                return task;
            }
            Message::Rec(m) => {
                // a finished connect/eject/erase changes the units: poll right away
                let repoll = matches!(
                    m,
                    RecMsg::Loaded(..) | RecMsg::Ejected(..) | RecMsg::EraseDone(..)
                );
                let units: Vec<DeviceStatus> = self.units.values().cloned().collect();
                let task = self.rec.update(m, &units, &mut self.cfg);
                if self.quitting && self.rec.export.is_none() {
                    return iced::exit();
                }
                if repoll {
                    return Task::batch([task, cosmic::task::message(Message::Poll)]);
                }
                return task;
            }
        }
        Task::none()
    }

    /// The header bar's close button.
    fn on_app_exit(&mut self) -> Option<Message> {
        Some(Message::Quit)
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        self.rec.dialog_view().or_else(|| self.dev.dialog_view())
    }

    fn view(&self) -> Element<'_, Message> {
        let space = cosmic::theme::spacing();
        if self.help.open {
            return widget::scrollable(
                widget::container(widget::container(self.help.view()).max_width(820))
                    .padding(space.space_m)
                    .center_x(Length::Fill),
            )
            .id(self.scroll.clone())
            .into();
        }
        let Some(u) = self.selected() else {
            return widget::container(
                widget::column::with_capacity(2)
                    .push(widget::text::title3("No device found"))
                    .push(widget::text::body(
                        "Connect a Wireless GO II transmitter or receiver by USB and switch it on.",
                    ))
                    .spacing(space.space_xs)
                    .align_x(Alignment::Center),
            )
            .center(Length::Fill)
            .into();
        };
        // the receiver has no recordings
        let tabs: Element<'_, Message> = if u.role == Role::Tx {
            widget::segmented_control::horizontal(&self.tx_tabs)
                .on_activate(Message::TxTab)
                .into()
        } else {
            widget::segmented_control::horizontal(&self.rx_tabs)
                .on_activate(Message::RxTab)
                .into()
        };
        let tab = self.tab(u.role);
        let page = match tab {
            Tab::Audio => self.audio_page(u),
            Tab::Recordings => self.rec.view(u, &self.cfg),
            Tab::Device => self.dev.view(u, &self.cfg, self.rec.live_offset),
        };
        let mut col = widget::column::with_capacity(5)
            .push(self.header(u))
            .push(tabs)
            .push(page)
            .spacing(space.space_m)
            .max_width(760);
        if let Some(s) = &self.status {
            col = col.push(widget::text::caption(s.clone()));
        }
        widget::scrollable(
            widget::container(col)
                .padding(space.space_m)
                .center_x(Length::Fill),
        )
        .id(self.scroll.clone())
        .into()
    }
}
