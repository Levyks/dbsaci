//! MariaDB backend primitives.
//!
//! MariaDB's `SQL_MODE=ORACLE` performs the Oracle-language work in the
//! database. This module deliberately starts with connection/session setup;
//! query execution will be moved behind the same backend contract as the
//! PostgreSQL implementation in a later step.

use mysql_async::{Opts, Pool, prelude::Queryable};

use crate::error::{Error, Result};

/// A MariaDB connection pool configured for Oracle compatibility mode.
pub struct MariaDbBackend {
    pool: Pool,
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
