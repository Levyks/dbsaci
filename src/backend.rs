use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use futures_util::StreamExt;
use tokio_postgres::types::{FromSql, ToSql, Type};
use tokio_postgres::{Client, Config as PostgresConfig, NoTls, Row, RowStream};

use crate::error::{Error, Result};
use crate::wire::{
    BindValue, ColumnMeta, encode_oracle_number_decimal, encode_oracle_number_i64,
    temporal_bind_text,
};

enum PostgresBind {
    Text(Option<String>),
    Bytes(Vec<u8>),
    Boolean(bool),
}

impl PostgresBind {
    fn from_oracle(value: &BindValue) -> Result<Self> {
        match value {
            BindValue::Null => Ok(Self::Text(None)),
            // Oracle treats the empty string as NULL, including for binds.
            BindValue::String(value) if value.is_empty() => Ok(Self::Text(None)),
            BindValue::String(value) | BindValue::Number(value) => {
                Ok(Self::Text(Some(value.clone())))
            }
            BindValue::Bytes(value) => Ok(Self::Bytes(value.clone())),
            BindValue::Temporal(value) => {
                Ok(Self::Text(Some(temporal_bind_text(value)?.to_owned())))
            }
            BindValue::Boolean(value) => Ok(Self::Boolean(*value)),
            BindValue::BinaryDouble(value) if value.is_finite() => {
                Ok(Self::Text(Some(value.to_string())))
            }
            BindValue::BinaryDouble(_) => Err(Error::DataConversionError(
                "non-finite floating bind is unsupported".into(),
            )),
        }
    }

    fn as_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Self::Text(value) => value,
            Self::Bytes(value) => value,
            Self::Boolean(value) => value,
        }
    }
}

pub struct PostgresBackend {
    client: Client,
    statement_timeout: Option<Duration>,
    cancel_token: tokio_postgres::CancelToken,
    /// Prepared-statement cache keyed by the translated PostgreSQL SQL text.
    /// A repeated statement skips the Parse/Describe round trip. Entries are
    /// dropped on any prepare/execute error (stale plan, reset connection).
    stmt_cache: tokio::sync::Mutex<std::collections::HashMap<String, tokio_postgres::Statement>>,
}

/// Upper bound on cached prepared statements per backend connection.
const STMT_CACHE_CAP: usize = 256;

/// A streamed server-side query cursor. Column metadata is known up front;
/// rows are pulled from PostgreSQL in batches so a large result is never held
/// whole in the proxy.
pub struct RowCursor {
    stream: Pin<Box<RowStream>>,
    columns: Vec<ColumnMeta>,
    exhausted: bool,
    savepoint_held: bool,
    /// OCI thick clients need the `0x40` explicit-offset bit in a TSTZ value's
    /// tz byte; the thin drivers do not. Carried from the describe caps.
    oci: bool,
}

impl RowCursor {
    pub fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Pull up to `n` more rows, each encoded to Oracle wire bytes. Fewer than
    /// `n` (possibly zero) means the cursor is exhausted.
    pub async fn next_batch(
        &mut self,
        backend: &PostgresBackend,
        n: usize,
    ) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
        let mut out = Vec::with_capacity(n.min(1024));
        while out.len() < n {
            match self.stream.next().await {
                Some(Ok(row)) => {
                    let mut enc = Vec::with_capacity(self.columns.len());
                    for (i, col) in self.columns.iter().enumerate() {
                        enc.push(pg_value_to_oracle_bytes(&row, i, col.oracle_type, self.oci));
                    }
                    out.push(enc);
                }
                Some(Err(e)) => {
                    self.exhausted = true;
                    let err = backend.recover_statement_error(e).await;
                    self.savepoint_held = false;
                    return Err(err);
                }
                None => {
                    self.finish(backend).await;
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Release the per-statement savepoint. Call when the client abandons the
    /// cursor or the session ends.
    pub async fn finish(&mut self, backend: &PostgresBackend) {
        self.exhausted = true;
        if self.savepoint_held {
            let _ = backend.finish_statement().await;
            self.savepoint_held = false;
        }
    }
}

impl PostgresBackend {
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        db: &str,
        statement_timeout: Option<Duration>,
    ) -> Result<Self> {
        // Oracle client usernames are typically upper-case; PostgreSQL role names are
        // case-sensitive in the catalog, so normalize to lower-case for the backend.
        let pg_user = user.to_lowercase();
        let mut conn_config = PostgresConfig::new();
        conn_config.host(host);
        conn_config.port(port);
        conn_config.user(&pg_user);
        conn_config.password(password);
        conn_config.dbname(db);
        tracing::debug!(
            "postgres connecting to host={} port={} user={} db={}",
            host,
            port,
            pg_user,
            db
        );
        let (client, connection) = match conn_config.connect(NoTls).await {
            Ok(c) => c,
            Err(e) => {
                let detail = pg_error_detail(&e);
                tracing::error!("postgres connect failed: {}", detail);
                return Err(Error::Postgres(detail));
            }
        };
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres connection error: {}", e);
            }
        });

