//! End-to-end Oracle-compatibility corpus.
//!
//! One PostgreSQL/orafce container and one PgSaci proxy are started once for the
//! whole binary. Every golden-file case under `tests/corpus/*.sql` is then
//! streamed across the real TNS wire through a single worker connection and its
//! result compared to the expected block declared beside it.
//!
//! Custom `libtest-mimic` harness so each case is an individually reportable
//! test (`cargo test --test corpus -- oracle_functions_date::add_months`) while
//! still sharing the expensive container/handshake setup.
//!
//! Format reference: `tests/corpus/README.md`.

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use libtest_mimic::{Arguments, Failed, Trial};
use mysql_async::{Conn as MariaConn, prelude::Queryable};
use oracle_rs::types::OracleDate;
use oracle_rs::{Config as OracleConfig, Connection as OracleConnection, Value};
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tokio_postgres::{Client, NoTls};

use pgsaci::{Config as PgSaciConfig, Server};

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");

fn main() {
    let args = Arguments::from_args();

    let groups = match load_groups(Path::new(CORPUS_DIR)) {
        Ok(groups) => groups,
        Err(e) => {
            eprintln!("failed to load corpus: {e}");
            std::process::exit(2);
        }
    };

    // Bring the backend up before enumerating trials so a Docker/environment
    // problem fails loudly once instead of once per case.
    let (job_tx, ready_rx, handle) = start_worker(
        groups
            .iter()
            .flat_map(|g| g.fixtures.iter().cloned())
            .collect(),
    );
    let pg_major = match ready_rx.recv() {
        Ok(Ok(major)) => major,
        Ok(Err(e)) => {
            eprintln!("corpus backend failed to start: {e}");
            std::process::exit(2);
        }
        Err(_) => {
            eprintln!("corpus worker exited before signalling readiness");
            std::process::exit(2);
        }
    };

    let mut trials = Vec::new();
    for group in &groups {
        // A group can declare a minimum PostgreSQL major (`# requires-pg: N`) for
        // features with a hard version floor (e.g. `MERGE` needs PG 15). Below
        // that, its cases run as ignored rather than red.
        let below_floor = group.min_pg.is_some_and(|m| pg_major != 0 && pg_major < m);
        for case in &group.cases {
            let name = format!("{}::{}", group.name, case.name);
            let job_tx = job_tx.clone();
            let case = case.clone();
            let skip = case.skip || below_floor;
            trials
                .push(Trial::test(name, move || run_trial(&job_tx, case)).with_ignored_flag(skip));
        }
    }

    let conclusion = libtest_mimic::run(&args, trials);

    // Tear the container down deterministically rather than leaning on process
    // exit to reap it.
    drop(job_tx);
    let _ = handle.join();

    conclusion.exit();
}

// ---------------------------------------------------------------------------
// Worker: owns the runtime, the container, the admin PG client and the one
// Oracle connection. Cases are executed strictly serially here, which is what
// makes per-case `SAVEPOINT` isolation sound.
// ---------------------------------------------------------------------------

enum Job {
    Run {
        case: Case,
        reply: Sender<Result<(), String>>,
    },
}

fn start_worker(
    fixtures: Vec<String>,
) -> (
    Sender<Job>,
    Receiver<Result<u32, String>>,
    thread::JoinHandle<()>,
) {
    let (job_tx, job_rx) = channel::<Job>();
    // The `Ok` payload is the backend PostgreSQL major version, so `main` can
    // skip cases carrying a `# requires-pg:` floor the backend does not meet.
    let (ready_tx, ready_rx) = channel::<Result<u32, String>>();

    let handle = thread::Builder::new()
        .name("corpus-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build worker runtime");

            rt.block_on(async move {
                let env = match TestBackend::start(fixtures).await {
                    Ok(env) => {
                        let major = env.admin.lock().await.server_major().await.unwrap_or(0);
                        let _ = ready_tx.send(Ok(major));
                        env
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                while let Ok(job) = job_rx.recv() {
                    match job {
                        Job::Run { case, reply } => {
                            let outcome = env.run_case(&case).await;
                            let _ = reply.send(outcome);
                        }
                    }
                }

                // Channel closed: main is done. Drop the container inside the
                // runtime so its async cleanup can complete.
                drop(env);
            });
        })
        .expect("spawn corpus worker");

    (job_tx, ready_rx, handle)
}

