//! Markdown rendered to ANSI for paging with `less -R` in a terminal tab.
//! pulldown-cmark parses — CommonMark, plus GitHub's tables, task lists,
//! strikethrough, footnotes, and alerts — and this module draws: text
//! wrapped under its own bullet or quote bar, code boxed and highlighted
//! with a "copy" link, tables laid out to the tab's width, and the HTML
//! that READMEs lean on.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, FontStyle, StyleModifier, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthChar;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

/// Scheme of the hyperlink on a code block's "copy" label; the block's index
/// in [`Rendered::code`] follows. The pane paging the preview acts on it.
pub const COPY_URI: &str = "omnipty-copy:";
const COPY_LABEL: &str = " ⧉ copy ";

pub struct Rendered {
    pub text: String,
    /// Each code block as written, for its "copy" link.
    pub code: Vec<String>,
}

/// Write rendered text under `~/.cache/omnipty` and return its path.
pub fn write_cache(name: &str, rendered: &str) -> Option<PathBuf> {
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    std::fs::write(&path, rendered).ok()?;
    Some(path)
}

/// Render a markdown file into the cache, one cache file per source path so
/// several previews can be open at once. `width` is the pager's columns.
/// Returns the path and the code blocks the preview's "copy" links refer to.
pub fn write_preview(source: &Path, width: usize) -> Option<(PathBuf, Vec<String>)> {
    let md = std::fs::read(source).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    let name = format!("preview-{:016x}.txt", hasher.finish());
    let rendered = render(&String::from_utf8_lossy(&md), width);
    Some((write_cache(&name, &rendered.text)?, rendered.code))
}

pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

pub fn render(md: &str, width: usize) -> Rendered {
    let mut renderer = Renderer {
        width: width.max(20),
        ..Default::default()
    };
    renderer.blocks(md, false);
    Rendered {
        text: renderer.out,
        code: renderer.code,
    }
}

/// What the parser is asked to recognise beyond CommonMark. Not math or
/// smart punctuation: a preview shouldn't restyle `$5` or retype quotes.
fn extensions() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

/// Text from the file, made safe to show: an escape character would be the
/// file driving the terminal, and a tab has no width to wrap by.
fn clean(text: &str) -> String {
    text.replace('\x1b', "␛").replace('\t', "    ")
}

/// A block that holds other blocks — a list item, a quote, a footnote —
/// as what it puts in front of their rows.
struct Frame {
    /// Before its first row: the bullet, the number.
    first: String,
    /// Before the rest, as wide as `first`, so text wraps under text.
    rest: String,
    /// `first` has been drawn.
    used: bool,
    item: bool,
    /// An item whose text is in paragraphs, which stand apart.
    loose: bool,
}

impl Frame {
    fn new(first: String, item: bool) -> Self {
        Self {
            rest: " ".repeat(visible_width(&first)),
            first,
            used: false,
            item,
            loose: false,
        }
    }
}

/// Walks the parser's events, gathering each block's text and drawing it
/// when the block ends.
#[derive(Default)]
struct Renderer {
    width: usize,
    out: String,
    code: Vec<String>,
    /// Open containers, outermost first.
    frames: Vec<Frame>,
    /// A blank row is owed before the next block.
    gap: bool,
    /// The text of the block being gathered, styled.
    text: String,
    /// Styles open around the end of `text`, outermost first.
    styles: Vec<String>,
    /// Open links: where each one's text starts in `text`, and its target.
    links: Vec<(usize, String)>,
    /// Open images: where each one's alt text starts in `text`.
    images: Vec<usize>,
    /// Open lists: the next number, for the ordered ones.
    lists: Vec<Option<u64>>,
    /// The code block being gathered: its language and its text.
    verbatim: Option<(String, String)>,
    /// The table being gathered: column alignments, and rows of cells.
    table: Option<(Vec<Align>, Vec<Vec<String>>)>,
    html_block: String,
    html: Html,
    /// Inside an HTML block's own text: whether that run is centered.
    /// Outside one, `html` says.
    centered: Option<bool>,
}

impl Renderer {
    /// Render `md` into `out`. `nested` is the text of an HTML block, which
    /// has had its HTML turned into markdown already: what is left in angle
    /// brackets is shown as written.
    fn blocks(&mut self, md: &str, nested: bool) {
        for event in Parser::new_ext(md, extensions()) {
            match event {
                Event::Start(tag) => self.start(tag),
                Event::End(tag) => self.end(tag, nested),
                Event::Text(text) => match &mut self.verbatim {
                    Some((_, code)) => code.push_str(&text),
                    None => self.text.push_str(&clean(&text)),
                },
                Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                    self.span(CYAN, &clean(&code))
                }
                Event::Html(html) => self.html_block.push_str(&html),
                Event::InlineHtml(html) => self.inline_html(&html),
                Event::FootnoteReference(label) => self.span(DIM, &format!("[{}]", clean(&label))),
                Event::SoftBreak => self.text.push(' '),
                Event::HardBreak => self.text.push('\n'),
                Event::Rule => {
                    self.flush();
                    let rule = "─".repeat(self.room().min(40));
                    self.emit([format!("{DIM}{rule}{RESET}")]);
                    self.gap = true;
                }
                Event::TaskListMarker(done) => {
                    if let Some(item) = self.frames.iter_mut().rev().find(|f| f.item) {
                        item.first = item.first.replace('•', if done { "☑" } else { "☐" });
                    }
                }
            }
        }
        self.flush();
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Emphasis => self.open(ITALIC),
            Tag::Strong => self.open(BOLD),
            Tag::Strikethrough => self.open(STRIKE),
            Tag::Superscript | Tag::Subscript => {}
            Tag::Link { dest_url, .. } => self.open_link(&dest_url),
            Tag::Image { .. } => self.images.push(self.text.len()),
            Tag::TableHead | Tag::TableRow => {
                if let Some((_, rows)) = &mut self.table {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.text.clear(),
            // Everything else opens a block, which ends the one before it:
            // a list item's own text, when a list starts inside it.
            block => {
                self.flush();
                self.start_block(block);
            }
        }
    }

