//! Source text -> tokens, with indentation resolved into `Indent`/`Dedent`.
//!
//! The only genuinely fiddly part of lexing Python is that a newline sometimes
//! means "end of statement" and sometimes means nothing at all. Three rules
//! cover it, and all three are implemented here:
//!
//! * inside `(` or `[`, newlines and indentation are invisible;
//! * a `\` at the end of a line joins it to the next;
//! * a line that is blank or only a comment produces no tokens, and -- crucially -- does not affect the
//!   indent stack, so a blank line in the middle of a block does not close it.

use alloc::string::String;
use alloc::vec::Vec;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    // literals and names
    Int(i32),
    Str(String),
    Name(String),

    // keywords
    If,
    Elif,
    Else,
    While,
    For,
    In,
    Def,
    Return,
    Break,
    Continue,
    Pass,
    Global,
    And,
    Or,
    Not,
    True,
    False,
    None,

    // structure
    Newline,
    Indent,
    Dedent,
    Eof,

    // punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,

    // operators
    Plus,
    Minus,
    Star,
    Slash,
    DblSlash,
    Percent,
    DblStar,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,

    // comparison
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,

    // assignment
    Assign,
    /// `+=`, `-=`, ... The payload is the token of the underlying operator.
    AugAssign(AugOp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AugOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
}

/// A token plus the line it came from, so errors can point somewhere useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned {
    pub tok: Tok,
    pub line: u32,
}

/// Width a tab advances to. Python 3 rejects mixed tabs and spaces outright;
/// we take the older, more forgiving reading and round up to a multiple of 8,
/// because a script dropped on the badge was very likely edited somewhere we
/// have no control over.
const TAB_WIDTH: u32 = 8;

/// Cap on nesting of `(`/`[`. Bounded so a pathological file cannot make the
/// parser -- which recurses per bracket -- run out of stack later on.
const MAX_BRACKET_DEPTH: u32 = 32;

