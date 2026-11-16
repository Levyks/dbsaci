//! Oracle bind values → MariaDB prepared-statement parameters, and the
//! statement-batch contract with the translator.

use mysql_async::Value;

use crate::error::{Error, Result};
use crate::translate::names::{Piece, pieces};
use crate::wire::BindValue;

/// Prefix the translator puts on a batch statement whose row count must not
/// be reported to the client (temporary-table set-up for MERGE, …).
pub const NOCOUNT_MARKER: &str = "/* dbsaci:nocount */";

/// One Oracle bind as a MariaDB parameter.
///
/// * the empty string is NULL (Oracle has no empty `VARCHAR2`);
/// * an integral NUMBER that fits `i64` is sent as an integer, any other
///   NUMBER as its exact decimal text (MariaDB parses it as DECIMAL);
/// * DATE / TIMESTAMP binds arrive as Oracle literals and are sent as the
///   bare `YYYY-MM-DD HH:MI:SS[.f]` text MariaDB reads as DATETIME. A trailing
///   UTC offset (TIMESTAMP WITH TIME ZONE) is applied: the value is converted
///   to UTC wall-clock time.
pub fn bind_value(bind: &BindValue) -> Result<Value> {
    Ok(match bind {
        BindValue::Null => Value::NULL,
        BindValue::String(s) if s.is_empty() => Value::NULL,
        BindValue::String(s) => Value::Bytes(s.as_bytes().to_vec()),
        BindValue::Number(s) => match s.trim().parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => Value::Bytes(s.trim().as_bytes().to_vec()),
        },
        BindValue::Bytes(b) => Value::Bytes(b.clone()),
        BindValue::Boolean(b) => Value::Int(i64::from(*b)),
        BindValue::BinaryDouble(v) if v.is_finite() => Value::Double(*v),
        BindValue::BinaryDouble(_) => {
            return Err(Error::ora(
                1426,
                "numeric overflow: non-finite BINARY_DOUBLE bind",
            ));
        }
        BindValue::Temporal(s) => Value::Bytes(temporal_text(s).into_bytes()),
    })
}

/// `TIMESTAMP '2024-02-29 13:14:15 +05:00'` → `2024-02-29 08:14:15`.
fn temporal_text(literal: &str) -> String {
    let t = literal.trim();
    let inner = ["TIMESTAMPTZ ", "TIMESTAMP ", "DATE "]
        .iter()
        .find_map(|p| {
            t.get(..p.len())
                .filter(|h| h.eq_ignore_ascii_case(p))
                .map(|_| &t[p.len()..])
        })
        .unwrap_or(t)
        .trim()
        .trim_matches('\'')
        .trim();
    if let Some((head, tail)) = inner.rsplit_once(' ')
        && tail.starts_with(['+', '-'])
        && tail[1..].bytes().all(|b| b.is_ascii_digit() || b == b':')
    {
        let digits: String = tail[1..].chars().filter(char::is_ascii_digit).collect();
        let (h, m) = match digits.len() {
            1 | 2 => (digits.parse::<i64>().unwrap_or(0), 0),
            _ => (
                digits[..2].parse::<i64>().unwrap_or(0),
                digits[2..].parse::<i64>().unwrap_or(0),
            ),
        };
        let sign = if tail.starts_with('-') { -1 } else { 1 };
        if let Some(dt) = super::types::parse_datetime(head.trim_end()) {
            let utc = dt - chrono::Duration::minutes(sign * (h * 60 + m));
            return utc.format("%Y-%m-%d %H:%M:%S%.f").to_string();
        }
        return head.trim_end().to_string();
    }
    inner.to_string()
}

/// Number of `?` placeholders in `sql`, ignoring literals and comments.
pub fn count_placeholders(sql: &str) -> usize {
    pieces(sql)
        .into_iter()
        .filter(|(p, _)| *p == Piece::Opaque)
        .map(|(_, r)| opaque_placeholders(&sql[r]))
        .sum()
}

/// `?`s in an opaque run, skipping the string literals / comments inside it.
fn opaque_placeholders(text: &str) -> usize {
    let b = text.as_bytes();
    let mut n = 0;
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
                i += 1;
            }
            b'`' => {
                i += 1;
                while i < b.len() && b[i] != b'`' {
                    i += 1;
                }
                i += 1;
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
                i += 2;
            }
            b'?' => {
                n += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    n
}

/// One statement of a translated batch with its own parameters.
pub struct BatchPart {
    pub sql: String,
    pub params: Vec<Value>,
    /// Row count excluded from the client's total.
    pub nocount: bool,
}

/// Split a translator batch on `sep`, giving each statement the parameters
/// whose `?` it contains (in order).
pub fn split_batch(sql: &str, sep: &str, params: Vec<Value>) -> Result<Vec<BatchPart>> {
    let mut params = params.into_iter();
    let mut parts = Vec::new();
    for stmt in sql.split(sep).map(str::trim).filter(|s| !s.is_empty()) {
        let n = count_placeholders(stmt);
        let own: Vec<Value> = params.by_ref().take(n).collect();
        if own.len() != n {
            return Err(Error::ora(1008, "not all variables bound"));
        }
        let nocount = stmt.starts_with(NOCOUNT_MARKER);
        parts.push(BatchPart {
            sql: stmt.to_string(),
            params: own,
            nocount,
        });
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_follow_oracle_rules() {
        assert_eq!(
            bind_value(&BindValue::String(String::new())).unwrap(),
            Value::NULL
        );
        assert_eq!(
            bind_value(&BindValue::Number("42".into())).unwrap(),
            Value::Int(42)
        );
        assert_eq!(
            bind_value(&BindValue::Number("1.50".into())).unwrap(),
            Value::Bytes(b"1.50".to_vec())
        );
        assert_eq!(
            bind_value(&BindValue::Temporal(
                "TIMESTAMP '2024-02-29 13:14:15 +05:00'".into()
            ))
            .unwrap(),
            Value::Bytes(b"2024-02-29 08:14:15".to_vec())
        );
        assert_eq!(
            bind_value(&BindValue::Temporal("DATE '2024-02-29'".into())).unwrap(),
            Value::Bytes(b"2024-02-29".to_vec())
        );
    }

    #[test]
    fn placeholders_outside_literals() {
        assert_eq!(
            count_placeholders("SELECT ?, '?', \"?\", `?` -- ?\n, ? /* ? */"),
            2
        );
    }

    #[test]
    fn batch_distributes_params() {
        let parts = split_batch(
            "/* dbsaci:nocount */ CREATE TEMPORARY TABLE s AS SELECT ? a, ? b;;UPDATE t SET x = 1;;INSERT INTO t SELECT ? FROM s",
            ";;",
            vec![Value::Int(1), Value::Int(2), Value::Int(3)],
        )
        .unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].nocount);
        assert_eq!(parts[0].params.len(), 2);
        assert_eq!(parts[1].params.len(), 0);
        assert_eq!(parts[2].params, vec![Value::Int(3)]);
    }
}
