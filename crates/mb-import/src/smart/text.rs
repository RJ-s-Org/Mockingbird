//! Text extraction: Markdown, HTML, PDF and plain text become a [`TextDoc`],
//! a sequence of prose and code blocks, each under its heading path.

use std::panic::{AssertUnwindSafe, catch_unwind};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use scraper::{ElementRef, Html, Node};

use super::{ImportKind, SmartError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Prose,
    Code,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// The headings above the block, outermost first.
    pub heading_path: Vec<String>,
    pub kind: BlockKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TextDoc {
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

/// Most pages of an accepted PDF.
pub const MAX_PDF_PAGES: usize = 500;

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Whether a line names an endpoint: an HTTP method (any case) followed by a
/// path (`/…`) or a URL (`http…`).
pub fn is_endpoint_line(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    let (Some(method), Some(target)) = (tokens.next(), tokens.next()) else {
        return false;
    };
    METHODS.iter().any(|m| m.eq_ignore_ascii_case(method))
        && (target.starts_with('/') || target.starts_with("http"))
}

/// Extract the text of an upload of the given kind. Markdown and plain text
/// are read as Markdown.
pub fn extract_text(kind: ImportKind, bytes: &[u8]) -> Result<TextDoc, SmartError> {
    match kind {
        ImportKind::Markdown | ImportKind::Text => Ok(extract_markdown(utf8(bytes)?)),
        ImportKind::Html => Ok(extract_html(utf8(bytes)?)),
        ImportKind::Pdf => extract_pdf(bytes),
        ImportKind::Har | ImportKind::Postman => Err(SmartError::Unsupported(format!(
            "{} is not a document kind",
            kind.as_str()
        ))),
    }
}

fn utf8(bytes: &[u8]) -> Result<&str, SmartError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| SmartError::Unreadable(format!("the text is not valid UTF-8: {e}")))?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// Collapse whitespace runs to single spaces and trim.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Accumulates blocks under the current heading path.
#[derive(Default)]
struct Builder {
    path: Vec<String>,
    prose: String,
    doc: TextDoc,
}

impl Builder {
    /// A heading of `level` (1-6) replaces the path from its level down.
    fn heading(&mut self, level: usize, text: &str) {
        self.flush_prose();
        let text = squash(text);
        if text.is_empty() {
            return;
        }
        if level == 1 && self.doc.title.is_none() {
            self.doc.title = Some(text.clone());
        }
        self.path.truncate(level.saturating_sub(1));
        self.path.push(text);
    }

    fn code(&mut self, text: &str) {
        self.flush_prose();
        let text = text.trim_end_matches(['\n', '\r']);
        if !text.trim().is_empty() {
            self.doc.blocks.push(Block {
                heading_path: self.path.clone(),
                kind: BlockKind::Code,
                text: text.to_string(),
            });
        }
    }

    /// End the current line of prose.
    fn newline(&mut self) {
        if !self.prose.is_empty() && !self.prose.ends_with('\n') {
            self.prose.push('\n');
        }
    }

    fn flush_prose(&mut self) {
        let prose = std::mem::take(&mut self.prose);
        push_prose(&mut self.doc.blocks, &self.path, &prose);
    }

    fn finish(mut self) -> TextDoc {
        self.flush_prose();
        self.doc
    }
}

/// Push `text` as prose blocks: lines are trimmed, blank lines dropped, and
/// every endpoint line starts a new block.
fn push_prose(blocks: &mut Vec<Block>, path: &[String], text: &str) {
    let mut current: Vec<&str> = Vec::new();
    let mut emit = |lines: &mut Vec<&str>| {
        if !lines.is_empty() {
            blocks.push(Block {
                heading_path: path.to_vec(),
                kind: BlockKind::Prose,
                text: lines.join("\n"),
            });
            lines.clear();
        }
    };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if is_endpoint_line(line) {
            emit(&mut current);
        }
        current.push(line);
    }
    emit(&mut current);
}

