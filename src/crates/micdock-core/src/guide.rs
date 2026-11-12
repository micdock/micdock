// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Offline copy of RØDE's Wireless GO II user guide, for the in-app help viewer.
//!
//! The guide at [`GUIDE_URL`] is rendered on the server: the text sits in the page's
//! `<template #content>` block as a list of "cms-module" divs. We keep only simple, safe markup
//! (headings, paragraphs, lists, emphasis, links, images), turn "Note"/"Pro Tip" modules into callout
//! boxes, download the images, and cache the result in `~/.cache/micdock/guide/` — the same files the
//! Python version reads and writes. Nothing from the page is executed; images only come from RØDE's hosts.
//! The guide is RØDE's content: it is fetched on the user's machine, never shipped with MicDock.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha1::{Digest, Sha1};

use crate::{paths, Error, Result};

pub const GUIDE_URL: &str = "https://rode.com/en-mt/user-guides/wireless-go-ii";
pub const IMG_HOSTS: [&str; 3] = ["edge.rode.com", "cdn.rode.com", "rode.com"];
/// Bump when the cleaned-up HTML changes shape, to force a rebuild (shared with the Python version).
pub const FORMAT: i64 = 3;

pub fn cache_dir() -> PathBuf {
    paths::cache_dir().join("guide")
}

/// A table-of-contents entry: (anchor, title).
pub type TocEntry = (String, String);

// --------------------------------------------------------------------------
// a small HTML tokenizer with the behaviour of Python's html.parser (what the guide format is defined by)
// --------------------------------------------------------------------------
enum Token {
    Start(String, Vec<(String, Option<String>)>, bool), // name, attributes, self-closing
    End(String),
    Text(String),
}

fn unescape(s: &str) -> String {
    htmlize::unescape(s).into_owned()
}

fn tokenize(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    let flush = |out: &mut Vec<Token>, from: usize, to: usize| {
        if to > from {
            out.push(Token::Text(unescape(&src[from..to])));
        }
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &src[i..];
        // comments, doctype, processing instructions
        if rest.starts_with("<!--") {
            flush(&mut out, text_start, i);
            i += rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            text_start = i;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            flush(&mut out, text_start, i);
            i += rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            text_start = i;
            continue;
        }
        let closing = rest.starts_with("</");
        let name_at = if closing { 2 } else { 1 };
        if !rest[name_at..].starts_with(|c: char| c.is_ascii_alphabetic()) {
            i += 1; // a lone '<' is text
            continue;
        }
        let Some(end) = tag_end(rest) else {
            i += 1;
            continue;
        };
        flush(&mut out, text_start, i);
        let inner = &rest[name_at..end];
        let name_len = inner
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(inner.len());
        let name = inner[..name_len].to_ascii_lowercase();
        i += end + 1;
        if closing {
            out.push(Token::End(name));
        } else {
            let self_closing = inner.trim_end().ends_with('/');
            let attrs = parse_attrs(&inner[name_len..]);
            let raw = name == "script" || name == "style";
            out.push(Token::Start(name.clone(), attrs, self_closing));
            if raw && !self_closing {
                // CDATA content: everything up to the matching end tag is text
                let close = format!("</{name}");
                let lower = src[i..].to_ascii_lowercase();
                let stop = lower.find(&close).map(|p| i + p).unwrap_or(src.len());
                if stop > i {
                    out.push(Token::Text(src[i..stop].to_string()));
                }
                i = stop;
            }
        }
        text_start = i;
    }
    flush(&mut out, text_start, b.len());
    out
}

/// Index of the '>' that ends the tag starting at rest[0] ('<'), respecting quoted attribute values.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (k, &c) in rest.as_bytes().iter().enumerate().skip(1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Some(k),
            None => {}
        }
    }
    None
}

fn parse_attrs(s: &str) -> Vec<(String, Option<String>)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && b[i] != b'='
            && b[i] != b'>'
            && b[i] != b'/'
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let name = s[start..i].to_ascii_lowercase();
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let (val, next) = if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                let e = s[j + 1..]
                    .find(q as char)
                    .map(|p| j + 1 + p)
                    .unwrap_or(s.len());
                (&s[j + 1..e], (e + 1).min(s.len()))
            } else {
                let e = s[j..]
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .map(|p| j + p)
                    .unwrap_or(s.len());
                (&s[j..e], e)
            };
            out.push((name, Some(unescape(val))));
            i = next;
        } else {
            out.push((name, None));
        }
    }
    out
}

