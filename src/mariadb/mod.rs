//! MariaDB backend.
//!
//! MariaDB 11.8+ in `SQL_MODE=ORACLE` does the Oracle-language work; DbSaci
//! supplies what it lacks: the wire protocol, Oracle identifier semantics
//! ([`crate::translate::names`]), Oracle's transaction model, the data
//! dictionary ([`dictionary`], installed once in `dbsaci_sys`), and exact
//! describe metadata ([`types`]).
//!
//! Each Oracle session owns one MariaDB connection, driven by a task
//! ([`session`]) so query results stream instead of being buffered.

pub mod binds;
pub mod dictionary;
pub mod errors;
pub mod install;
pub mod instead_of;
pub mod session;
pub mod types;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder};
use tokio::sync::{mpsc, oneshot};

use crate::backend::{DescribeCaps, OracleBackend, OracleCursor};
use crate::error::{Error, Result};
use crate::translate::IdentifierCase;
use crate::translate::names::{Piece, pieces};
use crate::wire::{BindValue, ColumnMeta};
use session::{Cmd, Row, SessionParams, Shared};

/// Translations kept per session (ojdbc/Hibernate re-execute the same SQL).
const TRANSLATION_CACHE: usize = 512;

/// How DbSaci reaches a MariaDB server.
#[derive(Clone)]
pub struct MariaDbConfig {
    pub host: String,
    pub port: u16,
    /// Database used when no database is named after the login.
    pub default_db: String,
    pub statement_timeout: Option<Duration>,
    pub ssl: bool,
    /// PEM CA bundle to verify the server certificate with (implies `ssl`);
    /// otherwise the public web roots are used.
    pub ssl_ca: Option<std::path::PathBuf>,
    pub case: IdentifierCase,
}

impl MariaDbConfig {
    /// TLS options for a connection to this server, if TLS is on.
    fn ssl_opts(&self) -> Option<mysql_async::SslOpts> {
        match &self.ssl_ca {
            Some(ca) => {
                Some(mysql_async::SslOpts::default().with_root_certs(vec![ca.clone().into()]))
            }
            None if self.ssl => Some(mysql_async::SslOpts::default()),
            None => None,
        }
    }
}

/// What this process learned about each server on first contact.
#[derive(Clone, Copy)]
struct ServerFacts {
    /// `@@lower_case_table_names` (0 = case-sensitive table names).
    lower_case_table_names: u8,
}

static SERVERS: Mutex<Option<HashMap<String, ServerFacts>>> = Mutex::new(None);

/// One Oracle session on MariaDB.
pub struct MariaDbBackend {
    tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    /// Options for side connections (`KILL QUERY` on cancel).
    opts: Opts,
    case: IdentifierCase,
    translations: Mutex<HashMap<String, String>>,
}

impl MariaDbBackend {
    /// Log `login` in: the MariaDB account is the login in lower case; the
    /// session starts in the database named after the login (Oracle's
    /// schema == user), else in `cfg.default_db`.
    pub async fn connect(cfg: &MariaDbConfig, login: &str, password: &str) -> Result<Self> {
        let account = login.to_ascii_lowercase();
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(cfg.host.clone())
            .tcp_port(cfg.port)
            .user(Some(account))
            .pass(Some(password.to_string()))
            .prefer_socket(false)
            // UPDATE row count = rows matched (Oracle), not rows changed.
            .client_found_rows(true)
            .stmt_cache_size(256);
        if let Some(tls) = cfg.ssl_opts() {
            builder = builder.ssl_opts(tls);
        }
        let opts: Opts = builder.into();
        let facts = server_facts(cfg, &opts).await;
        // A server with lower_case_table_names != 0 stores table and database
        // names in lower case itself, whatever DbSaci sends: names read back
        // from it map to Oracle names the `Lower` way.
        let case = if facts.lower_case_table_names != 0 {
            IdentifierCase::Lower
        } else {
            cfg.case
        };
        let cfg = &MariaDbConfig {
            case,
            ..cfg.clone()
        };

        let login_oracle = login.to_ascii_uppercase();
        let shared = Arc::new(Shared::default());
        let mut params = SessionParams {
            opts: opts.clone(),
            schema: cfg.case.physical(&login_oracle),
            login: login_oracle,
            case: cfg.case,
            statement_timeout: cfg.statement_timeout,
        };
        let tx = match session::start(params.clone(), shared.clone()).await {
            Ok(tx) => tx,
            Err(Error::Ora { code: 1918, .. }) if !cfg.default_db.is_empty() => {
                params.schema = cfg.default_db.clone();
                params.login = crate::translate::names::oracle_name(&cfg.default_db, cfg.case);
                session::start(params, shared.clone()).await?
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            tx,
            shared,
            opts,
            case: cfg.case,
            translations: Mutex::new(HashMap::new()),
        })
    }

    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Cmd) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| Error::SessionLost("MariaDB session ended".into()))?;
        rx.await
            .map_err(|_| Error::SessionLost("MariaDB session ended".into()))?
    }
}