    fn start_block(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => self.open(&match level {
                HeadingLevel::H1 => format!("{BOLD}{UNDERLINE}"),
                HeadingLevel::H2 => format!("{BOLD}{CYAN}"),
                HeadingLevel::H3 => format!("{BOLD}{YELLOW}"),
                _ => BOLD.to_string(),
            }),
            Tag::DefinitionListTitle => self.open(BOLD),
            Tag::BlockQuote(kind) => {
                let bar = format!("{DIM}│{RESET} ");
                self.frames.push(Frame {
                    rest: bar.clone(),
                    ..Frame::new(bar, false)
                });
                // GitHub's alerts: `> [!NOTE]`.
                let label = match kind {
                    Some(BlockQuoteKind::Note) => Some(("\x1b[34m", "Note")),
                    Some(BlockQuoteKind::Tip) => Some(("\x1b[32m", "Tip")),
                    Some(BlockQuoteKind::Important) => Some(("\x1b[35m", "Important")),
                    Some(BlockQuoteKind::Warning) => Some((YELLOW, "Warning")),
                    Some(BlockQuoteKind::Caution) => Some(("\x1b[31m", "Caution")),
                    None => None,
                };
                if let Some((color, name)) = label {
                    self.emit([format!("{BOLD}{color}{name}{RESET}")]);
                }
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.verbatim = Some((lang, String::new()));
            }
            // Front matter, shown as what it is.
            Tag::MetadataBlock(_) => self.verbatim = Some(("yaml".into(), String::new())),
            Tag::List(start) => self.lists.push(start),
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        *number += 1;
                        format!("{}. ", *number - 1)
                    }
                    _ => "• ".to_string(),
                };
                // Only the outermost list stands in from the margin; a list
                // inside an item starts under that item's text.
                let lead = if self.frames.iter().any(|f| f.item) {
                    ""
                } else {
                    "  "
                };
                self.frames
                    .push(Frame::new(format!("{lead}{marker}"), true));
            }
            Tag::FootnoteDefinition(label) => {
                let label = format!("{DIM}[{}]{RESET} ", clean(&label));
                self.frames.push(Frame::new(label, false));
            }
            Tag::DefinitionListDefinition => self.frames.push(Frame::new("    ".into(), false)),
            Tag::Table(aligns) => {
                let aligns = aligns
                    .iter()
                    .map(|a| match a {
                        Alignment::Center => Align::Center,
                        Alignment::Right => Align::Right,
                        Alignment::None | Alignment::Left => Align::Left,
                    })
                    .collect();
                self.table = Some((aligns, Vec::new()));
            }
            Tag::HtmlBlock => self.html_block.clear(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, nested: bool) {
        match tag {
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.close(),
            TagEnd::Superscript | TagEnd::Subscript => {}
            TagEnd::Link => self.close_link(),
            TagEnd::Image => {
                // Whatever styling the alt text had, it is a label now.
                if let Some(start) = self.images.pop() {
                    let alt = plain(&self.text.split_off(start));
                    let sep = if alt.is_empty() { "" } else { ": " };
                    self.span(ITALIC, &format!("[image{sep}{alt}]"));
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.text);
                if let Some((_, rows)) = &mut self.table
                    && let Some(row) = rows.last_mut()
                {
                    row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {}
            TagEnd::Heading(_) => {
                self.close();
                self.flush();
                self.gap = true;
            }
            // Its definition follows directly.
            TagEnd::DefinitionListTitle => {
                self.close();
                self.flush();
            }
            TagEnd::Paragraph => {
                self.flush();
                self.gap = true;
                if let Some(item) = self.frames.iter_mut().rev().find(|f| f.item) {
                    item.loose = true;
                }
            }
            TagEnd::DefinitionList => self.gap = true,
            TagEnd::CodeBlock | TagEnd::MetadataBlock(_) => {
                if let Some((lang, code)) = self.verbatim.take() {
                    let lines: Vec<String> = code.lines().map(clean).collect();
                    let mut boxed = String::new();
                    code_block(&mut boxed, &lang, &lines, self.code.len(), self.room());
                    // No trailing newline: pasted at a prompt, it would run.
                    self.code.push(lines.join("\n"));
                    self.emit(boxed.lines());
                }
                self.gap = true;
            }
            TagEnd::Table => {
                if let Some((aligns, rows)) = self.table.take() {
                    let mut drawn = String::new();
                    table(&mut drawn, &rows, &aligns, self.room());
                    self.emit(drawn.lines());
                }
                self.gap = true;
            }
            TagEnd::HtmlBlock => self.end_html_block(nested),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                // A list inside an item is part of the item: it stands
                // apart from what follows only if the item's text does.
                let item = self.frames.iter().rev().find(|f| f.item);
                self.gap = item.is_none_or(|item| item.loose);
            }
            TagEnd::Item => {
                self.flush();
                // An item with nothing in it still has its bullet.
                if self.frames.last().is_some_and(|f| !f.used) {
                    self.emit([""]);
                }
                self.frames.pop();
            }
            TagEnd::BlockQuote(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionListDefinition => {
                self.flush();
                self.frames.pop();
                self.gap = true;
            }
        }
    }

    // --- Inline: styles nest, and a reset ends them all ---

    fn outer(&self) -> String {
        self.styles.concat()
    }

    fn open(&mut self, style: &str) {
        self.text.push_str(style);
        self.styles.push(style.to_string());
    }

    /// End the innermost style. The only way to end one is to reset them
    /// all, so the ones around it are opened again — or code inside bold
    /// would leave the rest of the bold plain.
    fn close(&mut self) {
        self.styles.pop();
        self.text.push_str(RESET);
        self.text.push_str(&self.outer());
    }

    fn span(&mut self, style: &str, text: &str) {
        self.open(style);
        self.text.push_str(text);
        self.close();
    }

    fn open_link(&mut self, target: &str) {
        self.links.push((self.text.len(), clean(target)));
        self.open(UNDERLINE);
    }

    /// The URL goes after the text, where it can be cmd-clicked — unless
    /// the text is the URL, or it points within the page, which is nowhere
    /// in a pager.
    fn close_link(&mut self) {
        self.close();
        if let Some((start, url)) = self.links.pop()
            && !url.is_empty()
            && !url.starts_with('#')
            && url.trim_start_matches("mailto:") != plain(&self.text[start..])
        {
            self.text.push(' ');
            self.span(DIM, &format!("({url})"));
        }
    }

    /// A tag in running text: the ones with a style get it, the rest of the
    /// ones we know are dropped, and anything else in angle brackets is
    /// text — `Vec<String>` is not markup.
    fn inline_html(&mut self, html: &str) {
        if html.starts_with("<!--") {
            return;
        }
        let Some((name, closing, attrs, _)) = parse_tag(html) else {
            self.text.push_str(&clean(html));
            return;
        };
        let style = match name.as_str() {
            "strong" | "b" => BOLD,
            "em" | "i" => ITALIC,
            "code" | "kbd" | "samp" | "tt" => CYAN,
            "del" | "s" => STRIKE,
            "u" | "ins" => UNDERLINE,
            "br" => return self.text.push('\n'),
            "img" => {
                let alt = clean(attr(attrs, "alt").unwrap_or(""));
                let sep = if alt.is_empty() { "" } else { ": " };
                return self.span(ITALIC, &format!("[image{sep}{alt}]"));
            }
            "a" if closing => {
                if !self.links.is_empty() {
                    self.close_link();
                }
                return;
            }
            "a" => return self.open_link(attr(attrs, "href").unwrap_or("")),
            _ => return,
        };
        if !closing {
            self.open(style);
        } else if self.styles.last().is_some_and(|open| open == style) {
            self.close();
        }
    }

    // --- Blocks ---

    /// Columns left for a block's rows inside the containers it is in.
    fn room(&self) -> usize {
        let taken: usize = self.frames.iter().map(|f| visible_width(&f.rest)).sum();
        self.width.saturating_sub(taken).max(10)
    }

    /// Draw the text gathered so far as a block of its own, wrapped.
    fn flush(&mut self) {
        let text = std::mem::take(&mut self.text);
        if visible_width(text.trim()) > 0 {
            self.emit(wrap(text.trim_end(), self.room(), true));
        }
    }

    /// Write a block's rows, each behind what its containers put in front.
    fn emit<S: AsRef<str>>(&mut self, rows: impl IntoIterator<Item = S>) {
        let centered = self.centered.unwrap_or_else(|| self.html.center.is_some());
        let room = self.room();
        for row in rows {
            let row = row.as_ref();
            if std::mem::take(&mut self.gap) && !self.out.is_empty() {
                // Drawn by the containers already begun: a quote's bar
                // runs on, the bullet still to come does not.
                let open: String = self
                    .frames
                    .iter()
                    .filter(|f| f.used)
                    .map(|f| f.rest.as_str())
                    .collect();
                self.out.push_str(open.trim_end());
                self.out.push('\n');
            }
            for frame in &mut self.frames {
                self.out
                    .push_str(if std::mem::replace(&mut frame.used, true) {
                        &frame.rest
                    } else {
                        &frame.first
                    });
            }
            if centered {
                let pad = room.saturating_sub(visible_width(row)) / 2;
                self.out.push_str(&" ".repeat(pad));
            }
            self.out.push_str(row);
            self.out.push('\n');
        }
    }

    /// An HTML block, by CommonMark, is left as written — markdown inside
    /// it included. READMEs put their headers in one, so its HTML is turned
    /// into markdown and rendered: each run of lines, centered or not, as a
    /// document of its own.
    fn end_html_block(&mut self, nested: bool) {
        let block = std::mem::take(&mut self.html_block);
        if nested {
            self.text.push_str(&clean(block.trim()));
            self.flush();
            self.gap = true;
            return;
        }
        let mut runs: Vec<(bool, String)> = Vec::new();
        for line in block.lines() {
            let was_centered = self.html.center.is_some();
            let converted = self.html.convert(line);
            let centered = was_centered || self.html.centered_here;
            // A `<br>` that ends the line breaks it once, not twice.
            let converted = converted.strip_suffix('\n').unwrap_or(&converted);
            // Indentation was the HTML's. Kept, four spaces of it would
            // make the line a code block.
            let line: Vec<&str> = converted.split('\n').map(str::trim_start).collect();
            let line = line.join("\n");
            match runs.last_mut() {
                Some((run, text)) if *run == centered => {
                    text.push_str(&line);
                    text.push('\n');
                }
                _ => runs.push((centered, line + "\n")),
            }
        }
        for (centered, text) in runs {
            self.centered = Some(centered);
            self.blocks(&text, true);
        }
        self.centered = None;
        self.gap = true;
    }
}

// --- HTML ---

/// Tags worth understanding. Anything else in angle brackets is left alone,
/// so `Vec<String>` in prose survives.
#[rustfmt::skip]
const TAGS: &[&str] = &[
    "a", "b", "blockquote", "br", "center", "code", "del", "details", "div", "em", "h1", "h2",
    "h3", "h4", "h5", "h6", "hr", "i", "img", "ins", "kbd", "li", "ol", "p", "picture", "s",
    "samp", "source", "span", "strong", "sub", "summary", "sup", "table", "tbody", "td", "th",
    "thead", "tr", "tt", "u", "ul",
];

/// For text opening with a tag we know: its name, whether it is a closing
/// tag, its attributes, and its length in bytes.
fn parse_tag(text: &str) -> Option<(String, bool, &str, usize)> {
    let end = text.find('>')?;
    let body = text.strip_prefix('<')?.get(..end - 1)?;
    let (closing, body) = body.strip_prefix('/').map_or((false, body), |b| (true, b));
    let name_len = body
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric())
        .count();
    let (name, attrs) = body.split_at(name_len);
    let name = name.to_ascii_lowercase();
    let known = TAGS.contains(&name.as_str())
        && attrs
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '/');
    known.then_some((name, closing, attrs, end + 1))
}