/// Python's `html.escape(s)`.
fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#x27;"),
            c => o.push(c),
        }
    }
    o
}

/// Python's `urllib.parse.quote(s, safe=":/%?=&#~+,;@!$'()*[]")`.
fn quote(s: &str) -> String {
    const SAFE: &[u8] = b":/%?=&#~+,;@!$'()*[]_.-";
    let mut o = String::new();
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || SAFE.contains(&c) {
            o.push(c as char);
        } else {
            o.push_str(&format!("%{c:02X}"));
        }
    }
    o
}

/// `urljoin(GUIDE_URL, src)` for the forms the guide uses.
fn join_url(src: &str) -> String {
    if src.starts_with("http://") || src.starts_with("https://") {
        src.to_string()
    } else if let Some(r) = src.strip_prefix("//") {
        format!("https://{r}")
    } else if src.starts_with('/') {
        format!("https://rode.com{src}")
    } else if src.is_empty() {
        GUIDE_URL.to_string()
    } else {
        format!("{}/{src}", &GUIDE_URL[..GUIDE_URL.rfind('/').unwrap()])
    }
}

fn url_host(u: &str) -> String {
    let after = u.split_once("://").map(|(_, r)| r).unwrap_or("");
    let host = after.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    host.split(':').next().unwrap_or("").to_ascii_lowercase()
}

fn url_path(u: &str) -> &str {
    let after = u.split_once("://").map(|(_, r)| r).unwrap_or(u);
    let path = after.find('/').map(|p| &after[p..]).unwrap_or("");
    path.split(['?', '#']).next().unwrap_or("")
}

// --------------------------------------------------------------------------
// the sanitiser (port of the Python _Clean)
// --------------------------------------------------------------------------
fn heading_map(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "h1" | "h2" | "h3" => "h2",
        "h4" => "h3",
        "h5" | "h6" => "h4",
        _ => return None,
    })
}

fn inline_map(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "b" | "strong" => "b",
        "i" | "em" => "i",
        "u" => "u",
        _ => return None,
    })
}

const BLOCK: [&str; 4] = ["p", "ul", "ol", "li"];
const SKIP: [&str; 8] = [
    "script", "style", "svg", "button", "noscript", "iframe", "template", "form",
];

#[derive(Default)]
struct Clean {
    out: String,
    toc: Vec<TocEntry>,
    images: BTreeMap<String, String>,
    divs: Vec<bool>,
    skip: i32,
    callout: i32,
    heading: Option<(String, String, String, bool)>, // tag, anchor, text, in callout
    spans: Vec<bool>,
    n: u32,
    pending: Vec<String>,
}

impl Clean {
    fn open(&mut self, markup: &str) {
        self.out.push_str(markup);
        for name in std::mem::take(&mut self.pending) {
            self.out
                .push_str(&format!("<a name=\"{}\"></a>", escape(&name)));
        }
    }

