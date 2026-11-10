//! The lexer: source text to tokens, each with its byte offset.

use super::Fail;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    /// An integer literal's magnitude; the parser applies a leading `-` so that
    /// `-9223372036854775808` is representable.
    Int(u64),
    Double(f64),
    Str(String),
    True,
    False,
    Null,
    In,
    Ident(String),
    LParen,
    RParen,
    LBrack,
    RBrack,
    LBrace,
    RBrace,
    Dot,
    Comma,
    Colon,
    Question,
    And,
    Or,
    Not,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eof,
}

#[derive(Debug, Clone)]
pub(super) struct Token {
    pub tok: Tok,
    pub pos: usize,
}

/// Splits `src` into tokens, ending with [`Tok::Eof`].
pub(super) fn lex(src: &str) -> Result<Vec<Token>, Fail> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut lx = Lexer { src, chars, i: 0 };
    let mut out = Vec::new();
    loop {
        lx.skip_space();
        let pos = lx.offset();
        let Some(c) = lx.peek() else {
            out.push(Token { tok: Tok::Eof, pos });
            return Ok(out);
        };
        let tok = match c {
            '0'..='9' => lx.number()?,
            '\'' | '"' => {
                lx.bump();
                Tok::Str(lx.string(c, false, pos)?)
            }
            'r' | 'R' if matches!(lx.peek_at(1), Some('\'' | '"')) => {
                lx.bump();
                let q = lx.bump().unwrap_or('"');
                Tok::Str(lx.string(q, true, pos)?)
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                let start = lx.i;
                while matches!(lx.peek(), Some(c) if c == '_' || c.is_ascii_alphanumeric()) {
                    lx.bump();
                }
                let word: String = lx.chars[start..lx.i].iter().map(|(_, c)| *c).collect();
                match word.as_str() {
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "null" => Tok::Null,
                    "in" => Tok::In,
                    _ => Tok::Ident(word),
                }
            }
            _ => {
                lx.bump();
                let next = lx.peek();
                let two = |lx: &mut Lexer<'_>, t: Tok| {
                    lx.bump();
                    t
                };
                match (c, next) {
                    ('(', _) => Tok::LParen,
                    (')', _) => Tok::RParen,
                    ('[', _) => Tok::LBrack,
                    (']', _) => Tok::RBrack,
                    ('{', _) => Tok::LBrace,
                    ('}', _) => Tok::RBrace,
                    ('.', _) => Tok::Dot,
                    (',', _) => Tok::Comma,
                    (':', _) => Tok::Colon,
                    ('?', _) => Tok::Question,
                    ('+', _) => Tok::Plus,
                    ('-', _) => Tok::Minus,
                    ('*', _) => Tok::Star,
                    ('/', _) => Tok::Slash,
                    ('%', _) => Tok::Percent,
                    ('&', Some('&')) => two(&mut lx, Tok::And),
                    ('|', Some('|')) => two(&mut lx, Tok::Or),
                    ('=', Some('=')) => two(&mut lx, Tok::Eq),
                    ('!', Some('=')) => two(&mut lx, Tok::Ne),
                    ('<', Some('=')) => two(&mut lx, Tok::Le),
                    ('>', Some('=')) => two(&mut lx, Tok::Ge),
                    ('!', _) => Tok::Not,
                    ('<', _) => Tok::Lt,
                    ('>', _) => Tok::Gt,
                    ('=', _) => return Err(Fail::new(pos, "unexpected '=': use '==' to compare")),
                    ('&', _) => return Err(Fail::new(pos, "unexpected '&': use '&&'")),
                    ('|', _) => return Err(Fail::new(pos, "unexpected '|': use '||'")),
                    _ => return Err(Fail::new(pos, format!("unexpected character {c:?}"))),
                }
            }
        };
        out.push(Token { tok, pos });
    }
}

