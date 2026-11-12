//! Print the cached guide as Markdown, chapter by chapter (to check the conversion).
fn main() {
    let g = micdock_core::guide::cached().expect("no cached guide");
    for ((_, title), html) in micdock_core::guide::chapters(&g.html, &g.toc) {
        let app = micdock_core::guide::is_app_section(&title, &html);
        println!("=== {title}{}", if app { "  [app: left out]" } else { "" });
        println!("{}", micdock_core::guide::to_markdown(&html).0);
    }
}