/// First contact with a server (per process): install / upgrade the data
/// dictionary if needed and possible, and read `lower_case_table_names`.
/// A missing dictionary (no privileges) is logged once; sessions still work,
/// and `dbsaci install` fixes it.
async fn server_facts(cfg: &MariaDbConfig, opts: &Opts) -> ServerFacts {
    let key = format!("{}:{}", cfg.host, cfg.port);
    if let Some(f) = SERVERS
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(&key).copied()))
    {
        return f;
    }
    let (dictionary, lctn) = match session::ensure_dictionary(opts).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("DbSaci data dictionary check failed: {e}");
            (false, 0)
        }
    };
    if !dictionary {
        tracing::warn!(
            "the DbSaci data dictionary (`{}`) is missing or outdated and this login cannot \
             install it; Oracle dictionary views (ALL_TABLES, USER_SEQUENCES, ...) will fail \
             until an administrator runs `dbsaci install`",
            dictionary::SCHEMA
        );
    }
    let facts = ServerFacts {
        lower_case_table_names: lctn,
    };
    if let Ok(mut m) = SERVERS.lock() {
        m.get_or_insert_with(HashMap::new).insert(key, facts);
    }
    facts
}

fn params_of(binds: &[BindValue]) -> Result<Vec<mysql_async::Value>> {
    binds.iter().map(binds::bind_value).collect()
}

/// A streamed MariaDB result.
struct MariaDbCursor {
    tx: mpsc::Sender<Cmd>,
    id: u64,
    columns: Vec<ColumnMeta>,
    exhausted: bool,
    closed: bool,
}

#[async_trait::async_trait]
impl OracleCursor for MariaDbCursor {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    async fn next_batch(&mut self, n: usize) -> Result<Vec<Row>> {
        if self.exhausted {
            return Ok(Vec::new());
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Fetch {
                cursor: self.id,
                n: n.max(1),
                reply,
            })
            .await
            .map_err(|_| Error::SessionLost("MariaDB session ended".into()))?;
        match rx.await {
            Ok(Ok((rows, done))) => {
                self.exhausted = done;
                if done {
                    self.closed = true;
                }
                Ok(rows)
            }
            Ok(Err(e)) => {
                self.exhausted = true;
                self.closed = true;
                Err(e)
            }
            Err(_) => Err(Error::SessionLost("MariaDB session ended".into())),
        }
    }

    async fn finish(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.tx.send(Cmd::Close { cursor: self.id }).await;
        }
    }
}

impl Drop for MariaDbCursor {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.tx.try_send(Cmd::Close { cursor: self.id });
        }
    }
}

/// The words of a statement, upper-cased, with their byte ranges.
fn words(sql: &str) -> Vec<(String, std::ops::Range<usize>)> {
    pieces(sql)
        .into_iter()
        .filter(|(p, _)| *p == Piece::Word)
        .map(|(_, r)| (sql[r.clone()].to_ascii_uppercase(), r))
        .collect()
}