        // Oracle's model is "schema == user": every user owns a schema of its
        // own name, unqualified DDL/DML lands there, and other schemas are
        // reached by qualifying (`hr.emp`) or `ALTER SESSION SET CURRENT_SCHEMA`.
        // Give each pgSaci user a PostgreSQL schema of its own name so the
        // catalog facade can report honest owners and an IDE shows the user's
        // objects under the user's node. Created by the connecting role, so it
        // owns it. Best-effort: if the role lacks CREATE on the database the
        // connection still works — unqualified objects then resolve via the rest
        // of the search_path (`oracle`, `public`), exactly as before this schema
        // existed.
        let ensure_schema = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_identifier(&pg_user));
        if let Err(e) = client.execute(&ensure_schema, &[]).await {
            tracing::warn!(
                "could not ensure schema {:?} (objects will resolve via search_path; \
                 grant CREATE on the database or pre-create the schema to silence this): {}",
                pg_user,
                pg_error_detail(&e)
            );
        }

        // Orafce installs Oracle-compatible functions in the `oracle` schema.
        // Keep the authenticated user's own schema first, as Oracle does, then
        // the extension, then `public` as the shared/fallback namespace (also
        // always reachable explicitly as `public.<name>`).
        let stmt = format!(
            "SET search_path TO {}, oracle, public",
            quote_identifier(&pg_user)
        );
        if let Err(e) = client.execute(&stmt, &[]).await {
            let detail = pg_error_detail(&e);
            tracing::error!("postgres SET search_path failed: {}", detail);
            return Err(Error::Postgres(detail));
        }

        // One-shot environment diagnostics on the first backend connection.
        preflight(&client, &pg_user).await;

        // Permanent, cross-session objects (a `pgsaci` schema + the
        // `binary_float`/`binary_double` domains) must be committed on their own
        // — if they sat in the session's opening transaction, the first client
        // statement that errors and triggers a `ROLLBACK` would drop them.
        // `batch_execute` with no surrounding `BEGIN` autocommits each statement.
        if let Err(e) = client.batch_execute(PERSISTENT_SETUP).await {
            tracing::warn!("persistent setup failed: {}", pg_error_detail(&e));
        }
        // Cross-session `SYS.ALL_*` catalog views for IDE schema browsers.
        // `CREATE OR REPLACE VIEW` takes ACCESS EXCLUSIVE, and IDE clients leave
        // transactions open (`idle in transaction`) holding a read lock on these
        // views — so re-running the whole batch on *every* connect eventually
        // deadlocks a wave of connects behind one idle client. Run it only when
        // the stored version marker is stale (first connect after a deploy that
        // changed the facade), and serialize that one run with an advisory lock
        // so a concurrent connect wave doesn't all apply it at once.
        async fn facade_stale(c: &tokio_postgres::Client) -> bool {
            !c.query_opt(
                "SELECT ver = $1 FROM pgsaci.facade_ver",
                &[&SYS_CATALOG_FACADE_VERSION],
            )
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<_, bool>(0).ok())
            .unwrap_or(false)
        }
        if facade_stale(&client).await {
            let _ = client
                .batch_execute(
                    "SELECT pg_advisory_lock(hashtext('pgsaci:sys_catalog_facade')::bigint)",
                )
                .await;
            if facade_stale(&client).await {
                if let Err(e) = client.batch_execute(SYS_CATALOG_FACADE).await {
                    tracing::warn!("sys catalog facade failed: {}", pg_error_detail(&e));
                } else if let Err(e) = client.batch_execute(IDE_INTROSPECTION_STUBS).await {
                    tracing::warn!("ide introspection stubs failed: {}", pg_error_detail(&e));
                } else if let Err(e) = client.batch_execute(DBMS_METADATA_FACADE).await {
                    tracing::warn!("dbms_metadata facade failed: {}", pg_error_detail(&e));
                } else {
                    let _ = client
                        .batch_execute(&format!(
                            "INSERT INTO pgsaci.facade_ver(only_one, ver) VALUES (true, '{v}')
                               ON CONFLICT (only_one) DO UPDATE SET ver = '{v}'",
                            v = SYS_CATALOG_FACADE_VERSION
                        ))
                        .await;
                }
            }
            let _ = client
                .batch_execute(
                    "SELECT pg_advisory_unlock(hashtext('pgsaci:sys_catalog_facade')::bigint)",
                )
                .await;
        }

        // Session init is on the hot path of every connect. The read-only
        // catalog-facade temp views and the built-in / `DBMS_*` facade functions
        // are all `CREATE OR REPLACE` / `IF NOT EXISTS`; they go out in one
        // simple batch instead of ~20 round trips. They are **autocommitted**
        // (no surrounding `BEGIN`) — a client `ROLLBACK` mid-session must not
        // drop `nls_session_parameters` & friends. A bare `BEGIN` afterwards
        // still leaves the session in a transaction (Oracle's default). If the
        // combined batch fails (older PostgreSQL, partial facade), fall back to
        // per-statement best-effort.
        let combined = {
            let mut s = String::with_capacity(8192);
            s.push_str(SESSION_FACADE_VIEWS);
            for stmt in ORACLE_COMPAT_FACADE {
                s.push_str(stmt);
                s.push_str(";\n");
            }
            s
        };
        if let Err(combined_err) = client.batch_execute(&combined).await {
            tracing::warn!(
                "combined session init failed ({}); applying facade piecewise",
                pg_error_detail(&combined_err)
            );
            let _ = client.batch_execute("ROLLBACK").await;
            let _ = client.batch_execute(SESSION_FACADE_VIEWS).await;
            for stmt in ORACLE_COMPAT_FACADE {
                if let Err(e) = client.batch_execute(stmt).await {
                    tracing::warn!(
                        "oracle compat facade statement failed ({}): {}",
                        stmt.split_whitespace()
                            .take(6)
                            .collect::<Vec<_>>()
                            .join(" "),
                        pg_error_detail(&e)
                    );
                }
            }
        }
        let _ = client.batch_execute("BEGIN").await;

        let cancel_token = client.cancel_token();
        Ok(Self {
            client,
            statement_timeout,
            cancel_token,
            stmt_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Prepare `sql`, reusing a cached `Statement` when the same text was
    /// prepared before on this connection.
    async fn prepare_cached(
        &self,
        sql: &str,
    ) -> std::result::Result<tokio_postgres::Statement, tokio_postgres::Error> {
        {
            let cache = self.stmt_cache.lock().await;
            if let Some(stmt) = cache.get(sql) {
                return Ok(stmt.clone());
            }
        }
        let stmt = self.client.prepare(sql).await?;
        let mut cache = self.stmt_cache.lock().await;
        if cache.len() >= STMT_CACHE_CAP {
            cache.clear();
        }
        cache.insert(sql.to_string(), stmt.clone());
        Ok(stmt)
    }

    /// Forget a cached statement (e.g. after a "cached plan must not change
    /// result type" error or a connection reset).
    async fn forget_cached(&self, sql: &str) {
        self.stmt_cache.lock().await.remove(sql);
    }

    /// Best-effort cancellation of whatever statement is currently running on
    /// the backend connection (Oracle `OCIBreak` / Ctrl-C). PostgreSQL turns
    /// this into SQLSTATE `57014`, which maps to ORA-01013.
    pub async fn cancel(&self) {
        if let Err(e) = self.cancel_token.cancel_query(NoTls).await {
            tracing::debug!("cancel request failed (query may have already finished): {e}");
        }
    }

    /// Open a streamed query cursor. Metadata is available immediately; rows are
    /// pulled with [`RowCursor::next_batch`].
    pub async fn open_cursor(&self, sql: &str) -> Result<RowCursor> {
        self.open_cursor_with_binds(sql, &[], DescribeCaps::LENIENT)
            .await
    }

    /// Open a streamed cursor with real PostgreSQL parameters. [`DescribeCaps`]
    /// selects how much type fidelity the connected client's describe parser can
    /// take (see its docs).
    pub async fn open_cursor_with_binds(
        &self,
        sql: &str,
        binds: &[BindValue],
        caps: DescribeCaps,
    ) -> Result<RowCursor> {
        // The PostgreSQL `statement_timeout` GUC is NOT set on the streamed-cursor
        // path (for any client): it counts wall-clock across the whole portal, so
        // a large result pulled batch-by-batch over several seconds trips it
        // mid-stream → a cancel error partway through the rows → the client
        // desyncs on the wire. Instead the cap is enforced below, around
        // `query_raw` only: a genuinely blocking query (`SELECT pg_sleep(3)`)
        // produces no rows until it finishes, so `query_raw` stays pending and
        // the cap fires (→ ORA-01013); a query that merely streams a lot of rows
        // returns from `query_raw` immediately and is never cancelled.
        self.begin_statement_ex(false).await?;
        let open_cap = self.statement_timeout;
        let params: Vec<PostgresBind> = binds
            .iter()
            .map(PostgresBind::from_oracle)
            .collect::<Result<_>>()?;

        // One transparent retry: a cached plan can be invalidated by an
        // intervening DDL ("cached plan must not change result type"). Oracle
        // re-parses silently, so do the same.
        let mut attempt = 0;
        let (columns, stream) = loop {
            attempt += 1;
            let statement = match self.prepare_cached(sql).await {
                Ok(s) => s,
                Err(e) => {
                    self.forget_cached(sql).await;
                    return Err(self.recover_statement_error(e).await);
                }
            };
            // Describe metadata is finalised while the connection is idle: the
            // `pgsaci.binary_*` domain lookup needs a catalog query, which is not
            // possible once `query_raw` has an open `RowStream` on the socket.
            let mut columns: Vec<ColumnMeta> = statement
                .columns()
                .iter()
                .enumerate()
                .map(|(i, c)| pg_column_to_oracle_meta(c, i + 1, caps))
                .collect();
            self.refine_oracle_domain_columns(statement.columns(), &mut columns, caps)
                .await;
            let query_fut = self
                .client
                .query_raw(&statement, params.iter().map(PostgresBind::as_sql));
            let query_result = match open_cap {
                Some(cap) => {
                    tokio::pin!(query_fut);
                    match tokio::time::timeout(cap, &mut query_fut).await {
                        Ok(r) => r,
                        Err(_) => {
                            // Blocking query exceeded the cap — cancel it on the
                            // backend, then await so `query_fut` resolves with
                            // the SQLSTATE 57014 that maps to ORA-01013.
                            self.cancel().await;
                            query_fut.await
                        }
                    }
                }
                None => query_fut.await,
            };
            match query_result {
                Ok(s) => break (columns, s),
                Err(e) if attempt == 1 && is_stale_plan(&e) => {
                    self.forget_cached(sql).await;
                    continue;
                }
                Err(e) => {
                    self.forget_cached(sql).await;
                    return Err(self.recover_statement_error(e).await);
                }
            }
        };
        Ok(RowCursor {
            stream: Box::pin(stream),
            columns,
            exhausted: false,
            savepoint_held: true,
            oci: caps.oci,
        })
    }

    /// PostgreSQL `real`/`float8` back both a declared Oracle
    /// `BINARY_FLOAT`/`BINARY_DOUBLE` column and computed doubles (`POWER`,
    /// `AVG`, `DOUBLE PRECISION`), which Oracle reports as NUMBER. The DDL
    /// translator routes the declarations through transparent `pgsaci.binary_*`
    /// domains; here a catalog lookup on a result column's `table_oid` /
    /// `column_id` recovers the declared column and re-describes it with the
    /// native Oracle type (100 / 101). Expressions carry no `table_oid` and
    /// stay NUMBER.
    async fn refine_oracle_domain_columns(
        &self,
        pg_columns: &[tokio_postgres::Column],
        meta: &mut [ColumnMeta],
        caps: DescribeCaps,
    ) {
        use tokio_postgres::types::Type;
        for (i, c) in pg_columns.iter().enumerate() {
            let candidate = matches!(*c.type_(), Type::FLOAT4 | Type::FLOAT8);
            if !candidate {
                continue;
            }
            let (Some(relid), Some(attnum)) = (c.table_oid(), c.column_id()) else {
                continue;
            };
            let row = self
                .client
                .query_opt(
                    "SELECT t.typname, t.typnamespace::regnamespace::text AS nsp
                       FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
                      WHERE a.attrelid = $1 AND a.attnum = $2",
                    &[&relid, &attnum],
                )
                .await;
            let Ok(Some(row)) = row else { continue };
            let typname: String = row.get("typname");
            let nsp: String = row.get("nsp");
            if nsp != "pgsaci" {
                continue;
            }
            match typname.as_str() {
                "binary_double" if caps.native_binary_floats && meta[i].oracle_type == 2 => {
                    meta[i].oracle_type = 101;
                    meta[i].precision = 0;
                    meta[i].scale = 0;
                    meta[i].buffer_size = 8;
                    meta[i].max_size = 8;
                }
                "binary_float" if caps.native_binary_floats && meta[i].oracle_type == 2 => {
                    meta[i].oracle_type = 100;
                    meta[i].precision = 0;
                    meta[i].scale = 0;
                    meta[i].buffer_size = 4;
                    meta[i].max_size = 4;
                }
                _ => {}
            }
        }
    }

    /// Buffered convenience: open a cursor and drain it. Used where the caller
    /// genuinely needs every row at once.
    pub async fn execute(&self, sql: &str) -> Result<QueryResult> {
        let mut cursor = self.open_cursor(sql).await?;
        let columns = cursor.columns().to_vec();
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(self, 4096).await?;
            let short = batch.len() < 4096;
            rows.extend(batch);
            if short || cursor.is_exhausted() {
                break;
            }
        }
        Ok(QueryResult { columns, rows })
    }

    pub async fn execute_simple(&self, sql: &str) -> Result<u64> {
        self.execute_simple_with_binds(sql, &[]).await
    }

    /// Execute DDL/DML with real PostgreSQL parameters.
    pub async fn execute_simple_with_binds(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        let command = sql.trim().trim_end_matches(';');
        if command.eq_ignore_ascii_case("COMMIT") {
            self.client
                .batch_execute("COMMIT; BEGIN")
                .await
                .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
            return Ok(0);
        }
        if command.eq_ignore_ascii_case("ROLLBACK") {
            self.client
                .batch_execute("ROLLBACK; BEGIN")
                .await
                .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
            return Ok(0);
        }
        // A PostgreSQL transaction is already open for every Oracle session;
        // Oracle clients nevertheless emit BEGIN/START TRANSACTION markers.
        // Treat those markers as idempotent rather than nesting transactions.
        if command.eq_ignore_ascii_case("BEGIN")
            || command.eq_ignore_ascii_case("START TRANSACTION")
        {
            return Ok(0);
        }
        // Client-managed savepoints must run *outside* the per-statement
        // `SAVEPOINT pgsaci_statement ... RELEASE` wrapper: `RELEASE SAVEPOINT`
        // also destroys every savepoint established after it, so the wrapper
        // would silently discard a client `SAVEPOINT` the moment it was made.
        let upper = command.to_ascii_uppercase();
        if upper.starts_with("SAVEPOINT ")
            || upper.starts_with("RELEASE SAVEPOINT ")
            || upper.starts_with("RELEASE ")
            || upper.starts_with("ROLLBACK TO ")
            || upper.starts_with("SET TRANSACTION ")
            || upper.starts_with("SET CONSTRAINTS ")
        {
            self.client
                .batch_execute(command)
                .await
                .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
            return Ok(0);
        }
        self.begin_statement().await?;
        let params: Vec<PostgresBind> = binds
            .iter()
            .map(PostgresBind::from_oracle)
            .collect::<Result<_>>()?;
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(PostgresBind::as_sql).collect();
        let statement = match self.prepare_cached(sql).await {
            Ok(s) => s,
            Err(e) => {
                self.forget_cached(sql).await;
                return Err(self.recover_statement_error(e).await);
            }
        };
        match self.client.execute(&statement, &refs).await {
            Ok(rows) => {
                self.finish_statement().await?;
                Ok(rows)
            }
            Err(error) => {
                self.forget_cached(sql).await;
                Err(self.recover_statement_error(error).await)
            }
        }
    }

    /// Run a DML statement that ends in `RETURNING <exprs>` (the `INTO :binds`
    /// clause is stripped upstream). Returns, per returned expression, the list
    /// of wire-encoded values (one per affected row), plus the affected-row
    /// count.
    pub async fn execute_returning(
        &self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<(u64, Vec<Vec<Option<Vec<u8>>>>)> {
        let mut cursor = self
            .open_cursor_with_binds(sql, binds, DescribeCaps::LENIENT)
            .await?;
        let ncols = cursor.columns().len();
        let mut per_col: Vec<Vec<Option<Vec<u8>>>> = vec![Vec::new(); ncols];
        let mut rows: u64 = 0;
        loop {
            let batch = cursor.next_batch(self, 4096).await?;
            let short = batch.len() < 4096;
            for row in batch {
                rows += 1;
                for (c, val) in row.into_iter().enumerate() {
                    per_col[c].push(val);
                }
            }
            if short || cursor.is_exhausted() {
                break;
            }
        }
        cursor.finish(self).await;
        Ok((rows, per_col))
    }

    /// Oracle commits the current transaction both before and after DDL. Keep
    /// DDL outside the ordinary per-statement savepoint so a later ROLLBACK
    /// cannot undo prior DML that Oracle would already have committed.
    pub async fn execute_ddl_with_binds(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        self.client
            .batch_execute("COMMIT; BEGIN")
            .await
            .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;

        // Bind-free DDL goes through the simple protocol so a translation that
        // expands to several statements (e.g. a trigger → trigger function +
        // `CREATE TRIGGER`) runs as one call.
        if binds.is_empty() {
            return match self.client.batch_execute(sql).await {
                Ok(()) => {
                    self.client
                        .batch_execute("COMMIT; BEGIN")
                        .await
                        .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
                    Ok(0)
                }
                Err(error) => {
                    let detail = pg_error_detail(&error);
                    let _ = self.client.batch_execute("ROLLBACK; BEGIN").await;
                    Err(Error::Postgres(detail))
                }
            };
        }

        let params: Vec<PostgresBind> = binds
            .iter()
            .map(PostgresBind::from_oracle)
            .collect::<Result<_>>()?;
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(PostgresBind::as_sql).collect();
        match self.client.execute(sql, &refs).await {
            Ok(rows) => {
                self.client
                    .batch_execute("COMMIT; BEGIN")
                    .await
                    .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
                Ok(rows)
            }
            Err(error) => {
                let detail = pg_error_detail(&error);
                let _ = self.client.batch_execute("ROLLBACK; BEGIN").await;
                Err(Error::Postgres(detail))
            }
        }
    }

    pub(crate) async fn begin_statement(&self) -> Result<()> {
        self.begin_statement_ex(true).await
    }

    /// `apply_timeout = false` skips `SET LOCAL statement_timeout` for the
    /// streamed-cursor path: PostgreSQL's `statement_timeout` counts wall-clock
    /// across the whole portal, so a large result pulled batch-by-batch over
    /// several seconds trips it mid-stream. The client then receives ORA-01013
    /// for a query that was streaming fine — and python-oracledb thick reacts
    /// by re-driving the Execute, wedging the session. The timeout still guards
    /// the single-shot DML path.
    pub(crate) async fn begin_statement_ex(&self, apply_timeout: bool) -> Result<()> {
        self.client
            .batch_execute("SAVEPOINT pgsaci_statement")
            .await
            .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
        if apply_timeout && let Some(timeout) = self.statement_timeout {
            let millis = timeout.as_millis().max(1);
            self.client
                .batch_execute(&format!("SET LOCAL statement_timeout = '{millis}ms'"))
                .await
                .map_err(|e| Error::Postgres(pg_error_detail(&e)))?;
        }
        Ok(())
    }

    pub(crate) async fn finish_statement(&self) -> Result<()> {
        self.client
            .batch_execute("RELEASE SAVEPOINT pgsaci_statement")
            .await
            .map_err(|e| Error::Postgres(pg_error_detail(&e)))
    }

    pub(crate) async fn recover_statement_error(&self, original: tokio_postgres::Error) -> Error {
        let detail = pg_error_detail(&original);
        let position = pg_error_position(&original);
        match self
            .client
            .batch_execute(
                "ROLLBACK TO SAVEPOINT pgsaci_statement; RELEASE SAVEPOINT pgsaci_statement",
            )
            .await
        {
            Ok(()) => Error::PgStatement { detail, position },
            Err(recovery_error) => Error::PgStatement {
                detail: format!(
                    "{detail}; failed to recover transaction: {}",
                    pg_error_detail(&recovery_error)
                ),
                position,
            },
        }
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// One-time environment diagnostics, logged on the first backend connection.
/// PgSaci's compatibility rests on a few implicit assumptions about the target
/// database (orafce installed, PostgreSQL-native lower-case identifiers); when
/// one is missing the failure otherwise surfaces later as an opaque PostgreSQL
/// error. Surface it once, up front, as a single line. Best-effort: every probe
/// is optional and a failure here never blocks a connection.
async fn preflight(client: &Client, pg_user: &str) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }

    match client
        .query_opt(
            "SELECT extversion FROM pg_extension WHERE extname = 'orafce'",
            &[],
        )
        .await
    {
        Ok(Some(row)) => tracing::info!(
            "preflight: orafce {} present",
            row.try_get::<_, String>(0).unwrap_or_default()
        ),
        Ok(None) => tracing::warn!(
            "preflight: the `orafce` extension is NOT installed in this database — \
             most Oracle scalar functions (NVL, DECODE, TO_DATE, SYSDATE, …) will \
             fail. Run `CREATE EXTENSION orafce;` in the target database."
        ),
        Err(e) => tracing::debug!("preflight: orafce probe failed: {}", pg_error_detail(&e)),
    }

    if let Ok(row) = client.query_one("SHOW search_path", &[]).await {
        let sp: String = row.get(0);
        tracing::info!("preflight: search_path = {sp}");
    }

    if let Ok(row) = client
        .query_one(
            "SELECT count(*) FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','S') \
               AND c.relname <> lower(c.relname)",
            &[&pg_user],
        )
        .await
        && let Ok(n) = row.try_get::<_, i64>(0)
        && n > 0
    {
        tracing::warn!(
            "preflight: {n} object(s) in schema \"{pg_user}\" have upper- or mixed-case \
             names. An Oracle client's unquoted names fold to UPPER and its quoted \
             ALL-CAPS names map to lower-case — neither resolves to a mixed-case \
             PostgreSQL object. Use lower-case identifiers in the target schema."
        );
    }
}

/// Session-local Oracle data-dictionary views and niladic/built-in helpers that
/// orafce does not provide. Each entry is applied independently, best-effort.
/// Read-only Oracle catalog facade. `pg_table_is_visible` is evaluated per
/// query, so these are safe as ordinary (non-temp) `CREATE OR REPLACE` views;
/// they are recreated on every connect for resilience but cost one batch.
const SESSION_FACADE_VIEWS: &str = "
    CREATE OR REPLACE TEMP VIEW user_tables AS
      SELECT upper(c.relname)::varchar AS table_name
      FROM pg_catalog.pg_class c
      WHERE c.relkind IN ('r', 'p', 'v', 'm')
        AND pg_catalog.pg_table_is_visible(c.oid);
    CREATE OR REPLACE TEMP VIEW all_tables AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS table_name
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind IN ('r', 'p', 'v', 'm');
    CREATE OR REPLACE TEMP VIEW user_tab_columns AS
      SELECT upper(c.table_name)::varchar AS table_name,
             upper(c.column_name)::varchar AS column_name, c.data_type,
             c.ordinal_position::integer AS column_id
      FROM information_schema.columns c
      JOIN pg_catalog.pg_class r ON r.relname = c.table_name
      JOIN pg_catalog.pg_namespace n
        ON n.oid = r.relnamespace AND n.nspname = c.table_schema
      WHERE pg_catalog.pg_table_is_visible(r.oid);
";

const ORACLE_COMPAT_FACADE: &[&str] = &[
    "CREATE OR REPLACE TEMP VIEW dual AS SELECT 'X'::varchar AS dummy",
    "
    CREATE OR REPLACE TEMP VIEW all_tab_columns AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.table_name)::varchar AS table_name,
             upper(c.column_name)::varchar AS column_name,
             c.data_type, c.ordinal_position::integer AS column_id
      FROM information_schema.columns c
      JOIN pg_catalog.pg_class r ON r.relname = c.table_name
      JOIN pg_catalog.pg_namespace n
        ON n.oid = r.relnamespace AND n.nspname = c.table_schema",
    "
    CREATE OR REPLACE TEMP VIEW user_objects AS
      SELECT upper(c.relname)::varchar AS object_name,
             CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'p' THEN 'TABLE'
                            WHEN 'v' THEN 'VIEW'  WHEN 'm' THEN 'MATERIALIZED VIEW'
                            WHEN 'i' THEN 'INDEX' WHEN 'S' THEN 'SEQUENCE'
                            ELSE upper(c.relkind::text) END::varchar AS object_type
      FROM pg_catalog.pg_class c
      WHERE pg_catalog.pg_table_is_visible(c.oid)",
    "
    CREATE OR REPLACE TEMP VIEW user_constraints AS
      SELECT upper(con.conname)::varchar AS constraint_name,
             CASE con.contype WHEN 'p' THEN 'P' WHEN 'f' THEN 'R'
                              WHEN 'u' THEN 'U' WHEN 'c' THEN 'C'
                              ELSE con.contype::text END::varchar AS constraint_type,
             upper(rel.relname)::varchar AS table_name
      FROM pg_catalog.pg_constraint con
      JOIN pg_catalog.pg_class rel ON rel.oid = con.conrelid
      WHERE pg_catalog.pg_table_is_visible(rel.oid)",
    "
    CREATE OR REPLACE TEMP VIEW user_indexes AS
      SELECT upper(ic.relname)::varchar AS index_name,
             upper(tc.relname)::varchar AS table_name,
             CASE WHEN ix.indisunique THEN 'UNIQUE' ELSE 'NONUNIQUE' END::varchar AS uniqueness
      FROM pg_catalog.pg_index ix
      JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid
      JOIN pg_catalog.pg_class tc ON tc.oid = ix.indrelid
      WHERE pg_catalog.pg_table_is_visible(tc.oid)",
    "
    CREATE OR REPLACE TEMP VIEW user_sequences AS
      SELECT upper(c.relname)::varchar AS sequence_name
      FROM pg_catalog.pg_class c
      WHERE c.relkind = 'S' AND pg_catalog.pg_table_is_visible(c.oid)",
    "
    CREATE OR REPLACE TEMP VIEW all_sequences AS
      SELECT upper(n.nspname)::varchar   AS sequence_owner,
             upper(c.relname)::varchar   AS sequence_name,
             s.seqmin::numeric           AS min_value,
             s.seqmax::numeric           AS max_value,
             s.seqincrement::numeric     AS increment_by,
             (CASE WHEN s.seqcycle THEN 'Y' ELSE 'N' END)::varchar AS cycle_flag,
             'N'::varchar                AS order_flag,
             s.seqcache::numeric         AS cache_size,
             s.seqstart::numeric         AS last_number
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_catalog.pg_sequence  s ON s.seqrelid = c.oid
      WHERE c.relkind = 'S'",
    "
    CREATE OR REPLACE TEMP VIEW user_tab_comments AS
      SELECT upper(c.relname)::varchar AS table_name,
             CASE c.relkind WHEN 'v' THEN 'VIEW' ELSE 'TABLE' END::varchar AS table_type,
             pg_catalog.obj_description(c.oid, 'pg_class')::varchar AS comments
      FROM pg_catalog.pg_class c
      WHERE c.relkind IN ('r', 'p', 'v', 'm')
        AND pg_catalog.pg_table_is_visible(c.oid)",
    "CREATE OR REPLACE TEMP VIEW \"v$version\" AS
       SELECT ('PgSaci Oracle-compatibility proxy on ' || version())::varchar AS banner",
    "
    CREATE OR REPLACE TEMP VIEW nls_session_parameters AS
      SELECT * FROM (VALUES
        ('NLS_DATE_FORMAT', coalesce(current_setting('pgsaci.nls_date_format', true), 'DD-MON-RR')),
        ('NLS_TIMESTAMP_FORMAT', coalesce(current_setting('pgsaci.nls_timestamp_format', true), 'DD-MON-RR HH24.MI.SSXFF')),
        ('NLS_TIMESTAMP_TZ_FORMAT', coalesce(current_setting('pgsaci.nls_timestamp_tz_format', true), 'DD-MON-RR HH24.MI.SSXFF TZR')),
        ('NLS_NUMERIC_CHARACTERS', coalesce(current_setting('pgsaci.nls_numeric_characters', true), '.,')),
        ('NLS_LANGUAGE', coalesce(current_setting('pgsaci.nls_language', true), 'AMERICAN')),
        ('NLS_DATE_LANGUAGE', coalesce(current_setting('pgsaci.nls_date_language', true), 'AMERICAN')),
        ('NLS_TERRITORY', coalesce(current_setting('pgsaci.nls_territory', true), 'AMERICA')),
        ('NLS_SORT', upper(coalesce(current_setting('pgsaci.nls_sort', true), 'BINARY'))),
        ('NLS_COMP', upper(coalesce(current_setting('pgsaci.nls_comp', true), 'BINARY')))
      ) AS t(parameter, value)",
    // Version / DB-identity views Oracle tooling also reads *unqualified*
    // (SYS.* copies live in IDE_INTROSPECTION_STUBS for the qualified path).
    "CREATE OR REPLACE TEMP VIEW product_component_version AS
       SELECT 'PgSaci Oracle-compatibility proxy'::varchar AS product,
              '19.0.0.0.0'::varchar AS version,
              '19.0.0.0.0'::varchar AS version_full,
              'Production'::varchar AS status",
    "CREATE OR REPLACE TEMP VIEW nls_database_parameters AS
       SELECT * FROM (VALUES
         ('NLS_CHARACTERSET'::varchar, 'AL32UTF8'::varchar),
         ('NLS_NCHAR_CHARACTERSET', 'AL16UTF16'),
         ('NLS_LANGUAGE', 'AMERICAN'), ('NLS_TERRITORY', 'AMERICA'),
         ('NLS_RDBMS_VERSION', '19.0.0.0.0')
       ) AS t(parameter, value)",
    "CREATE OR REPLACE TEMP VIEW global_name AS
       SELECT (upper(current_database()) || '')::varchar AS global_name",
    // Pinned to `public` (always on the search_path, including after
    // `ALTER SESSION SET CURRENT_SCHEMA`), so an unqualified call resolves here
    // no matter which schema is current. Not `pg_temp` — temp functions are not
    // resolved for unqualified calls; not the user's own schema — it drops off
    // the path when CURRENT_SCHEMA is switched.
    "
    CREATE OR REPLACE FUNCTION public.sys_context(p_namespace text, p_parameter text)
      RETURNS text LANGUAGE sql STABLE AS $fn$
        SELECT CASE upper(p_parameter)
          WHEN 'SESSION_USER'   THEN upper(current_user)
          WHEN 'CURRENT_USER'   THEN upper(current_user)
          -- A PostgreSQL role can exist without a same-named schema; Oracle
          -- users always have one.  In that fallback startup shape the first
          -- usable search-path schema is `oracle`, but USERENV must still
          -- report the authenticated Oracle schema.  Explicit ALTER SESSION
          -- CURRENT_SCHEMA values remain visible through upper(current_schema()).
          WHEN 'CURRENT_SCHEMA' THEN upper(current_schema)
          WHEN 'SESSION_SCHEMA' THEN upper(current_schema)
          WHEN 'DB_NAME'        THEN current_database()
          WHEN 'DB_UNIQUE_NAME' THEN current_database()
          WHEN 'SID'            THEN pg_backend_pid()::text
          ELSE NULL
        END
      $fn$",
    "CREATE OR REPLACE FUNCTION public.hextoraw(p text) RETURNS bytea
       LANGUAGE sql IMMUTABLE AS $fn$ SELECT decode(p, 'hex') $fn$",
    "CREATE OR REPLACE FUNCTION public.rawtohex(p bytea) RETURNS text
       LANGUAGE sql IMMUTABLE AS $fn$ SELECT upper(encode(p, 'hex')) $fn$",
    "CREATE OR REPLACE FUNCTION public.numtodsinterval(p double precision, u text) RETURNS interval
       LANGUAGE sql IMMUTABLE AS $fn$ SELECT (p::text || ' ' || u)::interval $fn$",
    "CREATE OR REPLACE FUNCTION public.numtoyminterval(p double precision, u text) RETURNS interval
       LANGUAGE sql IMMUTABLE AS $fn$ SELECT (p::text || ' ' || u)::interval $fn$",
    // DBMS_LOB against inline CLOB(text)/BLOB(bytea) values. Locator-based
    // streaming ops are not implemented; a LOB value still travels inline and
    // must fit one TTC packet.
    "CREATE SCHEMA IF NOT EXISTS dbms_lob",
    "CREATE OR REPLACE FUNCTION dbms_lob.getlength(l text) RETURNS integer
       LANGUAGE sql IMMUTABLE AS $fn$ SELECT length(l) $fn$",
    "CREATE OR REPLACE FUNCTION dbms_lob.getlength(l bytea) RETURNS integer
       LANGUAGE sql IMMUTABLE AS $fn$ SELECT octet_length(l) $fn$",
    "CREATE OR REPLACE FUNCTION dbms_lob.substr(l text, amount integer DEFAULT 32767, offset_ integer DEFAULT 1)
       RETURNS text LANGUAGE sql IMMUTABLE AS $fn$ SELECT substr(l, offset_, amount) $fn$",
    "CREATE OR REPLACE FUNCTION dbms_lob.substr(l bytea, amount integer DEFAULT 32767, offset_ integer DEFAULT 1)
       RETURNS bytea LANGUAGE sql IMMUTABLE AS $fn$ SELECT substr(l, offset_, amount) $fn$",
    "CREATE OR REPLACE FUNCTION dbms_lob.instr(l text, pattern text, offset_ integer DEFAULT 1, nth integer DEFAULT 1)
       RETURNS integer LANGUAGE sql IMMUTABLE AS $fn$
         SELECT CASE WHEN p = 0 THEN 0 ELSE p + offset_ - 1 END
         FROM (SELECT position(pattern IN substr(l, offset_)) AS p) q $fn$",
];