fn run_trial(job_tx: &Sender<Job>, case: Case) -> Result<(), Failed> {
    let (reply_tx, reply_rx) = channel();
    job_tx
        .send(Job::Run {
            case,
            reply: reply_tx,
        })
        .map_err(|_| Failed::from("corpus worker is gone"))?;
    reply_rx
        .recv()
        .map_err(|_| Failed::from("corpus worker dropped the reply channel"))?
        .map_err(Failed::from)
}

struct TestBackend {
    _container: testcontainers::ContainerAsync<GenericImage>,
    admin: tokio::sync::Mutex<CorpusAdmin>,
    proxy_port: u16,
    oracle: tokio::sync::Mutex<OracleConnection>,
}

enum CorpusAdmin {
    Postgres(Client),
    #[allow(dead_code)]
    MariaDb(MariaConn),
}

impl CorpusAdmin {
    async fn batch_execute(&mut self, sql: &str) -> Result<(), String> {
        match self {
            Self::Postgres(client) => client.batch_execute(sql).await.map_err(|e| e.to_string()),
            Self::MariaDb(conn) => {
                for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                    conn.query_drop(statement)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        }
    }

    async fn server_major(&mut self) -> Result<u32, String> {
        match self {
            Self::Postgres(client) => client
                .query_one("SHOW server_version_num", &[])
                .await
                .map(|r| r.get::<_, String>(0).parse::<u32>().unwrap_or(0) / 10_000)
                .map_err(|e| e.to_string()),
            Self::MariaDb(conn) => conn
                .query_first::<String, _>("SELECT VERSION()")
                .await
                .map_err(|e| e.to_string())
                .map(|v| {
                    v.and_then(|v| v.split('.').next()?.parse().ok())
                        .unwrap_or(0)
                }),
        }
    }

    async fn scalar_text(&mut self, sql: &str) -> Result<String, String> {
        match self {
            Self::Postgres(client) => client
                .query_one(sql, &[])
                .await
                .map(|row| pg_scalar_text(&row))
                .map_err(|e| e.to_string()),
            Self::MariaDb(conn) => conn
                .query_first::<mysql_async::Value, _>(sql)
                .await
                .map(|row| {
                    row.map(|r| maria_scalar_text(&r))
                        .unwrap_or_else(|| "NULL".into())
                })
                .map_err(|e| e.to_string()),
        }
    }
}

impl TestBackend {
    async fn start(fixtures: Vec<String>) -> Result<Self, String> {
        // `PGSACI_TEST_PG_IMAGE` (e.g. `pgsaci-test-pg:16`) lets CI run the
        // corpus against several PostgreSQL majors; the default matches the
        // image `clients/run.sh` and the docs build.
        let image = std::env::var("PGSACI_TEST_PG_IMAGE")
            .unwrap_or_else(|_| "pgsaci-test-pg:18".to_string());
        let (image_name, image_tag) = image.rsplit_once(':').unwrap_or((image.as_str(), "latest"));
        let container = GenericImage::new(image_name.to_string(), image_tag.to_string())
            .with_wait_for(WaitFor::message_on_stdout(
                "database system is ready to accept connections",
            ))
            .with_exposed_port(ContainerPort::Tcp(5432))
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .with_env_var("POSTGRES_DB", "postgres")
            .start()
            .await
            .map_err(|e| {
                format!("start container (is Docker running? is the `{image}` image built?): {e}")
            })?;

        let host = container.get_host().await.map_err(|e| e.to_string())?;
        let host = if host.to_string() == "localhost" {
            "127.0.0.1".to_string()
        } else {
            host.to_string()
        };
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .map_err(|e| e.to_string())?;

        let admin = connect_admin(&host, port).await?;

        // Committed baseline. Every case runs against this and is rolled back to
        // it; an independent connection (`-- verify:`) also sees exactly this
        // plus whatever the case committed.
        admin
            .batch_execute(BASELINE_SQL)
            .await
            .map_err(|e| format!("seed baseline: {e}"))?;
        for fixture in &fixtures {
            admin
                .batch_execute(fixture)
                .await
                .map_err(|e| format!("apply fixture `{fixture}`: {e}"))?;
        }

        // Start PgSaci on a random port, pointed at the container.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let proxy_port = listener.local_addr().map_err(|e| e.to_string())?.port();
        tokio::spawn(async move {
            let _ = Server::new(PgSaciConfig {
                backend: pgsaci::BackendKind::Postgres,
                listen_addr: format!("127.0.0.1:{proxy_port}"),
                pg_host: host,
                pg_port: port,
                pg_db: "postgres".into(),
                credentials: pgsaci::Credentials::with_fallback(CORPUS_USER),
                // Exercise the proxy's per-statement timeout while leaving
                // ordinary corpus queries ample time to run.
                statement_timeout: Some(Duration::from_secs(2)),
                idle_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            })
            .run_with_listener(listener)
            .await;
        });

        let oracle = tokio::sync::Mutex::new(connect_oracle(proxy_port).await?);
        Ok(Self {
            _container: container,
            admin: tokio::sync::Mutex::new(CorpusAdmin::Postgres(admin)),
            proxy_port,
            oracle,
        })
    }

