// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors
//! Help inside the window: the MicDock manual (bundled, offline) and RØDE's Wireless GO II user guide
//! (downloaded from rode.com once and kept; its chapters about RØDE's own app are left out at display time
//! and replaced by a note pointing to the manual — RØDE's text itself is never changed or shipped).
//!
//! Both are shown one chapter at a time, as Markdown, with a chapter list and previous/next buttons.

use std::path::{Path, PathBuf};

use cosmic::app::Task;
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget::{self, markdown, segmented_button};
use micdock_core::guide;

use crate::util::{local, open_external};
use crate::{job, Message};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doc {
    Manual,
    Guide,
}

pub struct Chapter {
    pub title: String,
    /// Link targets inside this chapter (heading slugs, or the guide's anchor names).
    pub anchors: Vec<String>,
    pub items: Vec<markdown::Item>,
}

struct Book {
    chapters: Vec<Chapter>,
    /// Folder that relative image paths are relative to.
    base: PathBuf,
    current: usize,
}

enum GuideState {
    NotLoaded,
    Loading(String),
    Loaded {
        book: Book,
        fetched: f64,
        note: Option<String>,
    },
    Failed(String),
}

pub struct Help {
    pub open: bool,
    doc: Doc,
    tabs: segmented_button::SingleSelectModel,
    manual: Result<Book, String>,
    guide: GuideState,
    /// A guide download is running (also in the background, before the guide is opened).
    fetching: bool,
}

#[derive(Clone, Debug)]
pub enum HelpMsg {
    Open(Doc),
    Close,
    Tab(segmented_button::Entity),
    Chapter(usize),
    Prev,
    Next,
    Link(String),
    /// Download the guide (again).
    Update,
    /// Download the guide quietly if there is no copy yet (at start).
    Prefetch,
    Progress(String),
    Fetched(Result<(), String>, bool),
}

/// 'Audio settings (receiver)' → 'audio-settings-receiver', like the links in the manual.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// The user's languages, most preferred first, always ending in "en" (LANGUAGE, LC_ALL, LC_MESSAGES, LANG).
fn languages() -> Vec<String> {
    let raw = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    parse_languages(&raw)
}

/// "nl_NL.UTF-8:de" → ["nl_NL", "nl", "de", "en"].
fn parse_languages(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in raw.split(':') {
        let code = item
            .split('.')
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("");
        if code.is_empty() || code == "C" || code == "POSIX" {
            continue;
        }
        for c in [code, code.split('_').next().unwrap_or(code)] {
            if !out.iter().any(|o| o == c) {
                out.push(c.to_string());
            }
        }
    }
    if !out.iter().any(|o| o == "en") {
        out.push("en".into());
    }
    out
}

/// Where the manual is: $MICDOCK_DOCS, the installed copy, or the source checkout next to the binary.
fn docs_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("MICDOCK_DOCS") {
        dirs.push(PathBuf::from(d));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            dirs.push(prefix.join("share/micdock/docs"));
        }
        // source checkout: <checkout>/src/target/release/micdock → <checkout>/docs
        for a in exe.ancestors().skip(1) {
            dirs.push(a.join("docs"));
        }
    }
    dirs.push("/usr/share/micdock/docs".into());
    dirs
}

fn find_manual() -> Option<PathBuf> {
    let langs = languages();
    docs_dirs().into_iter().find_map(|d| {
        langs
            .iter()
            .map(|l| d.join(l).join("manual.md"))
            .find(|p| p.is_file())
    })
}