/// Permanent objects, committed once per connection before the session's
/// transaction opens (see the call site). Oracle BINARY_FLOAT / BINARY_DOUBLE
/// are native IEEE types distinct from NUMBER; PostgreSQL has no such distinct
/// type, so the DDL translator maps them to these transparent domains over
/// float4/float8 and a describe-time catalog lookup recovers the Oracle-ness
/// (type 100/101) that a bare `float8` column — `POWER(a,b)`, `DOUBLE
/// PRECISION` — would not get. Idempotent so a table created in one session is
/// describable in the next.
const PERSISTENT_SETUP: &str = "
    CREATE SCHEMA IF NOT EXISTS pgsaci;
    DO $b$ BEGIN CREATE DOMAIN pgsaci.binary_double AS double precision;
      EXCEPTION WHEN duplicate_object THEN NULL; END $b$;
    DO $b$ BEGIN CREATE DOMAIN pgsaci.binary_float AS real;
      EXCEPTION WHEN duplicate_object THEN NULL; END $b$;
    CREATE TABLE IF NOT EXISTS pgsaci.facade_ver (
      only_one boolean PRIMARY KEY DEFAULT true CHECK (only_one),
      ver      text NOT NULL);
";

/// Bump on any change to [`SYS_CATALOG_FACADE`]. Connects re-apply the facade
/// (an ACCESS EXCLUSIVE `CREATE OR REPLACE VIEW` storm) only when the value
/// stored in `pgsaci.facade_ver` differs from this.
const SYS_CATALOG_FACADE_VERSION: &str = "2026-09-02.8";