    async fn run_case(&self, case: &Case) -> Result<(), String> {
        let mut conn = self.oracle.lock().await;

        // A single case must never wedge the whole run (e.g. an accidental
        // non-terminating recursive CTE). On timeout, force a reconnect below.
        let mut timed_out = false;
        let mut admin = self.admin.lock().await;
        let result = match tokio::time::timeout(
            Duration::from_secs(20),
            run_case_body(&conn, &mut admin, case),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                timed_out = true;
                Err("case exceeded the 20s time limit".to_string())
            }
        };

        // Isolation. Client-side SAVEPOINTs are not usable here: PgSaci wraps every
        // statement in `SAVEPOINT pgsaci_statement ... RELEASE`, and `RELEASE`
        // also destroys any savepoint the client established afterwards. So a
        // case that touched state gets a full `ROLLBACK` (PgSaci turns that into
        // `ROLLBACK; BEGIN`) followed by a reconnect, because `ROLLBACK` also
        // drops the per-session temp views backing the Oracle catalog facade.
        // Pure read-only cases need neither.
        let crashed = timed_out
            || tokio::time::timeout(
                Duration::from_secs(5),
                conn.query("SELECT 1 FROM DUAL", &[]),
            )
            .await
            .map_or(true, |r| r.is_err());

        // Release any locks this session still holds from uncommitted setup DML
        // *before* the admin-side teardown runs. A lock-strong teardown
        // statement — e.g. `DROP TRIGGER IF EXISTS ... ON trg_people`, which
        // needs AccessExclusive — otherwise blocks on this session's row locks
        // until PgSaci's idle reaper closes the session `idle_timeout` (~30s)
        // later. That was the entire cost of the `triggers::` group.
        if crashed || case_mutates(case) {
            let _ = conn.execute("ROLLBACK", &[]).await;
        }

        // Best-effort teardown on the admin connection, for the handful of cases
        // that COMMIT and therefore escape the reset above.
        for stmt in &case.teardown {
            let _ = admin.batch_execute(stmt).await;
        }

        // `ROLLBACK` above also dropped the per-session catalog-facade temp
        // views, so a mutating case still needs a fresh session for the next run.
        if (crashed || case_mutates(case))
            && let Ok(fresh) = connect_oracle(self.proxy_port).await
        {
            *conn = fresh;
        }

        result
    }
}

/// Does this case change database state (and therefore need a reset afterwards)?
fn case_mutates(case: &Case) -> bool {
    if !case.setup.is_empty() || !case.teardown.is_empty() || case.verify.is_some() {
        return true;
    }
    if matches!(case.expect, Expect::RowCount(_)) {
        return true;
    }
    let head = case
        .sql
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        head.as_str(),
        "INSERT"
            | "UPDATE"
            | "DELETE"
            | "CREATE"
            | "DROP"
            | "ALTER"
            | "MERGE"
            | "TRUNCATE"
            | "COMMIT"
            | "ROLLBACK"
            | "GRANT"
            | "REVOKE"
            | "SET"
            | "SAVEPOINT"
            | "RELEASE"
    )
}

/// Run a query and return all rows.
///
/// Exercises PgSaci's Execute + client-driven Fetch streaming. The first Execute
/// returns up to the client's prefetch count and a cursor id when more rows
/// remain; subsequent `fetch_more` calls pull batches until the server signals
/// exhaustion.
async fn query_all(
    oracle: &OracleConnection,
    sql: &str,
    binds: &[Value],
) -> Result<Vec<oracle_rs::Row>, String> {
    let mut result = oracle.query(sql, binds).await.map_err(|e| e.to_string())?;
    let mut rows = std::mem::take(&mut result.rows);
    let cursor_id = result.cursor_id;
    let columns = result.columns.clone();
    if cursor_id != 0 {
        loop {
            let more = oracle
                .fetch_more(cursor_id, &columns, 5000)
                .await
                .map_err(|e| e.to_string())?;
            rows.extend(more.rows);
            if !more.has_more_rows {
                break;
            }
        }
    }
    Ok(rows)
}