/// A statement the session handles itself rather than translating.
enum SessionStatement {
    Alter(Vec<(String, String)>),
    SetTransaction(String),
    Commit,
    Rollback,
}

fn classify_session_statement(sql: &str) -> Option<SessionStatement> {
    let w = words(sql);
    let first = w.first()?.0.as_str();
    match first {
        "ALTER" if w.get(1)?.0 == "SESSION" && w.get(2)?.0 == "SET" => {
            let rest = &sql[w[2].1.end..];
            Some(SessionStatement::Alter(parse_settings(rest)))
        }
        "SET" if w.get(1)?.0 == "TRANSACTION" => {
            Some(SessionStatement::SetTransaction(sql.to_string()))
        }
        "COMMIT" => Some(SessionStatement::Commit),
        "ROLLBACK" if !w.iter().any(|(x, _)| x == "TO") => Some(SessionStatement::Rollback),
        _ => None,
    }
}

/// `NLS_DATE_FORMAT = 'YYYY-MM-DD' TIME_ZONE='UTC'` → pairs (keys upper-case).
fn parse_settings(rest: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut s = rest.trim().trim_end_matches(';');
    while let Some(eq) = s.find('=') {
        let key = s[..eq].trim().trim_matches('"').to_ascii_uppercase();
        let after = s[eq + 1..].trim_start();
        let (value, tail) = if let Some(stripped) = after.strip_prefix('\'') {
            let b = stripped.as_bytes();
            let mut end = 0;
            while end < b.len() {
                if b[end] == b'\'' {
                    if b.get(end + 1) == Some(&b'\'') {
                        end += 2;
                        continue;
                    }
                    break;
                }
                end += 1;
            }
            (
                stripped[..end.min(stripped.len())].replace("''", "'"),
                &stripped[(end + 1).min(stripped.len())..],
            )
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        if !key.is_empty() {
            out.push((key, value));
        }
        s = tail.trim_start();
    }
    out
}

#[async_trait::async_trait]
impl OracleBackend for Arc<MariaDbBackend> {
    async fn open_cursor(
        &self,
        sql: &str,
        binds: &[BindValue],
        caps: DescribeCaps,
    ) -> Result<Box<dyn OracleCursor>> {
        let params = params_of(binds)?;
        let sql = sql.to_string();
        let (id, columns) = self
            .call(|reply| Cmd::Query {
                sql,
                params,
                caps,
                reply,
            })
            .await?;
        Ok(Box::new(MariaDbCursor {
            tx: self.tx.clone(),
            id,
            columns,
            exhausted: false,
            closed: false,
        }))
    }

    async fn execute_simple(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        let params = params_of(binds)?;
        let sql = sql.to_string();
        self.call(|reply| Cmd::Exec { sql, params, reply }).await
    }

    async fn execute_ddl(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        // The session decides commit semantics from the statement itself.
        self.execute_simple(sql, binds).await
    }

    async fn execute_returning(
        &self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<(u64, Vec<Vec<Option<Vec<u8>>>>)> {
        let params = params_of(binds)?;
        let sql = sql.to_string();
        let (n, rows) = self
            .call(|reply| Cmd::Returning { sql, params, reply })
            .await?;
        // The wire layer wants values per column.
        let ncols = rows.first().map_or(0, Vec::len);
        let mut per_col: Vec<Vec<Option<Vec<u8>>>> = vec![Vec::with_capacity(rows.len()); ncols];
        for row in rows {
            for (i, v) in row.into_iter().enumerate() {
                if let Some(col) = per_col.get_mut(i) {
                    col.push(v);
                }
            }
        }
        Ok((n, per_col))
    }

    async fn execute_batch(&self, sql: &str, rows: &[Vec<BindValue>]) -> Result<u64> {
        let rows = rows
            .iter()
            .map(|r| params_of(r))
            .collect::<Result<Vec<_>>>()?;
        let sql = sql.to_string();
        self.call(|reply| Cmd::Batch { sql, rows, reply }).await
    }

    /// Oracle BREAK: kill the running statement from a side connection
    /// (MariaDB error 1317, which maps to ORA-01013).
    async fn cancel(&self) {
        if !self.shared.in_flight.load(Ordering::Relaxed) {
            return;
        }
        let id = self.shared.conn_id.load(Ordering::Relaxed);
        if id == 0 {
            return;
        }
        match Conn::new(self.opts.clone()).await {
            Ok(mut side) => {
                if self.shared.in_flight.load(Ordering::Relaxed) {
                    let _ = side.query_drop(format!("KILL QUERY {id}")).await;
                }
                let _ = side.disconnect().await;
            }
            Err(e) => tracing::warn!("cancel: cannot open a side connection: {e}"),
        }
    }

    async fn commit(&self) -> Result<()> {
        self.call(|reply| Cmd::Commit { reply }).await
    }

    async fn rollback(&self) -> Result<()> {
        self.call(|reply| Cmd::Rollback { reply }).await
    }

    fn cursors_survive_commit(&self) -> bool {
        true
    }

    fn session_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Relaxed)
    }

    async fn session_statement(&self, oracle_sql: &str) -> Option<Result<u64>> {
        let stmt = classify_session_statement(oracle_sql)?;
        Some(
            match stmt {
                SessionStatement::Alter(settings) => {
                    self.call(|reply| Cmd::AlterSession { settings, reply })
                        .await
                }
                SessionStatement::SetTransaction(sql) => {
                    self.call(|reply| Cmd::SetTransaction { sql, reply }).await
                }
                SessionStatement::Commit => self.call(|reply| Cmd::Commit { reply }).await,
                SessionStatement::Rollback => self.call(|reply| Cmd::Rollback { reply }).await,
            }
            .map(|()| 0),
        )
    }

    fn translate(&self, oracle_sql: &str, _binds: &[BindValue]) -> Result<String> {
        if let Some(hit) = self
            .translations
            .lock()
            .ok()
            .and_then(|c| c.get(oracle_sql).cloned())
        {
            return Ok(hit);
        }
        let out = crate::translate::oracle_to_mariadb_with_case(oracle_sql, self.case)?;
        let out = route_dictionary(&out);
        if let Ok(mut c) = self.translations.lock() {
            if c.len() >= TRANSLATION_CACHE {
                c.clear();
            }
            c.insert(oracle_sql.to_string(), out.clone());
        }
        Ok(out)
    }
}