/// Cut a Markdown text into chapters at its `## ` headings (the text before the first one becomes the first
/// chapter, titled after the `# ` heading).
fn split_markdown(text: &str) -> Vec<(String, Vec<String>, String)> {
    let mut out: Vec<(String, Vec<String>, String)> = Vec::new();
    let mut fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        let heading = (!fence)
            .then(|| {
                line.strip_prefix("## ")
                    .or_else(|| line.strip_prefix("# "))
                    .map(str::trim)
            })
            .flatten();
        let level2 = !fence && line.starts_with("## ");
        if level2 || out.is_empty() {
            let title = heading.unwrap_or("").to_string();
            out.push((title, Vec::new(), String::new()));
        }
        let ch = out.last_mut().unwrap();
        if !fence {
            if let Some(h) = line
                .trim_start_matches('#')
                .strip_prefix(' ')
                .filter(|_| line.starts_with('#'))
            {
                ch.1.push(slug(h));
            }
        }
        ch.2.push_str(line);
        ch.2.push('\n');
    }
    out
}

fn load_manual() -> Result<Book, String> {
    load_manual_at(&find_manual().ok_or("The MicDock manual is not installed.")?)
}

fn load_manual_at(path: &Path) -> Result<Book, String> {
    let path = path.to_path_buf();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let chapters = split_markdown(&text)
        .into_iter()
        .map(|(title, anchors, md)| Chapter {
            title,
            anchors,
            items: markdown::parse(&md).collect(),
        })
        .collect();
    Ok(Book {
        chapters,
        base: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        current: 0,
    })
}

/// Shown where RØDE's chapters about its own app are left out (our own text).
fn app_note() -> String {
    let links = [
        ("on-board-recording", "On-board recording"),
        ("getting-your-recordings", "Getting your recordings"),
        ("device", "Names, clocks and firmware"),
        (
            "what-micdock-doesn-t-do-yet",
            "What MicDock doesn't do (yet)",
        ),
    ];
    let mut md = String::from(
        "## Using the Wireless GO II with MicDock\n\nRØDE's guide continues here with chapters about RØDE's own app. \
         In MicDock, the same tasks are explained in the MicDock manual:\n\n",
    );
    for (s, t) in links {
        md.push_str(&format!("- [{t}](micdock:manual#{s})\n"));
    }
    md.push_str(&format!(
        "\nRØDE's complete guide: [rode.com]({})\n",
        guide::GUIDE_URL
    ));
    md
}

/// The cached guide as a book, or None when there is no copy yet.
fn load_guide() -> Option<(Book, f64)> {
    let g = guide::cached()?;
    let mut chapters = Vec::new();
    let mut noted = false;
    for ((_, title), html) in guide::chapters(&g.html, &g.toc) {
        if guide::is_app_section(&title, &html) {
            if !noted {
                noted = true;
                chapters.push(Chapter {
                    title: "Using the Wireless GO II with MicDock".into(),
                    anchors: Vec::new(),
                    items: markdown::parse(&app_note()).collect(),
                });
            }
            continue;
        }
        let (md, anchors) = guide::to_markdown(&html);
        chapters.push(Chapter {
            title,
            anchors,
            items: markdown::parse(&md).collect(),
        });
    }
    Some((
        Book {
            chapters,
            base: g.dir,
            current: 0,
        },
        g.fetched,
    ))
}

/// Renders Markdown with images from disk and links handled by the help page.
struct Viewer<'b> {
    base: &'b Path,
}

impl<'a, 'b: 'a> markdown::Viewer<'a, Message, cosmic::Theme, cosmic::Renderer> for Viewer<'b> {
    fn on_link_click(url: markdown::Uri) -> Message {
        Message::Help(HelpMsg::Link(url))
    }

    fn image(
        &self,
        _settings: markdown::Settings,
        url: &'a markdown::Uri,
        _title: &'a str,
        _alt: &markdown::Text,
    ) -> Element<'a, Message> {
        let path = self.base.join(url.replace("%20", " "));
        widget::container(
            widget::image(widget::image::Handle::from_path(path)).width(Length::Shrink),
        )
        .center_x(Length::Fill)
        .max_width(720)
        .into()
    }
}

impl Default for Help {
    fn default() -> Self {
        let tabs = segmented_button::ModelBuilder::default()
            .insert(|b| b.text("MicDock manual").data(Doc::Manual).activate())
            .insert(|b| b.text("Wireless GO II user guide").data(Doc::Guide))
            .build();
        Help {
            open: false,
            doc: Doc::Manual,
            tabs,
            manual: load_manual(),
            guide: GuideState::NotLoaded,
            fetching: false,
        }
    }
}