/// HTML state that outlives a line.
#[derive(Default)]
struct Html {
    in_comment: bool,
    /// The tag that turned centering on; its closing tag turns it off.
    center: Option<String>,
    /// Centering was switched on somewhere in the line just converted.
    centered_here: bool,
    /// The `href` of an `<a>` waiting for its `</a>`.
    href: Option<String>,
}

impl Html {
    /// Rewrite the HTML in a line of an HTML block as markdown, for the
    /// parser to take from there (entities included: it decodes those).
    /// `<br>` yields a hard break; inline code spans are left untouched.
    fn convert(&mut self, line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        let mut in_code = false;
        self.centered_here = false;
        loop {
            if self.in_comment {
                let Some(end) = rest.find("-->") else { break };
                rest = &rest[end + 3..];
                self.in_comment = false;
            }
            let Some(c) = rest.chars().next() else { break };
            if c == '`' {
                in_code = !in_code;
            } else if !in_code {
                if let Some(after) = rest.strip_prefix("<!--") {
                    self.in_comment = true;
                    rest = after;
                    continue;
                }
                if c == '<'
                    && let Some(len) = self.tag(rest, &mut out)
                {
                    rest = &rest[len..];
                    continue;
                }
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// Handle a known tag at the start of `rest`; returns its byte length.
    fn tag(&mut self, rest: &str, out: &mut String) -> Option<usize> {
        let (name, closing, attrs, len) = parse_tag(rest)?;
        if closing {
            if self.center.as_deref() == Some(&name) {
                self.center = None;
            }
        } else if name == "center" || attr(attrs, "align") == Some("center") {
            self.center = Some(name.clone());
            self.centered_here = true;
        }
        let starts_line = out.trim().is_empty();
        match (name.as_str(), closing) {
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) if starts_line => {
                let level = usize::from(name.as_bytes()[1] - b'0');
                out.push_str(&"#".repeat(level));
                out.push(' ');
            }
            ("strong" | "b", _) => out.push_str("**"),
            ("em" | "i", _) => out.push('*'),
            ("del" | "s", _) => out.push_str("~~"),
            ("code" | "kbd" | "samp" | "tt", _) => out.push('`'),
            ("summary", false) => out.push_str("**▸ "),
            ("summary", true) => out.push_str("**"),
            ("a", false) => {
                self.href = attr(attrs, "href").map(str::to_string);
                if self.href.is_some() {
                    out.push('[');
                }
            }
            ("a", true) => {
                if let Some(href) = self.href.take() {
                    out.push_str(&format!("]({href})"));
                }
            }
            ("img", false) => out.push_str(&format!(
                "![{}]({})",
                attr(attrs, "alt").unwrap_or(""),
                attr(attrs, "src").unwrap_or("")
            )),
            ("li", false) if starts_line => out.push_str("- "),
            ("hr", _) if starts_line => out.push_str("---"),
            ("blockquote", false) if starts_line => out.push_str("> "),
            // Two spaces before the newline: a break that holds.
            ("br", _) => out.push_str("  \n"),
            _ => {}
        }
        Some(len)
    }
}

/// The quoted value of `name="…"` in a tag's attributes.
fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let at = attrs.find(&format!("{name}="))? + name.len() + 1;
    let rest = &attrs[at..];
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let rest = &rest[1..];
    Some(&rest[..rest.find(quote)?])
}

// --- Code ---

/// A box around the code: the language and a "copy" link on its top edge,
/// the code highlighted when the language is known. Lines too long for the
/// tab are hard-wrapped inside it so the right edge holds.
fn code_block(out: &mut String, lang: &str, lines: &[String], index: usize, width: usize) {
    let lang = lang.split_whitespace().next().unwrap_or("");
    let highlighted = highlight(lang, lines);
    let rows: Vec<String> = highlighted
        .as_deref()
        .unwrap_or(lines)
        .iter()
        .flat_map(|l| wrap(l, width - 4, false))
        .collect();
    let label = if lang.is_empty() {
        String::new()
    } else {
        format!(" {lang} ")
    };
    let edge = visible_width(&label) + visible_width(COPY_LABEL);
    let inner = rows
        .iter()
        .map(|r| visible_width(r))
        .max()
        .unwrap_or(0)
        .max(edge)
        .min(width - 4);
    // ╭─ lang ───── ⧉ copy ─╮ spans inner + 4, like every row.
    let fill = "─".repeat(inner.saturating_sub(edge));
    out.push_str(&format!(
        "{DIM}╭─{RESET}{label}{DIM}{fill}{RESET}\x1b]8;;{COPY_URI}{index}\x1b\\{COPY_LABEL}\x1b]8;;\x1b\\{DIM}─╮{RESET}\n"
    ));
    for row in &rows {
        let pad = " ".repeat(inner - visible_width(row).min(inner));
        out.push_str(&format!("{DIM}│{RESET} {row}{pad} {DIM}│{RESET}\n"));
    }
    out.push_str(&format!("{DIM}╰{}╯{RESET}\n", "─".repeat(inner + 2)));
}

// --- Syntax highlighting ---

/// Theme "colours" are ANSI palette slots carried in `r`, so code follows
/// the terminal's theme instead of bringing its own: 0–15 the palette,
/// `DIM_SLOT` the default colour dimmed, `PLAIN_SLOT` no colour at all.
const DIM_SLOT: u8 = 16;
const PLAIN_SLOT: u8 = 255;

/// Scope selectors → palette slot and font style. Comments are dimmed
/// rather than "bright black", which some palettes make nearly invisible.
#[rustfmt::skip]
const SCOPE_STYLES: &[(&str, u8, FontStyle)] = &[
    ("comment, punctuation.definition.comment", DIM_SLOT, FontStyle::ITALIC),
    ("string, punctuation.definition.string", 2, FontStyle::empty()),
    ("string.regexp, constant.character.escape", 6, FontStyle::empty()),
    ("constant.numeric, constant.language, constant.character, support.constant", 3, FontStyle::empty()),
    ("keyword, storage", 5, FontStyle::empty()),
    ("keyword.operator, punctuation", PLAIN_SLOT, FontStyle::empty()),
    ("entity.name.function, support.function, support.macro, variable.function", 4, FontStyle::empty()),
    ("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class, entity.other.inherited-class", 3, FontStyle::empty()),
    ("entity.name.tag, variable.language, variable.other.readwrite.shell, punctuation.definition.variable.shell", 1, FontStyle::empty()),
    ("entity.other.attribute-name, variable.parameter.option, punctuation.definition.parameter", 6, FontStyle::empty()),
    ("meta.mapping.key string, meta.mapping.key punctuation.definition.string, entity.name.tag.toml, entity.name.tag.yaml, support.type.property-name", 4, FontStyle::empty()),
    ("entity.name.section, entity.name.table, markup.heading", 4, FontStyle::BOLD),
    ("markup.inserted, punctuation.definition.inserted", 2, FontStyle::empty()),
    ("markup.deleted, punctuation.definition.deleted", 1, FontStyle::empty()),
    ("markup.changed", 3, FontStyle::empty()),
    ("meta.diff.header, meta.diff.range, punctuation.definition.range.diff", 6, FontStyle::empty()),
    ("markup.bold", PLAIN_SLOT, FontStyle::BOLD),
    ("markup.italic", PLAIN_SLOT, FontStyle::ITALIC),
];

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

/// Loaded on the first preview with a code block: bat's grammar set (via
/// two-face — syntect's own has no TOML) and the palette theme above.
fn highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| {
        let slot = |r| Color {
            r,
            g: 0,
            b: 0,
            a: 0,
        };
        let scopes = SCOPE_STYLES
            .iter()
            .map(|&(selectors, fg, font_style)| ThemeItem {
                scope: selectors.parse().expect("static selectors"),
                style: StyleModifier {
                    foreground: Some(slot(fg)),
                    background: None,
                    font_style: Some(font_style),
                },
            })
            .collect();
        Highlighter {
            syntaxes: two_face::syntax::extra_newlines(),
            theme: Theme {
                settings: ThemeSettings {
                    foreground: Some(slot(PLAIN_SLOT)),
                    ..Default::default()
                },
                scopes,
                ..Default::default()
            },
        }
    })
}