/// Extract Markdown (CommonMark with tables).
pub fn extract_markdown(text: &str) -> TextDoc {
    let mut b = Builder::default();
    let mut heading: Option<(usize, String)> = None;
    let mut code: Option<String> = None;
    let mut cell: Option<String> = None;
    let mut row: Vec<String> = Vec::new();
    for event in Parser::new_ext(text, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some((level as usize, String::new()));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text)) = heading.take() {
                    b.heading(level, &text);
                }
            }
            Event::Start(Tag::CodeBlock(_)) => code = Some(String::new()),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(text) = code.take() {
                    b.code(&text);
                }
            }
            Event::Start(Tag::TableCell) => cell = Some(String::new()),
            Event::End(TagEnd::TableCell) => {
                row.push(squash(&cell.take().unwrap_or_default()));
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                b.newline();
                b.prose.push_str(&row.join(" | "));
                b.prose.push('\n');
                row.clear();
            }
            Event::Start(Tag::Item | Tag::Paragraph)
            | Event::End(TagEnd::Item | TagEnd::Paragraph)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Rule => {
                if let Some((_, text)) = heading.as_mut() {
                    text.push(' ');
                } else if let Some(text) = cell.as_mut() {
                    text.push(' ');
                } else {
                    b.newline();
                }
            }
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                if let Some(text) = code.as_mut() {
                    text.push_str(&t);
                } else if let Some(text) = cell.as_mut() {
                    text.push_str(&t);
                } else if let Some((_, text)) = heading.as_mut() {
                    text.push_str(&t);
                } else {
                    b.prose.push_str(&t);
                }
            }
            _ => {}
        }
    }
    b.finish()
}

/// Subtrees that never hold documentation.
const SKIPPED: [&str; 8] = [
    "script", "style", "noscript", "nav", "footer", "svg", "template", "iframe",
];

/// Elements that end a line of prose.
const LINE_BREAKING: [&str; 26] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "form",
    "header",
    "hr",
    "li",
    "main",
    "ol",
    "p",
    "section",
    "summary",
    "table",
    "tbody",
    "thead",
    "tr",
    "ul",
];

fn heading_level(name: &str) -> Option<usize> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn element_text(el: ElementRef<'_>) -> String {
    el.text().collect()
}

/// Extract HTML: the body in document order, without scripts, styles,
/// navigation and other chrome. The title is `<title>`, else the first
/// `<h1>`.
pub fn extract_html(html: &str) -> TextDoc {
    let page = Html::parse_document(html);
    let root = page.root_element();
    let title = root
        .descendants()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().name() == "title")
        .map(|e| squash(&element_text(e)))
        .filter(|t| !t.is_empty());
    let body = root
        .descendants()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().name() == "body")
        .unwrap_or(root);

    // An iterative depth-first walk, so deeply nested markup cannot
    // overflow the stack.
    let mut walker = HtmlWalker::default();
    let mut current = body.first_child();
    let mut depth = 0usize;
    while let Some(node) = current {
        walker.open(node.value());
        if let Some(child) = node.first_child() {
            current = Some(child);
            depth += 1;
            continue;
        }
        let mut done = node;
        current = loop {
            walker.close(done.value());
            if let Some(sibling) = done.next_sibling() {
                break Some(sibling);
            }
            match done.parent() {
                Some(parent) if depth > 0 => {
                    depth -= 1;
                    done = parent;
                }
                _ => break None,
            }
        };
    }
    let mut doc = walker.b.finish();
    doc.title = title.or(doc.title);
    doc
}

#[derive(Default)]
struct HtmlWalker {
    b: Builder,
    /// Depth inside skipped subtrees.
    skip: usize,
    heading: Option<(usize, String)>,
    /// Nesting depth of `<pre>` and its verbatim text.
    pre: Option<(usize, String)>,
    cell: Option<String>,
    row: Option<Vec<String>>,
}

