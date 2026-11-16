//! A lossless SQL tokenizer for the MariaDB translator.
//!
//! Every byte of the input belongs to exactly one token, so concatenating the
//! token texts reproduces the statement. Literals, comments (including
//! `/*+ hints */`), quoted identifiers and binds are single opaque tokens: no
//! rule can fire inside them.

/// Token kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum K {
    /// Unquoted word (keyword or identifier).
    Word,
    /// `"quoted identifier"`.
    Quoted,
    /// `` `back-quoted identifier` ``.
    Back,
    /// `'string'`, `n'…'`, `x'…'`.
    Str,
    /// Oracle alternative quoting `q'[…]'` / `nq'…'` (normalised on output).
    QStr,
    /// Numeric literal.
    Num,
    /// `?` positional bind.
    Bind,
    /// `:name` / `:1` (named bind or trigger row reference).
    Colon,
    /// `@var` / `@@var`.
    Var,
    /// `-- …` or `/* … */`.
    Comment,
    /// Whitespace run.
    Ws,
    LParen,
    RParen,
    Comma,
    Dot,
    Semi,
    /// Operator / other punctuation.
    Op,
}

#[derive(Debug, Clone, Copy)]
pub struct Tok {
    pub k: K,
    pub s: u32,
    pub e: u32,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#' || b >= 0x80
}

fn end_single(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn end_quoted(b: &[u8], start: usize, q: u8) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn end_q_quote(b: &[u8], quote: usize) -> usize {
    let Some(&open) = b.get(quote + 1) else {
        return b.len();
    };
    let close = match open {
        b'[' => b']',
        b'{' => b'}',
        b'(' => b')',
        b'<' => b'>',
        c => c,
    };
    let mut i = quote + 2;
    while i + 1 < b.len() {
        if b[i] == close && b[i + 1] == b'\'' {
            return i + 2;
        }
        i += 1;
    }
    b.len()
}

/// Tokenize `sql`.
pub fn lex(sql: &str) -> Vec<Tok> {
    let b = sql.as_bytes();
    let mut out = Vec::with_capacity(sql.len() / 3 + 4);
    let mut i = 0;
    let mut last_sig = K::Ws;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let k = match c {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c => {
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                K::Ws
            }
            b'\'' => {
                i = end_single(b, i);
                K::Str
            }
            b'"' => {
                i = end_quoted(b, i, b'"');
                K::Quoted
            }
            b'`' => {
                i = end_quoted(b, i, b'`');
                K::Back
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&x| x == b'\n')
                    .map_or(b.len(), |o| i + o);
                K::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = b[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map_or(b.len(), |o| i + 4 + o);
                K::Comment
            }
            b'?' => {
                i += 1;
                K::Bind
            }
            b':' if b.get(i + 1).is_some_and(|&x| is_word_byte(x)) => {
                i += 1;
                while i < b.len() && is_word_byte(b[i]) {
                    i += 1;
                }
                K::Colon
            }
            b'@' => {
                i += 1;
                if b.get(i) == Some(&b'@') {
                    i += 1;
                }
                while i < b.len() && (is_word_byte(b[i]) || b[i] == b'.') {
                    i += 1;
                }
                K::Var
            }
            b'0'..=b'9' => {
                i = end_number(b, i);
                K::Num
            }
            b'.' if b.get(i + 1).is_some_and(u8::is_ascii_digit)
                && !matches!(last_sig, K::Word | K::Quoted | K::Back | K::RParen) =>
            {
                i = end_number(b, i);
                K::Num
            }
            b'(' => {
                i += 1;
                K::LParen
            }
            b')' => {
                i += 1;
                K::RParen
            }
            b',' => {
                i += 1;
                K::Comma
            }
            b'.' => {
                i += 1;
                K::Dot
            }
            b';' => {
                i += 1;
                K::Semi
            }
            _ if is_word_byte(c) => {
                while i < b.len() && is_word_byte(b[i]) {
                    i += 1;
                }
                if b.get(i) == Some(&b'\'') {
                    let w = &sql[start..i];
                    if w.eq_ignore_ascii_case("q") || w.eq_ignore_ascii_case("nq") {
                        i = end_q_quote(b, i);
                        out.push(Tok {
                            k: K::QStr,
                            s: start as u32,
                            e: i as u32,
                        });
                        last_sig = K::Str;
                        continue;
                    }
                    if ["n", "x", "b", "u"]
                        .iter()
                        .any(|p| w.eq_ignore_ascii_case(p))
                    {
                        i = end_single(b, i);
                        out.push(Tok {
                            k: K::Str,
                            s: start as u32,
                            e: i as u32,
                        });
                        last_sig = K::Str;
                        continue;
                    }
                }
                K::Word
            }
            _ => {
                let two = b.get(i..i + 2).unwrap_or(&[]);
                if matches!(
                    two,
                    b"||" | b"<=" | b">=" | b"<>" | b"!=" | b"^=" | b"~=" | b":=" | b"=>" | b"**"
                ) {
                    i += 2;
                } else {
                    i += sql[i..].chars().next().map_or(1, char::len_utf8);
                }
                K::Op
            }
        };
        out.push(Tok {
            k,
            s: start as u32,
            e: i as u32,
        });
        if !matches!(k, K::Ws | K::Comment) {
            last_sig = k;
        }
    }
    out
}

