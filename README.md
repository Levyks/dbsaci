# DbSaci

**Docs: <https://levyks.github.io/dbsaci/>**

DbSaci is a proxy that speaks Oracle's **TNS/TTC wire protocol** on the front and
connects to **PostgreSQL or MariaDB** on the back. An unmodified Oracle client — `python-oracledb`
(thin), the Oracle JDBC thin driver, ODP.NET, OCI-based tools — points at DbSaci
instead of at an Oracle database, and DbSaci:

1. terminates the Oracle client handshake and authentication,
2. translates the Oracle SQL dialect it understands into backend SQL,
3. runs the query against PostgreSQL + [`orafce`](https://github.com/orafce/orafce)
   or MariaDB 11.4 in `SQL_MODE=ORACLE`,
4. re-encodes the backend result back into Oracle's binary framing.

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
engine**. It is far deeper than DbSaci on the *dialect*. But IvorySQL speaks the
**PostgreSQL wire protocol**: your application connects to it with a PostgreSQL
driver. An unmodified Oracle client (ojdbc, ODP.NET, `python-oracledb`, an
OCI-linked tool) cannot connect to IvorySQL at all.

DbSaci is the inverse trade. It speaks Oracle's **TNS/TTC wire protocol**, so the
Oracle client connects exactly as it would to Oracle — no driver swap, no
connection-string rewrite — while the database behind it is an ordinary,
unmodified PostgreSQL or MariaDB. The cost is coverage: DbSaci only supports
the slice of Oracle SQL/PL-SQL its translation layer plus backend compatibility
facilities can express. Use IvorySQL
if you can repoint the application at a new database and want broad Oracle SQL
semantics; use DbSaci if the application **and its Oracle driver** have to stay
exactly as they are.

## Requirements

- **PostgreSQL** (recent release) with the **`orafce`** extension available
  (`CREATE EXTENSION orafce`), or **MariaDB 11.4+**. DbSaci sets
  `SQL_MODE=ORACLE` on every backend session itself and folds table identifiers
  to **upper** case by default (`--identifier-case upper|lower`), so the MariaDB
  server needs no special `lower_case_table_names` setting. A PostgreSQL test
  image is built from `testcontainers/Dockerfile`.
- **Rust** (stable, 2024 edition) to build DbSaci.
- A login role on the selected backend for the proxy to connect as; one Oracle
  session maps to one dedicated backend connection, so size the backend's
  connection capacity accordingly.
- Transport defaults to **plaintext**. Enable TCPS on the TNS listener with
  `--tls-cert` / `--tls-key` (`DBSACI_TLS_CERT` / `DBSACI_TLS_KEY`) and require
  TLS to the backend with `--db-ssl` (`DBSACI_DB_SSL`). A TLS-terminating
  boundary in front of a plaintext listener is still valid.

## Running it

```bash
# build the Postgres+orafce test image once
docker build -t dbsaci-test-pg:18 testcontainers

# start it and note the mapped 5432 port
docker run -d -e POSTGRES_PASSWORD=postgres -P dbsaci-test-pg:18

# run the proxy (see src/bin/dbsaci.rs for every DBSACI_* variable)
DBSACI_LISTEN=0.0.0.0:1521 \
DBSACI_DB_HOST=127.0.0.1 DBSACI_DB_PORT=<mapped-port> \
DBSACI_DB_NAME=postgres DBSACI_DB_PASSWORD=<role-password> \
cargo run --bin dbsaci
```

For MariaDB, set `DBSACI_BACKEND=mariadb` and point the same options at it
(no server-side `sql-mode` or `lower-case-table-names` needed):

```bash
docker run -d --name dbsaci-mariadb \
  -e MARIADB_ROOT_PASSWORD=root \
  -e MARIADB_DATABASE=appdb -e MARIADB_USER=appuser -e MARIADB_PASSWORD=apppw \
  -p 3306:3306 mariadb:11.4

DBSACI_BACKEND=mariadb \
DBSACI_DB_HOST=127.0.0.1 DBSACI_DB_PORT=3306 \
DBSACI_DB_NAME=appdb DBSACI_DB_PASSWORD=apppw \
cargo run --bin dbsaci
```

See the [compatibility matrix](https://levyks.github.io/dbsaci/compatibility/)
for the backend-by-backend feature status.

Then connect any Oracle client to `//host:1521/FREEPDB1`.

Or run it from a container (`levyks/dbsaci:0.2.0`) —
`docker run -p 1521:1521 -e DBSACI_DB_HOST=… levyks/dbsaci:0.2.0`. See the
[docs](https://levyks.github.io/dbsaci/getting-started/) for a full
docker-compose.

Every option also has a CLI flag (`dbsaci --help`); the flag wins over the env var.

`DBSACI_ORACLE_VERSION` picks which release DbSaci claims to be — `19` (default) or
`11`. This changes the banner, `AUTH_VERSION_*`, and the auth verifier family so
that both modern and 11g-era clients negotiate successfully.

### Credentials (multi-user)

An Oracle login is a challenge/response — the password never crosses the wire —
so DbSaci must already hold each user's backend password. Declare them up
front and an Oracle client then authenticates with the *same* user/password it
would use against the backend directly:

```bash
dbsaci \
  --db-user alice:s3cret --db-user bob:hunter2 \   # repeatable, CLI only
  --db-users-file /etc/dbsaci/users              \ # or a file of user:password lines
  --db-password postgres                           # fallback for anyone not listed
# env equivalents: DBSACI_DB_USERS="alice:s3cret,bob:hunter2",
#                  DBSACI_DB_USERS_FILE=..., DBSACI_DB_PASSWORD=...
# There is no built-in default password. Unknown users are ORA-01017
# unless --db-password / a user list entry matches.
```

Sources layer file &lt; `DBSACI_DB_USERS` &lt; `--db-user`. The username is matched
case-insensitively; a user with no match and no fallback is rejected with
ORA-01017. The matched password drives both the login challenge and the backend
connection.

### Schemas

Oracle's *schema == user*.

* **PostgreSQL** — on connect DbSaci ensures a schema named after the user and
  sets `search_path` to `"<user>", oracle, public`. Unqualified names resolve in
  the user's own schema first, then in `public` (the shared fallback, also
  reachable as `public.<name>` — so an existing database whose tables live in
  `public` works unchanged). Other schemas are reached by qualifying
  (`SELECT * FROM hr.emp`, with `GRANT USAGE`/`SELECT`) or
  `ALTER SESSION SET CURRENT_SCHEMA`. If the role can't `CREATE` a schema the
  connection still works and logs a warning.
* **MariaDB** — a database is the schema, and `USE` selects exactly one (there is
  no `search_path`). DbSaci issues `USE <user>` when a database of that name
  exists, otherwise `USE <DBSACI_DB_NAME>`. So: give each Oracle login its own
  database, or point every login at one shared `DBSACI_DB_NAME` and qualify
  cross-schema references. `ALTER SESSION SET CURRENT_SCHEMA = x` maps to
  `USE x`.

### Identifiers and collation (MariaDB)

Identifiers in table-name position are folded to one case — via `sqlparser`'s
relation visitor, falling back to a text scan only for syntax it cannot
represent (anonymous PL/SQL blocks, some DDL bodies) — so `FROM MY_TABLE` /
`FROM my_table` / `FROM "MY_TABLE"` all resolve the same backend object no
matter how `lower_case_table_names` is set on the server. `--identifier-case
upper|lower` / `DBSACI_IDENTIFIER_CASE` picks the direction; **`upper` is the
default**, matching Oracle's own unquoted-identifier behaviour (and how a
vendored `data.sql`-style schema is already spelled) — author the MariaDB
schema in that case, or pass `lower` to match the PostgreSQL/MariaDB
convention instead. Genuinely mixed-case quoted identifiers (`"MixedCase"`,
`` `MixedCase` ``) are left exactly as written — the deliberate case-sensitive
escape hatch — and `DUAL` is never touched. This has no effect on the
PostgreSQL backend, which folds unquoted identifiers to lower case itself.

The session's `collation_connection` is pinned to the current schema's default
collation so string literals in client SQL aggregate cleanly with the schema's
columns (no `ER_CANT_AGGREGATE_2COLLATIONS` against `utf8mb4_uca1400_ai_ci`).

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
- **Anonymous PL/SQL blocks** and basic standalone function/procedure bodies
  (PostgreSQL lane is richer: `%TYPE`/`%ROWTYPE`, explicit cursors,
  `WHERE CURRENT OF`, `PRAGMA EXCEPTION_INIT` — see the compatibility matrix).
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
  triggers. On **MariaDB**, also no honest `ROWID`, autonomous transactions,
  `WHERE CURRENT OF`, or `PRAGMA EXCEPTION_INIT` (those are not faked with
  column-named-`id` shims). Anonymous blocks and simple routines still run
  where MariaDB Oracle mode accepts them.
- **OUT binds and `SYS_REFCURSOR`.** `RETURNING <cols> INTO :out` DML works for
  `python-oracledb` thin (ojdbc / ODP.NET get `ORA-03001`). PL/SQL OUT
  parameters (`BEGIN :x := … END`) and `SYS_REFCURSOR` are not implemented.
- **LOB streaming.** `CLOB`/`BLOB` values are delivered inline and capped at one
  ~64 MiB TTC packet. There are no TTC LOB locators and no multi-gigabyte
  streaming. `DBMS_LOB.GETLENGTH/SUBSTR/INSTR` exist as plain SQL functions.
- **Native TTC encodings for `INTERVAL`** on MariaDB result columns (text only
  today; PostgreSQL + python-oracledb thin get types 182/183). `BINARY_FLOAT` /
  `BINARY_DOUBLE` native wire is complete for python-oracledb thin.
  `NUMBER(p,s)` and `TIMESTAMP` describe are reported for ojdbc / ODP.NET from
  0.2.0.
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

dbSaci sits in the path as an extra hop: it decodes the Oracle TNS/TTC frame,
translates the SQL, does **one** backend round trip to PostgreSQL, and re-frames
the answer. So per query it is slower than talking to Oracle directly — but both
are in the low-millisecond range on a laptop.

`bench/run.sh` runs an identical single-connection, single-thread micro-workload
(`python-oracledb` thin) against a real **Oracle XE 21c** container and against
**PostgreSQL 18 via dbSaci**, and prints this table. It measures *per-operation
latency*, i.e. the overhead dbSaci adds — not database throughput. See
`bench/README.md` for methodology and caveats.

<!-- BENCH:START -->
One sample run — 2 000 iterations/op (30 for the heavy ops), single connection.
Everything runs in Docker on one bridge network — the client, Oracle XE,
PostgreSQL and dbSaci — so every hop is a container veth with no host
port-proxy in the path. dbSaci runs from its published image
(`levyks/dbsaci:0.2.0`, a distroless glibc build). Both database containers get
**2 CPU / 2.5 GiB**: Oracle XE spends its full 2 GiB licence (`INIT_SGA_SIZE`
1536M + `INIT_PGA_SIZE` 512M), PostgreSQL is **tuned to that envelope**
(`shared_buffers` 768 MB, 64 MB `work_mem`, parallel workers, `jit=off`). A
Windows laptop — re-run it on your own hardware, absolute numbers move a lot.

**Per-statement latency** — small ops; the wall-clock is the proxy overhead.

| operation | Oracle XE p50 | dbSaci p50 | dbSaci / Oracle |
| --- | ---: | ---: | ---: |
| `select_1_from_dual` | 0.11 ms | 0.58 ms | 5.2x |
| `point_select_by_pk` (1 bind) | 0.11 ms | 0.55 ms | 4.8x |
| `multi_bind_filter` (3 binds) | 0.21 ms | 0.94 ms | 4.5x |
| `range_scan_100_rows` | 0.16 ms | 0.60 ms | 3.9x |
| `insert_commit` | 1.55 ms | 1.95 ms | 1.3x |
| `update_commit` | 1.52 ms | 1.92 ms | 1.3x |
| `insert_then_rollback` | 1.56 ms | 0.67 ms | 0.4x |

dbSaci adds **~0.45 ms of fixed overhead per round trip** — a second hop
(client → dbSaci → PostgreSQL and back), plus SQL translation and re-encoding
the result into Oracle's wire format. It is a large *ratio* on the sub-0.2 ms
reads but still sub-millisecond in absolute terms. On the commit ops the WAL
fsync dominates and the ratio falls to ~1.3x. `insert + rollback` is *quicker*
via dbSaci — Oracle XE's redo/undo path for that pattern is heavier.

**Throughput** — scan / sort / aggregate / transfer over `bench_big` (100 000
rows); the wall-clock is dominated by the database engine, not the hop.

| operation | Oracle XE p50 | dbSaci p50 | dbSaci / Oracle |
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
(redo-log-switch stalls) versus dbSaci's ~200 ms.

**This is a single-connection latency benchmark** and says nothing about
concurrency, mixed OLTP, or a tuned deployment — the areas where PostgreSQL
usually shines. The shipped image is a plain glibc build (no custom allocator), which
costs a slice of the tiny-op latency.
<!-- BENCH:END -->

## Tests

The executable compatibility claim is the golden corpus — one real
PostgreSQL/`orafce` container and one real DbSaci proxy, every case driven over
TNS, asserting Oracle-correct values, row counts and error text (not merely "did
not error"):

```bash
docker build -t dbsaci-test-pg:18 testcontainers     # first time only
cargo test --test corpus -- --test-threads=1
```

Nothing is ignored except features with a hard PostgreSQL version floor on
older backends (only `MERGE`, which needs PG 15, marked `# requires-pg: 15`).
New behaviour must add Oracle-correct corpus coverage rather than weaken an
expectation.

CI runs the corpus against **PostgreSQL 18, 16 and 13** — build the image for a
different major with `docker build --build-arg PG_VERSION=16 …` and point the
tests at it with `DBSACI_TEST_PG_IMAGE=dbsaci-test-pg:16`.

Other suites:

- `cargo test --lib` — fast unit tests (auth crypto vectors, NUMBER codec,
  translator), no container.
- `cargo test --test translate_golden` — `oracle_to_postgres` and
  `oracle_to_mariadb` string→string goldens, no container.
- `clients/run.sh <python|java|dotnet> [11]` — end-to-end probe with a real
  third-party Oracle driver (`python-oracledb` thin, ojdbc thin, ODP.NET
  managed on .NET 10). Probes always send Oracle SQL;
  `DBSACI_CLIENT_BACKEND` only selects the container. Known reds live in
  `clients/expected-failures`; CI sets `DBSACI_CLIENT_LEDGER=1`. Matrix:
  `{python,java,dotnet}` × `{postgres,mariadb}` × `{19c,11g}`.
- Corpus known reds: `tests/corpus/expected-failures.<backend>`; CI sets
  `DBSACI_CORPUS_LEDGER=1` so the job stays green iff the failure set matches.
- `bench/run.sh` — the latency microbenchmark behind *How slow is this?* above.

## License

[Apache-2.0](LICENSE). (See *Legal / trademarks* below — that covers Oracle's
marks and drivers, not this code.)

## Legal / trademarks

DbSaci is an independent, clean-room implementation of a wire-compatible proxy.
It is **not affiliated with, endorsed by, or sponsored by Oracle Corporation.**
"Oracle", "TNS", "OCI", "JDBC", and related marks are trademarks of Oracle
Corporation and/or its affiliates, used here only descriptively to state what
DbSaci is compatible with.

- No Oracle software is redistributed with this repository. The JDBC thin
  driver, ODP.NET, and Instant Client used by the compatibility probes are
  downloaded from Oracle by `clients/run.sh` / NuGet under Oracle's own licence
  terms, which each user accepts directly.
- Compatibility is derived from the observable wire protocol and public
  documentation. Contributions must be derived only from those sources.
- This is not legal advice; anyone shipping this in a product or company should
  get their own review.