async fn run_case_body(
    oracle: &OracleConnection,
    admin: &mut CorpusAdmin,
    case: &Case,
) -> Result<(), String> {
    for setup in &case.setup {
        // `-- setup?:` lines are prefixed with a NUL sentinel and may fail
        // (used to exercise mid-transaction statement errors).
        if let Some(tolerant) = setup.strip_prefix('\u{0}') {
            let _ = oracle.execute(tolerant, &[]).await;
        } else {
            oracle
                .execute(setup, &[])
                .await
                .map_err(|e| format!("setup `{setup}` failed: {}", e))?;
        }
    }

    let binds = decode_binds(&case.binds)?;

    match &case.expect {
        Expect::Ok => {
            oracle
                .query(&case.sql, &binds)
                .await
                .map_err(|e| format!("expected success, got error: {e}"))?;
        }
        Expect::Rows(expected) => {
            let rows = query_all(oracle, &case.sql, &binds)
                .await
                .map_err(|e| format!("expected rows, got error: {e}"))?;
            let actual = format_rows(&rows);
            if &actual != expected {
                return Err(diff("row mismatch", expected, &actual));
            }
        }
        Expect::RowRegex(pattern) => {
            let rows = query_all(oracle, &case.sql, &binds)
                .await
                .map_err(|e| format!("expected a row, got error: {e}"))?;
            let actual = format_rows(&rows).join("\n");
            let re = regex_lite(pattern);
            if !re.is_match(&actual) {
                return Err(format!("row `{actual}` does not match regex `{pattern}`"));
            }
        }
        Expect::RowsExactly(n) => {
            let rows = query_all(oracle, &case.sql, &binds)
                .await
                .map_err(|e| format!("expected {n} rows, got error: {e}"))?;
            if rows.len() != *n {
                return Err(format!("expected exactly {n} rows, got {}", rows.len()));
            }
        }
        Expect::RowCount(n) => {
            let result = oracle
                .execute(&case.sql, &binds)
                .await
                .map_err(|e| format!("expected {n} rows affected, got error: {e}"))?;
            if result.rows_affected != *n {
                return Err(format!(
                    "expected {n} rows affected, got {}",
                    result.rows_affected
                ));
            }
        }
        Expect::Error(token) => {
            // `-- error: <must contain> [ ~ <must NOT contain>]`
            let (want, forbid) = token
                .split_once(" ~ ")
                .map(|(a, b)| (a.trim(), Some(b.trim())))
                .unwrap_or((token.as_str(), None));
            match oracle.query(&case.sql, &binds).await {
                Ok(_) => {
                    return Err(format!(
                        "expected error containing `{want}`, statement succeeded"
                    ));
                }
                Err(e) => {
                    let text = e.to_string();
                    let lower = text.to_ascii_lowercase();
                    if !lower.contains(&want.to_ascii_lowercase()) {
                        return Err(format!("expected error containing `{want}`, got `{text}`"));
                    }
                    if let Some(f) = forbid
                        && lower.contains(&f.to_ascii_lowercase())
                    {
                        return Err(format!("error should not contain `{f}`, got `{text}`"));
                    }
                }
            }
        }
    }

    if let Some(verify) = &case.verify {
        let actual = admin
            .scalar_text(&verify.sql)
            .await
            .map_err(|e| format!("`-- verify` query failed: {e}"))?;
        if actual != verify.expected {
            return Err(format!(
                "independent connection sees `{actual}`, expected `{}` (state was not committed as expected)",
                verify.expected
            ));
        }
    }

    Ok(())
}

const CORPUS_USER: &str = "corpus";

const BASELINE_SQL: &str = "
    CREATE EXTENSION IF NOT EXISTS orafce;
    DROP ROLE IF EXISTS corpus;
    CREATE ROLE corpus WITH LOGIN PASSWORD 'corpus' SUPERUSER;
    DROP TABLE IF EXISTS people, teams CASCADE;
    CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
    CREATE TABLE people (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        team_id INTEGER REFERENCES teams(id)
    );
    INSERT INTO teams (id, name) VALUES (1, 'Engineering'), (2, 'Sales'), (3, 'Marketing');
    INSERT INTO people (id, name, team_id) VALUES
        (1, 'Ada', 1), (2, 'Grace', 1), (3, 'Linus', 2), (4, 'Margaret', NULL);
";