fn end_number(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if b.get(i) == Some(&b'.') && b.get(i + 1) != Some(&b'.') {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E'))
        && (b.get(i + 1).is_some_and(u8::is_ascii_digit)
            || (matches!(b.get(i + 1), Some(b'+' | b'-'))
                && b.get(i + 2).is_some_and(u8::is_ascii_digit)))
    {
        i += 2;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    // Oracle `2.5d` / `1f` binary literal suffixes.
    if matches!(b.get(i), Some(b'd' | b'D' | b'f' | b'F'))
        && !b.get(i + 1).is_some_and(|&x| is_word_byte(x))
    {
        i += 1;
    }
    i
}

/// A static keyword set with allocation-free, case-insensitive lookup.
pub struct KwSet {
    words: &'static [&'static str],
    sorted: std::sync::OnceLock<Vec<&'static str>>,
}

impl KwSet {
    pub const fn new(words: &'static [&'static str]) -> Self {
        KwSet {
            words,
            sorted: std::sync::OnceLock::new(),
        }
    }

    pub fn contains(&self, w: &str) -> bool {
        let b = w.as_bytes();
        if b.is_empty() || b.len() > 24 {
            return false;
        }
        let mut buf = [0u8; 24];
        for (d, s) in buf.iter_mut().zip(b) {
            *d = s.to_ascii_uppercase();
        }
        let Ok(up) = std::str::from_utf8(&buf[..b.len()]) else {
            return false;
        };
        let sorted = self.sorted.get_or_init(|| {
            let mut v = self.words.to_vec();
            v.sort_unstable();
            v
        });
        sorted.binary_search(&up).is_ok()
    }
}

/// A tokenized statement with navigation helpers.
pub struct Ts<'a> {
    pub src: &'a str,
    pub t: Vec<Tok>,
    /// For `(`/`)`: index of the matching paren (`usize::MAX` if unmatched).
    pub pair: Vec<usize>,
    /// Innermost enclosing `(` of each token (`usize::MAX` at top level).
    pub parent: Vec<usize>,
}

pub const NONE: usize = usize::MAX;

impl<'a> Ts<'a> {
    pub fn new(src: &'a str) -> Self {
        let t = lex(src);
        let n = t.len();
        let mut pair = vec![NONE; n];
        let mut parent = vec![NONE; n];
        let mut stack: Vec<usize> = Vec::new();
        for i in 0..n {
            parent[i] = stack.last().copied().unwrap_or(NONE);
            match t[i].k {
                K::LParen => stack.push(i),
                K::RParen => {
                    if let Some(o) = stack.pop() {
                        pair[o] = i;
                        pair[i] = o;
                        parent[i] = stack.last().copied().unwrap_or(NONE);
                    }
                }
                _ => {}
            }
        }
        Ts {
            src,
            t,
            pair,
            parent,
        }
    }