impl HtmlWalker {
    fn open(&mut self, node: &Node) {
        match node {
            Node::Element(el) => {
                let name = el.name();
                if self.skip > 0 || SKIPPED.contains(&name) {
                    self.skip += 1;
                } else if let Some((depth, text)) = self.pre.as_mut() {
                    if name == "pre" {
                        *depth += 1;
                    } else if name == "br" {
                        text.push('\n');
                    }
                } else if let Some(level) = heading_level(name) {
                    self.heading = Some((level, String::new()));
                } else if name == "pre" {
                    self.b.flush_prose();
                    self.pre = Some((1, String::new()));
                } else if name == "tr" {
                    self.row = Some(Vec::new());
                } else if name == "td" || name == "th" {
                    self.cell = Some(String::new());
                } else if LINE_BREAKING.contains(&name) {
                    self.line_break();
                }
            }
            Node::Text(text) if self.skip == 0 => {
                if let Some((_, buf)) = self.pre.as_mut() {
                    buf.push_str(text);
                } else if let Some(buf) = self.cell.as_mut() {
                    buf.push_str(text);
                } else if let Some((_, buf)) = self.heading.as_mut() {
                    buf.push_str(text);
                } else {
                    push_inline(&mut self.b.prose, text);
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, node: &Node) {
        let Node::Element(el) = node else {
            return;
        };
        let name = el.name();
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        if let Some((depth, text)) = self.pre.as_mut() {
            if name == "pre" {
                *depth -= 1;
                if *depth == 0 {
                    let text = std::mem::take(text);
                    self.pre = None;
                    self.b.code(&text);
                }
            }
            return;
        }
        if heading_level(name).is_some() {
            if let Some((level, text)) = self.heading.take() {
                self.b.heading(level, &text);
            }
        } else if name == "td" || name == "th" {
            if let (Some(cell), Some(row)) = (self.cell.take(), self.row.as_mut()) {
                row.push(squash(&cell));
            }
        } else if name == "tr" {
            if let Some(row) = self.row.take() {
                self.b.newline();
                self.b.prose.push_str(&row.join(" | "));
                self.b.prose.push('\n');
            }
        } else if LINE_BREAKING.contains(&name) {
            self.line_break();
        }
    }

    fn line_break(&mut self) {
        if let Some((_, h)) = self.heading.as_mut() {
            h.push(' ');
        } else if let Some(c) = self.cell.as_mut() {
            c.push(' ');
        } else {
            self.b.newline();
        }
    }
}

/// Append inline text, mapping whitespace runs to one space.
fn push_inline(prose: &mut String, text: &str) {
    for c in text.chars() {
        if c.is_whitespace() {
            if !prose.is_empty() && !prose.ends_with([' ', '\n']) {
                prose.push(' ');
            }
        } else {
            prose.push(c);
        }
    }
}

/// Run a `pdf_extract` call, turning a panic into an error.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, SmartError> {
    catch_unwind(AssertUnwindSafe(f))
        .map_err(|_| SmartError::Unreadable("the PDF could not be parsed".into()))
}

/// Extract a PDF: one prose block per page under `Page N`, split further at
/// endpoint lines. Pages whose text cannot be extracted are skipped; when no
/// page can be read the PDF is unreadable.
pub fn extract_pdf(bytes: &[u8]) -> Result<TextDoc, SmartError> {
    let doc = guarded(|| pdf_extract::Document::load_mem(bytes))?
        .map_err(|e| SmartError::Unreadable(format!("the PDF could not be parsed: {e}")))?;
    if doc.is_encrypted() {
        return Err(SmartError::Unreadable(
            "encrypted PDFs are not supported".into(),
        ));
    }
    let pages = guarded(|| doc.get_pages())?;
    if pages.len() > MAX_PDF_PAGES {
        return Err(SmartError::Unreadable(format!(
            "the PDF has {} pages; at most {MAX_PDF_PAGES} are supported",
            pages.len()
        )));
    }
    let mut blocks = Vec::new();
    let mut failed = 0usize;
    for &page in pages.keys() {
        let mut text = String::new();
        let result = guarded(|| {
            let mut out = pdf_extract::PlainTextOutput::new(&mut text);
            pdf_extract::output_doc_page(&doc, &mut out, page)
        });
        match result {
            Ok(Ok(())) => push_prose(&mut blocks, &[format!("Page {page}")], &text),
            _ => failed += 1,
        }
    }
    if failed > 0 && failed == pages.len() {
        return Err(SmartError::Unreadable("the PDF could not be parsed".into()));
    }
    Ok(TextDoc {
        title: None,
        blocks,
    })
}
