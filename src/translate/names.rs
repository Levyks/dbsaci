//! Oracle identifier semantics, applied once, lexically.
//!
//! Oracle decides an identifier's case when it *reads* the statement: every
//! unquoted word is upper-cased, a `"quoted"` identifier is taken exactly as
//! written. Whether the word turns out to be a table, a view, a sequence, a
//! table alias, a schema or a column does not matter. DbSaci mirrors that rule
//! with one pass over the statement's tokens ([`canonicalize`]) instead of
//! folding selected AST positions, so `CREATE VIEW v`, `DROP TABLE t`,
//! `REFERENCES t(id)`, `seq.NEXTVAL` and `SELECT E.ID FROM emp e` all reach the
//! backend under the same name.
//!
//! The *physical* name an Oracle identifier gets on the backend is decided by
//! [`IdentifierCase`]: `Upper` (default) stores the Oracle name itself, so the
//! backend schema *is* the Oracle schema and the data dictionary needs no
//! conversion; `Lower` stores Oracle-normal (all upper-case) names in lower
//! case for schemas authored in the MariaDB/PostgreSQL convention.
//! [`oracle_name`] is the inverse used when reporting backend names back to
//! the client (data dictionary, column labels).
//!
//! Strings, comments, `q'[..]'` literals, back-quoted identifiers and bind
//! placeholders pass through byte-for-byte.

/// How Oracle-normal identifiers are stored on a case-sensitive backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentifierCase {
    /// Physical name == Oracle name (`EMP`, `"MixedCase"`).
    #[default]
    Upper,
    /// Oracle-normal names are stored lower-case (`EMP` -> `emp`); names with
    /// lower-case letters (only reachable quoted) are kept verbatim.
    Lower,
}

impl IdentifierCase {
    /// The physical spelling of an Oracle-canonical identifier.
    pub fn physical(self, oracle: &str) -> String {
        match self {
            IdentifierCase::Upper => oracle.to_string(),
            IdentifierCase::Lower if !has_ascii_lower(oracle) => oracle.to_ascii_lowercase(),
            IdentifierCase::Lower => oracle.to_string(),
        }
    }

    /// Kept for callers that fold a bare word (unquoted spelling).
    #[allow(dead_code)]
    pub(crate) fn apply(self, s: &str) -> String {
        self.physical(&s.to_ascii_uppercase())
    }
}

impl std::str::FromStr for IdentifierCase {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "upper" => Ok(IdentifierCase::Upper),
            "lower" => Ok(IdentifierCase::Lower),
            other => Err(format!(
                "unknown identifier case {other:?} (want upper or lower)"
            )),
        }
    }
}

fn has_ascii_lower(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_lowercase())
}

/// The Oracle name a backend object with physical name `physical` has.
/// `Upper` is the identity; `Lower` upper-cases an all-lower-case name (the
/// image of an Oracle-normal name) and keeps anything else verbatim.
pub fn oracle_name(physical: &str, case: IdentifierCase) -> String {
    match case {
        IdentifierCase::Upper => physical.to_string(),
        IdentifierCase::Lower if !physical.bytes().any(|b| b.is_ascii_uppercase()) => {
            physical.to_ascii_uppercase()
        }
        IdentifierCase::Lower => physical.to_string(),
    }
}

/// `true` for the bytes Oracle allows inside an unquoted identifier.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#' || b >= 0x80
}