/// Route Oracle dictionary relations, built-in functions and package
/// routines to the objects in `dbsaci_sys` (MariaDB has no public synonyms or
/// search path): `ALL_TABLES` / `SYS.ALL_TABLES` / `PUBLIC.ALL_TABLES` →
/// `` `dbsaci_sys`.`all_tables` ``, `MONTHS_BETWEEN(` →
/// `` `dbsaci_sys`.`months_between`( ``, `DBMS_OUTPUT.PUT_LINE(` →
/// `` `dbsaci_sys`.`dbms_output__put_line`( ``. Lexical: literals, comments
/// and quoted identifiers are untouched.
pub fn route_dictionary(sql: &str) -> String {
    let ps = pieces(sql);
    let text = |i: usize| -> &str { ps.get(i).map_or("", |(_, r)| &sql[r.clone()]) };
    let is_word = |i: usize| ps.get(i).is_some_and(|(p, _)| *p == Piece::Word);
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0;
    while i < ps.len() {
        let t = text(i);
        if is_word(i) {
            let next = text(i + 1);
            let call = next.trim_start().starts_with('(');
            let dotted_after = next == ".";
            let qualified = i >= 2 && text(i - 1) == "." && is_word(i - 2);
            // PACKAGE.MEMBER(
            if dotted_after
                && is_word(i + 2)
                && text(i + 3).trim_start().starts_with('(')
                && !qualified
                && let Some(routine) = dictionary::package_routine(t, text(i + 2))
            {
                out.push_str(&format!("`{}`.`{routine}`", dictionary::SCHEMA));
                i += 3;
                continue;
            }
            // SYS. / PUBLIC. qualifier in front of a dictionary relation.
            if dotted_after
                && (t.eq_ignore_ascii_case("SYS") || t.eq_ignore_ascii_case("PUBLIC"))
                && is_word(i + 2)
                && dictionary::is_relation(text(i + 2))
                && !text(i + 3).trim_start().starts_with('(')
                && !qualified
            {
                out.push_str(&format!(
                    "`{}`.`{}`",
                    dictionary::SCHEMA,
                    text(i + 2).to_ascii_lowercase()
                ));
                i += 3;
                continue;
            }
            if !qualified && !dotted_after && !call && dictionary::is_relation(t) {
                out.push_str(&format!(
                    "`{}`.`{}`",
                    dictionary::SCHEMA,
                    t.to_ascii_lowercase()
                ));
                i += 1;
                continue;
            }
            if !qualified && call && dictionary::is_function(t) {
                out.push_str(&format!(
                    "`{}`.`{}`",
                    dictionary::SCHEMA,
                    t.to_ascii_lowercase()
                ));
                i += 1;
                continue;
            }
        }
        out.push_str(t);
        i += 1;
    }
    out
}