impl Help {
    fn book(&self) -> Option<&Book> {
        match self.doc {
            Doc::Manual => self.manual.as_ref().ok(),
            Doc::Guide => match &self.guide {
                GuideState::Loaded { book, .. } => Some(book),
                _ => None,
            },
        }
    }

    fn book_mut(&mut self) -> Option<&mut Book> {
        match self.doc {
            Doc::Manual => self.manual.as_mut().ok(),
            Doc::Guide => match &mut self.guide {
                GuideState::Loaded { book, .. } => Some(book),
                _ => None,
            },
        }
    }

    fn show(&mut self, doc: Doc) -> Task<Message> {
        self.open = true;
        self.doc = doc;
        let pos = if doc == Doc::Manual { 0 } else { 1 };
        self.tabs.activate_position(pos);
        if doc == Doc::Guide {
            if let GuideState::NotLoaded | GuideState::Failed(_) = self.guide {
                match load_guide() {
                    Some((book, fetched)) => {
                        self.guide = GuideState::Loaded {
                            book,
                            fetched,
                            note: None,
                        }
                    }
                    None => return self.fetch(false),
                }
            }
        }
        Task::none()
    }

    fn fetch(&mut self, quiet: bool) -> Task<Message> {
        if self.fetching {
            return Task::none();
        }
        self.fetching = true;
        if !quiet && !matches!(self.guide, GuideState::Loaded { .. }) {
            self.guide = GuideState::Loading("Downloading the user guide from rode.com…".into());
        }
        job(move |send| {
            // the guide downloads images on several threads: pass progress through a channel
            let (tx, rx) = std::sync::mpsc::channel::<(usize, usize)>();
            let r: Result<(), String> = std::thread::scope(|s| {
                let worker = s.spawn(move || {
                    guide::fetch(&|step| {
                        if let guide::FetchStep::Images(a, b) = step {
                            let _ = tx.send((a, b));
                        }
                    })
                });
                for (a, b) in rx {
                    send(HelpMsg::Progress(format!(
                        "Downloading the user guide from rode.com… images {a}/{b}"
                    )));
                }
                match worker.join() {
                    Ok(r) => r.map(|_| ()).map_err(|e| e.to_string()),
                    Err(_) => Err("the download stopped".into()),
                }
            });
            HelpMsg::Fetched(r, quiet)
        })
    }

    /// Go to the chapter holding this anchor in the given document.
    fn goto(&mut self, doc: Doc, anchor: &str) -> Task<Message> {
        let task = self.show(doc);
        if let Some(b) = self.book_mut() {
            if let Some(i) = b
                .chapters
                .iter()
                .position(|c| c.anchors.iter().any(|a| a == anchor))
            {
                b.current = i;
            }
        }
        task
    }

