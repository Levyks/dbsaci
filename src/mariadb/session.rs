//! One Oracle session's MariaDB connection, driven by a dedicated task.
//!
//! The task owns the connection, so a query's rows can be *streamed*: a cursor
//! keeps its MariaDB result open and each Fetch reads the next rows from the
//! socket. MariaDB allows one active result per connection, so when any other
//! request arrives while a cursor is mid-stream, the task first reads the rest
//! of that cursor into memory ("spills" it) and then serves the request —
//! Oracle's freedom to interleave cursors is kept, and memory only grows for
//! the cursors that are actually interleaved.
//!
//! Transactions follow Oracle's model: `autocommit = 0` and `READ COMMITTED`,
//! so a transaction begins implicitly with the first statement, every
//! statement sees the data committed before it started, and DDL commits. No
//! `START TRANSACTION` is ever issued. A session that only read keeps a read
//! view (and metadata locks) open; after [`IDLE_RELEASE`] without requests the
//! task commits it so idle pooled sessions don't block DDL.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Column, Conn, Opts, OptsBuilder, Params, QueryResult, Value};
use tokio::sync::{mpsc, oneshot};

use super::binds::{self, BatchPart};
use super::errors::{self, Mapped};
use super::instead_of::{self, InsteadOfTrigger};
use super::types::{self, Wire};
use crate::backend::DescribeCaps;
use crate::error::{Error, Result};
use crate::translate::IdentifierCase;
use crate::wire::ColumnMeta;

/// An encoded row: one optional Oracle value per column.
pub type Row = Vec<Option<Vec<u8>>>;

/// How long a read-only transaction may sit idle before it is committed to
/// release its read view and metadata locks.
const IDLE_RELEASE: Duration = Duration::from_millis(500);

/// Session `sql_mode`. `ORACLE` supplies the dialect; the extra flags give
/// Oracle's error behaviour (raise on overflow/truncation, `x/0`, a
/// non-aggregated column outside GROUP BY), `''` as NULL, literal backslashes,
/// and blank-padded `CHAR` values.
pub const SQL_MODE: &str = "ORACLE,STRICT_ALL_TABLES,ERROR_FOR_DIVISION_BY_ZERO,\
ONLY_FULL_GROUP_BY,EMPTY_STRING_IS_NULL,NO_BACKSLASH_ESCAPES,PAD_CHAR_TO_FULL_LENGTH";

/// State shared between the session task and its handle.
#[derive(Default)]
pub struct Shared {
    /// MariaDB connection id (for `KILL QUERY`).
    pub conn_id: AtomicU32,
    /// A statement is running on the connection.
    pub in_flight: AtomicBool,
    /// The connection was lost with uncommitted work: the session is over.
    pub lost: AtomicBool,
    /// Bumped by DDL; part of translation cache keys.
    pub schema_version: AtomicU64,
    /// Bumped per backend call; a call timeout only fires for its own call.
    pub calls: AtomicU64,
}

/// Fixed parameters of the session.
#[derive(Clone)]
pub struct SessionParams {
    /// Connection options (credentials, TLS) without a default database.
    pub opts: Opts,
    /// Physical database the session starts in (Oracle's schema == user).
    pub schema: String,
    /// The login as an Oracle name (USER_* views, `SYS_CONTEXT`).
    pub login: String,
    pub case: IdentifierCase,
    pub statement_timeout: Option<Duration>,
}

