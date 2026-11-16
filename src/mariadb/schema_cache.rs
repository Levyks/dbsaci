//! Column types for type-aware translation, cached per process.
//!
//! The translator asks "is `ORDERS.CREATED` a date?" to lower `TRUNC(x)`,
//! `x - 7` and `TO_CHAR(x, …)` correctly. The answer comes from
//! `information_schema`, fetched once per table and kept until a DbSaci session
//! runs DDL (which clears the cache).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::translate::names::{Piece, pieces};
use crate::translate::schema::{SchemaInfo, ValueKind};

type Columns = Arc<HashMap<String, ValueKind>>;

/// `(physical schema, physical table)` → column kinds (keys upper-case).
static CACHE: LazyLock<Mutex<HashMap<(String, String), Columns>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Forget everything (after DDL).
pub fn invalidate() {
    VERSION.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut c) = CACHE.lock() {
        c.clear();
    }
}

/// Current cache generation (part of translation cache keys).
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

pub fn get(schema: &str, table: &str) -> Option<Columns> {
    CACHE
        .lock()
        .ok()?
        .get(&(schema.to_string(), table.to_ascii_uppercase()))
        .cloned()
}

pub fn put(schema: &str, table: &str, cols: &[(String, String, u32)]) {
    let map: HashMap<String, ValueKind> = cols
        .iter()
        .map(|(c, t, p)| (c.to_ascii_uppercase(), kind_of(t, *p)))
        .collect();
    if let Ok(mut c) = CACHE.lock() {
        c.insert(
            (schema.to_string(), table.to_ascii_uppercase()),
            Arc::new(map),
        );
    }
}

/// `information_schema.COLUMNS.DATA_TYPE` (+ `DATETIME_PRECISION`) → kind.
pub fn kind_of(data_type: &str, datetime_precision: u32) -> ValueKind {
    match data_type.to_ascii_lowercase().as_str() {
        "decimal" | "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "double"
        | "float" | "year" | "bit" => ValueKind::Number,
        "datetime" | "timestamp" if datetime_precision > 0 => ValueKind::Timestamp,
        "datetime" | "timestamp" | "date" => ValueKind::Date,
        "char" | "varchar" | "text" | "tinytext" | "mediumtext" | "longtext" | "enum" | "set"
        | "json" | "time" => ValueKind::Text,
        "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" => {
            ValueKind::Binary
        }
        _ => ValueKind::Unknown,
    }
}

/// Tables a statement reads or writes: the name after `FROM`, `JOIN`,
/// `UPDATE`, `INTO` (and each `,`-separated entry of a FROM list), as
/// `(schema, table)` in the statement's spelling.
pub fn referenced_tables(sql: &str) -> Vec<(Option<String>, String)> {
    let ps = pieces(sql);
    let text = |i: usize| ps.get(i).map_or("", |(_, r)| &sql[r.clone()]);
    let is_word = |i: usize| ps.get(i).is_some_and(|(p, _)| *p != Piece::Opaque);
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let mut i = 0;
    while i < ps.len() {
        let w = text(i).to_ascii_uppercase();
        if is_word(i) && matches!(w.as_str(), "FROM" | "JOIN" | "UPDATE" | "INTO") {
            // skip whitespace-only opaque runs
            let mut j = i + 1;
            while j < ps.len() && !is_word(j) && text(j).trim().is_empty() {
                j += 1;
            }
            if is_word(j) {
                let first = text(j).trim_matches('"').to_string();
                let entry = if text(j + 1) == "." && is_word(j + 2) {
                    (Some(first), text(j + 2).trim_matches('"').to_string())
                } else {
                    (None, first)
                };
                if !entry.1.eq_ignore_ascii_case("DUAL") && !out.contains(&entry) {
                    out.push(entry);
                }
            }
            i = j;
        }
        i += 1;
        if out.len() >= 16 {
            break;
        }
    }
    out
}

/// The column kinds of the tables one statement uses.
pub struct Snapshot {
    tables: Vec<(Option<String>, String, Columns)>,
}

impl Snapshot {
    pub fn new(tables: Vec<(Option<String>, String, Columns)>) -> Self {
        Snapshot { tables }
    }
}

impl SchemaInfo for Snapshot {
    fn column_kind(&self, schema: Option<&str>, table: &str, column: &str) -> Option<ValueKind> {
        self.tables
            .iter()
            .find(|(s, t, _)| {
                t.eq_ignore_ascii_case(table)
                    && match (s.as_deref(), schema) {
                        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                        (None, None) => true,
                        _ => false,
                    }
            })
            .and_then(|(_, _, cols)| cols.get(&column.to_ascii_uppercase()).copied())
    }

    fn version(&self) -> u64 {
        version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_referenced_tables() {
        assert_eq!(
            referenced_tables(
                "SELECT * FROM ORDERS o JOIN HR.EMP e ON 1=1 WHERE x IN (SELECT 1 FROM DUAL)"
            ),
            vec![(None, "ORDERS".into()), (Some("HR".into()), "EMP".into())]
        );
        assert_eq!(
            referenced_tables("INSERT INTO T (a) SELECT b FROM U"),
            vec![(None, "T".into()), (None, "U".into())]
        );
        assert!(referenced_tables("SELECT 'FROM x' FROM DUAL").is_empty());
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("datetime", 0), ValueKind::Date);
        assert_eq!(kind_of("datetime", 6), ValueKind::Timestamp);
        assert_eq!(kind_of("decimal", 0), ValueKind::Number);
        assert_eq!(kind_of("varchar", 0), ValueKind::Text);
    }
}
