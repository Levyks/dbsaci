# PgSaci

**Docs: <https://levyks.github.io/pgsaci/>**

PgSaci is a proxy that speaks Oracle's **TNS/TTC wire protocol** on the front and
**stock PostgreSQL** on the back. An unmodified Oracle client — `python-oracledb`
(thin), the Oracle JDBC thin driver, ODP.NET, OCI-based tools — points at PgSaci
instead of at an Oracle database, and PgSaci:

1. terminates the Oracle client handshake and authentication,
2. translates the Oracle SQL dialect it understands into PostgreSQL SQL,
3. runs the query against a normal PostgreSQL server that has the
   [`orafce`](https://github.com/orafce/orafce) extension installed,
4. re-encodes the PostgreSQL result back into Oracle's binary framing.

The application keeps its Oracle driver, its connection string shape, and most of
its SQL. Nothing about the database it actually talks to is Oracle.

> **Status: very early development. Mostly written by AI.** There are hundreds of
> passing end-to-end tests, but this has never run a real workload. **Do not
> point it at data you care about.** Bug reports, failing-case contributions, and
> design feedback are very welcome.

## How this differs from IvorySQL

[IvorySQL](https://github.com/IvorySQL/IvorySQL) is a *fork of the PostgreSQL
server* that builds Oracle SQL/PL-SQL compatibility — an Oracle parse mode,
PL/iSQL, packages, `NUMBER`/`DATE` semantics, `DUAL`, etc. — **into the database
engine**. It is far deeper than PgSaci on the *dialect*. But IvorySQL speaks the
**PostgreSQL wire protocol**: your application connects to it with a PostgreSQL
driver. An unmodified Oracle client (ojdbc, ODP.NET, `python-oracledb`, an
OCI-linked tool) cannot connect to IvorySQL at all.

PgSaci is the inverse trade. It speaks Oracle's **TNS/TTC wire protocol**, so the
Oracle client connects exactly as it would to Oracle — no driver swap, no
connection-string rewrite — while the database behind it is an ordinary,
unmodified PostgreSQL. The cost is coverage: PgSaci only supports the slice of
Oracle SQL/PL-SQL its translation layer plus `orafce` can express. Use IvorySQL
if you can repoint the application at a new database and want broad Oracle SQL
semantics; use PgSaci if the application **and its Oracle driver** have to stay
exactly as they are.

## Requirements

- **PostgreSQL** (recent release) with the **`orafce`** extension available
  (`CREATE EXTENSION orafce`). A ready-to-use image is built from
  [`testcontainers/Dockerfile`](testcontainers/Dockerfile).
- **Rust** (stable, 2024 edition) to build PgSaci.
- A login role on PostgreSQL for the proxy to connect as; one Oracle session maps
  to one dedicated PostgreSQL connection, so size `max_connections` (or put a
  session pooler behind PgSaci) accordingly.
- Transport is **plaintext** on both sides today. Run PgSaci behind a
  TLS-terminating boundary if you need encryption in transit.

## Running it

```bash
# build the Postgres+orafce test image once
docker build -t pgsaci-test-pg:18 testcontainers

# start it and note the mapped 5432 port
docker run -d -e POSTGRES_PASSWORD=postgres -P pgsaci-test-pg:18

# run the proxy (see src/bin/pgsaci.rs for every PGSACI_* variable)
PGSACI_LISTEN=0.0.0.0:1521 \
PGSACI_PG_HOST=127.0.0.1 PGSACI_PG_PORT=<mapped-port> \
PGSACI_PG_DB=postgres PGSACI_PG_PASSWORD=<role-password> \
cargo run --bin pgsaci
```

Then connect any Oracle client to `//host:1521/FREEPDB1`.

Or run it from a container (`levyks/pgsaci:0.0.7`, ~10 MB) —
`docker run -p 1521:1521 -e PGSACI_PG_HOST=… levyks/pgsaci:0.0.7`. See the
[docs](https://levyks.github.io/pgsaci/getting-started/) for a full
docker-compose.

Every option also has a CLI flag (`pgsaci --help`); the flag wins over the env var.

`PGSACI_ORACLE_VERSION` picks which release PgSaci claims to be — `19` (default) or
`11`. This changes the banner, `AUTH_VERSION_*`, and the auth verifier family so
that both modern and 11g-era clients negotiate successfully.

### Credentials (multi-user)

An Oracle login is a challenge/response — the password never crosses the wire —
so PgSaci must already hold each user's PostgreSQL password. Declare them up
front and an Oracle client then authenticates with the *same* user/password it
would use against PostgreSQL directly:

```bash
pgsaci \
  --pg-user alice:s3cret --pg-user bob:hunter2 \   # repeatable, CLI only
  --pg-users-file /etc/pgsaci/users              \ # or a file of user:password lines
  --pg-password postgres                           # fallback for anyone not listed
# env equivalents: PGSACI_PG_USERS="alice:s3cret,bob:hunter2",
#                  PGSACI_PG_USERS_FILE=..., PGSACI_PG_PASSWORD=...
```

Sources layer file &lt; `PGSACI_PG_USERS` &lt; `--pg-user`. The username is matched
case-insensitively; a user with no match and no fallback is rejected with
ORA-01017. The matched password drives both the login challenge and the backend
PostgreSQL connection.

### Schemas

Oracle's *schema == user*: on connect PgSaci ensures a PostgreSQL schema named
after the user and sets `search_path` to `"<user>", oracle, public`. Unqualified
names resolve in the user's own schema first, then in `public` (the shared
fallback, also reachable as `public.<name>` — so an existing PostgreSQL database
whose tables live in `public` works unchanged). Other schemas are reached by
qualifying (`SELECT * FROM hr.emp`, with `GRANT USAGE`/`SELECT`) or
`ALTER SESSION SET CURRENT_SCHEMA`. If the backend role can't `CREATE` a schema
the connection still works and logs a warning.

## What works

- **Real Oracle drivers connect and run a real session.** `python-oracledb`
  (thin), the Oracle JDBC thin driver, and ODP.NET managed
  (`Oracle.ManagedDataAccess.Core`) all pass an end-to-end probe — connect,
  auth (12c PBKDF2 with mutual server proof), metadata, parametrised `SELECT`,
  `INSERT` with row counts, `ROLLBACK`, a 2 500-row fetch loop, and array-bind
  batch DML (`executemany` / JDBC batch / ODP.NET `ArrayBindCount`) — against
  both the 19c and 11g personas.
- **Scalar and array binds** go across as real typed parameters, never string
  interpolation; a cached statement re-run with new binds (`REEXECUTE`) is
  handled. Results stream row-batch by row-batch, including results past 1M rows
  or one 64 MiB packet.
- **`SELECT ... FROM DUAL`**, `ROWNUM` (→ `LIMIT`), Oracle legacy `(+)` outer
  joins, `CONNECT BY` hierarchical queries (incl. `NOCYCLE`,
  `CONNECT_BY_ISCYCLE` / `_ISLEAF`), `MERGE` (incl. `WHEN MATCHED ... DELETE`),
  `INSERT ALL` / `INSERT FIRST`, `PIVOT` / `UNPIVOT`, analytic/window functions,
  recursive CTEs.
- **`NVL` / `DECODE` / `NVL2` / `SYSDATE` / `ADD_MONTHS` / `TO_CHAR` / `SUBSTR`**
  and the rest of the common Oracle scalar library — routed to `orafce`, not
  reimplemented.
- **Sequences** (`seq.NEXTVAL` / `.CURRVAL`), identity columns.
- **Transactions**: explicit control, `SAVEPOINT` / `ROLLBACK TO`, Oracle-style
  implicit commit on DDL, `SET TRANSACTION READ ONLY`, `SELECT ... FOR UPDATE`
  (`OF <cols>` dropped, `WAIT n` → blocking, `NOWAIT` / `SKIP LOCKED` passthrough).
- **DDL translation**: Oracle column types, `CREATE VIEW` / `CREATE TABLE AS`
  (the SELECT body is translated), many `ALTER TABLE` forms, `COMMENT ON`,
  `CREATE SYNONYM` → view, global/private temp tables → `TEMP ... IF NOT EXISTS`,
  ordinary and `BITMAP` (→ plain) indexes, basic materialized views with
  explicit refresh.
- **Simple triggers**: `CREATE [OR REPLACE] TRIGGER`, `BEFORE`/`AFTER`,
  row/statement, `WHEN (...)`, `:NEW`/`:OLD`, and bodies that do column
  assignment, an audit `INSERT`, or `RAISE_APPLICATION_ERROR` — lowered to a
  PostgreSQL trigger function.
- **Anonymous PL/SQL blocks** and basic standalone function/procedure bodies.
- **Errors**: ~40 PostgreSQL `SQLSTATE`s map to real `ORA-` numbers
  (deadlock, serialization failure, unique/foreign-key/not-null/check
  violations, lock timeout, statement cancellation, connection loss, …).
- **Operations**: TCP keepalive, idle-session reaping, per-statement timeout →
  `ORA-01013`, `OCIBreak` / Ctrl-C cancels the in-flight statement, graceful
  drain on `SIGINT`/`SIGTERM`, and dependency-free `/healthz` + `/readyz` +
  `/metrics` (Prometheus) endpoints.

## What does **not** work

Roughly most-likely-to-bite first.

- **Package-scale PL/SQL.** Packages (state, overloading, the PL/SQL type
  system), `BULK COLLECT` / `FORALL`, pipelined functions, and compound
  triggers. Anonymous blocks, standalone functions/procedures, and simple
  triggers translate — including `%TYPE`/`%ROWTYPE`, explicit cursors,
  `WHERE CURRENT OF`, statement `CASE`, `WHILE`/`LOOP`, exception handlers, and
  `PRAGMA EXCEPTION_INIT` — but the further a body strays from that, the more
  likely the translation is to be wrong rather than merely rejected.
- **OUT binds and `SYS_REFCURSOR`.** `RETURNING <cols> INTO :out` DML works for
  `python-oracledb` thin (ojdbc / ODP.NET get `ORA-03001`). PL/SQL OUT
  parameters (`BEGIN :x := … END`) and `SYS_REFCURSOR` are not implemented.
- **LOB streaming.** `CLOB`/`BLOB` values are delivered inline and capped at one
  ~64 MiB TTC packet. There are no TTC LOB locators and no multi-gigabyte
  streaming. `DBMS_LOB.GETLENGTH/SUBSTR/INSTR` exist as plain SQL functions.
- **Declared `NUMBER(p,s)` precision/scale in result metadata for ojdbc /
  ODP.NET.** Those two drivers see every `NUMBER` as `(38,0)` (their
  column-metadata parser desyncs on a non-zero scale field);
  `python-oracledb` thin and `oracle-rs` get the real precision/scale. Values
  are always exact.
- **Native TTC encodings for `INTERVAL`, `BINARY_FLOAT` / `BINARY_DOUBLE`** on
  result columns are complete for `python-oracledb` thin; other drivers get
  `NUMBER` / an Oracle-style interval text rendering.
- **Full Oracle implicit type conversion / NLS behaviour**, autonomous
  transactions.
- **Non-UTF-8 wire character sets.** The server side is AL32UTF8 only.
- **Structurally impossible on stock PostgreSQL + `orafce`** (out of scope):
  Flashback (`AS OF SCN|TIMESTAMP`, `VERSIONS BETWEEN`), the `MODEL` clause,
  `DBMS_SCHEDULER` / `DBMS_JOB`, Advanced Queueing, XA / distributed
  transactions, DRCP, Application Continuity, network compression, the
  client-side result cache. (`DBMS_PIPE` / `DBMS_ALERT` come from `orafce` and
  pass through.)

## How slow is this?

pgSaci sits in the path as an extra hop: it decodes the Oracle TNS/TTC frame,
translates the SQL, does **one** backend round trip to PostgreSQL, and re-frames
the answer. So per query it is slower than talking to Oracle directly — but both
are in the low-millisecond range on a laptop.

`bench/run.sh` runs an identical single-connection, single-thread micro-workload
(`python-oracledb` thin) against a real **Oracle XE 21c** container and against
**PostgreSQL 18 via pgSaci**, and prints this table. It measures *per-operation
latency*, i.e. the overhead pgSaci adds — not database throughput. See
`bench/README.md` for methodology and caveats.

<!-- BENCH:START -->
One sample run — 2 000 iterations/op (30 for the heavy ops), single connection.
Everything runs in Docker on one bridge network — the client, Oracle XE,
PostgreSQL and pgSaci — so every hop is a container veth with no host
port-proxy in the path. pgSaci runs from its published image
(`levyks/pgsaci:0.0.7`, a static musl build). Both database containers get
**2 CPU / 2.5 GiB**: Oracle XE spends its full 2 GiB licence (`INIT_SGA_SIZE`
1536M + `INIT_PGA_SIZE` 512M), PostgreSQL is **tuned to that envelope**
(`shared_buffers` 768 MB, 64 MB `work_mem`, parallel workers, `jit=off`). A
Windows laptop — re-run it on your own hardware, absolute numbers move a lot.

**Per-statement latency** — small ops; the wall-clock is the proxy overhead.

| operation | Oracle XE p50 | pgSaci p50 | pgSaci / Oracle |
| --- | ---: | ---: | ---: |
| `select_1_from_dual` | 0.11 ms | 0.58 ms | 5.2x |
| `point_select_by_pk` (1 bind) | 0.11 ms | 0.55 ms | 4.8x |
| `multi_bind_filter` (3 binds) | 0.21 ms | 0.94 ms | 4.5x |
| `range_scan_100_rows` | 0.16 ms | 0.60 ms | 3.9x |
| `insert_commit` | 1.55 ms | 1.95 ms | 1.3x |
| `update_commit` | 1.52 ms | 1.92 ms | 1.3x |
| `insert_then_rollback` | 1.56 ms | 0.67 ms | 0.4x |

pgSaci adds **~0.45 ms of fixed overhead per round trip** — a second hop
(client → pgSaci → PostgreSQL and back), plus SQL translation and re-encoding
the result into Oracle's wire format. It is a large *ratio* on the sub-0.2 ms
reads but still sub-millisecond in absolute terms. On the commit ops the WAL
fsync dominates and the ratio falls to ~1.3x. `insert + rollback` is *quicker*
via pgSaci — Oracle XE's redo/undo path for that pattern is heavier.

**Throughput** — scan / sort / aggregate / transfer over `bench_big` (100 000
rows); the wall-clock is dominated by the database engine, not the hop.

| operation | Oracle XE p50 | pgSaci p50 | pgSaci / Oracle |
| --- | ---: | ---: | ---: |
| `big_full_aggregate` (COUNT/SUM/AVG/MIN/MAX, `NUMBER` cols) | 2.8 ms | 9.2 ms | 3.3x |
| `big_scan_expr_count` (per-row `MOD` expr) | 15.3 ms | 21.1 ms | 1.4x |
| `big_group_by_50` (hash aggregate) | 8.8 ms | 14.3 ms | 1.6x |
| `big_window_sort` (full `ORDER BY` via window) | 15.8 ms | 33.1 ms | 2.1x |
| `big_fetch_25k_rows` (25 k rows across the wire) | 12.6 ms | 37.9 ms | 3.0x |
| `big_fetch_all_rows` (100 k rows across the wire) | 43.7 ms | 135.6 ms | 3.1x |
| `bulk_insert_5000` (`INSERT … SELECT` + commit) | 72 ms (p95 **2.9 s**) | 150 ms (p95 203 ms) | 2.1x |

The `big_fetch_*` ops are ~3x — that is the proxy decoding every row off the
PostgreSQL wire and re-encoding it onto the Oracle wire, which is structural for
a translating proxy. The pure-engine ops (aggregate / sort / group-by) mostly
reflect Oracle `NUMBER` → PostgreSQL `numeric` arithmetic (software decimal,
slower than `bigint` / `double precision`); even with the tuned config and
parallel query enabled, `big_full_aggregate` stays ~3x because the query is too
short to parallelise. Integer columns or more cores narrow it. The bulk write is
~2x slower on p50 but far **steadier**: Oracle XE's p95 is ~2.9 s
(redo-log-switch stalls) versus pgSaci's ~200 ms.

**This is a single-connection latency benchmark** and says nothing about
concurrency, mixed OLTP, or a tuned deployment — the areas where PostgreSQL
usually shines. The shipped image is a musl build (no custom allocator), which
costs a slice of the tiny-op latency.
<!-- BENCH:END -->

## Tests

The executable compatibility claim is the golden corpus — one real
PostgreSQL/`orafce` container and one real PgSaci proxy, every case driven over
TNS, asserting Oracle-correct values, row counts and error text (not merely "did
not error"):

```bash
docker build -t pgsaci-test-pg:18 testcontainers     # first time only
cargo test --test corpus -- --test-threads=1
```

Nothing is ignored except features with a hard PostgreSQL version floor on
older backends (only `MERGE`, which needs PG 15, marked `# requires-pg: 15`).
New behaviour must add Oracle-correct corpus coverage rather than weaken an
expectation.

CI runs the corpus against **PostgreSQL 18, 16 and 13** — build the image for a
different major with `docker build --build-arg PG_VERSION=16 …` and point the
tests at it with `PGSACI_TEST_PG_IMAGE=pgsaci-test-pg:16`.

Other suites:

- `cargo test --lib` — fast unit tests (auth crypto vectors, NUMBER codec,
  translator), no container.
- `cargo test --test translate_golden` — pure `oracle_to_postgres` string→string
  goldens, no container.
- `clients/run.sh <python|java|dotnet> [11]` — end-to-end probe with a real
  third-party Oracle driver (`python-oracledb` thin, ojdbc thin, ODP.NET
  managed on .NET 10). CI runs all three against both the 19c and 11g personas.
- `bench/run.sh` — the latency microbenchmark behind *How slow is this?* above.

## License

[WTFPL](LICENSE) — Do What The Fuck You Want To Public License, v2. Use it,
fork it, ship it, sell it; no attribution required. (See *Legal / trademarks*
below — that covers Oracle's marks and drivers, not this code.)

## Legal / trademarks

PgSaci is an independent, clean-room implementation of a wire-compatible proxy.
It is **not affiliated with, endorsed by, or sponsored by Oracle Corporation.**
"Oracle", "TNS", "OCI", "JDBC", and related marks are trademarks of Oracle
Corporation and/or its affiliates, used here only descriptively to state what
PgSaci is compatible with.

- No Oracle software is redistributed with this repository. The JDBC thin
  driver, ODP.NET, and Instant Client used by the compatibility probes are
  downloaded from Oracle by `clients/run.sh` / NuGet under Oracle's own licence
  terms, which each user accepts directly.
- Compatibility is derived from the observable wire protocol and public
  documentation. Contributions must be derived only from those sources.
- This is not legal advice; anyone shipping this in a product or company should
  get their own review.