/// Schema-qualified `SYS.ALL_*` / `SYS.USER_*` data-dictionary views that IDE
/// schema browsers (DataGrip/IntelliJ, SQL Developer, DBeaver) query directly by
/// their `sys.` name — the unqualified `all_tables` temp views in
/// [`ORACLE_COMPAT_FACADE`] are not enough for them. Cross-session, built over
/// `pg_catalog`, so they live with the persistent setup. Column sets cover what
/// those introspectors select; values are best-effort (`VALID` status, `USERS`
/// tablespace, NULL timestamps). Applied best-effort — a missing column here
/// only degrades IDE introspection, never a real query.
const SYS_CATALOG_FACADE: &str = "
    CREATE SCHEMA IF NOT EXISTS sys;
    CREATE OR REPLACE VIEW sys.session_roles AS SELECT NULL::varchar AS role      WHERE false;
    CREATE OR REPLACE VIEW sys.session_privs AS SELECT NULL::varchar AS privilege WHERE false;

    CREATE OR REPLACE VIEW sys.all_users AS
      SELECT upper(n.nspname)::varchar AS username, n.oid::bigint AS user_id,
             NULL::timestamp AS created, 'NO'::varchar AS common,
             (CASE WHEN n.nspname LIKE 'pg\\_%'
                    OR n.nspname IN ('information_schema','sys','pgsaci')
                   THEN 'YES' ELSE 'NO' END)::varchar AS oracle_maintained,
             'OPEN'::varchar  AS account_status,
             'USERS'::varchar AS default_tablespace,
             'TEMP'::varchar  AS temporary_tablespace
      FROM pg_catalog.pg_namespace n
      WHERE n.nspname NOT LIKE 'pg_temp_%' AND n.nspname NOT LIKE 'pg_toast%'
        -- orafce drops one schema per PL/SQL package; they are not Oracle
        -- schemas a browser should list. `oracle` itself is kept: it is
        -- remapped to the connected user (their objects live there).
        AND n.nspname NOT IN ('dbms_alert','dbms_assert','dbms_lob','dbms_metadata',
              'dbms_output','dbms_pipe','dbms_random','dbms_sql','dbms_utility',
              'utl_file','plunit','plvchr','plvdate','plvlex','plvstr','plvsubst');

    CREATE OR REPLACE VIEW sys.all_objects AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS object_name,
             NULL::varchar AS subobject_name, c.oid::bigint AS object_id,
             c.oid::bigint AS data_object_id,
             (CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'p' THEN 'TABLE'
                             WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW'
                             WHEN 'i' THEN 'INDEX' WHEN 'S' THEN 'SEQUENCE'
                             ELSE upper(c.relkind::text) END)::varchar AS object_type,
             NULL::timestamp AS created, NULL::timestamp AS last_ddl_time,
             NULL::timestamp AS timestamp, 'VALID'::varchar AS status,
             (CASE WHEN c.relpersistence='t' THEN 'Y' ELSE 'N' END)::varchar AS temporary,
             'N'::varchar AS generated, 'N'::varchar AS secondary
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE c.relkind IN ('r','p','v','m','i','S')
      UNION ALL
      SELECT upper(n.nspname)::varchar, upper(p.proname)::varchar, NULL::varchar, p.oid::bigint,
             p.oid::bigint,
             (CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END)::varchar,
             NULL::timestamp, NULL::timestamp, NULL::timestamp, 'VALID'::varchar,
             'N'::varchar, 'N'::varchar, 'N'::varchar
      FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace;

    -- IDE introspectors select many ALL_TABLES columns by name and fail on the
    -- first that is missing. The leading columns keep their historical
    -- name/order (PostgreSQL's `CREATE OR REPLACE VIEW` only allows *appending*
    -- columns); everything else is appended with Oracle-plausible
    -- constants/NULLs, real values where PostgreSQL has them.
    CREATE OR REPLACE VIEW sys.all_tables AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS table_name,
             'USERS'::varchar AS tablespace_name, 'VALID'::varchar AS status,
             c.reltuples::numeric AS num_rows,
             (CASE WHEN c.relpersistence='t' THEN 'Y' ELSE 'N' END)::varchar AS temporary,
             'NO'::varchar AS nested, NULL::varchar AS iot_type,
             (CASE WHEN c.relkind='p' THEN 'YES' ELSE 'NO' END)::varchar AS partitioned,
             -- appended
             NULL::varchar AS cluster_name, NULL::varchar AS iot_name,
             NULL::numeric AS pct_free, NULL::numeric AS pct_used,
             NULL::numeric AS ini_trans, NULL::numeric AS max_trans,
             NULL::numeric AS initial_extent, NULL::numeric AS next_extent,
             NULL::numeric AS min_extents, NULL::numeric AS max_extents,
             NULL::numeric AS pct_increase, NULL::numeric AS freelists,
             NULL::numeric AS freelist_groups, 'YES'::varchar AS logging,
             'N'::varchar AS backed_up, c.relpages::numeric AS blocks,
             NULL::numeric AS empty_blocks, NULL::numeric AS avg_space,
             NULL::numeric AS chain_cnt, NULL::numeric AS avg_row_len,
             NULL::numeric AS avg_space_freelist_blocks,
             NULL::numeric AS num_freelist_blocks, '1'::varchar AS degree,
             '1'::varchar AS instances, 'N'::varchar AS cache,
             'ENABLED'::varchar AS table_lock, NULL::numeric AS sample_size,
             NULL::timestamp AS last_analyzed, NULL::varchar AS duration,
             'NO'::varchar AS secondary, 'DEFAULT'::varchar AS buffer_pool,
             'DEFAULT'::varchar AS flash_cache, 'DEFAULT'::varchar AS cell_flash_cache,
             'DISABLED'::varchar AS row_movement, 'NO'::varchar AS global_stats,
             'NO'::varchar AS user_stats, 'YES'::varchar AS segment_created,
             NULL::varchar AS cluster_owner, 'DISABLED'::varchar AS dependencies,
             'DISABLED'::varchar AS compression, NULL::varchar AS compress_for,
             'DISABLED'::varchar AS skip_corrupt, 'YES'::varchar AS monitoring,
             'NO'::varchar AS dropped, 'NO'::varchar AS read_only,
             'DEFAULT'::varchar AS result_cache
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE c.relkind IN ('r','p');

    -- Object tables (tables built on an object type). PostgreSQL has none, so
    -- always empty — inherits every ALL_TABLES column plus the object-specific
    -- ones so an introspector selecting any of them still resolves.
    CREATE OR REPLACE VIEW sys.all_object_tables AS
      SELECT *, NULL::varchar AS table_type_owner, NULL::varchar AS table_type,
             NULL::varchar AS object_id_type
      FROM sys.all_tables WHERE false;

    CREATE OR REPLACE VIEW sys.all_views AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS view_name,
             length(pg_get_viewdef(c.oid))::numeric AS text_length,
             pg_get_viewdef(c.oid)::varchar AS text,
             NULL::varchar AS type_text, NULL::varchar AS oid_text,
             'N'::varchar AS read_only
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE c.relkind='v';

    CREATE OR REPLACE VIEW sys.all_mviews AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS mview_name,
             pg_get_viewdef(c.oid)::varchar AS query, 'N'::varchar AS updatable,
             'DEMAND'::varchar AS refresh_mode, 'FORCE'::varchar AS refresh_method,
             'VALID'::varchar AS compile_state
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE c.relkind='m';

    -- PostgreSQL has no materialized-view logs; always empty, but IDE
    -- introspectors probe it while walking the mview branch.
    CREATE OR REPLACE VIEW sys.all_mview_logs AS
      SELECT NULL::varchar AS log_owner, NULL::varchar AS master,
             NULL::varchar AS log_table, NULL::varchar AS log_trigger,
             'NO'::varchar AS rowids, 'NO'::varchar AS primary_key,
             NULL::bigint  AS object_id, NULL::varchar AS filter_columns,
             'NO'::varchar AS sequence, 'NO'::varchar AS include_new_values
      WHERE false;

    CREATE OR REPLACE VIEW sys.all_mview_comments AS
      SELECT NULL::varchar AS owner, NULL::varchar AS mview_name,
             NULL::varchar AS comments
      WHERE false;

    CREATE OR REPLACE VIEW sys.all_tab_columns AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS table_name,
             upper(a.attname)::varchar AS column_name,
             upper(format_type(a.atttypid, NULL))::varchar AS data_type,
             information_schema._pg_char_max_length(a.atttypid,a.atttypmod)::numeric AS data_length,
             information_schema._pg_numeric_precision(a.atttypid,a.atttypmod)::numeric AS data_precision,
             information_schema._pg_numeric_scale(a.atttypid,a.atttypmod)::numeric AS data_scale,
             (CASE WHEN a.attnotnull THEN 'N' ELSE 'Y' END)::varchar AS nullable,
             a.attnum::numeric AS column_id,
             pg_get_expr(ad.adbin, ad.adrelid)::varchar AS data_default,
             COALESCE(information_schema._pg_char_max_length(a.atttypid,a.atttypmod),0)::numeric AS char_length,
             'B'::varchar AS char_used
      FROM pg_catalog.pg_attribute a
      JOIN pg_catalog.pg_class c ON c.oid=a.attrelid
      JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid=a.attrelid AND ad.adnum=a.attnum
      WHERE a.attnum>0 AND NOT a.attisdropped AND c.relkind IN ('r','p','v','m');

    CREATE OR REPLACE VIEW sys.all_tab_cols AS
      SELECT c.*, 'NO'::varchar AS hidden_column, 'NO'::varchar AS virtual_column,
             'YES'::varchar AS user_generated
      FROM sys.all_tab_columns c;

    CREATE OR REPLACE VIEW sys.all_constraints AS
      SELECT upper(n.nspname)::varchar AS owner, upper(con.conname)::varchar AS constraint_name,
             (CASE con.contype WHEN 'p' THEN 'P' WHEN 'f' THEN 'R' WHEN 'u' THEN 'U'
                               WHEN 'c' THEN 'C' WHEN 'n' THEN 'C' ELSE con.contype::text END)::varchar AS constraint_type,
             upper(rel.relname)::varchar AS table_name,
             (CASE WHEN con.contype IN ('c','n') THEN pg_get_constraintdef(con.oid)
                   ELSE NULL END)::varchar AS search_condition,
             upper(rn.nspname)::varchar AS r_owner, upper(rc.conname)::varchar AS r_constraint_name,
             (CASE con.confdeltype WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL'
                                   WHEN 'a' THEN 'NO ACTION' ELSE NULL END)::varchar AS delete_rule,
             (CASE WHEN con.convalidated THEN 'VALID' ELSE 'NOT VALIDATED' END)::varchar AS status,
             (CASE WHEN con.conname ~ '_(pkey|fkey|key|check|excl)[0-9]*$'
                   THEN 'GENERATED NAME' ELSE 'USER NAME' END)::varchar AS generated
      FROM pg_catalog.pg_constraint con
      JOIN pg_catalog.pg_class rel ON rel.oid=con.conrelid
      JOIN pg_catalog.pg_namespace n ON n.oid=con.connamespace
      LEFT JOIN pg_catalog.pg_class rrel ON rrel.oid=con.confrelid
      LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid=rrel.relnamespace
      LEFT JOIN pg_catalog.pg_constraint rc ON rc.conrelid=con.confrelid AND rc.contype='p';

    CREATE OR REPLACE VIEW sys.all_cons_columns AS
      SELECT upper(n.nspname)::varchar AS owner, upper(con.conname)::varchar AS constraint_name,
             upper(rel.relname)::varchar AS table_name, upper(a.attname)::varchar AS column_name,
             k.ord::numeric AS position
      FROM pg_catalog.pg_constraint con
      JOIN pg_catalog.pg_class rel ON rel.oid=con.conrelid
      JOIN pg_catalog.pg_namespace n ON n.oid=con.connamespace
      JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS k(attnum,ord) ON true
      JOIN pg_catalog.pg_attribute a ON a.attrelid=con.conrelid AND a.attnum=k.attnum;

    -- Leading columns keep their historical name/order (CREATE OR REPLACE VIEW
    -- appends only); the rest of the Oracle ALL_INDEXES surface is appended with
    -- Oracle-plausible constants/NULLs (domain-index and CBO-stats columns have
    -- no PostgreSQL equivalent).
    CREATE OR REPLACE VIEW sys.all_indexes AS
      SELECT upper(tn.nspname)::varchar AS owner, upper(ic.relname)::varchar AS index_name,
             (CASE WHEN ix.indexprs IS NOT NULL THEN 'FUNCTION-BASED NORMAL'
                   ELSE 'NORMAL' END)::varchar AS index_type,
             upper(tn.nspname)::varchar AS table_owner,
             upper(tc.relname)::varchar AS table_name,
             (CASE WHEN ix.indisunique THEN 'UNIQUE' ELSE 'NONUNIQUE' END)::varchar AS uniqueness,
             'VALID'::varchar AS status, 'USERS'::varchar AS tablespace_name,
             -- appended
             'TABLE'::varchar AS table_type, NULL::varchar AS compression,
             NULL::numeric AS prefix_length, NULL::numeric AS ini_trans,
             NULL::numeric AS max_trans, NULL::numeric AS initial_extent,
             NULL::numeric AS next_extent, NULL::numeric AS min_extents,
             NULL::numeric AS max_extents, NULL::numeric AS pct_increase,
             NULL::numeric AS pct_threshold, NULL::varchar AS include_column,
             NULL::numeric AS freelists, NULL::numeric AS freelist_groups,
             NULL::numeric AS pct_free, 'YES'::varchar AS logging,
             NULL::numeric AS blevel, NULL::numeric AS leaf_blocks,
             NULL::numeric AS distinct_keys,
             NULL::numeric AS avg_leaf_blocks_per_key,
             NULL::numeric AS avg_data_blocks_per_key,
             NULL::numeric AS clustering_factor, NULL::numeric AS num_rows,
             NULL::numeric AS sample_size, NULL::timestamp AS last_analyzed,
             '1'::varchar AS degree, '1'::varchar AS instances,
             (CASE WHEN ic.relkind='I' THEN 'YES' ELSE 'NO' END)::varchar AS partitioned,
             'N'::varchar AS temporary, 'N'::varchar AS generated,
             'N'::varchar AS secondary, 'DEFAULT'::varchar AS buffer_pool,
             'DEFAULT'::varchar AS flash_cache, 'DEFAULT'::varchar AS cell_flash_cache,
             'NO'::varchar AS user_stats, NULL::varchar AS duration,
             NULL::numeric AS pct_direct_access, NULL::varchar AS ityp_owner,
             NULL::varchar AS ityp_name, NULL::varchar AS parameters,
             'NO'::varchar AS global_stats, NULL::varchar AS domidx_status,
             NULL::varchar AS domidx_opstatus,
             (CASE WHEN ix.indexprs IS NOT NULL THEN 'ENABLED' ELSE NULL END)::varchar AS funcidx_status,
             'NO'::varchar AS join_index, 'NO'::varchar AS dropped,
             'VISIBLE'::varchar AS visibility, 'YES'::varchar AS segment_created,
             NULL::varchar AS orphaned_entries, 'YES'::varchar AS indexing,
             'NO'::varchar AS auto,
             (CASE WHEN EXISTS (SELECT 1 FROM pg_catalog.pg_constraint co WHERE co.conindid=ic.oid)
                   THEN 'YES' ELSE 'NO' END)::varchar AS constraint_index
      FROM pg_catalog.pg_index ix
      JOIN pg_catalog.pg_class ic ON ic.oid=ix.indexrelid
      JOIN pg_catalog.pg_class tc ON tc.oid=ix.indrelid
      JOIN pg_catalog.pg_namespace tn ON tn.oid=tc.relnamespace;

    -- Function-based-index expressions. Reconstructed from pg_get_indexdef where
    -- possible is overkill for introspection; an empty view is enough to resolve.
    CREATE OR REPLACE VIEW sys.all_ind_expressions AS
      SELECT NULL::varchar AS index_owner, NULL::varchar AS index_name,
             NULL::varchar AS table_owner, NULL::varchar AS table_name,
             NULL::varchar AS column_expression, NULL::numeric AS column_position
      WHERE false;

    CREATE OR REPLACE VIEW sys.all_ind_columns AS
      SELECT upper(tn.nspname)::varchar AS index_owner, upper(ic.relname)::varchar AS index_name,
             upper(tn.nspname)::varchar AS table_owner, upper(tc.relname)::varchar AS table_name,
             upper(a.attname)::varchar AS column_name, k.ord::numeric AS column_position,
             'ASC'::varchar AS descend
      FROM pg_catalog.pg_index ix
      JOIN pg_catalog.pg_class ic ON ic.oid=ix.indexrelid
      JOIN pg_catalog.pg_class tc ON tc.oid=ix.indrelid
      JOIN pg_catalog.pg_namespace tn ON tn.oid=tc.relnamespace
      JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum,ord) ON k.attnum<>0
      JOIN pg_catalog.pg_attribute a ON a.attrelid=ix.indrelid AND a.attnum=k.attnum;

    CREATE OR REPLACE VIEW sys.all_sequences AS
      SELECT upper(n.nspname)::varchar AS sequence_owner, upper(c.relname)::varchar AS sequence_name,
             s.seqmin::numeric AS min_value, s.seqmax::numeric AS max_value,
             s.seqincrement::numeric AS increment_by,
             (CASE WHEN s.seqcycle THEN 'Y' ELSE 'N' END)::varchar AS cycle_flag,
             'N'::varchar AS order_flag, s.seqcache::numeric AS cache_size,
             s.seqstart::numeric AS last_number
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      JOIN pg_catalog.pg_sequence s ON s.seqrelid=c.oid
      WHERE c.relkind='S';

    CREATE OR REPLACE VIEW sys.all_synonyms AS
      SELECT NULL::varchar AS owner, NULL::varchar AS synonym_name,
             NULL::varchar AS table_owner, NULL::varchar AS table_name,
             NULL::varchar AS db_link WHERE false;

    CREATE OR REPLACE VIEW sys.all_tab_comments AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS table_name,
             (CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW'
                             ELSE 'TABLE' END)::varchar AS table_type,
             pg_catalog.obj_description(c.oid,'pg_class')::varchar AS comments
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE c.relkind IN ('r','p','v','m');

    CREATE OR REPLACE VIEW sys.all_col_comments AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS table_name,
             upper(a.attname)::varchar AS column_name,
             pg_catalog.col_description(c.oid,a.attnum)::varchar AS comments
      FROM pg_catalog.pg_attribute a
      JOIN pg_catalog.pg_class c ON c.oid=a.attrelid
      JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE a.attnum>0 AND NOT a.attisdropped AND c.relkind IN ('r','p','v','m');

    -- Real timing/event derived from pg_trigger.tgtype bits; leading columns
    -- keep their name/order/type (CREATE OR REPLACE VIEW appends only), the rest
    -- of the Oracle ALL_TRIGGERS surface is appended.
    CREATE OR REPLACE VIEW sys.all_triggers AS
      SELECT upper(n.nspname)::varchar AS owner, upper(t.tgname)::varchar AS trigger_name,
             (
               (CASE WHEN (t.tgtype & 64) <> 0 THEN 'INSTEAD OF'
                     WHEN (t.tgtype & 2)  <> 0 THEN 'BEFORE'
                     ELSE 'AFTER' END)
               || (CASE WHEN (t.tgtype & 1) <> 0 THEN ' EACH ROW' ELSE ' STATEMENT' END)
             )::varchar AS trigger_type,
             (
               array_to_string(ARRAY[
                 CASE WHEN (t.tgtype & 4)  <> 0 THEN 'INSERT' END,
                 CASE WHEN (t.tgtype & 16) <> 0 THEN 'UPDATE' END,
                 CASE WHEN (t.tgtype & 8)  <> 0 THEN 'DELETE' END
               ]::text[], ' OR ')
             )::varchar AS triggering_event,
             upper(n.nspname)::varchar AS table_owner, upper(c.relname)::varchar AS table_name,
             (CASE WHEN t.tgenabled = 'D' THEN 'DISABLED' ELSE 'ENABLED' END)::varchar AS status,
             -- trigger_body / when_clause stay NULL: pg_get_triggerdef and
             -- pg_get_expr carry a non-default collation that a CREATE OR REPLACE
             -- VIEW over an already-deployed facade rejects (42P16).
             NULL::varchar AS trigger_body,
             NULL::varchar AS description,
             NULL::varchar AS when_clause,
             -- appended
             'TABLE'::varchar AS base_object_type, NULL::varchar AS column_name,
             'REFERENCING NEW AS NEW OLD AS OLD'::varchar AS referencing_names,
             'PL/SQL'::varchar AS action_type, 'NO'::varchar AS crossedition,
             (CASE WHEN (t.tgtype & 2) <> 0 AND (t.tgtype & 1) =  0 THEN 'YES' ELSE 'NO' END)::varchar AS before_statement,
             (CASE WHEN (t.tgtype & 2) <> 0 AND (t.tgtype & 1) <> 0 THEN 'YES' ELSE 'NO' END)::varchar AS before_row,
             (CASE WHEN (t.tgtype & 66) = 0 AND (t.tgtype & 1) <> 0 THEN 'YES' ELSE 'NO' END)::varchar AS after_row,
             (CASE WHEN (t.tgtype & 66) = 0 AND (t.tgtype & 1) =  0 THEN 'YES' ELSE 'NO' END)::varchar AS after_statement,
             (CASE WHEN (t.tgtype & 64) <> 0 THEN 'YES' ELSE 'NO' END)::varchar AS instead_of_row,
             'YES'::varchar AS fire_once, 'NO'::varchar AS apply_server_only
      FROM pg_catalog.pg_trigger t
      JOIN pg_catalog.pg_class c ON c.oid=t.tgrelid
      JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE NOT t.tgisinternal;

    CREATE OR REPLACE VIEW sys.all_procedures AS
      SELECT upper(n.nspname)::varchar AS owner, upper(p.proname)::varchar AS object_name,
             NULL::varchar AS procedure_name,
             (CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END)::varchar AS object_type,
             'NO'::varchar AS aggregate, 'NO'::varchar AS pipelined
      FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace;

    CREATE OR REPLACE VIEW sys.all_arguments AS
      SELECT NULL::varchar AS owner, NULL::varchar AS object_name, NULL::varchar AS package_name,
             NULL::varchar AS argument_name, NULL::numeric AS position, NULL::numeric AS sequence,
             NULL::varchar AS data_type, NULL::varchar AS in_out, NULL::bigint AS object_id
      WHERE false;

    CREATE OR REPLACE VIEW sys.all_source AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS type,
             NULL::numeric AS line, NULL::varchar AS text WHERE false;

    CREATE OR REPLACE VIEW sys.all_types AS
      SELECT NULL::varchar AS owner, NULL::varchar AS type_name, NULL::bigint AS type_oid,
             NULL::varchar AS typecode WHERE false;

    CREATE OR REPLACE VIEW sys.all_dependencies AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS type,
             NULL::varchar AS referenced_owner, NULL::varchar AS referenced_name,
             NULL::varchar AS referenced_type WHERE false;

    CREATE OR REPLACE VIEW sys.all_scheduler_jobs AS
      SELECT NULL::varchar AS owner, NULL::varchar AS job_name, NULL::varchar AS state WHERE false;

    CREATE OR REPLACE VIEW sys.all_db_links AS
      SELECT NULL::varchar AS owner, NULL::varchar AS db_link, NULL::varchar AS username,
             NULL::varchar AS host, NULL::timestamp AS created WHERE false;

    CREATE OR REPLACE VIEW sys.all_queues AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS queue_table WHERE false;

    CREATE OR REPLACE VIEW sys.user_tablespaces AS
      SELECT upper(spcname)::varchar     AS tablespace_name, 8192::numeric AS block_size,
             'PERMANENT'::varchar AS contents,       'LOGGING'::varchar AS logging,
             'NO'::varchar        AS force_logging,  'ONLINE'::varchar AS status,
             'NO'::varchar        AS bigfile,        'LOCAL'::varchar AS extent_management,
             'AUTO'::varchar      AS segment_space_management
      FROM pg_catalog.pg_tablespace;
    CREATE OR REPLACE VIEW public.user_tablespaces AS SELECT * FROM sys.user_tablespaces;

    -- IDE schema browsers hash concatenated catalog rows to detect changes.
    -- orafce ships part of dbms_utility but not get_hash_value; only stability
    -- and change-sensitivity matter, so any deterministic hash works.
    CREATE SCHEMA IF NOT EXISTS dbms_utility;
    CREATE OR REPLACE FUNCTION dbms_utility.get_hash_value(name text, base numeric, hash_size numeric)
      RETURNS numeric LANGUAGE sql IMMUTABLE AS
      $fn$ SELECT (abs(pg_catalog.hashtext(name)::bigint) % (hash_size)::bigint + (base)::bigint)::numeric $fn$;

    -- USER_* = ALL_* for the current schema, minus the leading OWNER column
    -- (Oracle's USER_* views omit it). Introspectors filter further themselves.
    CREATE OR REPLACE VIEW sys.user_users AS
      SELECT username, user_id, account_status, NULL::timestamp AS lock_date,
             NULL::timestamp AS expiry_date, default_tablespace, temporary_tablespace,
             created, 'DEFAULT'::varchar AS profile, common, oracle_maintained
      FROM sys.all_users WHERE username = upper(current_schema()) OR username = upper(current_user);
    CREATE OR REPLACE VIEW sys.user_objects AS
      SELECT object_name, subobject_name, object_id, data_object_id, object_type, created,
             last_ddl_time, timestamp, status, temporary, generated, secondary
      FROM sys.all_objects WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_tables AS
      SELECT table_name, tablespace_name, status, num_rows, temporary, nested,
             iot_type, partitioned,
             cluster_name, iot_name, blocks, avg_row_len, degree, cache,
             last_analyzed, row_movement, compression, dropped, read_only
      FROM sys.all_tables WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_views AS
      SELECT view_name, text_length, text, type_text, oid_text, read_only
      FROM sys.all_views WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_mviews AS
      SELECT mview_name, query, updatable, refresh_mode, refresh_method, compile_state
      FROM sys.all_mviews WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_tab_columns AS
      SELECT table_name, column_name, data_type, data_length, data_precision, data_scale,
             nullable, column_id, data_default, char_length, char_used
      FROM sys.all_tab_columns WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_tab_cols AS
      SELECT c.*, 'NO'::varchar AS hidden_column, 'NO'::varchar AS virtual_column,
             'YES'::varchar AS user_generated
      FROM sys.user_tab_columns c;
    CREATE OR REPLACE VIEW sys.user_constraints AS
      SELECT constraint_name, constraint_type, table_name, search_condition, r_owner,
             r_constraint_name, delete_rule, status, generated
      FROM sys.all_constraints WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_cons_columns AS
      SELECT constraint_name, table_name, column_name, position
      FROM sys.all_cons_columns WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_indexes AS
      SELECT index_name, index_type, table_owner, table_name, uniqueness, status,
             tablespace_name,
             ityp_owner, ityp_name, parameters, funcidx_status, generated,
             partitioned, temporary, visibility, constraint_index, degree,
             last_analyzed, num_rows
      FROM sys.all_indexes WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_ind_columns AS
      SELECT index_name, table_name, column_name, column_position, descend
      FROM sys.all_ind_columns WHERE index_owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_sequences AS
      SELECT sequence_name, min_value, max_value, increment_by, cycle_flag, order_flag,
             cache_size, last_number
      FROM sys.all_sequences WHERE sequence_owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_synonyms AS
      SELECT synonym_name, table_owner, table_name, db_link FROM sys.all_synonyms WHERE false;
    CREATE OR REPLACE VIEW sys.user_tab_comments AS
      SELECT table_name, table_type, comments FROM sys.all_tab_comments WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_col_comments AS
      SELECT table_name, column_name, comments FROM sys.all_col_comments WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_triggers AS
      SELECT trigger_name, trigger_type, triggering_event, table_owner, table_name, status,
             trigger_body, description, when_clause,
             base_object_type, column_name, referencing_names, action_type,
             crossedition, before_statement, before_row, after_row,
             after_statement, instead_of_row, fire_once
      FROM sys.all_triggers WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_procedures AS
      SELECT object_name, procedure_name, object_type, aggregate, pipelined
      FROM sys.all_procedures WHERE owner = upper(current_schema());
    CREATE OR REPLACE VIEW sys.user_arguments AS
      SELECT object_name, package_name, argument_name, position, sequence, data_type, in_out, object_id
      FROM sys.all_arguments WHERE false;
    CREATE OR REPLACE VIEW sys.user_source AS
      SELECT name, type, line, text FROM sys.all_source WHERE false;
    CREATE OR REPLACE VIEW sys.user_types AS
      SELECT type_name, type_oid, typecode FROM sys.all_types WHERE false;
    CREATE OR REPLACE VIEW sys.user_dependencies AS
      SELECT name, type, referenced_owner, referenced_name, referenced_type
      FROM sys.all_dependencies WHERE false;
";

/// Empty (or single-row) stand-ins for the long tail of Oracle data-dictionary
/// views a JetBrains / SQL Developer / DBeaver Oracle introspector selects from
/// while walking a schema. None have a meaningful PostgreSQL equivalent
/// (partitioning, object types, LOB segments, privileges, scheduler, XML DB,
/// editions, optimizer-stats history, …); the goal is only that a
/// `SELECT … FROM sys.<view>` resolves instead of raising `ORA-00942` /
/// `ORA-00904` and aborting introspection. Applied right after
/// [`SYS_CATALOG_FACADE`], under the same version guard. Every view here is new
/// (never an existing one) so `CREATE OR REPLACE VIEW`'s append-only rule
/// cannot bite. Column lists follow Oracle's documented names.
const IDE_INTROSPECTION_STUBS: &str = "
    -- tables: variants & partitioning ------------------------------------------
    CREATE OR REPLACE VIEW sys.all_all_tables AS SELECT * FROM sys.all_tables WHERE false;
    CREATE OR REPLACE VIEW sys.all_external_tables AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS type_owner,
             NULL::varchar AS type_name, NULL::varchar AS default_directory_owner,
             NULL::varchar AS default_directory_name, NULL::varchar AS reject_limit,
             NULL::varchar AS access_type, NULL::varchar AS access_parameters,
             NULL::varchar AS property WHERE false;
    CREATE OR REPLACE VIEW sys.all_external_locations AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS location,
             NULL::varchar AS directory_owner, NULL::varchar AS directory_name WHERE false;
    CREATE OR REPLACE VIEW sys.all_part_tables AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name,
             NULL::varchar AS partitioning_type, NULL::varchar AS subpartitioning_type,
             NULL::numeric AS partition_count, NULL::numeric AS def_subpartition_count,
             NULL::numeric AS partitioning_key_count, NULL::varchar AS status,
             NULL::varchar AS def_tablespace_name, NULL::varchar AS interval,
             NULL::varchar AS autolist, NULL::varchar AS def_compression WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_partitions AS
      SELECT NULL::varchar AS table_owner, NULL::varchar AS table_name,
             NULL::varchar AS composite, NULL::varchar AS partition_name,
             NULL::numeric AS subpartition_count, NULL::varchar AS high_value,
             NULL::numeric AS high_value_length, NULL::numeric AS partition_position,
             NULL::varchar AS tablespace_name, NULL::numeric AS pct_free,
             NULL::numeric AS ini_trans, NULL::varchar AS logging,
             NULL::varchar AS compression, NULL::numeric AS num_rows,
             NULL::numeric AS blocks, NULL::numeric AS avg_row_len,
             NULL::numeric AS sample_size, NULL::timestamp AS last_analyzed,
             NULL::varchar AS interval, NULL::varchar AS segment_created,
             NULL::varchar AS indexing, NULL::varchar AS read_only WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_subpartitions AS
      SELECT NULL::varchar AS table_owner, NULL::varchar AS table_name,
             NULL::varchar AS partition_name, NULL::varchar AS subpartition_name,
             NULL::varchar AS high_value, NULL::numeric AS subpartition_position,
             NULL::varchar AS tablespace_name, NULL::numeric AS num_rows,
             NULL::timestamp AS last_analyzed WHERE false;
    CREATE OR REPLACE VIEW sys.all_part_key_columns AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS object_type,
             NULL::varchar AS column_name, NULL::numeric AS column_position WHERE false;
    CREATE OR REPLACE VIEW sys.all_subpart_key_columns AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS object_type,
             NULL::varchar AS column_name, NULL::numeric AS column_position WHERE false;
    CREATE OR REPLACE VIEW sys.all_part_indexes AS
      SELECT NULL::varchar AS owner, NULL::varchar AS index_name, NULL::varchar AS table_name,
             NULL::varchar AS partitioning_type, NULL::numeric AS partition_count,
             NULL::varchar AS locality, NULL::varchar AS alignment WHERE false;
    CREATE OR REPLACE VIEW sys.all_ind_partitions AS
      SELECT NULL::varchar AS index_owner, NULL::varchar AS index_name,
             NULL::varchar AS partition_name, NULL::varchar AS status,
             NULL::varchar AS tablespace_name, NULL::numeric AS partition_position,
             NULL::varchar AS high_value, NULL::timestamp AS last_analyzed WHERE false;
    CREATE OR REPLACE VIEW sys.all_ind_subpartitions AS
      SELECT NULL::varchar AS index_owner, NULL::varchar AS index_name,
             NULL::varchar AS partition_name, NULL::varchar AS subpartition_name,
             NULL::varchar AS status, NULL::numeric AS subpartition_position WHERE false;
    CREATE OR REPLACE VIEW sys.all_nested_tables AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name,
             NULL::varchar AS table_type_owner, NULL::varchar AS table_type_name,
             NULL::varchar AS parent_table_name, NULL::varchar AS parent_table_column WHERE false;

    -- columns: identity / lobs / stats ----------------------------------------
    CREATE OR REPLACE VIEW sys.all_tab_identity_cols AS
      SELECT upper(n.nspname)::varchar AS owner, upper(c.relname)::varchar AS table_name,
             upper(a.attname)::varchar AS column_name,
             (CASE a.attidentity WHEN 'a' THEN 'ALWAYS' WHEN 'd' THEN 'BY DEFAULT'
                                 ELSE NULL END)::varchar AS generation_type,
             NULL::varchar AS identity_options, NULL::varchar AS sequence_name
      FROM pg_catalog.pg_attribute a
      JOIN pg_catalog.pg_class c ON c.oid=a.attrelid
      JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
      WHERE a.attidentity IN ('a','d') AND NOT a.attisdropped;
    CREATE OR REPLACE VIEW sys.all_lobs AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS column_name,
             NULL::varchar AS segment_name, NULL::varchar AS tablespace_name,
             NULL::varchar AS index_name, NULL::numeric AS chunk, NULL::varchar AS pctversion,
             NULL::numeric AS retention, NULL::varchar AS cache, NULL::varchar AS logging,
             NULL::varchar AS in_row, NULL::varchar AS securefile WHERE false;
    CREATE OR REPLACE VIEW sys.all_lob_partitions AS
      SELECT NULL::varchar AS table_owner, NULL::varchar AS table_name, NULL::varchar AS column_name,
             NULL::varchar AS lob_name, NULL::varchar AS partition_name,
             NULL::varchar AS lob_partition_name, NULL::varchar AS tablespace_name WHERE false;
    CREATE OR REPLACE VIEW sys.all_varrays AS
      SELECT NULL::varchar AS owner, NULL::varchar AS parent_table_name,
             NULL::varchar AS parent_table_column, NULL::varchar AS type_owner,
             NULL::varchar AS type_name, NULL::varchar AS lob_name,
             NULL::varchar AS storage_spec, NULL::varchar AS return_type WHERE false;
    CREATE OR REPLACE VIEW sys.all_encrypted_columns AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS column_name,
             NULL::varchar AS encryption_alg, NULL::varchar AS salt WHERE false;
    CREATE OR REPLACE VIEW sys.all_unused_col_tabs AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::numeric AS count WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_col_statistics AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS column_name,
             NULL::numeric AS num_distinct, NULL::numeric AS density,
             NULL::numeric AS num_nulls, NULL::timestamp AS last_analyzed,
             NULL::numeric AS sample_size, NULL::varchar AS histogram WHERE false;
    CREATE OR REPLACE VIEW sys.all_part_col_statistics AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS partition_name,
             NULL::varchar AS column_name, NULL::numeric AS num_distinct WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_histograms AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS column_name,
             NULL::numeric AS endpoint_number, NULL::numeric AS endpoint_value WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_modifications AS
      SELECT NULL::varchar AS table_owner, NULL::varchar AS table_name, NULL::varchar AS partition_name,
             NULL::numeric AS inserts, NULL::numeric AS updates, NULL::numeric AS deletes,
             NULL::timestamp AS timestamp, NULL::varchar AS truncated WHERE false;

    -- indexes: extras --------------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_ind_statistics AS
      SELECT NULL::varchar AS owner, NULL::varchar AS index_name, NULL::varchar AS table_owner,
             NULL::varchar AS table_name, NULL::numeric AS blevel, NULL::numeric AS leaf_blocks,
             NULL::numeric AS distinct_keys, NULL::numeric AS clustering_factor,
             NULL::timestamp AS last_analyzed WHERE false;
    CREATE OR REPLACE VIEW sys.all_join_ind_columns AS
      SELECT NULL::varchar AS index_owner, NULL::varchar AS index_name,
             NULL::varchar AS inner_table_owner, NULL::varchar AS inner_table_name,
             NULL::varchar AS inner_column_name, NULL::varchar AS outer_table_owner,
             NULL::varchar AS outer_table_name, NULL::varchar AS outer_column_name WHERE false;

    -- triggers: extras -----------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_trigger_cols AS
      SELECT NULL::varchar AS trigger_owner, NULL::varchar AS trigger_name,
             NULL::varchar AS table_owner, NULL::varchar AS table_name,
             NULL::varchar AS column_name, NULL::varchar AS column_list,
             NULL::varchar AS column_usage WHERE false;
    CREATE OR REPLACE VIEW sys.all_trigger_ordering AS
      SELECT NULL::varchar AS trigger_owner, NULL::varchar AS trigger_name,
             NULL::varchar AS referenced_trigger_owner, NULL::varchar AS referenced_trigger_name,
             NULL::varchar AS ordering_type WHERE false;

    -- views: extras ------------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_updatable_columns AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS column_name,
             NULL::varchar AS updatable, NULL::varchar AS insertable, NULL::varchar AS deletable WHERE false;
    CREATE OR REPLACE VIEW sys.all_editioning_views AS
      SELECT NULL::varchar AS owner, NULL::varchar AS view_name, NULL::varchar AS table_owner,
             NULL::varchar AS table_name, NULL::varchar AS owner_id WHERE false;

    -- materialized views: refresh metadata --------------------------------
    CREATE OR REPLACE VIEW sys.all_registered_mviews AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS mview_site,
             NULL::varchar AS mview_owner, NULL::varchar AS mview_name,
             NULL::varchar AS can_use_log, NULL::varchar AS updatable, NULL::varchar AS refresh_method WHERE false;
    CREATE OR REPLACE VIEW sys.all_base_table_mviews AS
      SELECT NULL::varchar AS owner, NULL::varchar AS master, NULL::varchar AS mview_last_refresh_time,
             NULL::numeric AS mview_id WHERE false;
    CREATE OR REPLACE VIEW sys.all_mview_analysis AS
      SELECT NULL::varchar AS owner, NULL::varchar AS mview_name, NULL::varchar AS last_refresh_date,
             NULL::varchar AS query, NULL::numeric AS query_len WHERE false;
    CREATE OR REPLACE VIEW sys.all_mview_detail_relations AS
      SELECT NULL::varchar AS owner, NULL::varchar AS mview_name, NULL::varchar AS detailobj_owner,
             NULL::varchar AS detailobj_name, NULL::varchar AS detailobj_type WHERE false;
    CREATE OR REPLACE VIEW sys.all_refresh AS
      SELECT NULL::varchar AS rowner, NULL::varchar AS rname, NULL::numeric AS refgroup,
             NULL::varchar AS implicit_destroy, NULL::varchar AS broken WHERE false;
    CREATE OR REPLACE VIEW sys.all_refresh_children AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS type,
             NULL::varchar AS rowner, NULL::varchar AS rname WHERE false;
    CREATE OR REPLACE VIEW sys.all_summaries AS
      SELECT NULL::varchar AS owner, NULL::varchar AS summary_name, NULL::varchar AS last_refresh_date WHERE false;

    -- object types --------------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_type_attrs AS
      SELECT NULL::varchar AS owner, NULL::varchar AS type_name, NULL::varchar AS attr_name,
             NULL::varchar AS attr_type_mod, NULL::varchar AS attr_type_owner,
             NULL::varchar AS attr_type_name, NULL::numeric AS length, NULL::numeric AS precision,
             NULL::numeric AS scale, NULL::varchar AS character_set_name,
             NULL::numeric AS attr_no, NULL::varchar AS inherited WHERE false;
    CREATE OR REPLACE VIEW sys.all_type_methods AS
      SELECT NULL::varchar AS owner, NULL::varchar AS type_name, NULL::varchar AS method_name,
             NULL::numeric AS method_no, NULL::varchar AS method_type, NULL::numeric AS parameters,
             NULL::numeric AS results, NULL::varchar AS final, NULL::varchar AS instantiable,
             NULL::varchar AS inherited WHERE false;
    CREATE OR REPLACE VIEW sys.all_method_params AS
      SELECT NULL::varchar AS owner, NULL::varchar AS type_name, NULL::varchar AS method_name,
             NULL::numeric AS method_no, NULL::varchar AS param_name, NULL::numeric AS param_no,
             NULL::varchar AS param_mode, NULL::varchar AS param_type_owner, NULL::varchar AS param_type_name WHERE false;
    CREATE OR REPLACE VIEW sys.all_method_results AS
      SELECT NULL::varchar AS owner, NULL::varchar AS type_name, NULL::varchar AS method_name,
             NULL::numeric AS method_no, NULL::varchar AS result_type_owner, NULL::varchar AS result_type_name WHERE false;
    CREATE OR REPLACE VIEW sys.all_coll_types AS
      SELECT NULL::varchar AS owner, NULL::varchar AS type_name, NULL::varchar AS coll_type,
             NULL::numeric AS upper_bound, NULL::varchar AS elem_type_owner,
             NULL::varchar AS elem_type_name, NULL::numeric AS length, NULL::numeric AS precision,
             NULL::numeric AS scale, NULL::varchar AS elem_storage, NULL::varchar AS nulls_stored WHERE false;

    -- code / source -----------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_identifiers AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS signature,
             NULL::varchar AS type, NULL::varchar AS object_name, NULL::varchar AS object_type,
             NULL::varchar AS usage, NULL::numeric AS usage_id, NULL::numeric AS line, NULL::numeric AS col WHERE false;
    CREATE OR REPLACE VIEW sys.all_plsql_object_settings AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS type,
             NULL::varchar AS plsql_optimize_level, NULL::varchar AS plsql_code_type,
             NULL::varchar AS plsql_debug, NULL::varchar AS plsql_warnings, NULL::varchar AS nls_length_semantics WHERE false;
    CREATE OR REPLACE VIEW sys.all_stored_settings AS
      SELECT NULL::varchar AS owner, NULL::varchar AS object_name, NULL::bigint AS object_id,
             NULL::varchar AS param_name, NULL::varchar AS param_value WHERE false;

    -- privileges --------------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_tab_privs AS
      SELECT NULL::varchar AS grantor, NULL::varchar AS grantee, NULL::varchar AS table_schema,
             NULL::varchar AS table_name, NULL::varchar AS privilege, NULL::varchar AS grantable,
             NULL::varchar AS hierarchy, NULL::varchar AS common, NULL::varchar AS type,
             NULL::varchar AS inherited WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_privs_made AS
      SELECT NULL::varchar AS grantee, NULL::varchar AS owner, NULL::varchar AS table_name,
             NULL::varchar AS grantor, NULL::varchar AS privilege, NULL::varchar AS grantable WHERE false;
    CREATE OR REPLACE VIEW sys.all_tab_privs_recd AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS grantor,
             NULL::varchar AS grantee, NULL::varchar AS privilege, NULL::varchar AS grantable WHERE false;
    CREATE OR REPLACE VIEW sys.all_col_privs AS
      SELECT NULL::varchar AS grantor, NULL::varchar AS grantee, NULL::varchar AS table_schema,
             NULL::varchar AS table_name, NULL::varchar AS column_name, NULL::varchar AS privilege,
             NULL::varchar AS grantable WHERE false;
    CREATE OR REPLACE VIEW sys.all_sys_privs AS
      SELECT NULL::varchar AS grantee, NULL::varchar AS privilege, NULL::varchar AS admin_option,
             NULL::varchar AS common, NULL::varchar AS inherited WHERE false;
    CREATE OR REPLACE VIEW sys.all_role_privs AS
      SELECT NULL::varchar AS grantee, NULL::varchar AS granted_role, NULL::varchar AS admin_option,
             NULL::varchar AS default_role, NULL::varchar AS common WHERE false;

    -- storage-ish ------------------------------------------------------
    CREATE OR REPLACE VIEW sys.all_tablespaces AS
      SELECT 'USERS'::varchar AS tablespace_name, NULL::numeric AS block_size,
             NULL::numeric AS initial_extent, NULL::numeric AS next_extent,
             'ONLINE'::varchar AS status, 'PERMANENT'::varchar AS contents,
             'LOGGING'::varchar AS logging WHERE false;
    CREATE OR REPLACE VIEW sys.all_segments AS
      SELECT NULL::varchar AS owner, NULL::varchar AS segment_name, NULL::varchar AS segment_type,
             NULL::varchar AS tablespace_name, NULL::numeric AS bytes, NULL::numeric AS blocks WHERE false;
    CREATE OR REPLACE VIEW sys.all_objects_ae AS SELECT * FROM sys.all_objects WHERE false;

    -- server-side programs / schedulers / queues --------------------------
    CREATE OR REPLACE VIEW sys.all_scheduler_programs AS
      SELECT NULL::varchar AS owner, NULL::varchar AS program_name, NULL::varchar AS program_type,
             NULL::varchar AS program_action, NULL::numeric AS number_of_arguments,
             NULL::varchar AS enabled, NULL::varchar AS comments WHERE false;
    CREATE OR REPLACE VIEW sys.all_scheduler_schedules AS
      SELECT NULL::varchar AS owner, NULL::varchar AS schedule_name, NULL::varchar AS schedule_type,
             NULL::varchar AS start_date, NULL::varchar AS repeat_interval, NULL::varchar AS comments WHERE false;
    CREATE OR REPLACE VIEW sys.all_jobs AS
      SELECT NULL::numeric AS job, NULL::varchar AS log_user, NULL::varchar AS schema_user,
             NULL::varchar AS what, NULL::varchar AS broken, NULL::varchar AS interval WHERE false;
    CREATE OR REPLACE VIEW sys.all_queue_tables AS
      SELECT NULL::varchar AS owner, NULL::varchar AS queue_table, NULL::varchar AS type,
             NULL::varchar AS object_type, NULL::varchar AS sort_order, NULL::varchar AS recipients,
             NULL::varchar AS message_grouping, NULL::varchar AS compatible WHERE false;

    -- schema objects with no PG analogue -------------------------------
    CREATE OR REPLACE VIEW sys.all_directories AS
      SELECT NULL::varchar AS owner, NULL::varchar AS directory_name, NULL::varchar AS directory_path,
             NULL::varchar AS origin_con_id WHERE false;
    CREATE OR REPLACE VIEW sys.all_libraries AS
      SELECT NULL::varchar AS owner, NULL::varchar AS library_name, NULL::varchar AS file_spec,
             NULL::varchar AS dynamic, NULL::varchar AS status WHERE false;
    CREATE OR REPLACE VIEW sys.all_java_classes AS
      SELECT NULL::varchar AS owner, NULL::varchar AS name, NULL::varchar AS kind,
             NULL::varchar AS accessibility, NULL::varchar AS is_inner, NULL::varchar AS source WHERE false;
    CREATE OR REPLACE VIEW sys.all_clusters AS
      SELECT NULL::varchar AS owner, NULL::varchar AS cluster_name, NULL::varchar AS tablespace_name,
             NULL::varchar AS cluster_type, NULL::numeric AS hashkeys WHERE false;
    CREATE OR REPLACE VIEW sys.all_cluster_hash_expressions AS
      SELECT NULL::varchar AS owner, NULL::varchar AS cluster_name, NULL::varchar AS hash_expression WHERE false;
    CREATE OR REPLACE VIEW sys.all_operators AS
      SELECT NULL::varchar AS owner, NULL::varchar AS operator_name, NULL::numeric AS number_of_binds WHERE false;
    CREATE OR REPLACE VIEW sys.all_indextypes AS
      SELECT NULL::varchar AS owner, NULL::varchar AS indextype_name, NULL::varchar AS implementation_owner,
             NULL::varchar AS implementation_name, NULL::numeric AS number_of_operators, NULL::varchar AS status WHERE false;
    CREATE OR REPLACE VIEW sys.all_context AS
      SELECT NULL::varchar AS namespace, NULL::varchar AS schema, NULL::varchar AS package,
             NULL::varchar AS type, NULL::varchar AS origin_con_id WHERE false;
    CREATE OR REPLACE VIEW sys.all_policies AS
      SELECT NULL::varchar AS object_owner, NULL::varchar AS object_name, NULL::varchar AS policy_group,
             NULL::varchar AS policy_name, NULL::varchar AS pf_owner, NULL::varchar AS package,
             NULL::varchar AS function, NULL::varchar AS enable WHERE false;
    CREATE OR REPLACE VIEW sys.all_dimensions AS
      SELECT NULL::varchar AS owner, NULL::varchar AS dimension_name, NULL::varchar AS invalid,
             NULL::numeric AS revision WHERE false;
    CREATE OR REPLACE VIEW sys.all_xml_schemas AS
      SELECT NULL::varchar AS owner, NULL::varchar AS schema_url, NULL::varchar AS local,
             NULL::varchar AS schema, NULL::bigint AS schema_id, NULL::varchar AS qual_schema_url WHERE false;
    CREATE OR REPLACE VIEW sys.all_xml_tables AS
      SELECT NULL::varchar AS owner, NULL::varchar AS table_name, NULL::varchar AS xmlschema,
             NULL::varchar AS schema_owner, NULL::varchar AS element_name, NULL::varchar AS storage_type WHERE false;
    CREATE OR REPLACE VIEW sys.all_editions AS
      SELECT NULL::varchar AS edition_name, NULL::varchar AS parent_edition_name WHERE false;

    -- version / NLS / DB props ---------------------------------------------
    CREATE OR REPLACE VIEW sys.product_component_version AS
      SELECT 'PgSaci'::varchar AS product,
             (current_setting('server_version_num')::int / 10000 || '.0.0.0.0')::varchar AS version,
             (current_setting('server_version_num')::int / 10000 || '.0.0.0.0')::varchar AS version_full,
             'Production'::varchar AS status;
    CREATE OR REPLACE VIEW sys.nls_database_parameters AS
      SELECT * FROM (VALUES
        ('NLS_CHARACTERSET'::varchar, 'AL32UTF8'::varchar),
        ('NLS_NCHAR_CHARACTERSET', 'AL16UTF16'),
        ('NLS_LANGUAGE', 'AMERICAN'), ('NLS_TERRITORY', 'AMERICA'),
        ('NLS_DATE_FORMAT', 'DD-MON-RR'),
        ('NLS_TIMESTAMP_FORMAT', 'DD-MON-RR HH24.MI.SSXFF'),
        ('NLS_NUMERIC_CHARACTERS', '.,'), ('NLS_SORT', 'BINARY'),
        ('NLS_COMP', 'BINARY'), ('NLS_CALENDAR', 'GREGORIAN'),
        ('NLS_RDBMS_VERSION', '19.0.0.0.0')
      ) AS t(parameter, value);
    CREATE OR REPLACE VIEW sys.database_properties AS
      SELECT * FROM (VALUES
        ('DEFAULT_TEMP_TABLESPACE'::varchar, 'TEMP'::varchar, NULL::varchar),
        ('DEFAULT_PERMANENT_TABLESPACE', 'USERS', NULL),
        ('DEFAULT_EDITION', 'ORA$BASE', NULL),
        ('NLS_CHARACTERSET', 'AL32UTF8', NULL),
        ('DICT.BASE', '2', NULL)
      ) AS t(property_name, property_value, description);
    CREATE OR REPLACE VIEW sys.global_name AS
      SELECT (upper(current_database()) || '')::varchar AS global_name;
";

/// `DBMS_METADATA.GET_DDL('TABLE', name, schema)` — IDEs offer a "copy original
/// DDL" action that calls it. Reconstructs a readable `CREATE …` from
/// `pg_catalog` (PostgreSQL syntax, not Oracle's). Raw string so `E'\n'` etc.
/// reach PostgreSQL intact. Applied under the same version guard as
/// [`SYS_CATALOG_FACADE`].
const DBMS_METADATA_FACADE: &str = r#"
CREATE SCHEMA IF NOT EXISTS dbms_metadata;
CREATE OR REPLACE FUNCTION dbms_metadata.get_ddl(
  object_type text, name text, schema text DEFAULT NULL)
RETURNS text LANGUAGE plpgsql STABLE AS
$fn$
DECLARE
  nsp  text := lower(coalesce(nullif(schema, ''), current_schema()));
  rel  text := lower(name);
  q    text;
  oid_ oid;
  body text; cons text; idx text;
BEGIN
  -- Fall back to the session schema if the requested one does not exist (e.g.
  -- an IDE asking for schema = the connected user on a backend where the role
  -- could not create its own schema and objects live in `oracle`/`public`).
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = nsp) THEN
    nsp := current_schema();
  END IF;
  q := quote_ident(nsp) || '.' || quote_ident(rel);

  SELECT c.oid INTO oid_
  FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = nsp AND c.relname = rel;

  IF upper(object_type) IN ('TABLE') THEN
    IF oid_ IS NULL THEN RETURN '-- table ' || q || ' not found'; END IF;
    SELECT string_agg('  ' || quote_ident(a.attname) || ' '
             || format_type(a.atttypid, a.atttypmod)
             || CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END
             || COALESCE(' DEFAULT ' || pg_get_expr(ad.adbin, ad.adrelid), ''),
             E',\n' ORDER BY a.attnum)
      INTO body
      FROM pg_catalog.pg_attribute a
      LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
      WHERE a.attrelid = oid_ AND a.attnum > 0 AND NOT a.attisdropped;
    SELECT string_agg(E',\n  CONSTRAINT ' || quote_ident(conname) || ' '
                      || pg_get_constraintdef(c.oid), '' ORDER BY c.contype DESC, conname)
      INTO cons FROM pg_catalog.pg_constraint c WHERE c.conrelid = oid_;
    SELECT string_agg(E'\n' || pg_get_indexdef(i.indexrelid) || ';', '')
      INTO idx FROM pg_catalog.pg_index i
      WHERE i.indrelid = oid_ AND NOT i.indisprimary
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid = i.indexrelid);
    RETURN E'\n  CREATE TABLE ' || q || E' (\n' || body || COALESCE(cons, '')
           || E'\n);\n' || COALESCE(idx, '');
  ELSIF upper(object_type) IN ('VIEW') THEN
    IF oid_ IS NULL THEN RETURN '-- view ' || q || ' not found'; END IF;
    RETURN E'\n  CREATE VIEW ' || q || E' AS\n' || pg_get_viewdef(oid_, true);
  ELSIF upper(object_type) IN ('MATERIALIZED_VIEW', 'MATERIALIZED VIEW') THEN
    RETURN E'\n  CREATE MATERIALIZED VIEW ' || q || E' AS\n' || pg_get_viewdef(oid_, true);
  ELSIF upper(object_type) IN ('INDEX') THEN
    IF oid_ IS NULL THEN RETURN '-- index ' || q || ' not found'; END IF;
    RETURN E'\n  ' || pg_get_indexdef(oid_) || ';';
  ELSIF upper(object_type) IN ('SEQUENCE') THEN
    RETURN E'\n  CREATE SEQUENCE ' || q || ';';
  ELSIF upper(object_type) IN ('TRIGGER') THEN
    RETURN (SELECT E'\n  ' || pg_get_triggerdef(t.oid) || ';'
            FROM pg_catalog.pg_trigger t WHERE t.tgname = rel AND NOT t.tgisinternal LIMIT 1);
  ELSIF upper(object_type) IN ('FUNCTION', 'PROCEDURE') THEN
    RETURN (SELECT E'\n  ' || pg_get_functiondef(p.oid)
            FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
            WHERE n.nspname = nsp AND p.proname = rel LIMIT 1);
  ELSE
    RETURN '-- DBMS_METADATA.GET_DDL is not implemented for object type ' || object_type;
  END IF;