async fn connect_admin(host: &str, port: u16) -> Result<Client, String> {
    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
    let mut last = String::new();
    for _ in 0..50 {
        match tokio_postgres::connect(&conn_str, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                return Ok(client);
            }
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }
    Err(format!("admin connect to postgres: {last}"))
}

async fn connect_oracle(proxy_port: u16) -> Result<OracleConnection, String> {
    let mut last = String::new();
    for _ in 0..40 {
        let cfg = OracleConfig::new(
            "127.0.0.1",
            proxy_port,
            "FREEPDB1",
            CORPUS_USER,
            CORPUS_USER,
        );
        match OracleConnection::connect_with_config(cfg).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }
    Err(format!("oracle-rs connect through PgSaci: {last}"))
}

// ---------------------------------------------------------------------------
// Golden-file model
// ---------------------------------------------------------------------------

struct Group {
    name: String,
    fixtures: Vec<String>,
    cases: Vec<Case>,
    /// Minimum PostgreSQL major required for this group's feature (from a
    /// `# requires-pg: N` line). Cases run as ignored on older backends.
    min_pg: Option<u32>,
}

#[derive(Clone)]
struct Case {
    name: String,
    setup: Vec<String>,
    teardown: Vec<String>,
    binds: Vec<String>,
    sql: String,
    expect: Expect,
    verify: Option<Verify>,
    skip: bool,
}

#[derive(Clone)]
enum Expect {
    Ok,
    Rows(Vec<String>),
    RowRegex(String),
    /// `-- rows: N` — the query returns exactly N rows; values not inspected.
    /// Used to probe the result/fetch path with sizes larger than a golden
    /// block can spell out.
    RowsExactly(usize),
    RowCount(u64),
    Error(String),
}

#[derive(Clone)]
struct Verify {
    sql: String,
    expected: String,
}

fn load_groups(dir: &Path) -> Result<Vec<Group>, String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no *.sql corpus files in {}", dir.display()));
    }

    let mut groups = Vec::new();
    for path in files {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        groups.push(parse_group(&name, &text).map_err(|e| format!("{}: {e}", path.display()))?);
    }
    Ok(groups)
}

/// Line-oriented parser. Directive lines start with `-- <key>:` (or `-- ok`);
/// everything else between the directives and the expectation block is SQL.
fn parse_group(name: &str, text: &str) -> Result<Group, String> {
    let mut fixtures = Vec::new();
    let mut cases: Vec<Case> = Vec::new();
    let mut min_pg: Option<u32> = None;
    let mut lines = text.lines().peekable();

    while let Some(raw) = lines.next() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# requires-pg:") {
            min_pg = rest.trim().parse().ok();
            continue;
        }
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || (trimmed.starts_with("--") && !is_directive(trimmed))
        {
            continue;
        }
        if let Some(sql) = directive(trimmed, "fixture") {
            fixtures.push(sql.to_string());
            continue;
        }
        let Some(case_name) = directive(trimmed, "case") else {
            return Err(format!("unexpected line outside a case: `{line}`"));
        };

        let mut setup = Vec::new();
        let mut teardown = Vec::new();
        let mut binds = Vec::new();
        let mut skip = false;
        let mut sql_lines: Vec<String> = Vec::new();
        let mut expect: Option<Expect> = None;
        let mut verify: Option<Verify> = None;

        while let Some(raw) = lines.peek() {
            let l = raw.trim();
            if l.starts_with("-- case:") || l.starts_with("-- fixture:") {
                break;
            }
            let raw = lines.next().unwrap();
            let l = raw.trim();

            if l.is_empty() || l.starts_with('#') || (l.starts_with("--") && !is_directive(l)) {
                continue;
            }
            if let Some(v) = directive(l, "setup?") {
                setup.push(format!("\u{0}{v}"));
            } else if let Some(v) = directive(l, "setup") {
                setup.push(v.to_string());
            } else if let Some(v) = directive(l, "teardown") {
                teardown.push(v.to_string());
            } else if let Some(v) = directive(l, "bind") {
                binds.push(v.to_string());
            } else if let Some(v) = directive(l, "tag") {
                match v.trim() {
                    "skip" => skip = true,
                    other => return Err(format!("case `{case_name}`: unknown tag `{other}`")),
                }
            } else if let Some(v) = directive(l, "verify") {
                let (sql, exp) = v.split_once("=>").ok_or_else(|| {
                    format!("case `{case_name}`: `-- verify:` needs `SQL => EXPECTED`")
                })?;
                verify = Some(Verify {
                    sql: sql.trim().to_string(),
                    expected: exp.trim().to_string(),
                });
            } else if let Some(v) = directive(l, "expect-regex") {
                expect = Some(Expect::RowRegex(v.to_string()));
            } else if let Some(v) = directive(l, "rows") {
                expect =
                    Some(Expect::RowsExactly(v.trim().parse().map_err(|_| {
                        format!("case `{case_name}`: bad rows count `{v}`")
                    })?));
            } else if let Some(v) = directive(l, "rowcount") {
                expect =
                    Some(Expect::RowCount(v.trim().parse().map_err(|_| {
                        format!("case `{case_name}`: bad rowcount `{v}`")
                    })?));
            } else if let Some(v) = directive(l, "error") {
                expect = Some(Expect::Error(v.trim().to_string()));
            } else if l == "-- ok" {
                expect = Some(Expect::Ok);
            } else if l == "-- expect:" {
                let mut rows = Vec::new();
                for raw in lines.by_ref() {
                    if raw.trim() == "-- end" {
                        break;
                    }
                    rows.push(raw.trim_end().to_string());
                }
                expect = Some(Expect::Rows(rows));
            } else if l == "-- end" {
                break;
            } else if l.starts_with("--") && is_directive(l) {
                return Err(format!("case `{case_name}`: unknown directive `{l}`"));
            } else {
                sql_lines.push(raw.trim_end().to_string());
            }
        }

        let sql = sql_lines
            .join("\n")
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        if sql.is_empty() {
            return Err(format!("case `{case_name}`: no SQL body"));
        }
        let expect = expect.ok_or_else(|| {
            format!("case `{case_name}`: missing an expectation (`-- expect:`, `-- rowcount:`, `-- error:`, `-- ok`)")
        })?;

        if cases.iter().any(|c| c.name == case_name) {
            return Err(format!("duplicate case name `{case_name}`"));
        }
        cases.push(Case {
            name: case_name.to_string(),
            setup,
            teardown,
            binds,
            sql,
            expect,
            verify,
            skip,
        });
    }

    Ok(Group {
        name: name.to_string(),
        fixtures,
        cases,
        min_pg,
    })
}

