// SPDX-License-Identifier: GPL-3.0-or-later
//! `guide [--refresh]` — like `python3 -m micdock.cli.guide`: fetch (or reuse) the offline guide and print
//! its contents. `guide extract PAGE.html` — print the cleaned HTML, contents and images of a saved page
//! (useful to check the guide clean-up).
use micdock_core::guide;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("extract") {
        let page = std::fs::read(&args[1]).expect("page");
        let (html, toc, images) = guide::extract(&String::from_utf8_lossy(&page)).expect("extract");
        println!("{html}\n--TOC--");
        for (a, t) in toc {
            println!("{a}\t{t}");
        }
        println!("--IMAGES--");
        for (u, n) in images {
            println!("{u}\t{n}");
        }
        return;
    }
    let g = match (args.iter().any(|a| a == "--refresh"), guide::cached()) {
        (false, Some(g)) => g,
        _ => guide::fetch(&|step| eprintln!("{step:?}")).expect("fetch"),
    };
    let when = chrono::DateTime::from_timestamp(g.fetched as i64, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    println!("Guide cached in {} ({when})", g.dir.display());
    for (a, t) in g.toc {
        println!("  #{a:<5} {t}");
    }
}