END
$fn$;
CREATE OR REPLACE FUNCTION dbms_metadata.set_transform_param(
  transform_handle numeric, name text, value boolean DEFAULT true,
  object_type text DEFAULT NULL) RETURNS void LANGUAGE sql AS $$ SELECT $$;
CREATE OR REPLACE FUNCTION dbms_metadata.set_transform_param(
  transform_handle numeric, name text, value numeric,
  object_type text DEFAULT NULL) RETURNS void LANGUAGE sql AS $$ SELECT $$;
CREATE OR REPLACE FUNCTION dbms_metadata.session_transform() RETURNS numeric
  LANGUAGE sql IMMUTABLE AS $$ SELECT (-1)::numeric $$;
"#;

#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
}

/// How much column-describe fidelity the connected client's TTC describe parser
/// tolerates. The strict thin drivers accept more than the historical baseline.
#[derive(Debug, Clone, Copy)]
pub struct DescribeCaps {
    /// Report a declared `NUMBER(p, s)`'s real precision/scale instead of the
    /// `(38, 0)` fallback. python-oracledb thin and oracle-rs handle it; the
    /// ojdbc / ODP.NET column-metadata parser desyncs on a non-zero scale
    /// field, so they don't.
    pub report_number_scale: bool,
    /// Promote a `BINARY_FLOAT` / `BINARY_DOUBLE` column to the native Oracle
    /// type (100 / 101). Only python-oracledb thin decodes those from the wire
    /// correctly today.
    pub native_binary_floats: bool,
    /// Emit `INTERVAL YEAR TO MONTH` / `DAY TO SECOND` columns as the native
    /// Oracle types (182 / 183). python-oracledb thin decodes them; the others
    /// get an Oracle-style text rendering instead.
    pub native_intervals: bool,
    /// Describe PostgreSQL `timestamp` / `timestamptz` columns as the native
    /// Oracle `TIMESTAMP` (180) / `TIMESTAMP WITH TIME ZONE` (181) types.
    pub native_timestamps: bool,
    /// The connected client is the OCI thick driver. Its `TIMESTAMP WITH TIME
    /// ZONE` describe/value decoder differs from the thin drivers': it wants a
    /// `buffer_size` of 1 in the describe and the `0x40` "explicit UTC offset"
    /// bit in the value's tz byte. The thin path (oracle-rs / ojdbc thin /
    /// ODP.NET) wants the classic 13-byte form, so the two cannot share one
    /// encoding.
    pub oci: bool,
    /// ojdbc thin / ODP.NET describe PG `timestamp` / `timestamptz` columns as
    /// Oracle **DATE** (internal type 12), not the native `TIMESTAMP` (180) /
    /// `TIMESTAMP WITH TIME ZONE` (181). Two reasons: (1) their column-metadata
    /// parser desyncs on the native datetime descriptor (a non-zero scale field
    /// shifts its reads and it overruns an 8-byte scratch buffer — an
    /// `ArrayIndexOutOfBoundsException`); (2) the overwhelmingly common
    /// real-Oracle case is a `DATE` column (which the DDL translator turns into
    /// PG `timestamp(0)`), and ojdbc maps Oracle `DATE` straight to
    /// `java.sql.Timestamp`, so `rs.getObject()` returns what apps expect —
    /// native `TIMESTAMP` would return `oracle.sql.TIMESTAMP` and break a
    /// `(java.sql.Timestamp)` cast (exactly as against real Oracle, but pgSaci
    /// can't tell DATE-origin from TIMESTAMP-origin PG `timestamp` apart).
    /// Known limitation: an Oracle `TIMESTAMP(n>0)` column queried over
    /// ojdbc/ODP.NET comes back with second precision and
    /// `getColumnTypeName() == "DATE"`. OCI thick and the thin drivers keep the
    /// native types.
    pub datetime_as_date: bool,
}