    /// Returns the task and whether the page should scroll back to the top.
    pub fn update(&mut self, msg: HelpMsg) -> (Task<Message>, bool) {
        let task = match msg {
            HelpMsg::Open(doc) => self.show(doc),
            HelpMsg::Close => {
                self.open = false;
                Task::none()
            }
            HelpMsg::Tab(id) => {
                let doc = self.tabs.data::<Doc>(id).copied().unwrap_or(Doc::Manual);
                self.show(doc)
            }
            HelpMsg::Chapter(i) => {
                if let Some(b) = self.book_mut() {
                    b.current = i.min(b.chapters.len().saturating_sub(1));
                }
                Task::none()
            }
            HelpMsg::Prev | HelpMsg::Next => {
                let next = matches!(msg, HelpMsg::Next);
                if let Some(b) = self.book_mut() {
                    b.current = if next {
                        (b.current + 1).min(b.chapters.len().saturating_sub(1))
                    } else {
                        b.current.saturating_sub(1)
                    };
                }
                Task::none()
            }
            HelpMsg::Link(url) => {
                if let Some(anchor) = url.strip_prefix("micdock:manual#") {
                    self.goto(Doc::Manual, anchor)
                } else if let Some(anchor) = url.strip_prefix('#') {
                    self.goto(self.doc, anchor)
                } else if url.starts_with("http://") || url.starts_with("https://") {
                    open_external(&url);
                    return (Task::none(), false);
                } else {
                    return (Task::none(), false);
                }
            }
            HelpMsg::Update => self.fetch(false),
            HelpMsg::Prefetch => {
                if guide::cached().is_none() {
                    self.fetch(true)
                } else {
                    Task::none()
                }
            }
            HelpMsg::Progress(text) => {
                if let GuideState::Loading(t) = &mut self.guide {
                    *t = text;
                }
                return (Task::none(), false);
            }
            HelpMsg::Fetched(r, quiet) => {
                self.fetching = false;
                let current = match &self.guide {
                    GuideState::Loaded { book, .. } => book.current,
                    _ => 0,
                };
                let shown = !quiet
                    || matches!(
                        self.guide,
                        GuideState::Loaded { .. } | GuideState::Loading(_)
                    );
                if shown {
                    let err = r.err();
                    self.guide = match load_guide() {
                        Some((mut book, fetched)) => {
                            book.current = current.min(book.chapters.len().saturating_sub(1));
                            GuideState::Loaded {
                                book,
                                fetched,
                                note: err.map(|e| format!("Could not update ({e}).")),
                            }
                        }
                        None => GuideState::Failed(err.unwrap_or_else(|| "unknown error".into())),
                    };
                }
                return (Task::none(), false);
            }
        };
        (task, true)
    }

    pub fn view(&self) -> Element<'_, Message> {
        let space = cosmic::theme::spacing();
        let m = |h: HelpMsg| Message::Help(h);

        let mut top = widget::row::with_capacity(4)
            .spacing(space.space_s)
            .align_y(Alignment::Center)
            .push(
                widget::button::text("Back")
                    .leading_icon(widget::icon::from_name("go-previous-symbolic"))
                    .on_press(m(HelpMsg::Close)),
            )
            .push(
                widget::segmented_control::horizontal(&self.tabs)
                    .on_activate(|id| Message::Help(HelpMsg::Tab(id))),
            );
        if self.doc == Doc::Guide {
            top = top.push(
                widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
                    .tooltip("Download the latest version from rode.com")
                    .on_press_maybe((!self.fetching).then_some(m(HelpMsg::Update))),
            );
            top = top.push(
                widget::button::icon(widget::icon::from_name("web-browser-symbolic"))
                    .tooltip("Open RØDE's complete guide on rode.com")
                    .on_press(m(HelpMsg::Link(guide::GUIDE_URL.into()))),
            );
        }
        let mut col = widget::column::with_capacity(6)
            .spacing(space.space_m)
            .push(top);

        let Some(book) = self.book() else {
            let text = match (self.doc, &self.manual, &self.guide) {
                (Doc::Manual, Err(e), _) => e.clone(),
                (_, _, GuideState::Loading(t)) => t.clone(),
                (_, _, GuideState::Failed(e)) => format!(
                    "The user guide could not be loaded: {e}\n\nIt is downloaded from rode.com the first time you open \
                     it and kept for offline use. Check your internet connection and press the update button."
                ),
                _ => "Loading…".into(),
            };
            return col.push(widget::text::body(text)).into();
        };