/// A request to the session task.
pub enum Cmd {
    Query {
        sql: String,
        params: Vec<Value>,
        caps: DescribeCaps,
        reply: oneshot::Sender<Result<(u64, Vec<ColumnMeta>)>>,
    },
    Fetch {
        cursor: u64,
        n: usize,
        reply: oneshot::Sender<Result<(Vec<Row>, bool)>>,
    },
    Close {
        cursor: u64,
    },
    Exec {
        sql: String,
        params: Vec<Value>,
        reply: oneshot::Sender<Result<u64>>,
    },
    Returning {
        sql: String,
        params: Vec<Value>,
        reply: oneshot::Sender<Result<(u64, Vec<Row>)>>,
    },
    Batch {
        sql: String,
        rows: Vec<Vec<Value>>,
        reply: oneshot::Sender<Result<u64>>,
    },
    Commit {
        reply: oneshot::Sender<Result<()>>,
    },
    Rollback {
        reply: oneshot::Sender<Result<()>>,
    },
    /// `ALTER SESSION SET k = v …` (already parsed, keys upper-case).
    AlterSession {
        settings: Vec<(String, String)>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// `SET TRANSACTION …` (Oracle text).
    SetTransaction {
        sql: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Column kinds of a table, for type-aware translation.
    Columns {
        schema: Option<String>,
        table: String,
        reply: oneshot::Sender<Result<Vec<(String, String, u32)>>>,
    },
}

/// Per-session mutable state that survives a reconnect.
struct State {
    params: SessionParams,
    shared: Arc<Shared>,
    /// Uncommitted writes or row locks in the current transaction.
    dirty: bool,
    /// A read-only transaction may be holding a read view / MDL.
    read_open: bool,
    /// `SET TRANSACTION` is in force until the next commit/rollback.
    special_tx: bool,
    /// Physical name of the current schema (`ALTER SESSION SET CURRENT_SCHEMA`).
    current_schema: String,
    time_zone: Option<String>,
    nls: Vec<(String, String)>,
    /// Sequences on which this session has called NEXTVAL (CURRVAL is legal).
    nextval_seen: HashSet<String>,
    instead_of: HashMap<String, InsteadOfTrigger>,
    spilled: HashMap<u64, VecDeque<Row>>,
    /// A spilled cursor whose remaining rows could not all be read: the
    /// error is reported after its buffered rows.
    spill_errors: HashMap<u64, String>,
    next_cursor: u64,
}

/// Start a session: connect, then serve requests on the returned channel.
pub async fn start(params: SessionParams, shared: Arc<Shared>) -> Result<mpsc::Sender<Cmd>> {
    let mut state = State {
        current_schema: params.schema.clone(),
        params,
        shared,
        dirty: false,
        read_open: false,
        special_tx: false,
        time_zone: None,
        nls: Vec::new(),
        nextval_seen: HashSet::new(),
        instead_of: HashMap::new(),
        spilled: HashMap::new(),
        spill_errors: HashMap::new(),
        next_cursor: 1,
    };
    let conn = establish(&mut state).await?;
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(run(conn, state, rx));
    Ok(tx)
}

/// Open a connection and apply the session's settings in one round trip.
async fn establish(st: &mut State) -> Result<Conn> {
    let opts =
        OptsBuilder::from_opts(st.params.opts.clone()).db_name(Some(st.current_schema.clone()));
    let mut conn = Conn::new(opts)
        .await
        .map_err(|e| connect_error(e, &st.current_schema))?;
    st.shared.conn_id.store(conn.id(), Ordering::Relaxed);
    let mut set = format!(
        "SET SESSION sql_mode = '{SQL_MODE}', autocommit = 0, \
         transaction_isolation = 'READ-COMMITTED', \
         collation_connection = @@collation_database, max_recursive_iterations = 1100000, \
         @dbsaci_login_user = {login}, @dbsaci_lower_names = {lower}",
        login = quote(&st.params.login),
        lower = u8::from(st.params.case == IdentifierCase::Lower),
    );
    if let Some(tz) = &st.time_zone {
        set.push_str(&format!(", time_zone = {}", quote(tz)));
    }
    for (k, v) in &st.nls {
        set.push_str(&format!(
            ", @dbsaci_{} = {}",
            k.to_ascii_lowercase(),
            quote(v)
        ));
    }
    conn.query_drop(set)
        .await
        .map_err(|e| errors::map_error(e, "session setup").error)?;
    Ok(conn)
}

fn connect_error(e: mysql_async::Error, schema: &str) -> Error {
    match &e {
        mysql_async::Error::Server(s) if s.code == 1045 => {
            Error::ora(1017, "invalid username/password; logon denied")
        }
        mysql_async::Error::Server(s) if s.code == 1049 => {
            Error::ora(1918, format!("user (schema) '{schema}' does not exist"))
        }
        mysql_async::Error::Server(s) if s.code == 1040 => {
            Error::ora(18, "maximum number of sessions exceeded")
        }
        _ => Error::ora(12541, format!("cannot connect to MariaDB: {e}")),
    }
}

/// `'text'` with Oracle/MariaDB (NO_BACKSLASH_ESCAPES) quoting.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The session task.
async fn run(mut conn: Conn, mut st: State, mut rx: mpsc::Receiver<Cmd>) {
    let mut pending: Option<Cmd> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None if st.read_open && !st.dirty && !st.special_tx => {
                match tokio::time::timeout(IDLE_RELEASE, rx.recv()).await {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(_) => {
                        // Idle read-only transaction: release its read view.
                        if conn.query_drop("COMMIT").await.is_ok() {
                            st.read_open = false;
                        }
                        continue;
                    }
                }
            }
            None => match rx.recv().await {
                Some(c) => c,
                None => break,
            },
        };
        if st.shared.lost.load(Ordering::Relaxed) {
            fail(cmd, || Error::SessionLost("MariaDB connection lost".into()));
            continue;
        }
        pending = handle(&mut conn, &mut st, cmd, &mut rx).await;
        if st.shared.lost.load(Ordering::Relaxed) {
            continue;
        }
    }
    // Client gone. Without a LOGOFF (which commits first) Oracle rolls back;
    // closing the connection does exactly that.
    let _ = conn.disconnect().await;
}

/// A per-call timeout (Oracle's call timeout applies to each execute or fetch,
/// not to a cursor's lifetime — MariaDB's `max_statement_time` would kill a
/// streaming query whose client reads slowly). When it expires, the running
/// statement is killed from a side connection; MariaDB then fails it with
/// error 1317 (ORA-01013). Dropping the guard disarms it.
struct Deadline(Option<tokio::task::JoinHandle<()>>);