impl DescribeCaps {
    /// The historical baseline: everything as `NUMBER(38, 0)` / text.
    pub const LENIENT: Self = Self {
        report_number_scale: false,
        native_binary_floats: false,
        native_intervals: false,
        native_timestamps: false,
        oci: false,
        datetime_as_date: false,
    };

    /// Pick the describe-fidelity level from negotiated wire capabilities.
    /// `oac_strict` marks the clients whose column-describe parser needs an
    /// exact column descriptor (the newer describe path); it is independent of
    /// the OCI dialect.
    pub fn for_client(
        response_completion: bool,
        newer_describe_framing: bool,
        oci_dialect: bool,
        oac_strict: bool,
    ) -> Self {
        let thin_strict = response_completion && !newer_describe_framing;
        Self {
            report_number_scale: !newer_describe_framing,
            native_binary_floats: thin_strict,
            native_intervals: thin_strict,
            native_timestamps: true,
            oci: oci_dialect,
            datetime_as_date: oac_strict,
        }
    }
}

/// A cached prepared statement whose plan PostgreSQL rejected because an
/// intervening DDL changed the result shape.
fn is_stale_plan(e: &tokio_postgres::Error) -> bool {
    e.as_db_error().is_some_and(|db| {
        let m = db.message();
        m.contains("cached plan must not change result type")
            || m.contains("cached plan must not change")
    })
}

