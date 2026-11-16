//! `INSTEAD OF INSERT` triggers on views.
//!
//! MariaDB has no `INSTEAD OF` triggers. DbSaci keeps the trigger body for the
//! session and runs it in place of a single-row `INSERT INTO view (cols)
//! VALUES (…)`, with `:NEW.col` replaced by the inserted value. `INSTEAD OF
//! UPDATE` / `DELETE` (which need per-row `:OLD` values) and multi-row inserts
//! are rejected with an Oracle error rather than run with wrong semantics.

use std::collections::HashMap;

use mysql_async::Value;

use crate::error::{Error, Result};
use crate::translate::names::{Piece, pieces};

#[derive(Debug, Clone)]
pub struct InsteadOfTrigger {
    pub event: String,
    pub body: String,
}

/// `CREATE [OR REPLACE] TRIGGER name INSTEAD OF <event> ON <view> … BEGIN body END`
/// → `(view, trigger)`. Only a statement that *is* such a trigger definition
/// matches (a comment or string mentioning "instead of" does not).
pub fn parse(sql: &str) -> Option<(String, InsteadOfTrigger)> {
    let words: Vec<(usize, &str)> = pieces(sql)
        .into_iter()
        .filter(|(p, _)| *p != Piece::Opaque)
        .map(|(_, r)| (r.start, &sql[r]))
        .take(12)
        .collect();
    let w = |i: usize| words.get(i).map(|(_, s)| s.to_ascii_uppercase());
    let mut i = 0;
    if w(i)? != "CREATE" {
        return None;
    }
    i += 1;
    if w(i)? == "OR" && w(i + 1)? == "REPLACE" {
        i += 2;
    }
    if w(i)? != "TRIGGER" {
        return None;
    }
    i += 2; // TRIGGER name
    if w(i)? != "INSTEAD" || w(i + 1)? != "OF" {
        return None;
    }
    let event = w(i + 2)?;
    if w(i + 3)? != "ON" {
        return None;
    }
    let view = words.get(i + 4)?.1.trim_matches('"').to_string();
    let body = begin_body(sql)?;
    Some((view.to_ascii_uppercase(), InsteadOfTrigger { event, body }))
}

/// Statements between the outermost `BEGIN` and its final `END`.
fn begin_body(sql: &str) -> Option<String> {
    let ps = pieces(sql);
    let begin = ps
        .iter()
        .find(|(p, r)| *p == Piece::Word && sql[r.clone()].eq_ignore_ascii_case("BEGIN"))?
        .1
        .end;
    let end = ps
        .iter()
        .rev()
        .find(|(p, r)| *p == Piece::Word && sql[r.clone()].eq_ignore_ascii_case("END"))?
        .1
        .start;
    (end > begin).then(|| {
        sql[begin..end]
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string()
    })
}

/// If `sql` is DML against a view with an `INSTEAD OF` trigger, the statement
/// list to run instead (`Ok(Some)`), an Oracle error for an unsupported shape,
/// or `Ok(None)` when no trigger applies.
pub fn expand(
    sql: &str,
    params: &[Value],
    triggers: &HashMap<String, InsteadOfTrigger>,
) -> Result<Option<String>> {
    if triggers.is_empty() {
        return Ok(None);
    }
    let words: Vec<String> = pieces(sql)
        .into_iter()
        .filter(|(p, _)| *p != Piece::Opaque)
        .take(4)
        .map(|(_, r)| sql[r].trim_matches('"').to_ascii_uppercase())
        .collect();
    let (event, target) = match words.as_slice() {
        [a, b, t, ..] if a == "INSERT" && b == "INTO" => ("INSERT", t),
        [a, t, ..] if a == "UPDATE" => ("UPDATE", t),
        [a, b, t, ..] if a == "DELETE" && b == "FROM" => ("DELETE", t),
        _ => return Ok(None),
    };
    let view = target.rsplit('.').next().unwrap_or(target);
    let Some(trigger) = triggers.get(view) else {
        return Ok(None);
    };
    if !trigger.event.eq_ignore_ascii_case(event) {
        return Ok(None);
    }
    if event != "INSERT" {
        return Err(Error::ora(
            3001,
            format!("INSTEAD OF {event} triggers are not supported on MariaDB"),
        ));
    }
    let sql = inline_params(sql, params);
    let Some(values) = insert_values(&sql) else {
        return Err(Error::ora(
            3001,
            "INSTEAD OF INSERT supports only INSERT INTO view (columns) VALUES (...)",
        ));
    };
    Ok(Some(substitute_new(&trigger.body, &values)))
}

