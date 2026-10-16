//! Structure-aware chunking of a [`TextDoc`] for per-chunk LLM extraction.
//!
//! Blocks are packed greedily. A chunk breaks early at a new level-1 or
//! level-2 section once it is half full, a block that fits a fresh chunk is
//! never split, and a larger block is split at line boundaries (a single
//! over-long line at a char boundary). Every chunk after the first repeats
//! the tail of the previous one after a `[…continued]` line.

use super::text::{Block, TextDoc};

/// Token budget of one chunk.
pub const CHUNK_TOKENS: usize = 6000;
/// Tokens of the previous chunk repeated at the start of the next.
pub const CHUNK_OVERLAP_TOKENS: usize = 400;

/// First line of every chunk after the first.
const CONTINUED: &str = "[…continued]\n";
/// Smallest piece of an over-long line, so splitting always progresses.
const MIN_SEGMENT_CHARS: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub index: usize,
    /// Heading path of the chunk's first block, joined with `" > "`.
    pub heading: String,
    pub text: String,
    /// [`estimate_tokens`] of `text`.
    pub tokens: usize,
}

/// Estimated tokens of `text`: `ceil(chars / 4)`.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Split `doc` into chunks of at most `max_tokens` (the overlap included,
/// except for degenerate budgets where the overlap alone nearly fills a
/// chunk), repeating at least `overlap_tokens` of whole lines of the
/// previous chunk where possible.
pub fn chunk(doc: &TextDoc, max_tokens: usize, overlap_tokens: usize) -> Vec<Chunk> {
    let mut packer = Packer::new(max_tokens, overlap_tokens);
    let mut prev_path: Option<&[String]> = None;
    for block in &doc.blocks {
        let text = block.text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        let section_start =
            prev_path.is_some_and(|prev| section_of(prev) != section_of(&block.heading_path));
        prev_path = Some(&block.heading_path);
        packer.add_block(block, text, section_start);
    }
    packer.finish();
    packer.chunks
}

/// The level-1 and level-2 headings of a path.
fn section_of(path: &[String]) -> &[String] {
    &path[..path.len().min(2)]
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

struct Packer {
    max_chars: usize,
    overlap_tokens: usize,
    chunks: Vec<Chunk>,
    text: String,
    chars: usize,
    /// Whether the chunk holds any block text (beyond the overlap).
    has_content: bool,
    /// Whether the current block already has a line in this chunk.
    block_open: bool,
    last_path: Option<Vec<String>>,
    heading: Option<String>,
}

impl Packer {
    fn new(max_tokens: usize, overlap_tokens: usize) -> Self {
        Self {
            max_chars: max_tokens.saturating_mul(4),
            overlap_tokens,
            chunks: Vec::new(),
            text: String::new(),
            chars: 0,
            has_content: false,
            block_open: false,
            last_path: None,
            heading: None,
        }
    }

    fn add_block(&mut self, block: &Block, text: &str, section_start: bool) {
        self.block_open = false;
        if section_start && self.has_content && self.chars * 2 >= self.max_chars {
            self.finish();
        }
        let path = &block.heading_path;
        let len = char_len(text) + 1;
        if self.fits(self.separator() + self.header_len(path) + len) {
            self.push(path, text);
            return;
        }
        if self.has_content {
            let fresh = char_len(CONTINUED)
                + char_len(&overlap_tail(self.text.trim_end(), self.overlap_tokens))
                + header_line(path).map_or(0, |h| char_len(&h));
            if fresh + len <= self.max_chars {
                self.finish();
                self.push(path, text);
                return;
            }
        }
        // Too large for any chunk: fill chunks line by line.
        for line in text.split('\n') {
            let mut rest = line;
            loop {
                let need = self.separator() + self.header_len(path) + char_len(rest) + 1;
                if self.fits(need) {
                    self.push(path, rest);
                    break;
                }
                if self.has_content {
                    self.finish();
                    continue;
                }
                let room = self
                    .max_chars
                    .saturating_sub(self.chars + self.header_len(path) + 1)
                    .max(MIN_SEGMENT_CHARS);
                let cut = rest.char_indices().nth(room).map_or(rest.len(), |(i, _)| i);
                let (head, tail) = rest.split_at(cut);
                self.push(path, head);
                if tail.is_empty() {
                    break;
                }
                self.finish();
                rest = tail;
            }
        }
    }

    fn fits(&self, extra_chars: usize) -> bool {
        self.chars + extra_chars <= self.max_chars
    }

    /// A blank line between blocks.
    fn separator(&self) -> usize {
        usize::from(self.has_content && !self.block_open)
    }

    fn header_len(&self, path: &[String]) -> usize {
        if self.last_path.as_deref() == Some(path) {
            return 0;
        }
        header_line(path).map_or(0, |h| char_len(&h))
    }

    fn push(&mut self, path: &[String], piece: &str) {
        let mut add = String::new();
        if self.separator() == 1 {
            add.push('\n');
        }
        if self.last_path.as_deref() != Some(path)
            && let Some(header) = header_line(path)
        {
            add.push_str(&header);
        }
        add.push_str(piece);
        add.push('\n');
        self.chars += char_len(&add);
        self.text.push_str(&add);
        self.has_content = true;
        self.block_open = true;
        self.last_path = Some(path.to_vec());
        if self.heading.is_none() {
            self.heading = Some(path.join(" > "));
        }
    }

    /// Close the current chunk (if it has content) and start the next one
    /// with the overlap.
    fn finish(&mut self) {
        if !self.has_content {
            return;
        }
        let text = self.text.trim_end().to_string();
        let overlap = overlap_tail(&text, self.overlap_tokens);
        self.chunks.push(Chunk {
            index: self.chunks.len(),
            heading: self.heading.take().unwrap_or_default(),
            tokens: estimate_tokens(&text),
            text,
        });
        self.text = format!("{CONTINUED}{overlap}");
        self.chars = char_len(&self.text);
        self.has_content = false;
        self.block_open = false;
        self.last_path = None;
    }
}

/// `# A > B\n`, or None for an empty path.
fn header_line(path: &[String]) -> Option<String> {
    (!path.is_empty()).then(|| format!("# {}\n", path.join(" > ")))
}

/// The last whole lines of `text` totalling at least `overlap_tokens`, each
/// followed by `\n`. Lines are not taken past twice the target; when the
/// last line alone exceeds that, its trailing `overlap_tokens * 4` chars are
/// used instead.
fn overlap_tail(text: &str, overlap_tokens: usize) -> String {
    let target = overlap_tokens.saturating_mul(4);
    if target == 0 {
        return String::new();
    }
    let cap = target.saturating_mul(2);
    let mut taken: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for line in text.split('\n').rev() {
        if line == CONTINUED.trim_end() {
            break;
        }
        let len = char_len(line) + 1;
        if total + len > cap {
            if taken.is_empty() {
                let skip = char_len(line).saturating_sub(target);
                let start = line.char_indices().nth(skip).map_or(line.len(), |(i, _)| i);
                taken.push(&line[start..]);
            }
            break;
        }
        taken.push(line);
        total += len;
        if total >= target {
            break;
        }
    }
    taken.iter().rev().map(|l| format!("{l}\n")).collect()
}