fn is_directive(line: &str) -> bool {
    const KEYS: &[&str] = &[
        "-- case:",
        "-- fixture:",
        "-- setup:",
        "-- setup?:",
        "-- teardown:",
        "-- bind:",
        "-- tag:",
        "-- verify:",
        "-- expect:",
        "-- expect-regex:",
        "-- rows:",
        "-- rowcount:",
        "-- error:",
        "-- ok",
        "-- end",
    ];
    KEYS.iter().any(|k| line == *k || line.starts_with(k))
}

fn directive<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("-- {key}:");
    line.strip_prefix(&prefix).map(|rest| rest.trim())
}

// ---------------------------------------------------------------------------
// Value formatting / bind decoding
// ---------------------------------------------------------------------------

/// One row per line, columns joined by ` | `, SQL NULL rendered as `NULL`.
fn format_rows(rows: &[oracle_rs::Row]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            (0..row.len())
                .map(|i| format_value(row, i))
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect()
}

fn format_value(row: &oracle_rs::Row, i: usize) -> String {
    match row.get(i) {
        None | Some(Value::Null) => "NULL".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Integer(v)) => v.to_string(),
        Some(Value::Float(v)) => v.to_string(),
        Some(Value::Number(n)) => n.as_str().to_string(),
        Some(Value::Boolean(b)) => b.to_string(),
        Some(Value::Bytes(b)) => format!("0x{}", hex_encode(b)),
        Some(Value::Date(d)) => format_date(d),
        Some(Value::Timestamp(ts)) => format_timestamp(ts),
        Some(other) => format!("{other:?}"),
    }
}

fn format_timestamp(ts: &oracle_rs::types::OracleTimestamp) -> String {
    let has_tz = ts.tz_hour_offset != 0 || ts.tz_minute_offset != 0;
    let midnight = ts.hour == 0 && ts.minute == 0 && ts.second == 0 && ts.microsecond == 0;
    // Match `format_date`: a bare midnight with no sub-second and no offset
    // renders as just the date, so pre-existing DATE-shaped expectations hold.
    let mut s = if midnight && !has_tz {
        format!("{:04}-{:02}-{:02}", ts.year, ts.month, ts.day)
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            ts.year, ts.month, ts.day, ts.hour, ts.minute, ts.second
        )
    };
    if ts.microsecond > 0 {
        s.push_str(format!(".{:06}", ts.microsecond).trim_end_matches('0'));
    }
    if has_tz {
        let sign = if ts.tz_hour_offset < 0 || ts.tz_minute_offset < 0 {
            '-'
        } else {
            '+'
        };
        s.push_str(&format!(
            " {sign}{:02}:{:02}",
            ts.tz_hour_offset.unsigned_abs(),
            ts.tz_minute_offset.unsigned_abs()
        ));
    }
    s
}