/// The PostgreSQL server's 1-based character position into the failing query,
/// when it reported one. `Internal` positions (into a generated query) are not
/// meaningful to the Oracle client and are dropped.
fn pg_error_position(e: &tokio_postgres::Error) -> Option<u32> {
    use tokio_postgres::error::ErrorPosition;
    match e.as_db_error()?.position()? {
        ErrorPosition::Original(pos) => Some(*pos),
        ErrorPosition::Internal { .. } => None,
    }
}

fn pg_error_detail(e: &tokio_postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        format!("{}: {}", db.code().code(), db.message())
    } else {
        format!("{:#}", e)
    }
}

/// The raw components of a PostgreSQL `interval` value (binary wire form:
/// `i64` microseconds, `i32` days, `i32` months, all big-endian).
#[derive(Clone, Copy, Debug)]
struct PgInterval {
    months: i32,
    days: i32,
    micros: i64,
}

impl<'a> FromSql<'a> for PgInterval {
    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }

    fn from_sql(
        _: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 16 {
            return Err("interval payload is not 16 bytes".into());
        }
        let micros = i64::from_be_bytes(raw[0..8].try_into().unwrap());
        let days = i32::from_be_bytes(raw[8..12].try_into().unwrap());
        let months = i32::from_be_bytes(raw[12..16].try_into().unwrap());
        Ok(Self {
            months,
            days,
            micros,
        })
    }
}

impl PgInterval {
    /// Oracle INTERVAL YEAR TO MONTH wire form (5 bytes): `u32` years and a
    /// `u8` month, each biased (years by 2^31, month by 60).
    fn encode_year_to_month(&self) -> Vec<u8> {
        let years = self.months / 12;
        let rem_months = self.months % 12;
        let mut out = Vec::with_capacity(5);
        out.extend_from_slice(&((years as i64 + 0x8000_0000) as u32).to_be_bytes());
        out.push((rem_months + 60) as u8);
        out
    }

    /// Oracle INTERVAL DAY TO SECOND wire form (11 bytes): `u32` days (bias
    /// 2^31), `u8` hours/minutes/seconds (bias 60), `u32` fractional-second
    /// nanoseconds (bias 2^31). Months, if any, are folded in as 30-day units.
    fn encode_day_to_second(&self) -> Vec<u8> {
        let total_days = self.days as i64 + self.months as i64 * 30;
        let mut secs = self.micros / 1_000_000;
        let nanos = (self.micros % 1_000_000) * 1000;
        // Carry whole days out of the sub-day component. Each Oracle field is
        // stored signed via its own bias, so a negative interval yields
        // negative hours/minutes/seconds — consistent with how the client
        // recombines them.
        let days = total_days + secs / 86_400;
        secs %= 86_400;
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        let mut out = Vec::with_capacity(11);
        out.extend_from_slice(&((days + 0x8000_0000) as u32).to_be_bytes());
        out.push((hours + 60) as u8);
        out.push((minutes + 60) as u8);
        out.push((seconds + 60) as u8);
        out.extend_from_slice(&((nanos + 0x8000_0000) as u32).to_be_bytes());
        out
    }

    /// Oracle-style text rendering for the lenient (non-native) path.
    /// `±YY-MM` for a pure year/month interval, else `±DD HH:MI:SS.FFFFFF`.
    fn oracle_text(&self, year_to_month: bool) -> String {
        if year_to_month || (self.months != 0 && self.days == 0 && self.micros == 0) {
            let sign = if self.months < 0 { '-' } else { '+' };
            let m = self.months.abs();
            format!("{sign}{:02}-{:02}", m / 12, m % 12)
        } else {
            let total_days = self.days as i64 + self.months as i64 * 30;
            let mut secs = self.micros / 1_000_000;
            let frac = (self.micros % 1_000_000).abs();
            let days = total_days + secs / 86_400;
            secs %= 86_400;
            let neg = days < 0 || secs < 0 || (days == 0 && self.micros < 0);
            let sign = if neg { '-' } else { '+' };
            format!(
                "{sign}{:02} {:02}:{:02}:{:02}.{:06}",
                days.abs(),
                (secs.abs() / 3600),
                (secs.abs() % 3600) / 60,
                secs.abs() % 60,
                frac
            )
        }
    }
}

struct PgNumericText(String);

