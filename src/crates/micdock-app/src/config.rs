// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! User settings in `~/.config/micdock/config.json` — the same file (and keys) as the Python app, so both
//! share the export folder, format and the TX clock correction. Unknown keys are kept as they are.

use serde_json::{Map, Value};

/// The settings; `save` is false in demo mode (made-up settings, never written).
pub struct Config(Map<String, Value>, bool);

impl Config {
    pub fn load() -> Config {
        let mut m: Map<String, Value> = std::fs::read_to_string(micdock_core::paths::config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        for (k, v) in [
            ("export_dir", Value::from(format!("{home}/go2-recordings"))),
            ("format", Value::from("wav")),
            ("mp3_preset", Value::from(1)),
            ("join", Value::from(true)),
            ("legacy_offset", Value::Null),
            ("warn_storage", Value::from(true)),
        ] {
            m.entry(k).or_insert(v);
        }
        Config(m, true)
    }

    /// Demo mode: the defaults, with a home folder that doesn't show the user's name; never saved.
    pub fn demo() -> Config {
        let mut c = Config(Map::new(), false);
        for (k, v) in [
            ("export_dir", Value::from("~/go2-recordings")),
            ("format", Value::from("wav")),
            ("mp3_preset", Value::from(1)),
            ("join", Value::from(true)),
            ("legacy_offset", Value::Null),
            ("warn_storage", Value::from(true)),
        ] {
            c.0.insert(k.into(), v);
        }
        c
    }

    pub fn save(&self) {
        if !self.1 {
            return;
        }
        let path = micdock_core::paths::config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(&self.0) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn str(&self, k: &str) -> String {
        self.0
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    pub fn bool(&self, k: &str) -> bool {
        self.0.get(k).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn int(&self, k: &str) -> Option<i64> {
        self.0.get(k).and_then(Value::as_i64)
    }

    pub fn set(&mut self, k: &str, v: impl Into<Value>) {
        self.0.insert(k.to_string(), v.into());
        self.save();
    }
}