/// Replace each `?` (outside literals) with the SQL literal of its parameter.
fn inline_params(sql: &str, params: &[Value]) -> String {
    if params.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut it = params.iter();
    for (piece, r) in pieces(sql) {
        let text = &sql[r];
        if piece != Piece::Opaque {
            out.push_str(text);
            continue;
        }
        let mut in_lit = false;
        for ch in text.chars() {
            if ch == '\'' {
                in_lit = !in_lit;
            }
            if ch == '?' && !in_lit {
                out.push_str(&it.next().map_or_else(|| "NULL".to_string(), literal));
            } else {
                out.push(ch);
            }
        }
    }
    out
}

fn literal(v: &Value) -> String {
    match v {
        Value::NULL => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Bytes(b) => format!("'{}'", String::from_utf8_lossy(b).replace('\'', "''")),
        other => format!(
            "'{}'",
            other.as_sql(true).trim_matches('\'').replace('\'', "''")
        ),
    }
}

/// `INSERT INTO v (a, b) VALUES (x, y)` → `{A: x, B: y}`.
fn insert_values(sql: &str) -> Option<HashMap<String, String>> {
    let up = sql.to_ascii_uppercase();
    let values_at = up.find("VALUES")?;
    let head = &sql[..values_at];
    let open = head.find('(')?;
    let close = head.rfind(')')?;
    let cols: Vec<String> = head[open + 1..close]
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_ascii_uppercase())
        .collect();
    let tail = sql[values_at + 6..].trim();
    let inner = tail.strip_prefix('(')?;
    let vals = split_top_level(inner)?;
    (cols.len() == vals.len()).then(|| cols.into_iter().zip(vals).collect())
}

/// Split `a, f(b, c), 'x,y')…` at top-level commas up to the closing paren.
fn split_top_level(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut in_lit = false;
    for ch in s.chars() {
        match ch {
            '\'' => {
                in_lit = !in_lit;
                cur.push(ch);
            }
            '(' if !in_lit => {
                depth += 1;
                cur.push(ch);
            }
            ')' if !in_lit && depth == 0 => {
                out.push(cur.trim().to_string());
                return Some(out);
            }
            ')' if !in_lit => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if !in_lit && depth == 0 => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(ch),
        }
    }
    None
}

/// Replace `:NEW.col` / `NEW.col` (outside literals) in one pass; the inserted
/// value text is never re-scanned, so a value containing `NEW.x` is inert.
fn substitute_new(body: &str, values: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    for (piece, r) in pieces(body) {
        let text = &body[r];
        // A column word right after `NEW.` / `:NEW.` (`:NEW` lexes as a bind
        // name, `NEW` as a word; either way the output so far ends with it).
        if piece != Piece::Opaque
            && let Some(v) = values.get(&text.trim_matches('"').to_ascii_uppercase())
        {
            let up = out.to_ascii_uppercase();
            let prefix = [":NEW.", "NEW."].into_iter().find(|p| up.ends_with(p));
            if let Some(p) = prefix {
                let keep = out.len() - p.len();
                let boundary_ok = p.starts_with(':')
                    || !out[..keep]
                        .chars()
                        .last()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                if boundary_ok {
                    out.truncate(keep);
                    out.push_str(v);
                    continue;
                }
            }
        }
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_real_trigger_definitions() {
        assert!(parse("COMMENT ON COLUMN t.c IS 'use x instead of y on z'").is_none());
        let (view, t) = parse(
            "CREATE OR REPLACE TRIGGER trg_io INSTEAD OF INSERT ON trg_v FOR EACH ROW BEGIN INSERT INTO trg_base (id, name) VALUES (:NEW.id, UPPER(:NEW.name)); END;",
        )
        .unwrap();
        assert_eq!(view, "TRG_V");
        assert_eq!(t.event, "INSERT");
        assert_eq!(
            t.body,
            "INSERT INTO trg_base (id, name) VALUES (:NEW.id, UPPER(:NEW.name))"
        );
    }

    #[test]
    fn expands_insert_once_without_rescanning_values() {
        let mut m = HashMap::new();
        m.insert(
            "TRG_V".to_string(),
            InsteadOfTrigger {
                event: "INSERT".into(),
                body: "INSERT INTO trg_base (id, name) VALUES (:NEW.id, UPPER(:NEW.name))".into(),
            },
        );
        let out = expand(
            "INSERT INTO TRG_V (ID, NAME) VALUES (1, 'see NEW.name')",
            &[],
            &m,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            out,
            "INSERT INTO trg_base (id, name) VALUES (1, UPPER('see NEW.name'))"
        );
        let out = expand(
            "INSERT INTO TRG_V (ID, NAME) VALUES (?, ?)",
            &[Value::Int(2), Value::Bytes(b"o'brien".to_vec())],
            &m,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            out,
            "INSERT INTO trg_base (id, name) VALUES (2, UPPER('o''brien'))"
        );
        assert!(
            expand("UPDATE TRG_V SET NAME = 'x'", &[], &m)
                .unwrap()
                .is_none()
        );
    }
}