    pub fn len(&self) -> usize {
        self.t.len()
    }

    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }

    pub fn text(&self, i: usize) -> &'a str {
        let t = self.t[i];
        &self.src[t.s as usize..t.e as usize]
    }

    pub fn k(&self, i: usize) -> K {
        self.t[i].k
    }

    /// Source text of tokens `a..b`.
    pub fn slice(&self, a: usize, b: usize) -> &'a str {
        if a >= b || a >= self.t.len() {
            return "";
        }
        let s = self.t[a].s as usize;
        let e = self.t[b - 1].e as usize;
        &self.src[s..e]
    }

    pub fn is_trivia(&self, i: usize) -> bool {
        matches!(self.t[i].k, K::Ws | K::Comment)
    }

    /// Next significant token at or after `i` (before `end`).
    pub fn sig(&self, mut i: usize, end: usize) -> usize {
        while i < end && self.is_trivia(i) {
            i += 1;
        }
        i.min(end)
    }

    /// Next significant token strictly after `i`.
    pub fn next(&self, i: usize, end: usize) -> usize {
        self.sig(i + 1, end)
    }

    /// Previous significant token strictly before `i` (`NONE` if none at or after `floor`).
    pub fn prev(&self, i: usize, floor: usize) -> usize {
        let mut j = i;
        while j > floor {
            j -= 1;
            if !self.is_trivia(j) {
                return j;
            }
        }
        NONE
    }

    /// Is token `i` the (case-insensitive) word `kw`?
    pub fn is(&self, i: usize, kw: &str) -> bool {
        i < self.t.len() && self.t[i].k == K::Word && self.text(i).eq_ignore_ascii_case(kw)
    }

    pub fn is_any(&self, i: usize, kws: &[&str]) -> bool {
        i < self.t.len() && self.t[i].k == K::Word && {
            let w = self.text(i);
            kws.iter().any(|k| w.eq_ignore_ascii_case(k))
        }
    }

    /// Is token `i` a word in `set`?
    pub fn in_set(&self, i: usize, set: &KwSet) -> bool {
        i < self.t.len() && self.t[i].k == K::Word && set.contains(self.text(i))
    }

    pub fn is_op(&self, i: usize, op: &str) -> bool {
        i < self.t.len() && self.t[i].k == K::Op && self.text(i) == op
    }

    /// Does the keyword sequence `kws` start at significant token `i`? Returns
    /// the index after the last keyword.
    pub fn seq(&self, i: usize, end: usize, kws: &[&str]) -> Option<usize> {
        let mut j = self.sig(i, end);
        for (n, kw) in kws.iter().enumerate() {
            if n > 0 {
                j = self.next(j, end);
            }
            if !self.is(j, kw) {
                return None;
            }
        }
        Some(j + 1)
    }

    /// First token in `a..b` at paren depth equal to `a`'s that is one of `kws`.
    pub fn find_kw(&self, a: usize, b: usize, kws: &[&str]) -> Option<usize> {
        let mut i = a;
        while i < b {
            match self.t[i].k {
                K::LParen if self.pair[i] != NONE => {
                    i = self.pair[i] + 1;
                    continue;
                }
                K::Word if self.is_any(i, kws) => return Some(i),
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Split `a..b` on top-level commas into token ranges.
    pub fn split_commas(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut start = a;
        let mut i = a;
        while i < b {
            match self.t[i].k {
                K::LParen if self.pair[i] != NONE && self.pair[i] < b => {
                    i = self.pair[i] + 1;
                    continue;
                }
                K::Comma => {
                    out.push((start, i));
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        out.push((start, b));
        out
    }

    /// Trim trivia from both ends of a range.
    pub fn trim(&self, mut a: usize, mut b: usize) -> (usize, usize) {
        while a < b && self.is_trivia(a) {
            a += 1;
        }
        while b > a && self.is_trivia(b - 1) {
            b -= 1;
        }
        (a, b)
    }

    /// Trimmed source text of a range.
    pub fn tslice(&self, a: usize, b: usize) -> &'a str {
        let (a, b) = self.trim(a, b);
        self.slice(a, b)
    }

    /// Does any token of kind `k` occur in `a..b`?
    pub fn has(&self, a: usize, b: usize, k: K) -> bool {
        self.t[a.min(b)..b].iter().any(|t| t.k == k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_and_opaque() {
        let sql = "SELECT 'it''s -- x', q'[a 'b' c]', \"Q\"\"d\", `b`, :1, ?, @v, 1.5e3, .5 -- c'\n/*+ h */ FROM t WHERE a <= b || c";
        let toks = lex(sql);
        let rebuilt: String = toks
            .iter()
            .map(|t| &sql[t.s as usize..t.e as usize])
            .collect();
        assert_eq!(rebuilt, sql);
        let kinds: Vec<K> = toks
            .iter()
            .filter(|t| !matches!(t.k, K::Ws))
            .map(|t| t.k)
            .collect();
        assert!(kinds.contains(&K::QStr));
        assert!(kinds.contains(&K::Bind));
        assert_eq!(kinds.iter().filter(|k| **k == K::Comment).count(), 2);
    }

    #[test]
    fn parens_pair() {
        let ts = Ts::new("f(a, (b), c)");
        assert_eq!(ts.pair[1], ts.len() - 1);
        assert_eq!(ts.split_commas(2, ts.len() - 1).len(), 3);
    }
}