/// `dbsaci install`: create or upgrade `dbsaci_sys` and grant it to everyone.
/// With `drop_legacy`, also remove the per-schema objects DbSaci 0.2.x created.
pub async fn install_dictionary(
    cfg: &MariaDbConfig,
    user: &str,
    password: &str,
    drop_legacy: bool,
) -> Result<Vec<String>> {
    let mut builder = OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .user(Some(user.to_string()))
        .pass(Some(password.to_string()))
        .prefer_socket(false);
    if let Some(tls) = cfg.ssl_opts() {
        builder = builder.ssl_opts(tls);
    }
    let mut conn = Conn::new(Opts::from(builder))
        .await
        .map_err(|e| Error::ora(12541, format!("cannot connect to MariaDB: {e}")))?;
    conn.query_drop(format!("SET SESSION sql_mode = '{}'", session::SQL_MODE))
        .await
        .map_err(|e| errors::map_error(e, "SET").error)?;
    let mut report = Vec::new();
    session::install(&mut conn).await?;
    report.push(format!(
        "installed {} version {}",
        dictionary::SCHEMA,
        install::VERSION
    ));
    if drop_legacy {
        let (views, routines) = install::legacy_object_names();
        let schemas: Vec<String> = conn
            .query(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN \
                 ('mysql','information_schema','performance_schema','sys','dbsaci_sys')",
            )
            .await
            .map_err(|e| errors::map_error(e, "SCHEMATA").error)?;
        for db in schemas {
            let q = |n: &str| format!("`{}`.`{}`", db.replace('`', "``"), n.replace('`', "``"));
            for v in &views {
                // Only DbSaci's own views (defined over information_schema or empty stubs).
                let def: Option<String> = conn
                    .exec_first(
                        "SELECT VIEW_DEFINITION FROM information_schema.VIEWS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                        (&db, v),
                    )
                    .await
                    .ok()
                    .flatten();
                let ours = def.is_some_and(|d| {
                    let d = d.to_ascii_lowercase();
                    d.contains("information_schema") || d.contains("where 1 = 0")
                });
                if ours && conn.query_drop(format!("DROP VIEW {}", q(v))).await.is_ok() {
                    report.push(format!("dropped view {db}.{v}"));
                }
            }
            for r in &routines {
                for kind in ["FUNCTION", "PROCEDURE"] {
                    let _ = conn
                        .query_drop(format!("DROP {kind} IF EXISTS {}", q(r)))
                        .await;
                }
            }
        }
        for db in ["dbms_output", "dbms_metadata"] {
            let n: Option<i64> = conn
                .exec_first(
                    "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
                    (db,),
                )
                .await
                .ok()
                .flatten();
            if n == Some(0)
                && conn
                    .query_drop(format!("DROP DATABASE IF EXISTS `{db}`"))
                    .await
                    .is_ok()
            {
                report.push(format!("dropped legacy package schema {db}"));
            }
        }
    }
    let _ = conn.disconnect().await;
    Ok(report)
}