/// The lines with SGR colour, or `None` when `lang` (a fence's info word:
/// a name or an extension) isn't a language we have a grammar for.
fn highlight(lang: &str, lines: &[String]) -> Option<Vec<String>> {
    let h = highlighter();
    let syntax = h.syntaxes.find_syntax_by_token(lang)?;
    let mut state = HighlightLines::new(syntax, &h.theme);
    lines
        .iter()
        .map(|line| {
            let line = format!("{line}\n");
            // (SGR codes, text), neighbours with the same codes merged.
            let mut spans: Vec<(String, String)> = Vec::new();
            for (style, text) in state.highlight_line(&line, &h.syntaxes).ok()? {
                let mut codes: Vec<String> = Vec::new();
                for (flag, code) in [
                    (FontStyle::BOLD, 1),
                    (FontStyle::ITALIC, 3),
                    (FontStyle::UNDERLINE, 4),
                ] {
                    if style.font_style.contains(flag) {
                        codes.push(code.to_string());
                    }
                }
                match style.foreground.r {
                    n @ 0..=7 => codes.push((30 + n).to_string()),
                    n @ 8..=15 => codes.push((82 + n).to_string()),
                    DIM_SLOT => codes.push("2".into()),
                    _ => {}
                }
                let (codes, text) = (codes.join(";"), text.trim_end_matches('\n'));
                match spans.last_mut() {
                    Some((last, run)) if *last == codes => run.push_str(text),
                    _ => spans.push((codes, text.to_string())),
                }
            }
            Some(
                spans
                    .iter()
                    .map(|(codes, text)| match codes.as_str() {
                        "" => text.clone(),
                        _ => format!("\x1b[{codes}m{text}{RESET}"),
                    })
                    .collect(),
            )
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// Draw a table within `width`: columns start at their natural widths and
/// the widest give way, a column at a time, until the borders fit; cells
/// (already styled) wrap inside their column. `rows[0]` is the header.
fn table(out: &mut String, rows: &[Vec<String>], aligns: &[Align], width: usize) {
    let n = aligns.len();
    let avail = width.saturating_sub(3 * n + 1);
    // Every row as wide as the header, whatever the source gave it.
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..n)
                .map(|c| row.get(c).cloned().unwrap_or_default())
                .collect()
        })
        .collect();
    if avail < n * 3 {
        // Too many columns to draw; show the cells, skip the box.
        for row in &rendered {
            out.push_str(&row.join(&format!(" {DIM}│{RESET} ")));
            out.push('\n');
        }
        return;
    }
    let mut widths: Vec<usize> = (0..n)
        .map(|c| {
            rendered
                .iter()
                .map(|r| visible_width(&r[c]))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    while widths.iter().sum::<usize>() > avail {
        let widest = (0..n).max_by_key(|&c| widths[c]).unwrap();
        widths[widest] -= 1;
    }

    let wrapped: Vec<Vec<Vec<String>>> = rendered
        .iter()
        .map(|row| (0..n).map(|c| wrap(&row[c], widths[c], true)).collect())
        .collect();
    let border = |left: &str, mid: &str, right: &str| {
        let bars: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("{DIM}{left}{}{right}{RESET}\n", bars.join(mid))
    };

    out.push_str(&border("┌", "┬", "┐"));
    for (r, row) in wrapped.iter().enumerate() {
        if r == 1 {
            out.push_str(&border("├", "┼", "┤"));
        }
        let height = row.iter().map(Vec::len).max().unwrap_or(1);
        for l in 0..height {
            out.push_str(&format!("{DIM}│{RESET}"));
            for c in 0..n {
                let text = row[c].get(l).map_or("", |s| s);
                let gap = widths[c] - visible_width(text).min(widths[c]);
                let (before, after) = match aligns[c] {
                    Align::Left => (0, gap),
                    Align::Right => (gap, 0),
                    Align::Center => (gap / 2, gap - gap / 2),
                };
                let style = if r == 0 { BOLD } else { "" };
                out.push_str(&format!(
                    " {}{style}{text}{RESET}{} {DIM}│{RESET}",
                    " ".repeat(before),
                    " ".repeat(after)
                ));
            }
            out.push('\n');
        }
    }
    out.push_str(&border("└", "┴", "┘"));
}

// --- ANSI-aware measuring and wrapping ---

/// Byte length of the escape sequence `s` starts with: CSI through its
/// final byte (SGR, for us), OSC through its ST (the copy hyperlink).
fn escape_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    match bytes.get(1) {
        Some(b'[') => bytes[2..]
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map_or(s.len(), |i| i + 3),
        Some(b']') => s.find("\x1b\\").map_or(s.len(), |i| i + 2),
        _ => 1,
    }
}

/// Columns `s` occupies once the terminal has eaten the escape sequences.
fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '\x1b' {
            rest = &rest[escape_len(rest)..];
        } else {
            width += c.width().unwrap_or(0);
            rest = &rest[c.len_utf8()..];
        }
    }
    width
}

