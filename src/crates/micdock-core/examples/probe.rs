// SPDX-License-Identifier: GPL-3.0-or-later
//! Print what `devices::probe_devices()` and `devices::tx_mounts()` see, one line per field
//! (useful to check what MicDock sees).
use micdock_core::devices;

fn opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref()
        .map(|x| x.to_string())
        .unwrap_or_else(|| "None".into())
}

fn main() {
    // --light: the frequent poll MicDock uses (no button-mode / power-saver reads)
    let light = std::env::args().any(|a| a == "--light");
    let list = if light {
        devices::probe_devices_light()
    } else {
        devices::probe_devices()
    };
    for d in list {
        let fwv = d
            .fw_version
            .map(|(a, b, c)| format!("({a}, {b}, {c})"))
            .unwrap_or_else(|| "None".into());
        println!("{} node={} pid={:#06x} role={} product={} fw={} fw_v={} storage={} battery={} name={} error={} rec={} button={} channel={} clock_set={}",
            d.serial, d.node, d.pid, d.role.as_str(), d.product, d.fw, fwv, d.storage, opt(&d.battery), opt(&d.name),
            opt(&d.error), d.rec.map(|m| m.as_str()).unwrap_or("None"), d.button.map(|m| m.as_str()).unwrap_or("None"),
            d.channel.map(|m| m.as_str()).unwrap_or("None"), d.clock.is_some());
        // extra fields go to stderr, so stdout keeps one line per unit
        eprintln!(
            "{} id={:?} links={:?}",
            d.serial,
            d.id.map(|i| format!("{i:08x}")),
            d.links
        );
    }
    for (serial, (blk, mnt)) in devices::tx_mounts() {
        println!("mount {serial} {blk} {mnt}");
    }
}