    fn start(&mut self, tag: &str, attrs: &[(String, Option<String>)]) {
        let get = |k: &str| {
            attrs
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .and_then(|(_, v)| v.clone())
        };
        if self.skip > 0 || SKIP.contains(&tag) || tag.starts_with("rode-") {
            if tag != "br" && tag != "img" && !tag.starts_with("rode-") {
                self.skip += 1;
            }
            return;
        }
        let cls = get("class").unwrap_or_default();
        if tag == "div" {
            let boxed =
                cls.contains("module-row") && (cls.contains("pro-tip") || cls.contains(" note"));
            self.divs.push(boxed);
            if boxed {
                self.callout += 1;
                self.out.push_str(
                    "<table width=\"100%\" cellpadding=\"14\" bgcolor=\"#1c1c1c\" style=\"margin: 10px 0 14px 0;\"><tr><td>",
                );
            }
        } else if tag == "section" && get("id").is_some_and(|v| !v.is_empty()) {
            self.pending.push(get("id").unwrap());
        } else if let Some(h) = heading_map(tag) {
            self.n += 1;
            let anchor = format!("h{}", self.n);
            self.heading = Some((
                tag.to_string(),
                anchor.clone(),
                String::new(),
                self.callout > 0,
            ));
            self.pending.push(anchor);
            self.open(&format!("<{h}>"));
        } else if BLOCK.contains(&tag) {
            self.open(&format!("<{tag}>"));
        } else if let Some(t) = inline_map(tag) {
            self.out.push_str(&format!("<{t}>"));
        } else if tag == "br" {
            self.out.push_str("<br>");
        } else if tag == "span" {
            let point = cls.contains("feature-map-point");
            self.spans.push(point);
            if point {
                self.out.push_str("<b style=\"color:#f2c27a;\">");
            }
        } else if tag == "a" {
            let href = get("href").unwrap_or_default().trim().to_string();
            if href.starts_with('#') {
                self.out
                    .push_str(&format!("<a href=\"{}\">", escape(&href)));
            } else if href.starts_with(&format!("{GUIDE_URL}#")) {
                self.out.push_str(&format!(
                    "<a href=\"{}\">",
                    escape(&href[GUIDE_URL.len()..])
                ));
            } else if href.starts_with("http://") || href.starts_with("https://") {
                self.out
                    .push_str(&format!("<a href=\"{}\">", escape(&href)));
            } else {
                self.out.push_str("<a>");
            }
        } else if tag == "img" {
            let src = quote(&join_url(get("src").unwrap_or_default().trim()));
            let host = url_host(&src);
            if !src.starts_with("https://")
                || !IMG_HOSTS.contains(&host.as_str())
                || src.contains("/logo/")
            {
                return;
            }
            let path = url_path(&src);
            let file = path.rsplit('/').next().unwrap_or("");
            let ext = match file.rfind('.') {
                Some(p) if p > 0 => file[p..].to_lowercase(),
                _ => String::new(),
            };
            let ext = if [".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg"].contains(&ext.as_str())
            {
                ext
            } else {
                ".img".into()
            };
            let digest: String = Sha1::digest(src.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let name = format!("{}{ext}", &digest[..16]);
            self.images.insert(src.clone(), name.clone());
            self.open("<p align=\"center\">");
            self.out.push_str(&format!(
                "<img src=\"img/{name}\" alt=\"{}\"></p>",
                escape(&get("alt").unwrap_or_default())
            ));
        }
    }

    fn end(&mut self, tag: &str) {
        if self.skip > 0 {
            if SKIP.contains(&tag) || (tag != "br" && tag != "img" && !tag.starts_with("rode-")) {
                self.skip -= 1;
            }
            return;
        }
        if tag == "div" {
            if self.divs.pop() == Some(true) {
                self.callout -= 1;
                self.out.push_str("</td></tr></table>");
            }
        } else if let (Some(h), true) = (heading_map(tag), self.heading.is_some()) {
            let (htag, anchor, text, boxed) = self.heading.take().unwrap();
            let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !title.is_empty() && !boxed && (htag == "h3" || htag == "h4") {
                self.toc.push((anchor, title));
            }
            self.out.push_str(&format!("</{h}>"));
        } else if BLOCK.contains(&tag) {
            self.out.push_str(&format!("</{tag}>"));
        } else if let Some(t) = inline_map(tag) {
            self.out.push_str(&format!("</{t}>"));
        } else if tag == "span" {
            if self.spans.pop() == Some(true) {
                self.out.push_str("</b>&nbsp; ");
            }
        } else if tag == "a" {
            self.out.push_str("</a>");
        }
    }

    fn text(&mut self, data: &str) {
        if self.skip > 0 {
            return;
        }
        if let Some(h) = self.heading.as_mut() {
            h.2.push_str(data);
        }
        self.out.push_str(&escape(data));
    }

    fn html(&self) -> String {
        let blk = r"(?:p|h[1-6]|ul|ol|li|table|tr|td)";
        let s = regex::Regex::new(r"<p>\s*(<br>\s*)*</p>")
            .unwrap()
            .replace_all(&self.out, "");
        let s = regex::Regex::new(r"(?s)<a>(.*?)</a>")
            .unwrap()
            .replace_all(&s, "$1");
        let s = regex::Regex::new(&format!(r"\s+(</?{blk}\b)"))
            .unwrap()
            .replace_all(&s, "$1");
        let s = regex::Regex::new(&format!(r"(</?{blk}\b[^>]*>)\s+"))
            .unwrap()
            .replace_all(&s, "$1");
        s.into_owned()
    }
}

/// (clean html, table of contents, {image url: local file name}) from the guide page's HTML.
pub fn extract(page: &str) -> Result<(String, Vec<TocEntry>, BTreeMap<String, String>)> {
    const OPEN: &str = "<template #content>";
    let i = page
        .find(OPEN)
        .ok_or_else(|| Error::other("unexpected page layout (no content block)"))?;
    let from = i + OPEN.len();
    let to = page[i..]
        .find("</template>")
        .map(|j| i + j)
        .unwrap_or(page.len());
    let mut c = Clean::default();
    for t in tokenize(&page[from..to.max(from)]) {
        match t {
            Token::Start(name, attrs, self_closing) => {
                c.start(&name, &attrs);
                if self_closing {
                    c.end(&name);
                }
            }
            Token::End(name) => c.end(&name),
            Token::Text(s) => c.text(&s),
        }
    }
    if c.toc.is_empty() {
        return Err(Error::other("unexpected page layout (no headings)"));
    }
    Ok((c.html(), c.toc, c.images))
}

// --------------------------------------------------------------------------
// the cache
// --------------------------------------------------------------------------

/// The cached guide.
#[derive(Debug, Clone)]
pub struct Guide {
    pub html: String,
    pub toc: Vec<TocEntry>,
    /// When it was downloaded (unix seconds).
    pub fetched: f64,
    pub dir: PathBuf,
}

/// The cached guide, or None (missing, unreadable, or written by an older format).
pub fn cached() -> Option<Guide> {
    let dir = cache_dir();
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("guide.json")).ok()?).ok()?;
    let html = std::fs::read_to_string(dir.join("guide.html")).ok()?;
    if meta["format"].as_i64() != Some(FORMAT) {
        return None;
    }
    let toc = meta["toc"]
        .as_array()?
        .iter()
        .filter_map(|e| Some((e[0].as_str()?.to_string(), e[1].as_str()?.to_string())))
        .collect();
    Some(Guide {
        html,
        toc,
        fetched: meta["fetched"].as_f64()?,
        dir,
    })
}