fn format_date(d: &OracleDate) -> String {
    if d.hour == 0 && d.minute == 0 && d.second == 0 {
        format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            d.year, d.month, d.day, d.hour, d.minute, d.second
        )
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `-- bind: <type> <value>` -> `oracle_rs::Value`. Types: int, float, str,
/// date (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`), bytes (hex), null.
fn decode_binds(specs: &[String]) -> Result<Vec<Value>, String> {
    specs
        .iter()
        .map(|spec| {
            let (ty, rest) = spec
                .split_once(char::is_whitespace)
                .unwrap_or((spec.as_str(), ""));
            let rest = rest.trim();
            match ty {
                "null" => Ok(Value::Null),
                "int" => rest
                    .parse::<i64>()
                    .map(Value::Integer)
                    .map_err(|_| format!("bad int bind `{rest}`")),
                "float" => rest
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| format!("bad float bind `{rest}`")),
                "str" => Ok(Value::String(rest.to_string())),
                "bytes" => decode_hex(rest).map(Value::Bytes),
                "date" => parse_bind_date(rest).map(Value::Date),
                other => Err(format!("unknown bind type `{other}`")),
            }
        })
        .collect()
}

fn parse_bind_date(s: &str) -> Result<OracleDate, String> {
    let (date, time) = match s.split_once(' ') {
        Some((d, t)) => (d, t),
        None => (s, "00:00:00"),
    };
    let d: Vec<i64> = date
        .split('-')
        .map(|p| p.parse().map_err(|_| format!("bad date `{s}`")))
        .collect::<Result<_, _>>()?;
    let t: Vec<i64> = time
        .split(':')
        .map(|p| p.parse().map_err(|_| format!("bad time in `{s}`")))
        .collect::<Result<_, _>>()?;
    if d.len() != 3 || t.len() != 3 {
        return Err(format!("bad datetime bind `{s}`"));
    }
    Ok(OracleDate::new(
        d[0] as i32,
        d[1] as u8,
        d[2] as u8,
        t[0] as u8,
        t[1] as u8,
        t[2] as u8,
    ))
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex `{s}`"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("bad hex `{s}`")))
        .collect()
}

fn pg_scalar_text(row: &tokio_postgres::Row) -> String {
    // The `-- verify:` queries are authored to return exactly one text-castable
    // scalar; keep the reader tolerant of the common column types anyway.
    use tokio_postgres::types::Type;
    let col = &row.columns()[0];
    match *col.type_() {
        Type::INT2 => row.get::<_, Option<i16>>(0).map(|v| v.to_string()),
        Type::INT4 => row.get::<_, Option<i32>>(0).map(|v| v.to_string()),
        Type::INT8 => row.get::<_, Option<i64>>(0).map(|v| v.to_string()),
        Type::BOOL => row.get::<_, Option<bool>>(0).map(|v| v.to_string()),
        Type::FLOAT4 => row.get::<_, Option<f32>>(0).map(|v| v.to_string()),
        Type::FLOAT8 => row.get::<_, Option<f64>>(0).map(|v| v.to_string()),
        _ => row.get::<_, Option<String>>(0),
    }
    .unwrap_or_else(|| "NULL".to_string())
}

fn maria_scalar_text(value: &mysql_async::Value) -> String {
    match value {
        mysql_async::Value::NULL => "NULL".into(),
        mysql_async::Value::Bytes(v) => String::from_utf8_lossy(v).into_owned(),
        mysql_async::Value::Int(v) => v.to_string(),
        mysql_async::Value::UInt(v) => v.to_string(),
        mysql_async::Value::Float(v) => v.to_string(),
        mysql_async::Value::Double(v) => v.to_string(),
        mysql_async::Value::Date(y, m, d, h, min, s, micros) => {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}.{:06}", micros)
        }
        mysql_async::Value::Time(neg, days, h, min, s, micros) => {
            format!(
                "{}{}:{:02}:{:02}.{:06}",
                if *neg { "-" } else { "" },
                days * 24 + u32::from(*h),
                min,
                s,
                micros
            )
        }
    }
}

