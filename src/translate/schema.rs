//! Type information the MariaDB translator may consult.
//!
//! Some Oracle semantics depend on operand types that the SQL text alone does
//! not reveal: `TRUNC(x)` (date or number), `x - 7` (date arithmetic or
//! number arithmetic), `TO_CHAR(x, fmt)`. Instead of guessing from column
//! names, the translator asks a [`SchemaInfo`] for the type of a column and
//! uses the type of each bind. The MariaDB backend supplies a cached
//! implementation over `information_schema`; tests use [`NoSchemaInfo`] or a
//! fixed map.

/// The Oracle-relevant class of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    Number,
    Text,
    /// Oracle `DATE` (MariaDB `DATETIME` without fractional seconds).
    Date,
    /// Oracle `TIMESTAMP` (MariaDB `DATETIME(n)`, n > 0).
    Timestamp,
    Binary,
    Unknown,
}

/// Column types by *physical* names (already canonicalized for the backend).
pub trait SchemaInfo: Send + Sync {
    /// Type of `table.column`; `schema` `None` means the session's current
    /// schema. `None` when the table or column is unknown.
    fn column_kind(&self, schema: Option<&str>, table: &str, column: &str) -> Option<ValueKind>;

    /// Bumped whenever the cached schema changes (DDL); part of any
    /// translation cache key.
    fn version(&self) -> u64 {
        0
    }
}

/// A schema provider that knows nothing.
pub struct NoSchemaInfo;

impl SchemaInfo for NoSchemaInfo {
    fn column_kind(&self, _: Option<&str>, _: &str, _: &str) -> Option<ValueKind> {
        None
    }
}

/// A fixed in-memory schema: `(schema, table, column, kind)` rows, for tests.
pub struct StaticSchemaInfo(pub Vec<(Option<String>, String, String, ValueKind)>);

impl SchemaInfo for StaticSchemaInfo {
    fn column_kind(&self, schema: Option<&str>, table: &str, column: &str) -> Option<ValueKind> {
        self.0
            .iter()
            .find(|(s, t, c, _)| {
                s.as_deref() == schema
                    && t.eq_ignore_ascii_case(table)
                    && c.eq_ignore_ascii_case(column)
            })
            .map(|(_, _, _, k)| *k)
    }
}