fn http_get(url: &str, timeout: u64) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout))
        .user_agent(paths::CLIENT_NAME)
        .build();
    let mut body = Vec::new();
    agent
        .get(url)
        .call()
        .map_err(|e| Error::other(e.to_string()))?
        .into_reader()
        .read_to_end(&mut body)?;
    Ok(body)
}

/// Progress messages from [`fetch`].
#[derive(Debug, Clone)]
pub enum FetchStep {
    Page,
    /// Images downloaded so far, of how many.
    Images(usize, usize),
}

/// Download the guide and its images, store them in the cache and return it.
pub fn fetch(progress: &(dyn Fn(FetchStep) + Sync)) -> Result<Guide> {
    progress(FetchStep::Page);
    let page = String::from_utf8_lossy(&http_get(GUIDE_URL, 30)?).into_owned();
    let (body, toc, images) = extract(&page)?;
    let dir = cache_dir();
    let img_dir = dir.join("img");
    std::fs::create_dir_all(&img_dir)?;
    let todo: Vec<(String, String)> = images
        .into_iter()
        .filter(|(_, n)| !img_dir.join(n).exists())
        .collect();
    let total = todo.len();
    let queue = Mutex::new(todo);
    let done = Mutex::new(0usize);
    std::thread::scope(|s| {
        for _ in 0..6 {
            s.spawn(|| loop {
                let Some((url, name)) = queue.lock().unwrap().pop() else {
                    break;
                };
                // a missing picture should not stop the guide
                let Ok(data) = http_get(&url, 60) else {
                    continue;
                };
                let part = img_dir.join(format!("{name}.part"));
                if std::fs::write(&part, data).is_ok()
                    && std::fs::rename(&part, img_dir.join(&name)).is_ok()
                {
                    let mut d = done.lock().unwrap();
                    *d += 1;
                    progress(FetchStep::Images(*d, total));
                }
            });
        }
    });
    let part = dir.join("guide.html.part");
    std::fs::write(&part, &body)?;
    std::fs::rename(&part, dir.join("guide.html"))?;
    let fetched = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let meta = serde_json::json!({
        "format": FORMAT, "url": GUIDE_URL, "fetched": fetched,
        "toc": toc.iter().map(|(a, t)| serde_json::json!([a, t])).collect::<Vec<_>>(),
    });
    std::fs::write(
        dir.join("guide.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )?;
    cached().ok_or_else(|| Error::other("the guide could not be stored"))
}

// --------------------------------------------------------------------------
// display: leave out the chapters about RØDE's own app
// --------------------------------------------------------------------------
pub const APP_MENTIONS: [&str; 2] = ["RØDE Central", "RODE Central"];

/// Whether a chapter is about RØDE's own app: its title names it, or its text mentions it at least twice
/// (the hardware chapters mention it once at most).
pub fn is_app_section(title: &str, html: &str) -> bool {
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = tags.replace_all(&unescape(html), " ").into_owned();
    let mentions: usize = APP_MENTIONS.iter().map(|m| text.matches(m).count()).sum();
    APP_MENTIONS.iter().any(|m| title.contains(m)) || mentions >= 2
}

/// The guide cut into its chapters (one per contents entry): (entry, the chapter's HTML).
/// Text before the first chapter is left out (the page has none).
pub fn chapters(body: &str, toc: &[TocEntry]) -> Vec<(TocEntry, String)> {
    let mut starts: Vec<(usize, &TocEntry)> = toc
        .iter()
        .filter_map(|e| {
            let pos = body.find(&format!("<a name=\"{}\"></a>", e.0))?;
            Some((body[..pos].rfind("<h").unwrap_or(0), e))
        })
        .collect();
    starts.sort_by_key(|(s, _)| *s);
    starts
        .iter()
        .enumerate()
        .map(|(k, (start, e))| {
            let end = starts.get(k + 1).map(|(s, _)| *s).unwrap_or(body.len());
            ((*e).clone(), body[*start..end].to_string())
        })
        .collect()
}

/// Escape text so Markdown shows it as it is.
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '[' | ']' | '`' | '<' | '>' | '#' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Turn the cleaned guide HTML (see [`extract`]: headings, paragraphs, bold/italic, links, images, lists and
/// the boxed notes, which are tables) into Markdown for a Markdown viewer. Returns (markdown, anchor names
/// found), so links to `#anchor` can be followed.
pub fn to_markdown(html: &str) -> (String, Vec<String>) {
    let mut out = String::new(); // finished blocks
    let mut boxed: Option<String> = None; // a note box being collected (shown as a quote)
    let mut line = String::new(); // the block being written
    let mut anchors = Vec::new();
    let mut links: Vec<Option<String>> = Vec::new();
    let mut list_depth = 0usize;
    fn flush(line: &mut String, out: &mut String, boxed: &mut Option<String>) {
        let t = line.trim();
        if !t.is_empty() {
            let dst = boxed.as_mut().unwrap_or(out);
            dst.push_str(t);
            dst.push_str("\n\n");
        }
        line.clear();
    }
    let spaces = regex::Regex::new(" {2,}").unwrap();
    for t in tokenize(html) {
        match t {
            Token::Start(name, attrs, _) => {
                let attr = |k: &str| {
                    attrs
                        .iter()
                        .find(|(n, _)| n == k)
                        .and_then(|(_, v)| v.clone())
                };
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush(&mut line, &mut out, &mut boxed);
                        let level = name[1..].parse::<usize>().unwrap_or(2).clamp(2, 4);
                        line.push_str(&"#".repeat(level));
                        line.push(' ');
                    }
                    "p" | "div" => flush(&mut line, &mut out, &mut boxed),
                    "br" => line.push_str("  \n"),
                    "b" | "strong" => line.push_str("**"),
                    "i" | "em" => line.push('*'),
                    "ul" | "ol" => {
                        flush(&mut line, &mut out, &mut boxed);
                        list_depth += 1;
                    }
                    "li" => {
                        if !line.trim().is_empty() {
                            line.push('\n');
                        }
                        line.push_str(&"  ".repeat(list_depth.saturating_sub(1)));
                        line.push_str("- ");
                    }
                    "table" => {
                        flush(&mut line, &mut out, &mut boxed);
                        boxed = Some(String::new());
                    }
                    "a" => match (attr("name"), attr("href")) {
                        (Some(n), _) => {
                            anchors.push(n);
                            links.push(None);
                        }
                        (None, Some(h)) => {
                            line.push('[');
                            links.push(Some(h));
                        }
                        _ => links.push(None),
                    },
                    "img" => {
                        flush(&mut line, &mut out, &mut boxed);
                        if let Some(src) = attr("src") {
                            let alt = attr("alt").unwrap_or_default();
                            line.push_str(&format!(
                                "![{}]({})",
                                md_escape(&alt),
                                src.replace(' ', "%20")
                            ));
                            flush(&mut line, &mut out, &mut boxed);
                        }
                    }
                    _ => {}
                }
            }
            Token::End(name) => match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "div" => {
                    flush(&mut line, &mut out, &mut boxed)
                }
                "b" | "strong" => line.push_str("**"),
                "i" | "em" => line.push('*'),
                "ul" | "ol" => {
                    list_depth = list_depth.saturating_sub(1);
                    if list_depth == 0 {
                        flush(&mut line, &mut out, &mut boxed);
                    }
                }
                "a" => {
                    if let Some(Some(h)) = links.pop() {
                        line.push_str(&format!("]({})", h.replace(' ', "%20")));
                    }
                }
                "table" => {
                    flush(&mut line, &mut out, &mut boxed);
                    if let Some(b) = boxed.take() {
                        for l in b.trim_end().lines() {
                            out.push_str(if l.is_empty() { ">" } else { "> " });
                            out.push_str(l);
                            out.push('\n');
                        }
                        out.push('\n');
                    }
                }
                _ => {}
            },
            Token::Text(s) => {
                let s = s.replace(['\n', '\r', '\t'], " ");
                let s = spaces.replace_all(&s, " ").into_owned();
                if line.is_empty()
                    || line.ends_with('\n')
                    || line.ends_with("- ")
                    || line.ends_with("# ")
                {
                    line.push_str(&md_escape(s.trim_start()));
                } else {
                    line.push_str(&md_escape(&s));
                }
            }
        }
    }
    flush(&mut line, &mut out, &mut boxed);
    (out.replace("****", ""), anchors)
}