fn diff(what: &str, expected: &[String], actual: &[String]) -> String {
    format!(
        "{what}\n  expected ({} row(s)):\n{}\n  actual ({} row(s)):\n{}",
        expected.len(),
        indent(expected),
        actual.len(),
        indent(actual),
    )
}

fn indent(rows: &[String]) -> String {
    if rows.is_empty() {
        return "    <no rows>".to_string();
    }
    rows.iter()
        .map(|r| format!("    {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Minimal regex: enough for `-- expect-regex:` (anchors, `.`, `\d`, `\.`, `*`,
// `+`, `[...]`, `|` at top level). Avoids pulling the `regex` crate into
// dev-deps just for a handful of shape assertions.
// ---------------------------------------------------------------------------

struct RegexLite {
    alternatives: Vec<Vec<Tok>>,
    anchored_start: bool,
    anchored_end: bool,
}

enum Tok {
    Char(char),
    AnyDigit,
    Any,
    Class(Vec<char>),
    Star(Box<Tok>),
    Plus(Box<Tok>),
}

fn regex_lite(pattern: &str) -> RegexLite {
    let anchored_start = pattern.starts_with('^');
    let anchored_end = pattern.ends_with('$') && !pattern.ends_with("\\$");
    let body = &pattern[usize::from(anchored_start)..pattern.len() - usize::from(anchored_end)];
    let alternatives = body
        .split('|')
        .map(|alt| {
            let mut toks = Vec::new();
            let mut chars = alt.chars().peekable();
            while let Some(c) = chars.next() {
                let base = match c {
                    '\\' => match chars.next() {
                        Some('d') => Tok::AnyDigit,
                        Some(other) => Tok::Char(other),
                        None => Tok::Char('\\'),
                    },
                    '.' => Tok::Any,
                    '[' => {
                        let mut set = Vec::new();
                        for cc in chars.by_ref() {
                            if cc == ']' {
                                break;
                            }
                            set.push(cc);
                        }
                        Tok::Class(set)
                    }
                    other => Tok::Char(other),
                };
                match chars.peek() {
                    Some('*') => {
                        chars.next();
                        toks.push(Tok::Star(Box::new(base)));
                    }
                    Some('+') => {
                        chars.next();
                        toks.push(Tok::Plus(Box::new(base)));
                    }
                    _ => toks.push(base),
                }
            }
            toks
        })
        .collect();
    RegexLite {
        alternatives,
        anchored_start,
        anchored_end,
    }
}

impl RegexLite {
    fn is_match(&self, text: &str) -> bool {
        let joined = text; // single-row expectations only
        let chars: Vec<char> = joined.chars().collect();
        for alt in &self.alternatives {
            let starts: Vec<usize> = if self.anchored_start {
                vec![0]
            } else {
                (0..=chars.len()).collect()
            };
            for start in starts {
                if let Some(end) = match_seq(alt, &chars, start)
                    && (!self.anchored_end || end == chars.len())
                {
                    return true;
                }
            }
        }
        false
    }
}

fn match_seq(toks: &[Tok], chars: &[char], mut pos: usize) -> Option<usize> {
    if toks.is_empty() {
        return Some(pos);
    }
    let (head, tail) = toks.split_first().unwrap();
    match head {
        Tok::Star(inner) => {
            // Greedy with backtracking.
            let mut reach = vec![pos];
            while let Some(next) = match_one(inner, chars, *reach.last().unwrap()) {
                reach.push(next);
            }
            for &p in reach.iter().rev() {
                if let Some(end) = match_seq(tail, chars, p) {
                    return Some(end);
                }
            }
            None
        }
        Tok::Plus(inner) => {
            let first = match_one(inner, chars, pos)?;
            let mut reach = vec![first];
            while let Some(next) = match_one(inner, chars, *reach.last().unwrap()) {
                reach.push(next);
            }
            for &p in reach.iter().rev() {
                if let Some(end) = match_seq(tail, chars, p) {
                    return Some(end);
                }
            }
            None
        }
        other => {
            pos = match_one(other, chars, pos)?;
            match_seq(tail, chars, pos)
        }
    }
}

fn match_one(tok: &Tok, chars: &[char], pos: usize) -> Option<usize> {
    let c = *chars.get(pos)?;
    let ok = match tok {
        Tok::Char(want) => c == *want,
        Tok::AnyDigit => c.is_ascii_digit(),
        Tok::Any => true,
        Tok::Class(set) => set.contains(&c),
        Tok::Star(_) | Tok::Plus(_) => unreachable!("quantifiers handled in match_seq"),
    };
    ok.then_some(pos + 1)
}
