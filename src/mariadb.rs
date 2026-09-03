//! MariaDB backend primitives.
//!
//! MariaDB's `SQL_MODE=ORACLE` performs the Oracle-language work in the
//! database. The adapter keeps one backend connection per Oracle session so
//! transactions, temporary objects, and session settings retain their state.

use std::sync::Arc;

use mysql_async::{Conn, Opts, Params, Value, prelude::Queryable};

use crate::backend::{DescribeCaps, OracleBackend, OracleCursor};
use crate::error::{Error, Result};
use crate::wire::{BindValue, ColumnMeta, encode_oracle_number_decimal};

/// A MariaDB connection configured for Oracle compatibility mode.
pub struct MariaDbBackend {
    conn: tokio::sync::Mutex<Conn>,
}

struct MariaDbCursor {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
    offset: usize,
}

#[async_trait::async_trait]
impl OracleCursor for MariaDbCursor {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
    fn is_exhausted(&self) -> bool {
        self.offset >= self.rows.len()
    }
    async fn next_batch(&mut self, n: usize) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
        let end = (self.offset + n).min(self.rows.len());
        let batch = self.rows[self.offset..end].to_vec();
        self.offset = end;
        Ok(batch)
    }
    async fn finish(&mut self) {
        self.offset = self.rows.len();
    }
}

impl MariaDbBackend {
    /// Connect and enable MariaDB's Oracle compatibility mode for this session.
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            // Oracle usernames are case-insensitive and the server passes the
            // authenticated name in uppercase; MariaDB account names are not.
            urlencoding(&user.to_lowercase()),
            urlencoding(password),
            host,
            port,
            urlencoding(database),
        );
        let opts = Opts::from_url(&url)
            .map_err(|e| Error::Postgres(format!("invalid MariaDB connection URL: {e}")))?;
        let mut conn = Conn::new(opts)
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB connection failed: {e}")))?;
        conn.query_drop("SET sql_mode = 'ORACLE'")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB Oracle mode failed: {e}")))?;
        conn.query_drop("SET NAMES utf8mb4")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB charset setup failed: {e}")))?;
        // SQL_MODE=ORACLE supplies syntax and built-ins, but it does not
        // provide Oracle's catalog views. Install the small portable core
        // used by the corpus and by common migration tools in this schema.
        for ddl in [
            "CREATE OR REPLACE VIEW user_tables AS SELECT UPPER(table_name) AS table_name FROM information_schema.tables WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE'",
            "CREATE OR REPLACE VIEW all_tables AS SELECT UPPER(table_schema) AS owner, UPPER(table_name) AS table_name FROM information_schema.tables WHERE table_type = 'BASE TABLE'",
            "CREATE OR REPLACE VIEW user_tab_columns AS SELECT UPPER(table_name) AS table_name, UPPER(column_name) AS column_name, ordinal_position AS column_id FROM information_schema.columns WHERE table_schema = DATABASE()",
            "CREATE OR REPLACE VIEW all_tab_columns AS SELECT UPPER(table_schema) AS owner, UPPER(table_name) AS table_name, UPPER(column_name) AS column_name, ordinal_position AS column_id FROM information_schema.columns",
            "CREATE OR REPLACE VIEW user_objects AS SELECT UPPER(table_name) AS object_name, 'TABLE' AS object_type FROM information_schema.tables WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE'",
            "CREATE OR REPLACE VIEW user_sequences AS SELECT UPPER(table_name) AS sequence_name FROM information_schema.tables WHERE table_schema = DATABASE() AND table_type = 'SEQUENCE'",
            "CREATE OR REPLACE VIEW all_sequences AS SELECT UPPER(table_schema) AS sequence_owner, UPPER(table_name) AS sequence_name FROM information_schema.tables WHERE table_type = 'SEQUENCE'",
        ] {
            conn.query_drop(ddl)
                .await
                .map_err(|e| Error::Postgres(format!("MariaDB catalog setup failed: {e}")))?;
        }
        conn.query_drop("START TRANSACTION")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB transaction setup failed: {e}")))?;
        Ok(Self {
            conn: tokio::sync::Mutex::new(conn),
        })
    }

    /// Lightweight connectivity probe.
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.lock().await;
        conn.query_drop("SELECT 1")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB query failed: {e}")))
    }
}