/// Leave out the guide's chapters about RØDE's own app (display only; nothing is stored).
///
/// A chapter counts as one if its title names RØDE Central or its text mentions it at least twice
/// (the hardware chapters mention it once at most). `note_html` replaces the first chapter left out.
/// Returns (html, toc, titles left out).
pub fn without_app_sections(
    body: &str,
    toc: &[TocEntry],
    note_html: &str,
) -> (String, Vec<TocEntry>, Vec<String>) {
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let mut starts: Vec<(usize, &TocEntry)> = toc
        .iter()
        .filter_map(|e| {
            let pos = body.find(&format!("<a name=\"{}\"></a>", e.0))?;
            Some((body[..pos].rfind("<h").unwrap_or(0), e))
        })
        .collect();
    starts.sort_by_key(|(s, _)| *s);
    let (mut html, mut keep, mut dropped) = (String::new(), Vec::new(), Vec::new());
    let (mut last, mut noted) = (0usize, false);
    for (k, (start, entry)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map(|(s, _)| *s).unwrap_or(body.len());
        let text = tags
            .replace_all(&unescape(&body[*start..end]), " ")
            .into_owned();
        let mentions: usize = APP_MENTIONS.iter().map(|m| text.matches(m).count()).sum();
        if APP_MENTIONS.iter().any(|m| entry.1.contains(m)) || mentions >= 2 {
            html.push_str(&body[last..*start]);
            if !noted {
                html.push_str(note_html);
                noted = true;
            }
            last = end;
            dropped.push(entry.1.clone());
        } else {
            keep.push((*entry).clone());
        }
    }
    html.push_str(&body[last..]);
    (html, keep, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><template #content>
      <div class='cms-module'><section id="section-overview" class="anchor-hash"></section>
        <div class="rode-text "><h3><b>Wireless GO II</b></h3>
        <p>The &amp; <i>dual</i> channel&nbsp;receiver.</p><p><br></p>
        <img class="img-fluid" src="https://edge.rode.com/images/a (2).jpg" alt="Receiver &quot;front&quot;">
        <rode-light-box img="" alt="x"></rode-light-box>
        <script>var x = "<h4>not a heading</h4>";</script></div></div>
      <div class="module-row module-generic-paragraph note container"><h4>Note</h4><p>Boxed.</p></div>
      <h4>RØDE Central</h4><p>RØDE Central here, RØDE Central there.</p>
      <h4>Charging</h4><p><a href="/x">rel</a> <a href="https://rode.com/en-mt/user-guides/wireless-go-ii#section-x">in</a></p>
    </template></html>"#;

    #[test]
    fn markdown_from_the_cleaned_page() {
        let (html, toc, _) = extract(PAGE).unwrap();
        let ch = chapters(&html, &toc);
        assert_eq!(ch.len(), 3);
        assert!(is_app_section(&ch[1].0 .1, &ch[1].1) && !is_app_section(&ch[0].0 .1, &ch[0].1));
        let (md, anchors) = to_markdown(&ch[0].1);
        assert!(anchors.contains(&"section-overview".to_string()));
        assert!(md.starts_with("## **Wireless GO II**\n\n"), "{md}");
        assert!(md.contains("The & *dual* channel\u{a0}receiver."), "{md}");
        assert!(
            md.contains("![Receiver \"front\"](https://edge.rode.com/images/a%20(2).jpg)")
                || md.contains("![Receiver \"front\"](img/"),
            "{md}"
        );
        assert!(md.contains("> ### Note\n>\n> Boxed."), "{md}");
        let (md, _) = to_markdown(&ch[2].1);
        assert!(md.contains("[in](#section-x)"), "{md}");
    }

    #[test]
    fn extract_keeps_safe_markup_and_builds_the_contents() {
        let (html, toc, images) = extract(PAGE).unwrap();
        let titles: Vec<&str> = toc.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(titles, ["Wireless GO II", "RØDE Central", "Charging"]); // "Note" is inside a callout
        assert!(html.contains("<a name=\"section-overview\"></a><a name=\"h1\"></a>"));
        assert!(html.contains("The &amp; <i>dual</i> channel\u{a0}receiver."));
        assert!(
            !html.contains("<p></p>")
                && !html.contains("not a heading")
                && !html.contains("<script")
        );
        assert!(html.contains("<table width=\"100%\""));
        assert!(html.contains("rel <a href=\"#section-x\">in</a>"));
        let (url, name) = images.iter().next().unwrap();
        assert_eq!(url, "https://edge.rode.com/images/a%20(2).jpg");
        assert!(name.ends_with(".jpg") && name.len() == 20);
        assert!(html.contains("alt=\"Receiver &quot;front&quot;\""));
    }

    #[test]
    fn app_chapters_are_left_out() {
        let (html, toc, _) = extract(PAGE).unwrap();
        let (out, keep, dropped) = without_app_sections(&html, &toc, "<NOTE>");
        assert_eq!(dropped, ["RØDE Central"]);
        assert_eq!(keep.len(), 2);
        assert!(out.contains("<NOTE>") && !out.contains("there."));
    }

    #[test]
    fn url_helpers() {
        assert_eq!(quote("https://a/b c(1).jpg"), "https://a/b%20c(1).jpg");
        assert_eq!(url_host("https://Edge.rode.com:443/x"), "edge.rode.com");
        assert_eq!(url_path("https://a/b/c.JPG?x=1"), "/b/c.JPG");
        assert_eq!(join_url("/images/x.png"), "https://rode.com/images/x.png");
    }
}