/// End (exclusive) of a `'…'` literal opening at `start` (`''` escapes).
fn end_of_single_quoted(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// End (exclusive) of a `"…"` / `` `…` `` quoted identifier (doubled quote escapes).
fn end_of_quoted(bytes: &[u8], start: usize, q: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == q {
            if bytes.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// End (exclusive) of an Oracle alternative-quote literal `q'X…X'` whose `'`
/// is at `quote`.
fn end_of_q_quote(bytes: &[u8], quote: usize) -> usize {
    let Some(&open) = bytes.get(quote + 1) else {
        return bytes.len();
    };
    let close = match open {
        b'[' => b']',
        b'{' => b'}',
        b'(' => b')',
        b'<' => b'>',
        c => c,
    };
    let mut i = quote + 2;
    while i + 1 < bytes.len() {
        if bytes[i] == close && bytes[i + 1] == b'\'' {
            return i + 2;
        }
        i += 1;
    }
    bytes.len()
}

/// A lexical piece of a statement, as far as identifier semantics care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    /// Unquoted word (keyword or identifier).
    Word,
    /// `"quoted identifier"` including the quotes.
    Quoted,
    /// Anything that must pass through verbatim: literals, comments,
    /// back-quoted names, binds, numbers, punctuation, whitespace.
    Opaque,
}

/// Split `sql` into `(piece, byte range)` runs. Every byte belongs to exactly
/// one piece, so concatenating the ranges reproduces the input.
pub fn pieces(sql: &str) -> Vec<(Piece, std::ops::Range<usize>)> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut opaque_start = 0;
    let flush = |out: &mut Vec<(Piece, std::ops::Range<usize>)>, from: usize, to: usize| {
        if to > from {
            out.push((Piece::Opaque, from..to));
        }
    };
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' => i = end_of_single_quoted(bytes, i),
            b'`' => i = end_of_quoted(bytes, i, b'`'),
            b'"' => {
                flush(&mut out, opaque_start, i);
                let end = end_of_quoted(bytes, i, b'"');
                out.push((Piece::Quoted, i..end));
                i = end;
                opaque_start = i;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = bytes[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(bytes.len(), |o| i + o);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = bytes[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map_or(bytes.len(), |o| i + 4 + o);
            }
            b':' | b'@' if bytes.get(i + 1).is_some_and(|&c| is_word_byte(c)) => {
                // bind placeholder / user variable: name stays verbatim
                i += 1;
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += 1;
                }
            }
            b'0'..=b'9' => {
                // An Oracle numeric literal: digits[.digits][e[+-]digits][f|d].
                // It ends there: `93as` is the number 93 then `AS` (ojdbc's
                // own getTypeInfo SQL relies on this).
                let digits = |i: &mut usize| {
                    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                        *i += 1;
                    }
                };
                digits(&mut i);
                if bytes.get(i) == Some(&b'.') {
                    i += 1;
                    digits(&mut i);
                }
                if matches!(bytes.get(i), Some(b'e' | b'E')) {
                    let mut j = i + 1;
                    if matches!(bytes.get(j), Some(b'+' | b'-')) {
                        j += 1;
                    }
                    if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                        i = j;
                        digits(&mut i);
                    }
                }
                if matches!(bytes.get(i), Some(b'f' | b'F' | b'd' | b'D'))
                    && !bytes.get(i + 1).is_some_and(|&c| is_word_byte(c))
                {
                    i += 1;
                }
            }
            _ if is_word_byte(b) => {
                let start = i;
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += 1;
                }
                // `q'…'` / `nq'…'` alternative quoting and `n'…'` / `x'…'`
                // literal prefixes belong to the literal that follows.
                if bytes.get(i) == Some(&b'\'') {
                    let word = &sql[start..i];
                    if word.eq_ignore_ascii_case("q") || word.eq_ignore_ascii_case("nq") {
                        i = end_of_q_quote(bytes, i);
                        continue;
                    }
                    if matches!(word.to_ascii_lowercase().as_str(), "n" | "x" | "b" | "u") {
                        i = end_of_single_quoted(bytes, i);
                        continue;
                    }
                }
                flush(&mut out, opaque_start, start);
                out.push((Piece::Word, start..i));
                opaque_start = i;
            }
            _ => {
                // advance one UTF-8 char
                i += sql[i..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    flush(&mut out, opaque_start, bytes.len());
    out
}

/// Apply Oracle's identifier rule to every identifier in `sql` and map the
/// result to its physical spelling: unquoted words become the physical form of
/// their upper-case spelling, quoted identifiers the physical form of their
/// exact content. Idempotent.
pub fn canonicalize(sql: &str, case: IdentifierCase) -> String {
    let mut out = String::with_capacity(sql.len());
    for (piece, range) in pieces(sql) {
        let text = &sql[range];
        match piece {
            Piece::Opaque => out.push_str(text),
            Piece::Word => {
                // A word glued to a number (`93as`): Oracle reads two tokens.
                if out.ends_with(|c: char| c.is_ascii_digit()) {
                    out.push(' ');
                }
                out.push_str(&case.physical(&text.to_ascii_uppercase()))
            }
            Piece::Quoted => {
                let inner = &text[1..text.len().saturating_sub(1).max(1)];
                if text.len() < 2 || !text.ends_with('"') {
                    out.push_str(text);
                    continue;
                }
                out.push('"');
                out.push_str(&case.physical(inner));
                out.push('"');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(s: &str) -> String {
        canonicalize(s, IdentifierCase::Upper)
    }
    fn low(s: &str) -> String {
        canonicalize(s, IdentifierCase::Lower)
    }

    #[test]
    fn folds_every_identifier_position() {
        assert_eq!(
            up("create view v_emp as select e.id from emp e"),
            "CREATE VIEW V_EMP AS SELECT E.ID FROM EMP E"
        );
        assert_eq!(up("drop table emp"), "DROP TABLE EMP");
        assert_eq!(
            up("create table d (e number references emp(id))"),
            "CREATE TABLE D (E NUMBER REFERENCES EMP(ID))"
        );
        assert_eq!(
            up("select my_seq.nextval from dual"),
            "SELECT MY_SEQ.NEXTVAL FROM DUAL"
        );
        assert_eq!(low("SELECT E.ID FROM Emp e"), "select e.id from emp e");
    }

    #[test]
    fn quoted_identifiers_are_exact() {
        assert_eq!(
            up(r#"select "MyCol" from "MyTab""#),
            r#"SELECT "MyCol" FROM "MyTab""#
        );
        assert_eq!(up(r#"select "abc" from t"#), r#"SELECT "abc" FROM T"#);
        // Lower: an Oracle-normal quoted name is the same object as unquoted.
        assert_eq!(
            low(r#"select "ABC", "MyCol" from t"#),
            r#"select "abc", "MyCol" from t"#
        );
    }

    #[test]
    fn literals_comments_binds_pass_through() {
        let sql = "select 'it''s TO_NUMBER(x), dual' -- Mixed comment\n, q'[a 'b' c]' /*+ Hint */ from t where a = :Bind and b = @Var and c = `Back`";
        assert_eq!(
            up(sql),
            "SELECT 'it''s TO_NUMBER(x), dual' -- Mixed comment\n, q'[a 'b' c]' /*+ Hint */ FROM T WHERE A = :Bind AND B = @Var AND C = `Back`"
        );
        assert_eq!(
            up("select n'abc', x'0aF' from t"),
            "SELECT n'abc', x'0aF' FROM T"
        );
        assert_eq!(
            up("select 1e10, 2.5d from dual"),
            "SELECT 1e10, 2.5d FROM DUAL"
        );
    }

    #[test]
    fn idempotent_and_utf8_safe() {
        for s in [
            "select émile, 'ç' from tâble",
            r#"select "x""y" from t"#,
            "select a from t -- unterminated 'quote",
        ] {
            let once = up(s);
            assert_eq!(up(&once), once);
            let once = low(s);
            assert_eq!(low(&once), once);
        }
    }

    #[test]
    fn inverse_reports_oracle_names() {
        assert_eq!(oracle_name("EMP", IdentifierCase::Upper), "EMP");
        assert_eq!(oracle_name("MyTab", IdentifierCase::Upper), "MyTab");
        assert_eq!(oracle_name("emp", IdentifierCase::Lower), "EMP");
        assert_eq!(oracle_name("MyTab", IdentifierCase::Lower), "MyTab");
        for name in ["EMP", "MyTab", "A_B$1"] {
            for case in [IdentifierCase::Upper, IdentifierCase::Lower] {
                assert_eq!(oracle_name(&case.physical(name), case), name);
            }
        }
    }
}