pub fn tokenize(src: &str) -> Result<Vec<Spanned>, Error> { Lexer::new(src)?.run() }

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    out: Vec<Spanned>,
    /// Indent columns currently open. Always starts with 0.
    indents: Vec<u32>,
    /// Depth of unclosed `(` / `[`.
    brackets: u32,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Result<Self, Error> {
        // Reserve the true upper bound once, and never grow.
        //
        // Two reasons this is worth the over-reservation. A vector that doubles
        // its way to the answer needs the old and the new buffer live at the
        // same time, so its transient peak is 1.5x what it ends up using -- on
        // a heap where the token vector is already the largest single thing.
        // And `Vec::push` cannot fail politely: an allocation failure inside it
        // is a panic, and a panic on this device prints and spins forever.
        // `try_reserve` up front turns "this file is too big" into an ordinary
        // syntax error with a line number.
        //
        // The bound is one token per byte. Nothing produces more: the shortest
        // token is one character, and the only tokens with no bytes behind them
        // are the `Dedent` run at end of file, which cannot exceed the number
        // of indents opened, each of which cost at least a byte.
        let mut out = Vec::new();
        if out.try_reserve_exact(src.len() + 8).is_err() {
            return Err(Error::new(0, "not enough memory to read this script"));
        }
        Ok(Lexer { src: src.as_bytes(), pos: 0, line: 1, out, indents: alloc::vec![0], brackets: 0 })
    }

    fn err(&self, msg: impl Into<String>) -> Error { Error::new(self.line, msg) }

    fn peek(&self) -> u8 { if self.pos < self.src.len() { self.src[self.pos] } else { 0 } }

    fn peek_at(&self, n: usize) -> u8 {
        if self.pos + n < self.src.len() { self.src[self.pos + n] } else { 0 }
    }

    fn bump(&mut self) -> u8 {
        let c = self.peek();
        self.pos += 1;
        c
    }

    fn push(&mut self, tok: Tok) { self.out.push(Spanned { tok, line: self.line }); }

    fn run(mut self) -> Result<Vec<Spanned>, Error> {
        // `at_line_start` drives the indent machinery. It is only true at the
        // very beginning of a logical line, which is why implicit joining
        // inside brackets simply never sets it.
        let mut at_line_start = true;

        loop {
            if at_line_start && self.brackets == 0 {
                // `None` means the line was blank or comment-only: skip it
                // without touching the indent stack.
                if self.line_indent()?.is_none() {
                    if self.pos >= self.src.len() {
                        break;
                    }
                    continue;
                }
                at_line_start = false;
            }

            // Horizontal whitespace between tokens is never significant.
            while matches!(self.peek(), b' ' | b'\t' | b'\r') {
                self.pos += 1;
            }

            if self.pos >= self.src.len() {
                break;
            }

            let c = self.peek();

            if c == b'#' {
                while self.pos < self.src.len() && self.peek() != b'\n' {
                    self.pos += 1;
                }
                continue;
            }

            if c == b'\\' && (self.peek_at(1) == b'\n' || (self.peek_at(1) == b'\r')) {
                // Explicit line join: swallow the backslash and the newline.
                self.pos += 1;
                if self.peek() == b'\r' {
                    self.pos += 1;
                }
                if self.peek() == b'\n' {
                    self.pos += 1;
                }
                self.line += 1;
                continue;
            }

            if c == b'\n' {
                self.pos += 1;
                self.line += 1;
                if self.brackets == 0 {
                    // Collapse runs of newlines; the parser only ever needs one.
                    if !matches!(self.out.last().map(|s| &s.tok), None | Some(Tok::Newline)) {
                        self.out.push(Spanned { tok: Tok::Newline, line: self.line - 1 });
                    }
                    at_line_start = true;
                }
                continue;
            }

            if c.is_ascii_digit() {
                self.lex_number()?;
                continue;
            }

            if c == b'_' || c.is_ascii_alphabetic() {
                self.lex_name();
                continue;
            }

            if c == b'"' || c == b'\'' {
                self.lex_string()?;
                continue;
            }

            self.lex_operator()?;
        }

        // A file that ends mid-expression is a syntax error, not something to
        // paper over -- silently closing the brackets would mis-parse.
        if self.brackets > 0 {
            return Err(self.err("unclosed '(' or '['"));
        }

        if !matches!(self.out.last().map(|s| &s.tok), None | Some(Tok::Newline)) {
            self.push(Tok::Newline);
        }
        while self.indents.len() > 1 {
            self.indents.pop();
            self.push(Tok::Dedent);
        }
        self.push(Tok::Eof);
        Ok(self.out)
    }

    /// Measure the indentation of the line at `self.pos` and emit the matching
    /// `Indent`/`Dedent` tokens.
    ///
    /// Returns `None` if the line turned out to be blank or comment-only, in
    /// which case the caller should move on to the next line without treating
    /// its indentation as meaningful.
    fn line_indent(&mut self) -> Result<Option<()>, Error> {
        let mut col = 0u32;
        loop {
            match self.peek() {
                b' ' => {
                    col += 1;
                    self.pos += 1;
                }
                b'\t' => {
                    col = (col / TAB_WIDTH + 1) * TAB_WIDTH;
                    self.pos += 1;
                }
                b'\r' => {
                    self.pos += 1;
                }
                _ => break,
            }
        }

        // Blank line, comment-only line, or end of file: no indent change.
        if self.pos >= self.src.len() {
            return Ok(None);
        }
        if self.peek() == b'\n' {
            self.pos += 1;
            self.line += 1;
            return Ok(None);
        }
        if self.peek() == b'#' {
            while self.pos < self.src.len() && self.peek() != b'\n' {
                self.pos += 1;
            }
            return Ok(None);
        }

        let cur = *self.indents.last().unwrap();
        if col > cur {
            self.indents.push(col);
            self.push(Tok::Indent);
        } else if col < cur {
            while *self.indents.last().unwrap() > col {
                self.indents.pop();
                self.push(Tok::Dedent);
            }
            if *self.indents.last().unwrap() != col {
                return Err(self.err("unindent does not match any enclosing block"));
            }
        }
        Ok(Some(()))
    }

    fn lex_number(&mut self) -> Result<(), Error> {
        let start = self.pos;
        let (radix, skip) = if self.peek() == b'0' {
            match self.peek_at(1) | 0x20 {
                b'x' => (16, 2),
                b'b' => (2, 2),
                b'o' => (8, 2),
                _ => (10, 0),
            }
        } else {
            (10, 0)
        };
        self.pos += skip;
        let digits_start = self.pos;

        // Wrapping accumulate: a literal that does not fit in i32 wraps, which
        // matches how the rest of the interpreter treats integer overflow.
        let mut val: i32 = 0;
        let mut any = false;
        loop {
            let c = self.peek();
            if c == b'_' {
                self.pos += 1;
                continue;
            }
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a') as u32 + 10,
                b'A'..=b'F' => (c - b'A') as u32 + 10,
                _ => break,
            };
            if d >= radix {
                break;
            }
            val = val.wrapping_mul(radix as i32).wrapping_add(d as i32);
            any = true;
            self.pos += 1;
        }

        if !any {
            self.pos = start;
            return Err(self.err("malformed number"));
        }
        // `1.5` and `1e3` are rejected rather than truncated: this interpreter
        // has no floats, and quietly turning 1.5 into 1 would be worse than
        // saying so.
        if self.peek() == b'.' && self.peek_at(1).is_ascii_digit() {
            return Err(self.err("floating point numbers are not supported"));
        }
        if (self.peek() | 0x20) == b'e' && radix == 10 && self.pos > digits_start {
            let next = self.peek_at(1);
            if next.is_ascii_digit() || ((next == b'+' || next == b'-') && self.peek_at(2).is_ascii_digit()) {
                return Err(self.err("floating point numbers are not supported"));
            }
        }

        self.push(Tok::Int(val));
        Ok(())
    }

    fn lex_name(&mut self) {
        let start = self.pos;
        while matches!(self.peek(), b'_') || self.peek().is_ascii_alphanumeric() {
            self.pos += 1;
        }
        // safety: the run is ASCII by construction.
        let word = core::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
        let tok = match word {
            "if" => Tok::If,
            "elif" => Tok::Elif,
            "else" => Tok::Else,
            "while" => Tok::While,
            "for" => Tok::For,
            "in" => Tok::In,
            "def" => Tok::Def,
            "return" => Tok::Return,
            "break" => Tok::Break,
            "continue" => Tok::Continue,
            "pass" => Tok::Pass,
            "global" => Tok::Global,
            "and" => Tok::And,
            "or" => Tok::Or,
            "not" => Tok::Not,
            "True" => Tok::True,
            "False" => Tok::False,
            "None" => Tok::None,
            _ => Tok::Name(String::from(word)),
        };
        self.push(tok);
    }

    fn lex_string(&mut self) -> Result<(), Error> {
        let quote = self.bump();
        let mut s = String::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(self.err("unterminated string"));
            }
            let c = self.bump();
            if c == quote {
                break;
            }
            if c == b'\n' {
                return Err(self.err("unterminated string"));
            }
            if c != b'\\' {
                s.push(c as char);
                continue;
            }
            let e = self.bump();
            match e {
                b'n' => s.push('\n'),
                b'r' => s.push('\r'),
                b't' => s.push('\t'),
                b'0' => s.push('\0'),
                b'\\' => s.push('\\'),
                b'\'' => s.push('\''),
                b'"' => s.push('"'),
                b'x' => {
                    let hi = hex_val(self.bump()).ok_or_else(|| self.err("bad \\x escape"))?;
                    let lo = hex_val(self.bump()).ok_or_else(|| self.err("bad \\x escape"))?;
                    s.push((hi * 16 + lo) as char);
                }
                b'\n' => {
                    // escaped newline inside a string: line continuation
                    self.line += 1;
                }
                _ => return Err(self.err("unknown escape sequence")),
            }
        }
        self.push(Tok::Str(s));
        Ok(())
    }

    fn lex_operator(&mut self) -> Result<(), Error> {
        let c = self.bump();
        // Two- and three-character operators are checked before the one-character
        // fallbacks, longest first, so `//=` never lexes as `//` then `=`.
        let tok = match c {
            b'(' => {
                self.brackets += 1;
                if self.brackets > MAX_BRACKET_DEPTH {
                    return Err(self.err("expression nested too deeply"));
                }
                Tok::LParen
            }
            b')' => {
                self.brackets = self.brackets.saturating_sub(1);
                Tok::RParen
            }
            b'[' => {
                self.brackets += 1;
                if self.brackets > MAX_BRACKET_DEPTH {
                    return Err(self.err("expression nested too deeply"));
                }
                Tok::LBracket
            }
            b']' => {
                self.brackets = self.brackets.saturating_sub(1);
                Tok::RBracket
            }
            b',' => Tok::Comma,
            b':' => Tok::Colon,
            b'.' => Tok::Dot,
            b'~' => Tok::Tilde,
            b'+' => self.maybe_aug(AugOp::Add, Tok::Plus),
            b'-' => self.maybe_aug(AugOp::Sub, Tok::Minus),
            b'%' => self.maybe_aug(AugOp::Mod, Tok::Percent),
            b'&' => self.maybe_aug(AugOp::And, Tok::Amp),
            b'|' => self.maybe_aug(AugOp::Or, Tok::Pipe),
            b'^' => self.maybe_aug(AugOp::Xor, Tok::Caret),
            b'*' => {
                if self.peek() == b'*' {
                    self.pos += 1;
                    Tok::DblStar
                } else {
                    self.maybe_aug(AugOp::Mul, Tok::Star)
                }
            }
            b'/' => {
                if self.peek() == b'/' {
                    self.pos += 1;
                    self.maybe_aug(AugOp::FloorDiv, Tok::DblSlash)
                } else {
                    self.maybe_aug(AugOp::Div, Tok::Slash)
                }
            }
            b'<' => {
                if self.peek() == b'<' {
                    self.pos += 1;
                    self.maybe_aug(AugOp::Shl, Tok::Shl)
                } else if self.peek() == b'=' {
                    self.pos += 1;
                    Tok::Le
                } else {
                    Tok::Lt
                }
            }
            b'>' => {
                if self.peek() == b'>' {
                    self.pos += 1;
                    self.maybe_aug(AugOp::Shr, Tok::Shr)
                } else if self.peek() == b'=' {
                    self.pos += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'=' => {
                if self.peek() == b'=' {
                    self.pos += 1;
                    Tok::Eq
                } else {
                    Tok::Assign
                }
            }
            b'!' => {
                if self.peek() == b'=' {
                    self.pos += 1;
                    Tok::Ne
                } else {
                    return Err(self.err("expected '!=' "));
                }
            }
            _ => {
                return Err(self.err(alloc::format!("unexpected character '{}'", c as char)));
            }
        };
        self.push(tok);
        Ok(())
    }

    /// If the next character is `=`, this was an augmented assignment.
    fn maybe_aug(&mut self, aug: AugOp, plain: Tok) -> Tok {
        if self.peek() == b'=' {
            self.pos += 1;
            Tok::AugAssign(aug)
        } else {
            plain
        }
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> { tokenize(src).unwrap().into_iter().map(|s| s.tok).collect() }

    #[test]
    fn indentation_opens_and_closes_blocks() {
        let t = toks("if x:\n    y\nz\n");
        assert_eq!(
            t,
            alloc::vec![
                Tok::If,
                Tok::Name("x".into()),
                Tok::Colon,
                Tok::Newline,
                Tok::Indent,
                Tok::Name("y".into()),
                Tok::Newline,
                Tok::Dedent,
                Tok::Name("z".into()),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn blank_and_comment_lines_do_not_close_a_block() {
        let t = toks("if x:\n    a\n\n  # note\n    b\nc\n");
        let dedents = t.iter().filter(|x| **x == Tok::Dedent).count();
        assert_eq!(dedents, 1, "a blank/comment line must not dedent: {:?}", t);
    }

    #[test]
    fn newlines_inside_brackets_are_invisible() {
        let t = toks("f(1,\n  2)\n");
        assert!(!t[..t.len() - 2].contains(&Tok::Indent));
        assert_eq!(t.iter().filter(|x| **x == Tok::Newline).count(), 1);
    }

    #[test]
    fn number_bases_and_underscores() {
        assert_eq!(toks("0xff\n")[0], Tok::Int(255));
        assert_eq!(toks("0b1010\n")[0], Tok::Int(10));
        assert_eq!(toks("0o17\n")[0], Tok::Int(15));
        assert_eq!(toks("1_000\n")[0], Tok::Int(1000));
    }

    #[test]
    fn floats_are_rejected_rather_than_truncated() {
        assert!(tokenize("x = 1.5\n").is_err());
        assert!(tokenize("x = 1e3\n").is_err());
        // ...but attribute access on an integer-looking name still lexes.
        assert!(tokenize("a.b\n").is_ok());
    }

    #[test]
    fn operators_prefer_the_longest_match() {
        assert_eq!(toks("a //= 2\n")[1], Tok::AugAssign(AugOp::FloorDiv));
        assert_eq!(toks("a // 2\n")[1], Tok::DblSlash);
        assert_eq!(toks("a <<= 2\n")[1], Tok::AugAssign(AugOp::Shl));
        assert_eq!(toks("a <= 2\n")[1], Tok::Le);
        assert_eq!(toks("a ** 2\n")[1], Tok::DblStar);
    }

    #[test]
    fn string_escapes() {
        assert_eq!(toks("'a\\nb'\n")[0], Tok::Str("a\nb".into()));
        assert_eq!(toks("\"\\x41\"\n")[0], Tok::Str("A".into()));
        assert!(tokenize("'oops\n").is_err());
    }

    #[test]
    fn explicit_line_join() {
        let t = toks("a = 1 + \\\n    2\n");
        assert_eq!(t.iter().filter(|x| **x == Tok::Newline).count(), 1);
    }

    #[test]
    fn unclosed_bracket_is_an_error() {
        assert!(tokenize("f(1\n").is_err());
    }

    #[test]
    fn bad_dedent_is_an_error() {
        assert!(tokenize("if a:\n        b\n    c\n").is_err());
    }
}