/// The oldest MariaDB release DbSaci supports.
pub const MIN_VERSION: (u32, u32) = (11, 8);

/// One line of a `dbsaci check` report.
#[derive(Debug, Clone)]
pub struct CheckLine {
    pub ok: bool,
    pub what: String,
}

/// `dbsaci check`: can `login` use this server through DbSaci? Reports the
/// server version, case sensitivity, the login's schema and the data
/// dictionary. Never changes anything.
pub async fn check(cfg: &MariaDbConfig, login: &str, password: &str) -> Vec<CheckLine> {
    let mut out = Vec::new();
    let mut line = |ok: bool, what: String| out.push(CheckLine { ok, what });
    let mut builder = OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .user(Some(login.to_ascii_lowercase()))
        .pass(Some(password.to_string()))
        .prefer_socket(false);
    if let Some(tls) = cfg.ssl_opts() {
        builder = builder.ssl_opts(tls);
    }
    let mut conn = match Conn::new(Opts::from(builder)).await {
        Ok(c) => c,
        Err(e) => {
            line(
                false,
                format!(
                    "connect to {}:{} as '{}': {e}",
                    cfg.host,
                    cfg.port,
                    login.to_ascii_lowercase()
                ),
            );
            return out;
        }
    };
    line(
        true,
        format!(
            "connected to {}:{} as '{}'",
            cfg.host,
            cfg.port,
            login.to_ascii_lowercase()
        ),
    );
    let version: String = conn
        .query_first("SELECT VERSION()")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let is_mariadb = version.to_ascii_lowercase().contains("mariadb");
    let new_enough = (major, minor) >= MIN_VERSION;
    line(
        is_mariadb && new_enough,
        format!(
            "server version {version} (DbSaci needs MariaDB {}.{} or newer)",
            MIN_VERSION.0, MIN_VERSION.1
        ),
    );
    let lctn: u8 = conn
        .query_first("SELECT @@lower_case_table_names")
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    let effective = if lctn != 0 {
        IdentifierCase::Lower
    } else {
        cfg.case
    };
    line(
        true,
        match lctn {
            0 => format!(
                "lower_case_table_names = 0: table names are case-sensitive; DbSaci stores Oracle names {} (--identifier-case {})",
                if cfg.case == IdentifierCase::Upper {
                    "as written in Oracle (EMP)"
                } else {
                    "in lower case (emp)"
                },
                if cfg.case == IdentifierCase::Upper {
                    "upper"
                } else {
                    "lower"
                }
            ),
            n => format!(
                "lower_case_table_names = {n}: the server stores names in lower case; quoted mixed-case Oracle names (\"MyTable\") lose their case"
            ),
        },
    );
    let login_oracle = login.to_ascii_uppercase();
    let schema = effective.physical(&login_oracle);
    let exists: Option<String> = conn
        .exec_first(
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            (&schema,),
        )
        .await
        .ok()
        .flatten();
    let default_exists: Option<String> = if cfg.default_db.is_empty() {
        None
    } else {
        conn.exec_first(
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            (&cfg.default_db,),
        )
        .await
        .ok()
        .flatten()
    };
    match (exists, default_exists) {
        (Some(_), _) => line(
            true,
            format!("schema `{schema}` (Oracle user {login_oracle}) exists"),
        ),
        (None, Some(d)) => line(
            true,
            format!("no schema `{schema}`; sessions of {login_oracle} start in the default `{d}`"),
        ),
        (None, None) => {
            let near: Vec<String> = conn
                .exec(
                    "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE LOWER(SCHEMA_NAME) = LOWER(?)",
                    (&schema,),
                )
                .await
                .unwrap_or_default();
            line(
                false,
                if near.is_empty() {
                    format!(
                        "no schema `{schema}` for Oracle user {login_oracle} and no usable --db-name; create it: CREATE DATABASE `{schema}`"
                    )
                } else {
                    format!(
                        "no schema `{schema}` for Oracle user {login_oracle}, but `{}` exists: rename it, or run with --identifier-case {}",
                        near.join("`, `"),
                        if cfg.case == IdentifierCase::Upper {
                            "lower"
                        } else {
                            "upper"
                        }
                    )
                },
            )
        }
    }
    let installed: Option<String> = conn
        .query_first("SELECT version FROM dbsaci_sys.install_info")
        .await
        .ok()
        .flatten();
    match installed {
        Some(v) if v == install::VERSION => line(
            true,
            format!("data dictionary `{}` is current ({v})", dictionary::SCHEMA),
        ),
        Some(v) => line(
            false,
            format!(
                "data dictionary `{}` is version {v}, this DbSaci needs {}: run `dbsaci install`",
                dictionary::SCHEMA,
                install::VERSION
            ),
        ),
        None => line(
            false,
            format!(
                "data dictionary `{}` is not installed (or not readable): run `dbsaci install` with an administrative account",
                dictionary::SCHEMA
            ),
        ),
    }
    let _ = conn.disconnect().await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_dictionary_names() {
        assert_eq!(
            route_dictionary("SELECT SEQUENCE_NAME FROM ALL_SEQUENCES WHERE X = 'ALL_TABLES'"),
            "SELECT SEQUENCE_NAME FROM `dbsaci_sys`.`all_sequences` WHERE X = 'ALL_TABLES'"
        );
        assert_eq!(
            route_dictionary("select * from sys.all_tables t, user_tables"),
            "select * from `dbsaci_sys`.`all_tables` t, `dbsaci_sys`.`user_tables`"
        );
        assert_eq!(
            route_dictionary("SELECT MONTHS_BETWEEN(A, B), T.ALL_TABLES FROM T"),
            "SELECT `dbsaci_sys`.`months_between`(A, B), T.ALL_TABLES FROM T"
        );
        assert_eq!(
            route_dictionary("BEGIN DBMS_OUTPUT.PUT_LINE('x'); END"),
            "BEGIN `dbsaci_sys`.`dbms_output__put_line`('x'); END"
        );
        assert_eq!(
            route_dictionary("SELECT * FROM V$VERSION"),
            "SELECT * FROM `dbsaci_sys`.`v$version`"
        );
    }

    #[test]
    fn alter_session_settings_parse() {
        let Some(SessionStatement::Alter(s)) = classify_session_statement(
            "alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS' time_zone='-03:00' current_schema=hr",
        ) else {
            panic!()
        };
        assert_eq!(
            s,
            vec![
                (
                    "NLS_DATE_FORMAT".to_string(),
                    "YYYY-MM-DD HH24:MI:SS".to_string()
                ),
                ("TIME_ZONE".to_string(), "-03:00".to_string()),
                ("CURRENT_SCHEMA".to_string(), "hr".to_string()),
            ]
        );
    }

    #[test]
    fn session_statements_are_recognised() {
        assert!(matches!(
            classify_session_statement("COMMIT WORK"),
            Some(SessionStatement::Commit)
        ));
        assert!(matches!(
            classify_session_statement("rollback"),
            Some(SessionStatement::Rollback)
        ));
        assert!(classify_session_statement("ROLLBACK TO SAVEPOINT a").is_none());
        assert!(matches!(
            classify_session_statement("SET TRANSACTION READ ONLY"),
            Some(SessionStatement::SetTransaction(_))
        ));
        assert!(classify_session_statement("SELECT 1 FROM DUAL").is_none());
    }
}
