//! Splitting streamed text deltas into newline-delimited lines.

/// Accumulates text deltas and hands out each complete, non-blank line
/// (without its `\n` or a trailing `\r`), however the deltas were cut.
#[derive(Debug, Default)]
pub struct LineSplitter {
    buffer: String,
}

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `delta` and returns the lines it completed, in order.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        let Some(last_newline) = self.buffer.rfind('\n') else {
            return Vec::new();
        };
        let rest = self.buffer.split_off(last_newline + 1);
        let complete = std::mem::replace(&mut self.buffer, rest);
        complete.split('\n').filter_map(non_blank).collect()
    }

    /// The unterminated last line, if it is not blank.
    pub fn finish(self) -> Option<String> {
        non_blank(&self.buffer)
    }
}

fn non_blank(line: &str) -> Option<String> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    (!line.trim().is_empty()).then(|| line.to_string())
}