/// Break styled text into lines of at most `width` columns — at spaces when
/// `at_spaces`, mid-word when there's no other way, and wherever it has a
/// newline. A style that spans a break is closed on the one line and
/// reopened on the next.
fn wrap(s: &str, width: usize, at_spaces: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let (mut cur, mut cur_w) = (String::new(), 0);
    // The SGR state at the end of `cur`.
    let mut active = String::new();
    // The last space in `cur`: byte index, columns before it, style there.
    let mut space: Option<(usize, usize, String)> = None;
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '\x1b' {
            let (esc, tail) = rest.split_at(escape_len(rest));
            rest = tail;
            if esc == RESET {
                active.clear();
            } else if esc.starts_with("\x1b[") && esc.ends_with('m') {
                active.push_str(esc);
            }
            cur.push_str(esc);
            continue;
        }
        rest = &rest[c.len_utf8()..];
        if c == '\n' {
            if !active.is_empty() {
                cur.push_str(RESET);
            }
            lines.push(std::mem::replace(&mut cur, active.clone()));
            (cur_w, space) = (0, None);
            continue;
        }
        if c == ' ' && at_spaces {
            if cur_w == 0 {
                continue;
            }
            if cur_w + 1 > width {
                // The line is full: this space is the break.
                if !active.is_empty() {
                    cur.push_str(RESET);
                }
                lines.push(std::mem::replace(&mut cur, active.clone()));
                (cur_w, space) = (0, None);
                continue;
            }
            space = Some((cur.len(), cur_w, active.clone()));
        }
        let w = c.width().unwrap_or(0);
        while cur_w > 0 && cur_w + w > width {
            if let Some((ix, before, style)) = space.take() {
                let rest = cur.split_off(ix);
                if !style.is_empty() {
                    cur.push_str(RESET);
                }
                lines.push(std::mem::replace(
                    &mut cur,
                    format!("{style}{}", &rest[1..]),
                ));
                cur_w -= before + 1;
            } else {
                if !active.is_empty() {
                    cur.push_str(RESET);
                }
                lines.push(std::mem::replace(&mut cur, active.clone()));
                cur_w = 0;
            }
        }
        cur.push(c);
        cur_w += w;
    }
    // A break can leave nothing behind but a reopened style.
    if lines.is_empty() || visible_width(&cur) > 0 {
        lines.push(cur);
    }
    lines
}

