//! MariaDB backend primitives.
//!
//! MariaDB's `SQL_MODE=ORACLE` performs the Oracle-language work in the
//! database. This module deliberately starts with connection/session setup;
//! query execution will be moved behind the same backend contract as the
//! PostgreSQL implementation in a later step.

use std::sync::Arc;

use mysql_async::{Opts, Params, Pool, Value, prelude::Queryable};

use crate::backend::{DescribeCaps, OracleBackend, OracleCursor};
use crate::error::{Error, Result};
use crate::wire::{BindValue, ColumnMeta, encode_oracle_number_decimal};

/// A MariaDB connection pool configured for Oracle compatibility mode.
pub struct MariaDbBackend {
    pool: Pool,
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
    /// Connect and enable MariaDB's Oracle compatibility mode for the initial
    /// session. The eventual execution adapter will apply the setting whenever
    /// it checks out a pooled connection.
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            urlencoding(user),
            urlencoding(password),
            host,
            port,
            urlencoding(database),
        );
        let opts = Opts::from_url(&url)
            .map_err(|e| Error::Postgres(format!("invalid MariaDB connection URL: {e}")))?;
        let pool = Pool::new(opts);
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB connection failed: {e}")))?;
        conn.query_drop("SET sql_mode = 'ORACLE'")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB Oracle mode failed: {e}")))?;
        drop(conn);
        Ok(Self { pool })
    }

    /// Lightweight connectivity probe used while the backend contract is
    /// being extracted.
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB connection failed: {e}")))?;
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
        let mut conn = self.pool.get_conn().await.map_err(mariadb_error)?;
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
                    .map(|i| row.as_ref(i).map(encode_value).transpose())
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
        let mut conn = self.pool.get_conn().await.map_err(mariadb_error)?;
        conn.query_drop("SET sql_mode = 'ORACLE'")
            .await
            .map_err(mariadb_error)?;
        let result = conn
            .exec_iter(mariadb_sql(sql), Params::Positional(bind_values(binds)?))
            .await
            .map_err(mariadb_error)?;
        Ok(result.affected_rows())
    }

    async fn execute_ddl(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        self.execute_simple(sql, binds).await
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
    Error::Postgres(format!("MariaDB error: {e}"))
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