impl<'a> FromSql<'a> for PgNumericText {
    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }

    fn from_sql(
        _: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 8 {
            return Err("numeric payload too short".into());
        }
        let read =
            |at: usize| -> std::result::Result<i16, Box<dyn std::error::Error + Sync + Send>> {
                let b = raw.get(at..at + 2).ok_or("numeric payload truncated")?;
                Ok(i16::from_be_bytes([b[0], b[1]]))
            };
        let n = read(0)? as usize;
        let weight = read(2)? as i32;
        let sign = read(4)? as u16;
        let scale = read(6)?.max(0) as usize;
        if sign == 0xC000 {
            return Ok(Self("0".into()));
        }
        if raw.len() < 8 + n * 2 {
            return Err("numeric digits truncated".into());
        }
        let mut groups = Vec::with_capacity(n);
        for i in 0..n {
            let group = read(8 + i * 2)? as u16;
            groups.push(group);
        }
        // `point` is the number of base-10000 groups that belong to the integer
        // part.  Every group is a 4-digit base-10000 "digit": all of them are
        // zero-padded to four places here, and only the leading zeros of the
        // whole assembled integer part are stripped afterwards.  (The previous
        // implementation skipped padding on group 0 unconditionally, which
        // silently dropped the leading zeros of the first fractional group, so
        // e.g. 0.05 decoded as 0.5.)
        let point = weight + 1;
        let padded = |index: usize| -> String {
            groups
                .get(index)
                .map_or_else(|| "0000".to_string(), |g| format!("{g:04}"))
        };
        let integer_digits: String = if point <= 0 {
            "0".to_string()
        } else {
            let raw = (0..point as usize).map(padded).collect::<String>();
            let trimmed = raw.trim_start_matches('0');
            if trimmed.is_empty() {
                "0".to_string()
            } else {
                trimmed.to_string()
            }
        };
        let fraction_digits: String = if point <= 0 {
            let mut s = "0".repeat((-point) as usize * 4);
            s.push_str(&(0..n).map(padded).collect::<String>());
            s
        } else {
            (point as usize..n).map(padded).collect::<String>()
        };
        let mut text = if fraction_digits.is_empty() {
            integer_digits
        } else {
            format!("{integer_digits}.{fraction_digits}")
        };
        if scale == 0 {
            text = text.split('.').next().unwrap_or("0").to_string();
        } else if let Some((whole, fraction)) = text.split_once('.') {
            let fraction = fraction.trim_end_matches('0');
            text = if fraction.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{fraction}")
            };
        }
        if sign == 0x4000 {
            text.insert(0, '-');
        }
        Ok(Self(text))
    }
}

/// True when a PostgreSQL `interval` column's type modifier restricts it to the
/// YEAR / MONTH fields (`INTERVAL YEAR TO MONTH`). A bare `interval` (typmod -1)
/// or any day/time fields present ⇒ DAY TO SECOND.
fn interval_is_year_to_month(type_modifier: i32) -> bool {
    if type_modifier < 0 {
        return false;
    }
    let fields = (type_modifier >> 16) & 0x7FFF;
    // DAY=3, HOUR=10, MINUTE=11, SECOND=12 ⇒ bits 0x8 | 0x400 | 0x800 | 0x1000.
    const DAY_TIME: i32 = 0x1C08;
    // YEAR=2, MONTH=1 ⇒ bits 0x4 | 0x2.
    const YEAR_MONTH: i32 = 0x6;
    fields & DAY_TIME == 0 && fields & YEAR_MONTH != 0
}

fn pg_column_to_oracle_meta(
    col: &tokio_postgres::Column,
    position: usize,
    caps: DescribeCaps,
) -> ColumnMeta {
    use tokio_postgres::types::Type;
    let name = col.name();
    let ty = col.type_();
    let report_number_scale = caps.report_number_scale;
    // Fixed-shape helper for the non-NUMBER/non-VARCHAR types.
    let scalar = |oracle_type: u8, width: u32| ColumnMeta {
        name: name.into(),
        oracle_type,
        flags: 0,
        precision: 0,
        scale: 0,
        buffer_size: width,
        max_size: width,
        charset_id: 0,
        charset_form: 0,
        nullable: true,
        schema: None,
        type_name: None,
        position: position as u16,
    };
    if *ty == Type::NUMERIC {
        // PostgreSQL sends the RowDescription type modifier for a `numeric(p,s)`
        // column reference or an explicit cast; it is `-1` for bare `numeric`
        // and for computed expressions whose scale it cannot pin down (which is
        // also where Oracle stops reporting a useful precision). Decode it into
        // the Oracle NUMBER (precision, scale) pair instead of the `(38,0)`
        // fallback. `atttypmod` layout: `((p << 16) | s) + VARHDRSZ(4)`.
        let (precision, scale) = match col.type_modifier() {
            m if report_number_scale && m >= 4 => {
                let packed = (m - 4) as u32;
                (((packed >> 16) & 0xFFFF) as i8, (packed & 0xFFFF) as i8)
            }
            _ => (38, 0),
        };
        ColumnMeta::number(name, precision, scale)
    } else if *ty == Type::INT2
        || *ty == Type::INT4
        || *ty == Type::INT8
        || *ty == Type::FLOAT4
        || *ty == Type::FLOAT8
        || *ty == Type::OID
    {
        // PostgreSQL `float4`/`float8` back Oracle `POWER`/`SQRT`/`AVG`/`FLOAT` /
        // `DOUBLE PRECISION`, all of which are Oracle NUMBER, so NUMBER is the
        // right default. A column *declared* `BINARY_FLOAT`/`BINARY_DOUBLE` is
        // also reported as NUMBER (value-exact, not the native IEEE wire form) —
        // the PostgreSQL wire protocol does not tell the client which `float8`s
        // came from a `BINARY_DOUBLE` column.
        ColumnMeta::number(name, 38, 0)
    } else if *ty == Type::BYTEA {
        ColumnMeta {
            name: name.into(),
            oracle_type: 23,
            flags: 0,
            precision: 0,
            scale: 0,
            buffer_size: 4000,
            max_size: 4000,
            charset_id: 0,
            charset_form: 0,
            nullable: true,
            schema: None,
            type_name: None,
            position: position as u16,
        }
    } else if *ty == Type::VARCHAR
        || *ty == Type::TEXT
        || *ty == Type::BPCHAR
        || *ty == Type::NAME
        || *ty == Type::UNKNOWN
    {
        ColumnMeta::varchar(name, 4000)
    } else if *ty == Type::BOOL {
        ColumnMeta::number(name, 1, 0)
    } else if *ty == Type::DATE {
        // Oracle DATE — 7-byte form, no fractional seconds.
        scalar(12, 7)
    } else if *ty == Type::TIMESTAMP {
        if !caps.native_timestamps || caps.datetime_as_date {
            // OCI thick / ojdbc / ODP.NET: Oracle DATE (7-byte, second
            // precision). See `DescribeCaps::datetime_as_date`.
            scalar(12, 7)
        } else {
            // Oracle TIMESTAMP — 7-byte date + 4-byte big-endian nanoseconds.
            // `scale` carries the fractional-second precision (6 = default); the
            // OCI thick client's value decoder desyncs when it is left 0.
            let mut m = scalar(180, 11);
            m.scale = 6;
            m
        }
    } else if *ty == Type::TIMESTAMPTZ {
        if !caps.native_timestamps || caps.datetime_as_date {
            scalar(12, 7)
        } else {
            // Oracle TIMESTAMP WITH TIME ZONE (181). PostgreSQL stores
            // TIMESTAMPTZ as UTC. The OCI thick client's describe parser wants
            // `buffer_size` 1 / `max_size` 0 (verified against a live 21c
            // capture — the 13-byte value is length-framed in the row); the
            // thin drivers want the classic 13-byte declared form.
            let mut m = if caps.oci {
                let mut m = scalar(181, 1);
                m.max_size = 0;
                m
            } else {
                scalar(181, 13)
            };
            m.scale = 6;
            m
        }
    } else if *ty == Type::INTERVAL {
        let ytm = interval_is_year_to_month(col.type_modifier());
        match (caps.native_intervals, ytm) {
            // Oracle INTERVAL YEAR TO MONTH (182, 5 bytes) / DAY TO SECOND
            // (183, 11 bytes).
            (true, true) => scalar(182, 5),
            (true, false) => scalar(183, 11),
            // Lenient clients get an Oracle-style text rendering.
            (false, _) => ColumnMeta::varchar(name, 30),
        }
    } else {
        ColumnMeta::varchar(name, 4000)
    }
}

fn pg_value_to_oracle_bytes(row: &Row, idx: usize, oracle_type: u8, oci: bool) -> Option<Vec<u8>> {
    use tokio_postgres::types::Type;
    let col = row.columns().get(idx)?;

    if oracle_type == 2 {
        if *col.type_() == Type::INT2 {
            let v: Option<i16> = row.try_get(idx).ok().flatten();
            v.map(|v| encode_oracle_number_i64(v as i64))
        } else if *col.type_() == Type::INT4 {
            let v: Option<i32> = row.try_get(idx).ok().flatten();
            v.map(|v| encode_oracle_number_i64(v as i64))
        } else if *col.type_() == Type::INT8 {
            let v: Option<i64> = row.try_get(idx).ok().flatten();
            v.map(encode_oracle_number_i64)
        } else if *col.type_() == Type::BOOL {
            let v: Option<bool> = row.try_get(idx).ok().flatten();
            v.map(|v| encode_oracle_number_i64(if v { 1 } else { 0 }))
        } else if *col.type_() == Type::FLOAT4 {
            let v: Option<f32> = row.try_get(idx).ok().flatten();
            v.and_then(|v| encode_oracle_number_decimal(&v.to_string()).ok())
        } else if *col.type_() == Type::FLOAT8 {
            let v: Option<f64> = row.try_get(idx).ok().flatten();
            v.and_then(|v| encode_oracle_number_decimal(&v.to_string()).ok())
        } else if *col.type_() == Type::NUMERIC {
            let v: Option<PgNumericText> = row.try_get(idx).ok().flatten();
            v.and_then(|v| encode_oracle_number_decimal(&v.0).ok())
        } else {
            let v: Option<String> = row.try_get(idx).ok().flatten();
            v.map(|v| v.into_bytes())
        }
    } else if oracle_type == 23 {
        let v: Option<Vec<u8>> = row.try_get(idx).ok().flatten();
        v
    } else if oracle_type == 100 {
        // Oracle BINARY_FLOAT: 4-byte sign-adjusted IEEE, big-endian.
        let v: Option<f32> = row.try_get(idx).ok().flatten();
        v.map(|v| encode_binary_float(v).to_vec())
    } else if oracle_type == 101 {
        // Oracle BINARY_DOUBLE: 8-byte sign-adjusted IEEE, big-endian.
        let v: Option<f64> = row.try_get(idx).ok().flatten();
        v.map(|v| encode_binary_double(v).to_vec())
    } else if oracle_type == 12 {
        // Oracle DATE (7-byte, second precision). The source column is usually
        // PostgreSQL `date`, but `timestamp` / `timestamptz` also land here when
        // native TIMESTAMP describe is disabled (OCI thick) — accept all three.
        match *col.type_() {
            Type::TIMESTAMP => {
                let v: Option<NaiveDateTime> = row.try_get(idx).ok().flatten();
                v.map(encode_oracle_date)
            }
            Type::TIMESTAMPTZ => {
                let v: Option<DateTime<Utc>> = row.try_get(idx).ok().flatten();
                v.map(|v| encode_oracle_date(v.naive_utc()))
            }
            _ => {
                let v: Option<NaiveDate> = row.try_get(idx).ok().flatten();
                v.map(|v| encode_oracle_date(v.and_hms_opt(0, 0, 0).expect("midnight is valid")))
            }
        }
    } else if oracle_type == 180 {
        // Oracle TIMESTAMP: 7-byte date + 4-byte big-endian nanoseconds.
        let value: Option<NaiveDateTime> = row.try_get(idx).ok().flatten();
        value.map(|value| encode_oracle_timestamp(value, None))
    } else if oracle_type == 181 {
        // Oracle TIMESTAMP WITH TIME ZONE: 11-byte form + 2 tz bytes.
        // PostgreSQL hands back UTC, so the offset is +00:00.
        let value: Option<DateTime<Utc>> = row.try_get(idx).ok().flatten();
        value.map(|value| encode_oracle_timestamp_tz(value.naive_utc(), (0, 0), oci))
    } else if oracle_type == 182 {
        // Oracle INTERVAL YEAR TO MONTH (5 bytes).
        let v: Option<PgInterval> = row.try_get(idx).ok().flatten();
        v.map(|v| v.encode_year_to_month())
    } else if oracle_type == 183 {
        // Oracle INTERVAL DAY TO SECOND (11 bytes).
        let v: Option<PgInterval> = row.try_get(idx).ok().flatten();
        v.map(|v| v.encode_day_to_second())
    } else if *col.type_() == Type::INTERVAL {
        // Lenient path: a PostgreSQL `interval` described as VARCHAR — render it
        // in Oracle interval text form rather than failing the string decode.
        let v: Option<PgInterval> = row.try_get(idx).ok().flatten();
        v.map(|v| {
            v.oracle_text(interval_is_year_to_month(col.type_modifier()))
                .into_bytes()
        })
    } else {
        let v: Option<String> = row.try_get(idx).ok().flatten();
        v.map(|v| v.into_bytes())
    }
}

fn encode_oracle_date(value: NaiveDateTime) -> Vec<u8> {
    vec![
        (value.year() / 100 + 100) as u8,
        (value.year() % 100 + 100) as u8,
        value.month() as u8,
        value.day() as u8,
        (value.hour() + 1) as u8,
        (value.minute() + 1) as u8,
        (value.second() + 1) as u8,
    ]
}

/// Oracle TIMESTAMP wire form: the 7-byte DATE followed by 4 big-endian bytes of
/// sub-second nanoseconds.
fn encode_oracle_timestamp(value: NaiveDateTime, tz: Option<(i8, i8)>) -> Vec<u8> {
    let mut out = encode_oracle_date(value);
    // `nanosecond()` folds a leap second into 1_000_000_000..2e9; clamp so the
    // wire value never exceeds a second's worth of nanoseconds.
    let nanos = value.nanosecond().min(999_999_999);
    out.extend_from_slice(&nanos.to_be_bytes());
    if let Some((tz_hour, tz_minute)) = tz {
        out.push((tz_hour + 20) as u8);
        out.push((tz_minute + 60) as u8);
    }
    out
}

/// Oracle TIMESTAMP WITH TIME ZONE wire form: the 11-byte TIMESTAMP plus two tz
/// bytes. The tz-hour byte is `offset_hours + 20`; the thick (OCI) client also
/// needs bit `0x40` set to mark an explicit UTC offset (bit clear = named
/// region id) — verified against a live 21c capture (`-03:00` -> `0x51`,
/// `-05:00` -> `0x4f`). The thin drivers decode the classic form without it.
fn encode_oracle_timestamp_tz(
    value: NaiveDateTime,
    (tz_hour, tz_minute): (i8, i8),
    oci: bool,
) -> Vec<u8> {
    let mut out = encode_oracle_timestamp(value, None);
    let hour_byte = (tz_hour + 20) as u8;
    out.push(if oci { hour_byte | 0x40 } else { hour_byte });
    out.push((tz_minute + 60) as u8);
    out
}

/// Oracle BINARY_FLOAT wire form: IEEE-754 big-endian with the sign bit
/// manipulated so the byte order sorts numerically — positive values get the
/// high bit set, negative values are bitwise-inverted.
fn encode_binary_float(value: f32) -> [u8; 4] {
    let mut b = value.to_bits().to_be_bytes();
    if b[0] & 0x80 == 0 {
        b[0] |= 0x80;
    } else {
        for byte in &mut b {
            *byte = !*byte;
        }
    }
    b
}

/// Oracle BINARY_DOUBLE wire form (same sign-adjusted big-endian scheme).
fn encode_binary_double(value: f64) -> [u8; 8] {
    let mut b = value.to_bits().to_be_bytes();
    if b[0] & 0x80 == 0 {
        b[0] |= 0x80;
    } else {
        for byte in &mut b {
            *byte = !*byte;
        }
    }
    b
}