/// What's left when the styling is gone — what the reader sees.
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        let len = if c == '\x1b' {
            escape_len(rest)
        } else {
            out.push(c);
            c.len_utf8()
        };
        rest = &rest[len..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-line document, rendered: what running text comes out as.
    fn inline(md: &str) -> String {
        render(md, 200).text.trim_end().to_string()
    }

    #[test]
    fn renders_blocks() {
        let md = "# Title\n\n#### Deep ##\n\n* * *\n\n- top\n  - nested\n- [x] done\n- [ ] todo\n\n> quoted\n\n1. first\n";
        let out = render(md, 80).text;
        assert!(out.contains("\x1b[1m\x1b[4mTitle\x1b[0m"), "{out}");
        assert!(
            out.contains("\x1b[1mDeep\x1b[0m"),
            "closing #s dropped: {out}"
        );
        assert!(out.contains(&"─".repeat(40)), "spaced rule is not a bullet");
        assert!(
            out.contains("  • top\n    • nested\n"),
            "a nested list starts under its item's text: {out}"
        );
        assert!(out.contains("  ☑ done\n  ☐ todo\n"), "{out}");
        assert!(out.contains("│\x1b[0m quoted"));
        assert!(out.contains("  1. first"));
    }

    /// What the line-by-line renderer this replaced could not read.
    #[test]
    fn the_rest_of_commonmark_and_gfm() {
        let md = "---\ntitle: x\n---\n\nSetext\n======\n\nsoft\nwrapped <https://a.b> and [ref] and note[^1].\n\n- item\n\n  its second paragraph\n\n      indented code\n\n> [!WARNING]\n> careful\n\nTerm\n: what it means\n\n[ref]: https://r.ef \"title\"\n[^1]: the footnote\n";
        let out = render(md, 80);
        let text = plain(&out.text);
        assert!(
            text.starts_with("╭─ yaml "),
            "front matter is boxed: {text}"
        );
        assert!(out.text.contains("\x1b[1m\x1b[4mSetext\x1b[0m"), "{text}");
        assert!(
            text.contains("soft wrapped https://a.b and ref (https://r.ef) and note[1]."),
            "one paragraph; the autolink isn't repeated; the reference resolves: {text}"
        );
        assert!(
            text.contains("  • item\n\n    its second paragraph\n\n    ╭─"),
            "an item holds paragraphs and code, under its text: {text}"
        );
        assert_eq!(out.code, ["title: x", "indented code"]);
        assert!(text.contains("│ Warning\n│ careful\n"), "{text}");
        assert!(text.contains("Term\n    what it means\n"), "{text}");
        assert!(text.contains("[1] the footnote"), "{text}");
    }

    #[test]
    fn a_file_cannot_drive_the_terminal() {
        let out = render(
            "text \x1b[2J `code \x1b]0;x\x07`\n\n```\n\x1b[31mred\n```\n",
            40,
        );
        assert!(!plain(&out.text).contains('\x1b') && out.text.contains("␛[2J"));
        assert_eq!(out.code, ["␛[31mred"]);
    }

    #[test]
    fn code_fences_are_boxed_and_verbatim() {
        let md = "  ````rust\n  ```\n    **not bold** <p> &amp;\n  ```\n  ````\nafter **bold**\n";
        let out = plain(&render(md, 40).text);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[..5],
            [
                "╭─ rust ────────── ⧉ copy ─╮",
                "│ ```                      │",
                "│   **not bold** <p> &amp; │",
                "│ ```                      │",
                "╰──────────────────────────╯",
            ],
            "shorter fence stays inside; fence indent stripped; nothing parsed"
        );
        assert_eq!(lines[5..], ["", "after bold"], "fence closed");
    }

    #[test]
    fn code_is_highlighted_and_copyable() {
        let md = "```toml\n# note\npreset = \"nord\"\n```\n\n```nonsense\nplain **text**\n```\n";
        let out = render(md, 60);
        assert_eq!(out.code, ["# note\npreset = \"nord\"", "plain **text**"]);
        assert!(
            out.text
                .contains("\x1b]8;;omnipty-copy:0\x1b\\ ⧉ copy \x1b]8;;\x1b\\"),
            "the label is a hyperlink naming the block: {:?}",
            out.text
        );
        assert!(out.text.contains("omnipty-copy:1"));
        assert!(
            out.text.contains("\x1b[3;2m# note\x1b[0m"),
            "comment: dim italic, one span: {:?}",
            out.text
        );
        assert!(
            out.text.contains("\x1b[32m\"nord\"\x1b[0m"),
            "string: green"
        );
        assert!(
            out.text.contains("│\x1b[0m plain **text** "),
            "unknown language: boxed, not coloured"
        );
        for boxed in plain(&out.text).split("\n\n") {
            let widths: Vec<usize> = boxed.lines().map(visible_width).collect();
            assert!(
                widths.iter().all(|w| *w == widths[0]),
                "the copy label doesn't skew the top edge: {boxed}"
            );
        }
    }

    /// What the pane's click handler relies on: once the rendering has been
    /// through the terminal's parser, the label's cells carry the link.
    #[test]
    fn the_terminal_sees_the_copy_link() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;

        let size = crate::terminal::session::TermSize {
            columns: 40,
            screen_lines: 10,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let text = render("```sh\nls\n```\n\n```sh\npwd\n```\n", 40).text;
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, text.replace('\n', "\r\n").as_bytes());

        let uri_at = |line: i32, col: usize| {
            term.grid()[Line(line)][Column(col)]
                .hyperlink()
                .map(|l| l.uri().to_string())
        };
        let row: String = (0..40).map(|c| term.grid()[Line(0)][Column(c)].c).collect();
        let icon = row.chars().position(|c| c == '⧉').expect("label drawn");
        assert_eq!(uri_at(0, icon).as_deref(), Some("omnipty-copy:0"));
        assert_eq!(
            uri_at(0, icon + 5).as_deref(),
            Some("omnipty-copy:0"),
            "…copy"
        );
        assert_eq!(uri_at(0, icon - 2), None, "the border isn't part of it");
        assert_eq!(uri_at(1, icon), None, "nor is the code");
        assert_eq!(uri_at(4, icon).as_deref(), Some("omnipty-copy:1"));
    }

    #[test]
    fn highlighting_survives_a_wrap() {
        let out = render(
            "```rust\n// a comment that is much too long for this box\n```\n",
            24,
        )
        .text;
        let rows: Vec<&str> = out.lines().filter(|l| l.contains("\x1b[3;2m")).collect();
        assert!(
            rows.len() > 1,
            "each wrapped row reopens the style: {out:?}"
        );
        for line in plain(&out).lines() {
            assert_eq!(visible_width(line), 24);
        }
    }

    #[test]
    fn long_code_wraps_inside_the_box() {
        let out = plain(&render(&format!("```\n{}\n```\n", "x".repeat(50)), 24).text);
        for line in out.lines() {
            assert_eq!(visible_width(line), 24, "{out}");
        }
        assert_eq!(out.lines().count(), 5, "50 chars over 20 columns: {out}");
    }

    #[test]
    fn tables_align_and_fit() {
        let md = "| Key | Action |\n|:--|--:|\n| `a\\|b` | go |\n| c | **x** |\n";
        let out = plain(&render(md, 80).text);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "┌─────┬────────┐",
                "│ Key │ Action │",
                "├─────┼────────┤",
                "│ a|b │     go │",
                "│ c   │      x │",
                "└─────┴────────┘",
            ]
        );
    }

    #[test]
    fn wide_tables_wrap_cells_to_the_width() {
        let md = "| Keys | Action |\n|---|---|\n| `cmd-f` | search the scrollback with **regex** and a very long tail |\n| x | y |\n";
        let out = render(md, 40).text;
        let text = plain(&out);
        for line in text.lines() {
            assert_eq!(visible_width(line), 40, "every row fills the width: {text}");
        }
        assert!(
            text.contains("│ cmd-f │"),
            "narrow column kept whole: {text}"
        );
        assert_eq!(
            text.matches('├').count(),
            1,
            "only the header is ruled off: {text}"
        );
        let flat = text.replace(['│', '\n'], " ");
        let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("with regex and a very long tail"), "{flat}");
    }

    #[test]
    fn wrap_carries_styles_across_breaks() {
        let lines = wrap("aa \x1b[1mbb cc\x1b[0m dd", 5, true);
        assert_eq!(lines, ["aa \x1b[1mbb\x1b[0m", "\x1b[1mcc\x1b[0m dd"]);
        assert_eq!(wrap("abcdefg", 3, true), ["abc", "def", "g"]);
        assert_eq!(wrap("", 3, true), [""]);
    }

    #[test]
    fn html_becomes_markdown() {
        let md = "<!-- hidden\nstill hidden --><p align=\"center\">\n  <img src=\"i.png\" width=\"9\" alt=\"Icon\" />\n</p>\n\n<h1 align=\"center\">OmniPTY</h1>\n<p>\n  A &amp; B<br/>\n  <em>it</em> <strong>b</strong> <kbd>k</kbd> <a href=\"https://x\">site</a>\n</p>\nVec<String> and `<p>` stay\n";
        let out = render(md, 40).text;
        let text = plain(&out);
        assert!(!text.contains("hidden") && !text.contains("<p align") && !text.contains("</"));
        assert!(text.contains("[image: Icon]"), "{text}");
        let title = out.lines().find(|l| l.contains("OmniPTY")).unwrap();
        assert!(title.contains("\x1b[1m\x1b[4mOmniPTY"), "h1 is a heading");
        assert_eq!(plain(title), format!("{}OmniPTY", " ".repeat(16)), "centered");
        assert!(
            text.contains("\nA & B\nit b k site (https://x)\n"),
            "entity decoded, br breaks, the HTML's indentation dropped: {text}"
        );
        assert!(out.contains("\x1b[3mit\x1b[0m \x1b[1mb\x1b[0m \x1b[36mk\x1b[0m"));
        assert!(text.contains("Vec<String> and <p> stay"), "{text}");
        assert!(
            !text.contains("\n\n\n"),
            "tag-only lines don't stack blanks"
        );
    }

    #[test]
    fn html_in_running_text_is_styled() {
        let out = inline("a <b>bold <kbd>k</kbd> still</b> <del>gone</del><!-- x --> b<br>c </i>");
        assert_eq!(
            out,
            "a \x1b[1mbold \x1b[36mk\x1b[0m\x1b[1m still\x1b[0m \x1b[9mgone\x1b[0m b\nc"
        );
    }

    #[test]
    fn inline_styles_and_links() {
        assert_eq!(inline("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(inline("*it*"), "\x1b[3mit\x1b[0m");
        assert_eq!(
            inline("[docs](https://x \"t\")"),
            "\x1b[4mdocs\x1b[0m \x1b[2m(https://x)\x1b[0m"
        );
        assert_eq!(inline("[top](#top)"), "\x1b[4mtop\x1b[0m");
        assert_eq!(inline("![shot](a.png)"), "\x1b[3m[image: shot]\x1b[0m");
        assert_eq!(
            plain(&inline("[![CI](badge.svg)](https://ci)")),
            "[image: CI] (https://ci)",
            "a badge: image inside a link"
        );
        assert_eq!(inline("a [b] c"), "a [b] c");
    }

    #[test]
    fn styles_nest_and_survive_what_they_enclose() {
        // The checked-off line from a task list: struck through, code and all.
        assert_eq!(
            inline("~~`Drawer`: resize~~ ok"),
            "\x1b[9m\x1b[36mDrawer\x1b[0m\x1b[9m: resize\x1b[0m ok"
        );
        assert_eq!(
            inline("**a *b* c**"),
            "\x1b[1ma \x1b[3mb\x1b[0m\x1b[1m c\x1b[0m"
        );
        assert_eq!(
            inline("_it_ and __bold__"),
            "\x1b[3mit\x1b[0m and \x1b[1mbold\x1b[0m"
        );
        for literal in ["snake_case_name", "a ~~ b ~~ c", "~~unclosed", r"\*not\*"] {
            assert_eq!(
                plain(&inline(literal)),
                literal.replace('\\', ""),
                "{literal}"
            );
            assert!(!inline(literal).contains('\x1b'), "{literal}");
        }
        let heading = render("## Set `this` up\n", 80).text;
        assert!(
            heading.contains("\x1b[36mthis\x1b[0m\x1b[1m\x1b[36m up"),
            "the heading's style resumes after the code: {heading:?}"
        );
    }

    #[test]
    fn long_lines_wrap_under_their_own_text() {
        let md = "- [x] ~~one two three four~~ five\n\n> quoted words go here\n\n10. numbered item wraps too\n\nplain words wrap\n";
        let out = render(md, 20).text;
        assert_eq!(
            plain(&out).lines().collect::<Vec<_>>(),
            [
                "  ☑ one two three",
                "    four five",
                "",
                "│ quoted words go",
                "│ here",
                "",
                "  10. numbered item",
                "      wraps too",
                "",
                "plain words wrap",
            ]
        );
        assert!(
            out.contains("\x1b[9mfour\x1b[0m five"),
            "a style carries over the break: {out:?}"
        );
    }

    #[test]
    fn markdown_extensions() {
        assert!(is_markdown(Path::new("/x/README.MD")));
        assert!(is_markdown(Path::new("notes.markdown")));
        assert!(!is_markdown(Path::new("md")) && !is_markdown(Path::new("a.mdx")));
    }
}