        // chapter list and the chapter itself
        let titles: Vec<String> = book
            .chapters
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {}", i + 1, c.title))
            .collect();
        col = col.push(
            widget::dropdown(titles, Some(book.current), |i| {
                Message::Help(HelpMsg::Chapter(i))
            })
            .width(Length::Fill),
        );
        let Some(ch) = book.chapters.get(book.current) else {
            return col.into();
        };
        let t = cosmic::theme::active();
        let c = t.cosmic();
        let style = markdown::Style {
            font: cosmic::font::default(),
            inline_code_highlight: markdown::Highlight {
                background: cosmic::iced::Color::from(c.background(false).component.base).into(),
                border: cosmic::iced::border::rounded(4),
            },
            inline_code_padding: cosmic::iced::Padding::from([0, 2]),
            inline_code_color: c.background(false).component.on.into(),
            inline_code_font: cosmic::font::mono(),
            code_block_font: cosmic::font::mono(),
            link_color: c.accent_text_color().into(),
        };
        let settings = markdown::Settings::with_text_size(15, style);
        col = col.push(markdown::view_with(
            ch.items.iter(),
            settings,
            &Viewer { base: &book.base },
        ));

        // previous / next
        let mut nav = widget::row::with_capacity(3)
            .spacing(space.space_s)
            .align_y(Alignment::Center);
        if book.current > 0 {
            nav = nav.push(
                widget::button::text(book.chapters[book.current - 1].title.clone())
                    .leading_icon(widget::icon::from_name("go-previous-symbolic"))
                    .on_press(m(HelpMsg::Prev)),
            );
        }
        nav = nav.push(widget::Space::new().width(Length::Fill));
        if book.current + 1 < book.chapters.len() {
            nav = nav.push(
                widget::button::text(book.chapters[book.current + 1].title.clone())
                    .trailing_icon(widget::icon::from_name("go-next-symbolic"))
                    .on_press(m(HelpMsg::Next)),
            );
        }
        col = col.push(nav);
        if let GuideState::Loaded { fetched, note, .. } = &self.guide {
            if self.doc == Doc::Guide {
                let mut s = note.clone().map(|n| n + " ").unwrap_or_default();
                s.push_str(&format!(
                    "Saved copy of RØDE's online guide from {}. Chapters about RØDE's own app are left out.",
                    local(*fetched, "%Y-%m-%d %H:%M")
                ));
                col = col.push(widget::text::caption(s));
            }
        }
        col.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_lists() {
        assert_eq!(
            parse_languages("nl_NL.UTF-8:de"),
            ["nl_NL", "nl", "de", "en"]
        );
        assert_eq!(parse_languages("C.UTF-8"), ["en"]);
        assert_eq!(parse_languages(""), ["en"]);
        assert_eq!(parse_languages("sr_RS@latin"), ["sr_RS", "sr", "en"]);
    }

    /// The bundled manual is found from the source checkout and split into chapters with link targets.
    #[test]
    fn the_manual_loads() {
        // the manual in this repository (not via the binary's location: the build folder may be elsewhere)
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/en/manual.md");
        let book = load_manual_at(&path).expect("docs/en/manual.md");
        assert!(book.chapters.len() >= 10);
        assert!(book
            .chapters
            .iter()
            .any(|c| c.anchors.iter().any(|a| a == "deleting-recordings")));
        assert!(
            book.base.join("img/recordings.png").is_file(),
            "screenshots next to the manual"
        );
        // every link inside the manual points to a heading that exists
        let text = std::fs::read_to_string(book.base.join("manual.md")).unwrap();
        let anchors: Vec<&String> = book.chapters.iter().flat_map(|c| &c.anchors).collect();
        for part in text.split("](#").skip(1) {
            let target = &part[..part.find(')').unwrap()];
            assert!(
                anchors.iter().any(|a| *a == target),
                "broken link #{target}"
            );
        }
    }

    #[test]
    fn slugs_and_chapters() {
        assert_eq!(
            slug("What MicDock doesn't do (yet)"),
            "what-micdock-doesn-t-do-yet"
        );
        assert_eq!(slug("The Ø (power) button"), "the-power-button");
        let md = "# Title\n\nIntro\n\n## One\n\ntext\n\n### Sub part\n\n```\n## not a heading\n```\n\n## Two\n";
        let ch = split_markdown(md);
        let titles: Vec<&str> = ch.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(titles, ["Title", "One", "Two"]);
        assert_eq!(ch[1].1, ["one", "sub-part"]);
        assert!(ch[1].2.contains("## not a heading"));
    }
}
