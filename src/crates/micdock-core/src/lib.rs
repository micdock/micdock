// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! MicDock core: everything about the RØDE Wireless GO II that is not user interface.
//!
//! * [`hid`] — hidraw access and the settings protocol (report 9 GET/SET) with the decoded settings.
//! * [`devices`] — finding and probing units, USB storage mode, mounting the transmitter's drive.
//! * [`recordings`] — reading the on-board recordings (`.UGG`, `.WAV`), grouping and exporting them.
//! * [`firmware`] — RØDE's update manifest, verified firmware downloads.
//! * [`fwupdate`] — the firmware installer (radio chip + main MCU).
//! * [`guide`] — an offline, cleaned-up copy of RØDE's user guide.
//! * [`paths`] — where MicDock keeps its files (shared with the Python version).
//!
//! The protocol was recovered from RØDE Central's `rode_bridge.dll`; see `PROTOCOL.md` in the repository.
//! Nothing here depends on a GUI toolkit: the command-line tools and the app share it.

pub mod devices;
pub mod firmware;
pub mod fwupdate;
pub mod guide;
pub mod hid;
pub mod paths;
pub mod recordings;

use std::time::Duration;

/// Errors from talking to a unit or reading recordings.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The unit did not answer in time.
    #[error("{0}")]
    Timeout(String),
    /// The unit answered, but not with an ACK.
    #[error("{0}")]
    NoAck(String),
    /// The hidraw node went away (unit reset, switched modes or unplugged).
    #[error("{0}")]
    DeviceGone(String),
    /// A firmware update step failed (the installer retries the whole sequence on these).
    #[error("{0}")]
    Update(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub(crate) fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    /// True for a timeout or a refused request: the parameter is simply not available.
    pub fn is_unanswered(&self) -> bool {
        matches!(self, Error::Timeout(_) | Error::NoAck(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.0))
}