#[async_trait::async_trait]
impl OracleBackend for Arc<MariaDbBackend> {
    async fn open_cursor(
        &self,
        sql: &str,
        binds: &[BindValue],
        _caps: DescribeCaps,
    ) -> Result<Box<dyn OracleCursor>> {
        let mut conn = self.conn.lock().await;
        conn.query_drop("SET sql_mode = 'ORACLE'")
            .await
            .map_err(mariadb_error)?;
        let sql = mariadb_sql(sql);
        let rows: Vec<mysql_async::Row> = if binds.is_empty() {
            conn.query(&sql).await.map_err(mariadb_error)?
        } else {
            conn.exec(&sql, Params::Positional(bind_values(binds)?))
                .await
                .map_err(mariadb_error)?
        };
        let columns = rows
            .first()
            .map(|row| {
                row.columns_ref()
                    .iter()
                    .map(|col| {
                        let name = col.name_str().into_owned();
                        match col.column_type() {
                            mysql_async::consts::ColumnType::MYSQL_TYPE_FLOAT
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_DOUBLE
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_DECIMAL
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONGLONG
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONG
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_SHORT
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_TINY => {
                                ColumnMeta::number(name, 38, 0)
                            }
                            _ => ColumnMeta::varchar(name, 4000),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let encoded = rows
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|i| {
                        row.as_ref(i)
                            .map(|value| {
                                encode_value_for_column(
                                    value,
                                    is_numeric_column(row.columns_ref()[i].column_type()),
                                )
                            })
                            .transpose()
                    })
                    .collect()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Box::new(MariaDbCursor {
            columns,
            rows: encoded,
            offset: 0,
        }))
    }

    async fn execute_simple(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        let command = sql.trim().trim_end_matches(';');
        if command.eq_ignore_ascii_case("COMMIT") {
            conn.query_drop("COMMIT").await.map_err(mariadb_error)?;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        if command.eq_ignore_ascii_case("ROLLBACK") {
            conn.query_drop("ROLLBACK").await.map_err(mariadb_error)?;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        if command.eq_ignore_ascii_case("BEGIN")
            || command.eq_ignore_ascii_case("START TRANSACTION")
        {
            return Ok(0);
        }
        let upper = command.to_ascii_uppercase();
        if upper.starts_with("SAVEPOINT ")
            || upper.starts_with("RELEASE SAVEPOINT ")
            || upper.starts_with("ROLLBACK TO ")
            || upper.starts_with("SET TRANSACTION ")
        {
            conn.query_drop(mariadb_sql(command))
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        conn.query_drop("SET sql_mode = 'ORACLE'")
            .await
            .map_err(mariadb_error)?;
        conn.query_drop("SAVEPOINT pgsaci_statement")
            .await
            .map_err(mariadb_error)?;
        let result = conn
            .exec_iter(
                mariadb_sql(command),
                Params::Positional(bind_values(binds)?),
            )
            .await;
        match result {
            Ok(result) => {
                let affected = result.affected_rows();
                conn.query_drop("RELEASE SAVEPOINT pgsaci_statement")
                    .await
                    .map_err(mariadb_error)?;
                Ok(affected)
            }
            Err(error) => {
                let _ = conn
                    .query_drop("ROLLBACK TO SAVEPOINT pgsaci_statement")
                    .await;
                let _ = conn.query_drop("RELEASE SAVEPOINT pgsaci_statement").await;
                Err(mariadb_error(error))
            }
        }
    }

    async fn execute_ddl(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        conn.query_drop("COMMIT").await.map_err(mariadb_error)?;
        let result = conn
            .exec_iter(mariadb_sql(sql), Params::Positional(bind_values(binds)?))
            .await
            .map_err(mariadb_error)?;
        let affected = result.affected_rows();
        conn.query_drop("COMMIT").await.map_err(mariadb_error)?;
        conn.query_drop("START TRANSACTION")
            .await
            .map_err(mariadb_error)?;
        Ok(affected)
    }

    async fn execute_returning(
        &self,
        _sql: &str,
        _binds: &[BindValue],
    ) -> Result<(u64, Vec<Vec<Option<Vec<u8>>>>)> {
        Err(Error::Postgres(
            "MariaDB RETURNING adapter not implemented".into(),
        ))
    }

    async fn cancel(&self) {}
}

fn mariadb_error(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(server) => {
            Error::Postgres(format!("{}: {}", server.state, server.message))
        }
        other => Error::Postgres(format!("MariaDB error: {other}")),
    }
}

fn bind_values(binds: &[BindValue]) -> Result<Vec<Value>> {
    binds
        .iter()
        .map(|bind| match bind {
            BindValue::Null => Ok(Value::NULL),
            BindValue::String(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
            BindValue::Number(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
            BindValue::Bytes(b) => Ok(Value::Bytes(b.clone())),
            BindValue::Temporal(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
            BindValue::Boolean(b) => Ok(Value::Int(i64::from(*b))),
            BindValue::BinaryDouble(v) if v.is_finite() => Ok(Value::Double(*v)),
            BindValue::BinaryDouble(_) => Err(Error::DataConversionError(
                "non-finite floating bind is unsupported".into(),
            )),
        })
        .collect()
}

/// PgSaci's bind rewriter emits PostgreSQL-style `$1`, `$2`, … placeholders;
/// MariaDB prepared statements use `?`. Preserve quoted SQL while converting
/// only actual parameter tokens.
fn mariadb_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            out.push(b as char);
            if b == q {
                if bytes.get(i + 1) == Some(&q) {
                    out.push(q as char);
                    i += 1;
                } else {
                    quote = None;
                }
            }
            i += 1;
        } else if matches!(b, b'\'' | b'"' | b'`') {
            quote = Some(b);
            out.push(b as char);
            i += 1;
        } else if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            out.push('?');
            i += 2;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    // The shared bind rewriter annotates PostgreSQL parameters with casts.
    // MariaDB infers these from the bound value and treats `::name` as a
    // named-parameter marker, so remove only casts attached to `?`.
    for ty in [
        "text",
        "numeric",
        "bytea",
        "timestamp",
        "timestamptz",
        "boolean",
        "double precision",
    ] {
        out = out.replace(&format!("?::{ty}"), "?");
    }
    out
}

fn encode_value(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::NULL => Ok(Vec::new()),
        Value::Bytes(bytes) => Ok(bytes.clone()),
        Value::Int(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::UInt(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::Float(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::Double(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::Date(y, m, d, h, min, s, _) => Ok(vec![
            (*y / 100 + 100) as u8,
            (*y % 100 + 100) as u8,
            *m,
            *d,
            h.saturating_add(1),
            min.saturating_add(1),
            s.saturating_add(1),
        ]),
        Value::Time(_, _, h, min, s, _) => Ok(vec![
            100,
            100,
            1,
            1,
            h.saturating_add(1),
            min.saturating_add(1),
            s.saturating_add(1),
        ]),
    }
}

fn encode_value_for_column(value: &Value, number: bool) -> Result<Vec<u8>> {
    if number {
        return match value {
            Value::Bytes(bytes) => encode_oracle_number_decimal(
                std::str::from_utf8(bytes)
                    .map_err(|e| Error::DataConversionError(e.to_string()))?,
            ),
            _ => encode_value(value),
        };
    }
    encode_value(value)
}

fn is_numeric_column(kind: mysql_async::consts::ColumnType) -> bool {
    matches!(
        kind,
        mysql_async::consts::ColumnType::MYSQL_TYPE_FLOAT
            | mysql_async::consts::ColumnType::MYSQL_TYPE_DOUBLE
            | mysql_async::consts::ColumnType::MYSQL_TYPE_DECIMAL
            | mysql_async::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONGLONG
            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONG
            | mysql_async::consts::ColumnType::MYSQL_TYPE_SHORT
            | mysql_async::consts::ColumnType::MYSQL_TYPE_TINY
    )
}

/// Percent-encode the small set of URL-significant bytes accepted in
/// configured credentials/database names without adding another URL parser.
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::mariadb_sql;

    #[test]
    fn converts_backend_parameters_but_not_literals_or_identifiers() {
        assert_eq!(
            mariadb_sql("SELECT $1, '$2', \"$3\", `$4` WHERE x = $12"),
            "SELECT ?, '$2', \"$3\", `$4` WHERE x = ?"
        );
    }
}