struct Lexer<'s> {
    src: &'s str,
    chars: Vec<(usize, char)>,
    i: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).map(|(_, c)| *c)
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.i + n).map(|(_, c)| *c)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn offset(&self) -> usize {
        self.chars.get(self.i).map_or(self.src.len(), |(o, _)| *o)
    }

    fn skip_space(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn number(&mut self) -> Result<Tok, Fail> {
        let pos = self.offset();
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X')) {
            self.bump();
            self.bump();
            let mut n: u64 = 0;
            let mut any = false;
            while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                self.bump();
                any = true;
                n = n
                    .checked_mul(16)
                    .and_then(|n| n.checked_add(u64::from(d)))
                    .ok_or_else(|| Fail::new(pos, "integer literal out of range"))?;
            }
            if !any {
                return Err(Fail::new(pos, "hex literal without digits"));
            }
            return self.after_number(pos, Tok::Int(n));
        }
        while matches!(self.peek(), Some('0'..='9')) {
            self.bump();
        }
        let mut double = false;
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some('0'..='9')) {
            double = true;
            self.bump();
            while matches!(self.peek(), Some('0'..='9')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let sign = usize::from(matches!(self.peek_at(1), Some('+' | '-')));
            if matches!(self.peek_at(1 + sign), Some('0'..='9')) {
                double = true;
                for _ in 0..=sign {
                    self.bump();
                }
                while matches!(self.peek(), Some('0'..='9')) {
                    self.bump();
                }
            }
        }
        let text = self.src.get(pos..self.offset()).unwrap_or("");
        let tok = if double {
            match text.parse::<f64>() {
                Ok(d) if d.is_finite() => Tok::Double(d),
                _ => return Err(Fail::new(pos, "double literal out of range")),
            }
        } else {
            Tok::Int(
                text.parse::<u64>()
                    .map_err(|_| Fail::new(pos, "integer literal out of range"))?,
            )
        };
        self.after_number(pos, tok)
    }

    /// A number runs straight into an identifier (`1u`, `2abc`): refused, not split.
    fn after_number(&self, pos: usize, tok: Tok) -> Result<Tok, Fail> {
        match self.peek() {
            Some(c) if c == '_' || c.is_ascii_alphanumeric() => Err(Fail::new(
                pos,
                "malformed number (unsigned `u` literals are not supported)",
            )),
            _ => Ok(tok),
        }
    }

    fn string(&mut self, quote: char, raw: bool, start: usize) -> Result<String, Fail> {
        let mut out = String::new();
        loop {
            let at = self.offset();
            match self.bump() {
                None | Some('\n') | Some('\r') => {
                    return Err(Fail::new(start, "unterminated string literal"));
                }
                Some(c) if c == quote => return Ok(out),
                Some('\\') if !raw => out.push(self.escape(at)?),
                Some(c) => out.push(c),
            }
        }
    }

    fn escape(&mut self, at: usize) -> Result<char, Fail> {
        let bad = || Fail::new(at, "invalid escape sequence");
        let c = self.bump().ok_or_else(bad)?;
        Ok(match c {
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            '`' => '`',
            '?' => '?',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'a' => '\u{7}',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'v' => '\u{b}',
            'x' | 'X' => self.hex_char(2, at)?,
            'u' => self.hex_char(4, at)?,
            'U' => self.hex_char(8, at)?,
            '0'..='3' => {
                let mut n = c.to_digit(8).ok_or_else(bad)?;
                for _ in 0..2 {
                    let d = self.bump().and_then(|c| c.to_digit(8)).ok_or_else(bad)?;
                    n = n * 8 + d;
                }
                char::from_u32(n).ok_or_else(bad)?
            }
            _ => return Err(bad()),
        })
    }

    fn hex_char(&mut self, digits: usize, at: usize) -> Result<char, Fail> {
        let mut n: u32 = 0;
        for _ in 0..digits {
            let d = self
                .bump()
                .and_then(|c| c.to_digit(16))
                .ok_or_else(|| Fail::new(at, "invalid escape sequence"))?;
            n = n.saturating_mul(16).saturating_add(d);
        }
        char::from_u32(n).ok_or_else(|| Fail::new(at, "escape is not a valid character"))
    }
}
