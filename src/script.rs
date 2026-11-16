//! Splitting Oracle SQL scripts (SQL*Plus style) into statements, for
//! `dbsaci migrate`.
//!
//! * A SQL statement ends at a top-level `;`.
//! * A PL/SQL unit (`CREATE [OR REPLACE] PROCEDURE/FUNCTION/PACKAGE/TRIGGER/
//!   TYPE`, an anonymous `DECLARE`/`BEGIN` block) ends at a line holding only
//!   `/`; its inner `;`s belong to it.
//! * SQL*Plus commands (`SET`, `PROMPT`, `SPOOL`, `REM`, `@file`, `WHENEVER`,
//!   `EXIT`, …) are skipped, as are a lone `/` after a plain statement.

use crate::translate::names::{Piece, pieces};

const SQLPLUS: &[&str] = &[
    "SET",
    "PROMPT",
    "SPOOL",
    "REM",
    "REMARK",
    "WHENEVER",
    "EXIT",
    "QUIT",
    "DEFINE",
    "UNDEFINE",
    "COLUMN",
    "COL",
    "TTITLE",
    "BTITLE",
    "BREAK",
    "COMPUTE",
    "SHOW",
    "CONNECT",
    "CONN",
    "DISCONNECT",
    "EXEC",
    "EXECUTE",
    "PAUSE",
    "ACCEPT",
    "HOST",
    "CLEAR",
    "STORE",
    "START",
];

/// The statements of `script`, without terminators.
pub fn split_oracle_script(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_plsql = false;
    for line in script.lines() {
        let trimmed = line.trim();
        if cur.trim().is_empty() {
            cur.clear();
            let first = trimmed
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if trimmed.is_empty()
                || trimmed == "/"
                || trimmed.starts_with("--")
                || trimmed.starts_with('@')
                || (SQLPLUS.contains(&first.as_str())
                    && !(first == "SET" && is_set_transaction(trimmed)))
            {
                continue;
            }
            in_plsql = starts_plsql_unit(trimmed);
        }
        if in_plsql {
            if trimmed == "/" {
                push(&mut out, &cur);
                cur.clear();
                in_plsql = false;
            } else {
                cur.push_str(line);
                cur.push('\n');
            }
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
        // Split at top-level `;` (outside literals / comments).
        while let Some(at) = top_level_semicolon(&cur) {
            let stmt = cur[..at].to_string();
            let rest = cur[at + 1..].to_string();
            push(&mut out, &stmt);
            cur = rest;
            if starts_plsql_unit(cur.trim()) {
                in_plsql = true;
                break;
            }
        }
    }
    push(&mut out, &cur);
    out
}

fn is_set_transaction(line: &str) -> bool {
    line.to_ascii_uppercase()
        .split_whitespace()
        .nth(1)
        .is_some_and(|w| w == "TRANSACTION")
}

fn push(out: &mut Vec<String>, stmt: &str) {
    let s = stmt.trim().trim_end_matches(';').trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
}

fn starts_plsql_unit(text: &str) -> bool {
    let words: Vec<String> = pieces(text)
        .into_iter()
        .filter(|(p, _)| *p == Piece::Word)
        .take(6)
        .map(|(_, r)| text[r].to_ascii_uppercase())
        .collect();
    match words.first().map(String::as_str) {
        Some("DECLARE" | "BEGIN") => true,
        Some("CREATE") => words.iter().skip(1).any(|w| {
            matches!(
                w.as_str(),
                "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE"
            )
        }),
        _ => false,
    }
}

/// Byte offset of the first `;` outside literals, quoted identifiers and comments.
fn top_level_semicolon(text: &str) -> Option<usize> {
    let mut offset = 0;
    for (piece, r) in pieces(text) {
        if piece == Piece::Opaque {
            let chunk = &text[r.clone()];
            let b = chunk.as_bytes();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'\'' => {
                        i += 1;
                        while i < b.len() {
                            if b[i] == b'\'' {
                                if b.get(i + 1) == Some(&b'\'') {
                                    i += 2;
                                    continue;
                                }
                                break;
                            }
                            i += 1;
                        }
                    }
                    b'-' if b.get(i + 1) == Some(&b'-') => {
                        while i < b.len() && b[i] != b'\n' {
                            i += 1;
                        }
                    }
                    b'/' if b.get(i + 1) == Some(&b'*') => {
                        i += 2;
                        while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                            i += 1;
                        }
                        i += 1;
                    }
                    b';' => return Some(r.start + i),
                    _ => {}
                }
                i += 1;
            }
        }
        offset = r.end;
    }
    let _ = offset;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sql_and_plsql_units() {
        let script = "\
SET ECHO OFF
PROMPT creating tables
CREATE TABLE a (id NUMBER, s VARCHAR2(10) DEFAULT 'x;y');
-- comment ; here
INSERT INTO a VALUES (1, 'a''b;c'); INSERT INTO a VALUES (2, 'z');
CREATE OR REPLACE PROCEDURE p AS
BEGIN
  INSERT INTO a VALUES (3, 'p');
END;
/
BEGIN
  p;
END;
/
COMMIT;
";
        let s = split_oracle_script(script);
        assert_eq!(s.len(), 6, "{s:#?}");
        assert_eq!(
            s[0],
            "CREATE TABLE a (id NUMBER, s VARCHAR2(10) DEFAULT 'x;y')"
        );
        assert_eq!(s[1], "INSERT INTO a VALUES (1, 'a''b;c')");
        assert_eq!(s[2], "INSERT INTO a VALUES (2, 'z')");
        assert!(s[3].starts_with("CREATE OR REPLACE PROCEDURE p AS"));
        assert!(s[3].ends_with("END"));
        assert!(s[4].starts_with("BEGIN"));
        assert_eq!(s[5], "COMMIT");
    }
}