impl Deadline {
    fn arm(st: &State) -> Self {
        let Some(after) = st.params.statement_timeout else {
            return Deadline(None);
        };
        let opts = st.params.opts.clone();
        let shared = st.shared.clone();
        let generation = shared.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Deadline(Some(tokio::spawn(async move {
            tokio::time::sleep(after).await;
            if shared.calls.load(Ordering::Relaxed) != generation {
                return;
            }
            let id = shared.conn_id.load(Ordering::Relaxed);
            if let Ok(mut side) = Conn::new(opts).await {
                if shared.calls.load(Ordering::Relaxed) == generation {
                    let _ = side.query_drop(format!("KILL QUERY {id}")).await;
                }
                let _ = side.disconnect().await;
            }
        })))
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Answer `cmd` with an error.
fn fail(cmd: Cmd, err: impl Fn() -> Error) {
    match cmd {
        Cmd::Query { reply, .. } => drop(reply.send(Err(err()))),
        Cmd::Fetch { reply, .. } => drop(reply.send(Err(err()))),
        Cmd::Exec { reply, .. } | Cmd::Batch { reply, .. } => drop(reply.send(Err(err()))),
        Cmd::Returning { reply, .. } => drop(reply.send(Err(err()))),
        Cmd::Commit { reply } | Cmd::Rollback { reply } => drop(reply.send(Err(err()))),
        Cmd::AlterSession { reply, .. } | Cmd::SetTransaction { reply, .. } => {
            drop(reply.send(Err(err())))
        }
        Cmd::Columns { reply, .. } => drop(reply.send(Err(err()))),
        Cmd::Close { .. } => {}
    }
}

/// Serve one request. Returns a request that arrived while a cursor was being
/// streamed and must be served next.
async fn handle(
    conn: &mut Conn,
    st: &mut State,
    cmd: Cmd,
    rx: &mut mpsc::Receiver<Cmd>,
) -> Option<Cmd> {
    match cmd {
        Cmd::Query {
            sql,
            params,
            caps,
            reply,
        } => query(conn, st, sql, params, caps, reply, rx).await,
        Cmd::Fetch { cursor, n, reply } => {
            let _ = reply.send(fetch_spilled(st, cursor, n));
            None
        }
        Cmd::Close { cursor } => {
            st.spilled.remove(&cursor);
            st.spill_errors.remove(&cursor);
            None
        }
        Cmd::Exec { sql, params, reply } => {
            st.shared.in_flight.store(true, Ordering::Relaxed);
            let deadline = Deadline::arm(st);
            let mut r = exec(conn, st, &sql, &params).await;
            drop(deadline);
            if is_link_lost(&r) {
                r = match recover(conn, st).await {
                    Ok(()) => exec(conn, st, &sql, &params).await,
                    Err(e) => Err(e),
                };
            }
            st.shared.in_flight.store(false, Ordering::Relaxed);
            let _ = reply.send(r);
            None
        }
        Cmd::Batch { sql, rows, reply } => {
            st.shared.in_flight.store(true, Ordering::Relaxed);
            let deadline = Deadline::arm(st);
            let r = batch(conn, st, &sql, &rows).await;
            drop(deadline);
            st.shared.in_flight.store(false, Ordering::Relaxed);
            let _ = reply.send(r);
            None
        }
        Cmd::Returning { sql, params, reply } => {
            let r = returning(conn, st, &sql, &params).await;
            let _ = reply.send(r);
            None
        }
        Cmd::Commit { reply } => {
            let r = end_transaction(conn, st, "COMMIT").await;
            let _ = reply.send(r);
            None
        }
        Cmd::Rollback { reply } => {
            let r = end_transaction(conn, st, "ROLLBACK").await;
            let _ = reply.send(r);
            None
        }
        Cmd::AlterSession { settings, reply } => {
            let r = alter_session(conn, st, settings).await;
            let _ = reply.send(r);
            None
        }
        Cmd::SetTransaction { sql, reply } => {
            let r = set_transaction(conn, st, &sql).await;
            let _ = reply.send(r);
            None
        }
        Cmd::Columns {
            schema,
            table,
            reply,
        } => {
            let r = columns(conn, schema.as_deref(), &table).await;
            let _ = reply.send(r);
            None
        }
    }
}

fn is_link_lost<T>(r: &Result<T>) -> bool {
    matches!(r, Err(Error::Postgres(m)) if m.starts_with("connection lost"))
}

/// After a lost link: with no uncommitted writes, reconnect (restoring the
/// session state) so the statement can run once more. With uncommitted
/// writes the session is over — Oracle would have ended it (ORA-03113), and
/// continuing on a new connection would commit only part of a transaction.
async fn recover(conn: &mut Conn, st: &mut State) -> Result<()> {
    if st.dirty {
        st.shared.lost.store(true, Ordering::Relaxed);
        return Err(Error::SessionLost(
            "MariaDB connection lost with uncommitted changes".into(),
        ));
    }
    tracing::warn!("MariaDB connection lost; reconnecting");
    match establish(st).await {
        Ok(fresh) => {
            *conn = fresh;
            st.read_open = false;
            Ok(())
        }
        Err(e) => {
            st.shared.lost.store(true, Ordering::Relaxed);
            Err(Error::SessionLost(e.to_string()))
        }
    }
}

/// Map a driver error, recording a backend-side rollback (deadlock).
fn mapped(st: &mut State, e: mysql_async::Error, sql: &str) -> Error {
    let Mapped {
        error,
        transaction_rolled_back,
    } = errors::map_error(e, sql);
    if transaction_rolled_back {
        st.dirty = false;
        st.read_open = false;
    }
    error
}

/// First keyword (upper-case) of a statement, skipping comments.
fn verb(sql: &str) -> String {
    crate::translate::names::pieces(sql)
        .into_iter()
        .find(|(p, _)| *p == crate::translate::names::Piece::Word)
        .map(|(_, r)| sql[r].to_ascii_uppercase())
        .unwrap_or_default()
}

/// Does this statement commit implicitly in MariaDB (DDL)?
fn commits_implicitly(sql: &str) -> bool {
    let v = verb(sql);
    let words: Vec<String> = crate::translate::names::pieces(sql)
        .into_iter()
        .filter(|(p, _)| *p == crate::translate::names::Piece::Word)
        .take(3)
        .map(|(_, r)| sql[r].to_ascii_uppercase())
        .collect();
    let temporary = words.iter().any(|w| w == "TEMPORARY");
    matches!(
        v.as_str(),
        "CREATE"
            | "ALTER"
            | "DROP"
            | "TRUNCATE"
            | "RENAME"
            | "GRANT"
            | "REVOKE"
            | "ANALYZE"
            | "OPTIMIZE"
            | "LOCK"
            | "UNLOCK"
    ) && !temporary
}

fn is_program(sql: &str) -> bool {
    let words: Vec<String> = crate::translate::names::pieces(sql)
        .into_iter()
        .filter(|(p, _)| *p == crate::translate::names::Piece::Word)
        .take(6)
        .map(|(_, r)| sql[r].to_ascii_uppercase())
        .collect();
    match words.first().map(String::as_str) {
        Some("BEGIN" | "DECLARE") => true,
        Some("CREATE") => words.iter().any(|w| {
            matches!(
                w.as_str(),
                "TRIGGER" | "FUNCTION" | "PROCEDURE" | "PACKAGE" | "EVENT" | "TYPE"
            )
        }),
        _ => false,
    }
}

/// After a successful statement, update the transaction bookkeeping.
fn after_statement(st: &mut State, sql: &str, wrote: bool) {
    if commits_implicitly(sql) {
        st.dirty = false;
        st.read_open = false;
        st.special_tx = false;
        st.shared.schema_version.fetch_add(1, Ordering::Relaxed);
        super::schema_cache::invalidate();
    } else if wrote {
        st.dirty = true;
    } else {
        st.read_open = true;
    }
}

/// Oracle: CURRVAL before NEXTVAL in this session is ORA-08002 (MariaDB
/// answers with the last value instead). Records NEXTVAL uses on success.
fn check_currval(st: &State, sql: &str) -> Result<Vec<String>> {
    let ps = crate::translate::names::pieces(sql);
    let mut nextvals = Vec::new();
    for w in ps.windows(3) {
        let (a, dot, b) = (&w[0], &w[1], &w[2]);
        if a.0 == crate::translate::names::Piece::Opaque
            || &sql[dot.1.clone()] != "."
            || b.0 != crate::translate::names::Piece::Word
        {
            continue;
        }
        let seq = sql[a.1.clone()].trim_matches('"').to_ascii_uppercase();
        let what = sql[b.1.clone()].to_ascii_uppercase();
        if what == "NEXTVAL" {
            nextvals.push(seq);
        } else if what == "CURRVAL" && !st.nextval_seen.contains(&seq) && !nextvals.contains(&seq) {
            return Err(Error::ora(
                8002,
                format!("sequence {seq}.CURRVAL is not yet defined in this session"),
            ));
        }
    }
    Ok(nextvals)
}

/// Run a non-row statement (DML, DDL, PL/SQL, a translator batch).
async fn exec(conn: &mut Conn, st: &mut State, sql: &str, params: &[Value]) -> Result<u64> {
    let nextvals = check_currval(st, sql)?;
    // INSTEAD OF triggers: a definition is kept for the session; DML on the
    // view runs the trigger body instead.
    if let Some((view, trigger)) = instead_of::parse(sql) {
        st.instead_of.insert(view, trigger);
        return Ok(0);
    }
    if let Some(body) = instead_of::expand(sql, params, &st.instead_of)? {
        let n = run_text(conn, &body)
            .await
            .map_err(|e| mapped(st, e, &body))?;
        st.dirty = true;
        return Ok(n);
    }
    // `UPDATE … RETURNING` without INTO (row-count path): MariaDB has no
    // UPDATE RETURNING, and the client only wants the count.
    let stripped;
    let sql = match split_update_returning(sql) {
        Some((update, _)) => {
            stripped = update;
            stripped.as_str()
        }
        None => sql,
    };
    let sep = crate::translate::MARIADB_BATCH_SEP.trim();
    let affected = if sql.contains(sep) {
        let parts = binds::split_batch(sql, sep, params.to_vec())?;
        run_batch(conn, st, parts).await?
    } else if params.is_empty() || is_program(sql) {
        run_text(conn, sql).await.map_err(|e| mapped(st, e, sql))?
    } else {
        run_prepared(conn, sql, params)
            .await
            .map_err(|e| mapped(st, e, sql))?
    };
    st.nextval_seen.extend(nextvals);
    let wrote = !matches!(verb(sql).as_str(), "SELECT" | "WITH" | "SET" | "DO");
    after_statement(st, sql, wrote);
    Ok(affected)
}

/// Text-protocol execution, fully draining every result set (an anonymous
/// block can return rows; leaving them unread desyncs the connection).
async fn run_text(conn: &mut Conn, sql: &str) -> mysql_async::Result<u64> {
    let mut result = conn.query_iter(sql).await?;
    let mut affected = result.affected_rows();
    let mut rows = 0u64;
    while result.next().await?.is_some() {
        rows += 1;
    }
    if affected == 0 {
        affected = rows;
    }
    result.drop_result().await?;
    Ok(affected)
}

/// Prepared-statement execution. `INSERT/DELETE … RETURNING` reports its
/// returned rows (MariaDB's affected-rows is -1 then).
async fn run_prepared(conn: &mut Conn, sql: &str, params: &[Value]) -> mysql_async::Result<u64> {
    let mut result = conn
        .exec_iter(sql, Params::Positional(params.to_vec()))
        .await?;
    let mut affected = result.affected_rows();
    let mut rows = 0u64;
    while result.next().await?.is_some() {
        rows += 1;
    }
    if rows > 0 {
        affected = rows;
    }
    result.drop_result().await?;
    Ok(affected)
}

/// A translator batch, atomically: a savepoint, every part, and a rollback
/// to the savepoint if any part fails (Oracle's statement-level atomicity).
async fn run_batch(conn: &mut Conn, st: &mut State, parts: Vec<BatchPart>) -> Result<u64> {
    conn.query_drop("SAVEPOINT dbsaci_batch")
        .await
        .map_err(|e| mapped(st, e, "SAVEPOINT"))?;
    let mut total = 0u64;
    for part in &parts {
        let r = if part.params.is_empty() {
            run_text(conn, &part.sql).await
        } else {
            run_prepared(conn, &part.sql, &part.params).await
        };
        match r {
            Ok(n) if !part.nocount => total += n,
            Ok(_) => {}
            Err(e) => {
                let err = mapped(st, e, &part.sql);
                let _ = conn.query_drop("ROLLBACK TO SAVEPOINT dbsaci_batch").await;
                let _ = conn.query_drop("RELEASE SAVEPOINT dbsaci_batch").await;
                return Err(err);
            }
        }
    }
    let _ = conn.query_drop("RELEASE SAVEPOINT dbsaci_batch").await;
    Ok(total)
}

/// Most rows per multi-row INSERT (and at most ~60k placeholders).
const BATCH_ROWS: usize = 1000;

/// Array DML. A plain `INSERT … VALUES (?, …)` runs as multi-row INSERTs of up
/// to [`BATCH_ROWS`] rows (one round trip per chunk instead of per row). A
/// chunk that fails was rolled back as a unit by MariaDB; it is then replayed
/// row by row so the rows before the failing one are applied and that row's
/// error is the one reported, as Oracle does.
async fn batch(conn: &mut Conn, st: &mut State, sql: &str, rows: &[Vec<Value>]) -> Result<u64> {
    let width = rows.first().map_or(0, Vec::len);
    let tuple = split_values_tuple(sql)
        .filter(|(_, t)| width > 0 && binds::count_placeholders(t) == width)
        .filter(|_| {
            st.instead_of.is_empty() && !sql.contains(crate::translate::MARIADB_BATCH_SEP.trim())
        });
    let Some((head, tuple)) = tuple else {
        let mut total = 0;
        for row in rows {
            total += exec(conn, st, sql, row).await?;
        }
        return Ok(total);
    };
    let per_chunk = BATCH_ROWS.min((60_000 / width).max(1));
    let mut total = 0u64;
    for chunk in rows.chunks(per_chunk) {
        let multi = format!("{head}{}", vec![tuple.as_str(); chunk.len()].join(", "));
        let params: Vec<Value> = chunk.iter().flatten().cloned().collect();
        match run_prepared(conn, &multi, &params).await {
            Ok(n) => total += n,
            Err(e) if errors::is_link_error(&e) => return Err(mapped(st, e, sql)),
            Err(_) => {
                for row in chunk {
                    total += exec(conn, st, sql, row).await?;
                }
            }
        }
    }
    after_statement(st, sql, true);
    Ok(total)
}

/// `INSERT INTO t (a, b) VALUES (?, ?)` → (`INSERT INTO t (a, b) VALUES `,
/// `(?, ?)`) when the statement is exactly one VALUES tuple at its end.
fn split_values_tuple(sql: &str) -> Option<(String, String)> {
    use crate::translate::names::{Piece, pieces};
    if verb(sql) != "INSERT" {
        return None;
    }
    let ps = pieces(sql);
    let words: Vec<String> = ps
        .iter()
        .filter(|(p, _)| *p == Piece::Word)
        .map(|(_, r)| sql[r.clone()].to_ascii_uppercase())
        .collect();
    if words.iter().filter(|w| *w == "VALUES").count() != 1
        || words
            .iter()
            .any(|w| matches!(w.as_str(), "SELECT" | "RETURNING" | "ALL" | "FIRST"))
    {
        return None;
    }
    let values_end = ps
        .iter()
        .find(|(p, r)| *p == Piece::Word && sql[r.clone()].eq_ignore_ascii_case("VALUES"))?
        .1
        .end;
    let tail = sql[values_end..].trim().trim_end_matches(';').trim_end();
    if !tail.starts_with('(') || !tail.ends_with(')') {
        return None;
    }
    // One balanced group spanning the whole tail.
    let mut depth = 0i32;
    let mut in_lit = false;
    for (i, ch) in tail.char_indices() {
        match ch {
            '\'' => in_lit = !in_lit,
            '(' if !in_lit => depth += 1,
            ')' if !in_lit => {
                depth -= 1;
                if depth == 0 && i + 1 != tail.len() {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some((
        format!("{} ", sql[..values_end].trim_end()),
        tail.to_string(),
    ))
}

async fn end_transaction(conn: &mut Conn, st: &mut State, verb: &str) -> Result<()> {
    let r = conn.query_drop(verb).await.map_err(|e| mapped(st, e, verb));
    if r.is_ok() {
        st.dirty = false;
        st.read_open = false;
        st.special_tx = false;
    }
    r
}

/// `ALTER SESSION SET …`: session state lives here (re-applied on reconnect)
/// and is not transactional, as in Oracle.
async fn alter_session(
    conn: &mut Conn,
    st: &mut State,
    settings: Vec<(String, String)>,
) -> Result<()> {
    for (key, value) in settings {
        let value = value.trim().trim_matches('\'').to_string();
        match key.as_str() {
            "TIME_ZONE" => {
                let tz = match value.to_ascii_uppercase().as_str() {
                    "LOCAL" | "DBTIMEZONE" => "SYSTEM".to_string(),
                    _ => value.clone(),
                };
                conn.query_drop(format!("SET time_zone = {}", quote(&tz)))
                    .await
                    .map_err(|_| Error::ora(1882, format!("timezone region {value} not found")))?;
                st.time_zone = Some(tz);
            }
            "CURRENT_SCHEMA" => {
                let oracle = if value.starts_with('"') {
                    value.trim_matches('"').to_string()
                } else {
                    value.to_ascii_uppercase()
                };
                let physical = st.params.case.physical(&oracle);
                conn.query_drop(format!("USE `{}`", physical.replace('`', "``")))
                    .await
                    .map_err(|_| Error::ora(1435, format!("user {oracle} does not exist")))?;
                st.current_schema = physical;
            }
            k if k.starts_with("NLS_") => {
                conn.query_drop(format!(
                    "SET @dbsaci_{} = {}",
                    k.to_ascii_lowercase(),
                    quote(&value)
                ))
                .await
                .map_err(|e| mapped(st, e, "SET"))?;
                st.nls.retain(|(n, _)| n != k);
                st.nls.push((k.to_string(), value));
            }
            // Optimizer, tracing, edition, … parameters have no MariaDB
            // equivalent and no visible effect; accept them.
            _ => {}
        }
    }
    Ok(())
}

/// `SET TRANSACTION …`. Oracle allows it only as the first statement of a
/// transaction (ORA-01453); it applies to that transaction only.
async fn set_transaction(conn: &mut Conn, st: &mut State, sql: &str) -> Result<()> {
    if st.dirty {
        return Err(Error::ora(
            1453,
            "SET TRANSACTION must be first statement of transaction",
        ));
    }
    let up = sql.to_ascii_uppercase();
    let mariadb = if up.contains("SERIALIZABLE") {
        // Oracle SERIALIZABLE is snapshot isolation: MariaDB REPEATABLE READ
        // (MariaDB's SERIALIZABLE would turn every read into a locking read).
        Some("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
    } else if up.contains("READ ONLY") {
        Some("SET TRANSACTION READ ONLY")
    } else if up.contains("READ COMMITTED") || up.contains("READ WRITE") {
        None
    } else {
        // NAME '…', USE ROLLBACK SEGMENT: no effect on MariaDB.
        None
    };
    let Some(stmt) = mariadb else {
        return Ok(());
    };
    // A read-only transaction may already be open (reads start one); end it
    // so the setting applies to the transaction the client is beginning.
    if st.read_open {
        conn.query_drop("COMMIT")
            .await
            .map_err(|e| mapped(st, e, "COMMIT"))?;
        st.read_open = false;
    }
    conn.query_drop(stmt)
        .await
        .map_err(|e| mapped(st, e, stmt))?;
    // Start it now so the snapshot is taken at this point, like Oracle.
    conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
        .await
        .map_err(|e| mapped(st, e, "START TRANSACTION"))?;
    st.special_tx = true;
    st.read_open = true;
    Ok(())
}

/// `UPDATE … RETURNING` (MariaDB has none) runs as the UPDATE then a SELECT
/// of the returned expressions under the same WHERE; INSERT/DELETE RETURNING
/// are native.
async fn returning(
    conn: &mut Conn,
    st: &mut State,
    sql: &str,
    params: &[Value],
) -> Result<(u64, Vec<Row>)> {
    st.shared.in_flight.store(true, Ordering::Relaxed);
    let deadline = Deadline::arm(st);
    let r = returning_inner(conn, st, sql, params).await;
    drop(deadline);
    st.shared.in_flight.store(false, Ordering::Relaxed);
    if r.is_ok() {
        after_statement(st, sql, true);
    }
    r
}

async fn returning_inner(
    conn: &mut Conn,
    st: &mut State,
    sql: &str,
    params: &[Value],
) -> Result<(u64, Vec<Row>)> {
    if let Some((update, select)) = split_update_returning(sql) {
        let n_update = binds::count_placeholders(&update);
        let (up_params, sel_params) = params.split_at(n_update.min(params.len()));
        let affected = run_prepared(conn, &update, up_params)
            .await
            .map_err(|e| mapped(st, e, &update))?;
        // The SELECT re-reads the updated rows through the UPDATE's WHERE,
        // whose binds come first; reuse them.
        let n_where = binds::count_placeholders(&select);
        let mut sel: Vec<Value> =
            up_params[up_params.len() - n_where.min(up_params.len())..].to_vec();
        sel.extend_from_slice(sel_params);
        let rows = collect_rows(conn, st, &select, &sel).await?;
        return Ok((affected, rows));
    }
    let rows = collect_rows(conn, st, sql, params).await?;
    Ok((rows.len() as u64, rows))
}

/// Run a row-returning statement to completion, values encoded with the plain
/// Oracle forms (RETURNING has no describe to negotiate).
async fn collect_rows(
    conn: &mut Conn,
    st: &mut State,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Row>> {
    let caps = DescribeCaps::LENIENT;
    let mut result = match conn
        .exec_iter(sql, Params::Positional(params.to_vec()))
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(mapped(st, e, sql)),
    };
    let wires: Vec<Wire> = result
        .columns_ref()
        .iter()
        .map(|c| types::describe(c, &caps, IdentifierCase::Upper).1)
        .collect();
    let mut out = Vec::new();
    loop {
        match result.next().await {
            Ok(Some(row)) => out.push(encode_row(&row, &wires)?),
            Ok(None) => break,
            Err(e) => return Err(mapped(st, e, sql)),
        }
    }
    if let Err(e) = result.drop_result().await {
        return Err(mapped(st, e, sql));
    }
    Ok(out)
}

/// `UPDATE t SET … WHERE w RETURNING e` → (`UPDATE t SET … WHERE w`,
/// `SELECT e FROM t WHERE w`).
fn split_update_returning(sql: &str) -> Option<(String, String)> {
    if verb(sql) != "UPDATE" {
        return None;
    }
    let up = sql.to_ascii_uppercase();
    let r = up.rfind(" RETURNING ")?;
    let update = sql[..r].trim().to_string();
    let exprs = sql[r + " RETURNING ".len()..].trim().trim_end_matches(';');
    let after_update = update.trim_start()[6..].trim_start();
    let set_at = after_update.to_ascii_uppercase().find(" SET ")?;
    let table = after_update[..set_at].trim();
    let where_clause = after_update
        .to_ascii_uppercase()
        .rfind(" WHERE ")
        .map(|w| &after_update[w..])
        .unwrap_or("");
    Some((
        update.clone(),
        format!("SELECT {exprs} FROM {table}{where_clause}"),
    ))
}

fn encode_row(row: &mysql_async::Row, wires: &[Wire]) -> Result<Row> {
    (0..row.len())
        .map(|i| match row.as_ref(i) {
            Some(v) => types::encode(v, wires.get(i).copied().unwrap_or(Wire::Text)),
            None => Ok(None),
        })
        .collect()
}

fn fetch_spilled(st: &mut State, cursor: u64, n: usize) -> Result<(Vec<Row>, bool)> {
    let Some(buf) = st.spilled.get_mut(&cursor) else {
        return Err(Error::ora(1001, "invalid cursor"));
    };
    if buf.is_empty()
        && let Some(e) = st.spill_errors.remove(&cursor)
    {
        st.spilled.remove(&cursor);
        return Err(Error::ora(1013, e));
    }
    let take = n.min(buf.len());
    let rows: Vec<Row> = buf.drain(..take).collect();
    let exhausted = buf.is_empty() && !st.spill_errors.contains_key(&cursor);
    if exhausted {
        st.spilled.remove(&cursor);
    }
    Ok((rows, exhausted))
}

/// Execute a query and serve its cursor, streaming.
async fn query(
    conn: &mut Conn,
    st: &mut State,
    sql: String,
    params: Vec<Value>,
    caps: DescribeCaps,
    reply: oneshot::Sender<Result<(u64, Vec<ColumnMeta>)>>,
    rx: &mut mpsc::Receiver<Cmd>,
) -> Option<Cmd> {
    match check_currval(st, &sql) {
        Ok(nextvals) => st.nextval_seen.extend(nextvals),
        Err(e) => {
            let _ = reply.send(Err(e));
            return None;
        }
    }
    st.shared.in_flight.store(true, Ordering::Relaxed);
    let deadline = Deadline::arm(st);
    let locks = sql.to_ascii_uppercase().contains("FOR UPDATE");
    // Start the query (reconnecting once if the link died and nothing is
    // uncommitted). The result borrows the connection, so the reconnect
    // decision happens before the stream is taken.
    let mut attempt = 0;
    loop {
        let started = if params.is_empty() {
            match conn.query_iter(sql.as_str()).await {
                Ok(r) => {
                    drop(deadline);
                    let out = serve(r, st, &sql, &caps, locks, reply, rx).await;
                    st.shared.in_flight.store(false, Ordering::Relaxed);
                    return out;
                }
                Err(e) => e,
            }
        } else {
            match conn
                .exec_iter(sql.as_str(), Params::Positional(params.clone()))
                .await
            {
                Ok(r) => {
                    drop(deadline);
                    let out = serve(r, st, &sql, &caps, locks, reply, rx).await;
                    st.shared.in_flight.store(false, Ordering::Relaxed);
                    return out;
                }
                Err(e) => e,
            }
        };
        let link = errors::is_link_error(&started);
        if link && attempt == 0 && !st.dirty {
            attempt += 1;
            tracing::warn!("MariaDB connection lost ({started}); reconnecting");
            match establish(st).await {
                Ok(fresh) => {
                    *conn = fresh;
                    st.read_open = false;
                    continue;
                }
                Err(e) => {
                    st.shared.lost.store(true, Ordering::Relaxed);
                    let _ = reply.send(Err(Error::SessionLost(e.to_string())));
                    break;
                }
            }
        }
        if link {
            st.shared.lost.store(true, Ordering::Relaxed);
            let _ = reply.send(Err(Error::SessionLost(started.to_string())));
        } else {
            let _ = reply.send(Err(mapped(st, started, &sql)));
        }
        break;
    }
    st.shared.in_flight.store(false, Ordering::Relaxed);
    None
}

/// Describe the result, answer the Query, then serve Fetches on it until the
/// rows run out or another request needs the connection.
async fn serve<P: Protocol>(
    mut result: QueryResult<'_, 'static, P>,
    st: &mut State,
    sql: &str,
    caps: &DescribeCaps,
    locks: bool,
    reply: oneshot::Sender<Result<(u64, Vec<ColumnMeta>)>>,
    rx: &mut mpsc::Receiver<Cmd>,
) -> Option<Cmd> {
    let cols: Vec<Column> = result.columns_ref().to_vec();
    let (mut metas, wires): (Vec<ColumnMeta>, Vec<Wire>) = cols
        .iter()
        .map(|c| types::describe(c, caps, st.params.case))
        .unzip();
    for (i, m) in metas.iter_mut().enumerate() {
        m.position = i as u16;
    }
    let cursor = st.next_cursor;
    st.next_cursor += 1;
    if locks {
        st.dirty = true;
    } else {
        st.read_open = true;
    }
    if reply.send(Ok((cursor, metas))).is_err() {
        let _ = result.drop_result().await;
        return None;
    }
    let _ = sql;
    // Stream.
    loop {
        let Some(cmd) = rx.recv().await else {
            let _ = result.drop_result().await;
            return None;
        };
        match cmd {
            Cmd::Fetch {
                cursor: c,
                n,
                reply,
            } if c == cursor => {
                let mut rows = Vec::with_capacity(n.min(4096));
                let mut done = false;
                let mut err = None;
                let deadline = Deadline::arm(st);
                st.shared.in_flight.store(true, Ordering::Relaxed);
                while rows.len() < n {
                    match result.next().await {
                        Ok(Some(row)) => match encode_row(&row, &wires) {
                            Ok(r) => rows.push(r),
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        },
                        Ok(None) => {
                            done = true;
                            break;
                        }
                        Err(e) => {
                            err = Some(mapped(st, e, sql));
                            break;
                        }
                    }
                }
                drop(deadline);
                st.shared.in_flight.store(false, Ordering::Relaxed);
                if let Some(e) = err {
                    let _ = reply.send(Err(e));
                    let _ = result.drop_result().await;
                    return None;
                }
                let _ = reply.send(Ok((rows, done)));
                if done {
                    let _ = result.drop_result().await;
                    return None;
                }
            }
            Cmd::Fetch {
                cursor: c,
                n,
                reply,
            } => {
                let _ = reply.send(fetch_spilled(st, c, n));
            }
            Cmd::Close { cursor: c } if c == cursor => {
                let _ = result.drop_result().await;
                return None;
            }
            Cmd::Close { cursor: c } => {
                st.spilled.remove(&c);
            }
            other => {
                // Another request needs the connection: read the rest of this
                // cursor into memory so it stays fetchable, then hand over.
                let mut rest = VecDeque::new();
                let mut failure = None;
                loop {
                    match result.next().await {
                        Ok(Some(row)) => match encode_row(&row, &wires) {
                            Ok(r) => rest.push_back(r),
                            Err(e) => {
                                failure = Some(e.to_string());
                                break;
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            failure = Some(mapped(st, e, sql).to_string());
                            break;
                        }
                    }
                }
                let _ = result.drop_result().await;
                if let Some(f) = failure {
                    st.spill_errors.insert(cursor, f);
                }
                // Kept even when empty: the client's next Fetch then gets
                // "no more rows" rather than "invalid cursor".
                st.spilled.insert(cursor, rest);
                return Some(other);
            }
        }
    }
}

/// `(column, DATA_TYPE, DATETIME_PRECISION)` of a table, for typed translation.
async fn columns(
    conn: &mut Conn,
    schema: Option<&str>,
    table: &str,
) -> Result<Vec<(String, String, u32)>> {
    let rows: Vec<(String, String, Option<u32>)> = match schema {
        Some(s) => {
            conn.exec(
                "SELECT COLUMN_NAME, DATA_TYPE, DATETIME_PRECISION FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                (s, table),
            )
            .await
        }
        None => {
            conn.exec(
                "SELECT COLUMN_NAME, DATA_TYPE, DATETIME_PRECISION FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
                (table,),
            )
            .await
        }
    }
    .map_err(|e| errors::map_error(e, "information_schema").error)?;
    Ok(rows
        .into_iter()
        .map(|(c, t, p)| (c, t, p.unwrap_or(0)))
        .collect())
}

/// Run the dictionary install if `dbsaci_sys` is missing or stale. Returns
/// whether the dictionary is current afterwards, and the server's
/// `lower_case_table_names`.
pub async fn ensure_dictionary(opts: &Opts) -> Result<(bool, u8)> {
    let mut conn = Conn::new(opts.clone())
        .await
        .map_err(|e| connect_error(e, "-"))?;
    let lctn: u8 = conn
        .query_first::<u8, _>("SELECT @@lower_case_table_names")
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    conn.query_drop(format!("SET SESSION sql_mode = '{SQL_MODE}'"))
        .await
        .map_err(|e| errors::map_error(e, "SET").error)?;
    let current = installed_version(&mut conn).await;
    if current.as_deref() == Some(super::install::VERSION) {
        let _ = conn.disconnect().await;
        return Ok((true, lctn));
    }
    let got: Option<i64> = conn
        .query_first("SELECT GET_LOCK('dbsaci_install', 60)")
        .await
        .ok()
        .flatten();
    let mut ok = false;
    if got == Some(1) {
        if installed_version(&mut conn).await.as_deref() == Some(super::install::VERSION) {
            ok = true;
        } else {
            match install(&mut conn).await {
                Ok(()) => ok = true,
                Err(e) => tracing::warn!(
                    "could not install the DbSaci data dictionary in `{}` ({e}); \
                     run `dbsaci install` with an administrative account",
                    super::dictionary::SCHEMA
                ),
            }
        }
        let _ = conn.query_drop("DO RELEASE_LOCK('dbsaci_install')").await;
    }
    let _ = conn.disconnect().await;
    Ok((ok, lctn))
}

async fn installed_version(conn: &mut Conn) -> Option<String> {
    conn.query_first::<String, _>("SELECT version FROM dbsaci_sys.install_info")
        .await
        .ok()
        .flatten()
}

/// Run every install statement, then the grants (best-effort).
pub async fn install(conn: &mut Conn) -> Result<()> {
    for stmt in super::install::install_statements() {
        conn.query_drop(&stmt).await.map_err(|e| {
            let m = errors::map_error(e, &stmt).error;
            Error::Protocol(format!("{m} while running: {}", head(&stmt)))
        })?;
    }
    for stmt in super::install::grant_statements() {
        if let Err(e) = conn.query_drop(&stmt).await {
            tracing::warn!("{stmt} failed ({e}); grant SELECT, EXECUTE ON dbsaci_sys.* yourself");
        }
    }
    Ok(())
}

fn head(s: &str) -> String {
    s.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_commit_classification() {
        assert!(commits_implicitly("CREATE TABLE T (A INT)"));
        assert!(!commits_implicitly("CREATE TEMPORARY TABLE T (A INT)"));
        assert!(!commits_implicitly(
            "/* dbsaci:nocount */ CREATE TEMPORARY TABLE S AS SELECT 1"
        ));
        assert!(commits_implicitly("DROP TABLE T"));
        assert!(!commits_implicitly("INSERT INTO T VALUES (1)"));
    }

    #[test]
    fn values_tuple_split() {
        let (h, t) =
            split_values_tuple("INSERT INTO T (A, B) VALUES (?, CAST(? AS BINARY))").unwrap();
        assert_eq!(h, "INSERT INTO T (A, B) VALUES ");
        assert_eq!(t, "(?, CAST(? AS BINARY))");
        assert!(split_values_tuple("INSERT INTO T SELECT ? FROM DUAL").is_none());
        assert!(split_values_tuple("INSERT INTO T VALUES (?), (?)").is_none());
        assert!(split_values_tuple("UPDATE T SET A = ?").is_none());
    }

    #[test]
    fn update_returning_split() {
        let (u, s) = split_update_returning("UPDATE T SET A = ? WHERE ID = ? RETURNING A").unwrap();
        assert_eq!(u, "UPDATE T SET A = ? WHERE ID = ?");
        assert_eq!(s, "SELECT A FROM T WHERE ID = ?");
    }

    #[test]
    fn programs_are_detected() {
        assert!(is_program("BEGIN NULL; END;"));
        assert!(is_program(
            "CREATE OR REPLACE TRIGGER X BEFORE INSERT ON T FOR EACH ROW BEGIN NULL; END"
        ));
        assert!(!is_program("CREATE TABLE T (A INT)"));
    }
}
