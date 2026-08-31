# PgSaci compatibility matrix

PgSaci is an Oracle TNS/TTC proxy that runs Oracle clients against stock
PostgreSQL with the `orafce` extension. It is not an Oracle database
implementation, and it is in very early development. See `README.md` for the
project overview; this file is the detailed per-area breakdown.

| Area | Status | Notes |
| --- | --- | --- |
| Connection/authentication | Supported | Full TNS/TTC handshake + auth for `python-oracledb` thin, the Oracle JDBC thin driver, ODP.NET managed (`Oracle.ManagedDataAccess.Core`), and oracle-rs — each verified end to end (connect, 12c PBKDF2 auth with mutual server proof, metadata, parametrised SELECT/DML, array binds, rollback, multi-thousand-row fetch loop) against both the 19c and 11g personas (`PGSACI_ORACLE_VERSION`). 11g clients that only speak O5LOGON (python-oracledb) get the MD5 verifier; the JDBC and ODP.NET 11g paths use 12c PBKDF2. ODP.NET needs three negotiation-phase quirks (DataTypes reply carries an 11-byte TZ blob + a UB2 type list; auth phases end on a `0x04` end-of-call rather than a `0x09` STATUS; `AUTH_CONNECT_STRING` is read with single-byte CLR chunks); the row/describe/fetch/end-of-call path is shared with the JDBC thin driver after advertising the FSAP capability bit. One Oracle session maps to one dedicated PostgreSQL connection. **Credential model:** a pre-declared `pg_user → pg_password` list (repeatable `--pg-user u:p`, `PGSACI_PG_USERS` comma list, `--pg-users-file` / `PGSACI_PG_USERS_FILE`, layered file < env < CLI) with an optional single fallback password (`--pg-password` / `PGSACI_PG_PASSWORD`). PgSaci matches the Oracle username case-insensitively, runs the login challenge *and* the backend PostgreSQL connection with that password, and rejects an unmatched user with no fallback as ORA-01017 — so an Oracle client authenticates with the same credentials it would use against PostgreSQL directly. |
| SQL SELECT/DML | Supported subset | The golden corpus covers joins, CTEs (incl. recursive), windows/analytics, `ROWNUM`, legacy `(+)`, `CONNECT BY` (incl. `NOCYCLE`, `CONNECT_BY_ISCYCLE/ISLEAF`), `MERGE` (incl. matched `DELETE`), `INSERT ALL`, `PIVOT`/`UNPIVOT`, sequences, and common `orafce` functions. |
| Result delivery | Supported | PostgreSQL `RowStream` results are delivered through Execute/Fetch batches without whole-result buffering; multi-packet results (>64 MiB) stream via the client fetch loop. `REEXECUTE` / `REEXECUTE_AND_FETCH` (a cached statement re-run with new binds on the same cursor — python-oracledb's default) are handled: the Oracle SQL and bind datatypes from the prior Execute are reused. |
| Scalar binds | Supported | Text, NUMBER, bytes, temporal values, Boolean, and binary floating input values are passed as PostgreSQL parameters, never interpolated, on both `EXECUTE` and `REEXECUTE`. |
| Array binds / batch DML | Supported | `executemany` (python-oracledb), `Statement.addBatch`/`executeBatch` (JDBC thin), and `OracleCommand.ArrayBindCount` (ODP.NET) run one statement over an N-row value matrix; PgSaci replays each row against the cached prepared statement inside the session transaction and returns the summed row count. |
| OUT binds | Partial | `RETURNING <cols> INTO :out` DML works for **python-oracledb thin** (INSERT/UPDATE, one or many affected rows): PgSaci strips the `INTO` clause, runs the plain `RETURNING`, and marshals the returned values back as OUT-bind data (IO-vector + returning row-data + return-parameters). ojdbc and ODP.NET frame OUT binds differently and get ORA-03001 rather than a silent drop. PL/SQL OUT parameters (`BEGIN :x := … END`) and `SYS_REFCURSOR` are not implemented. |
| DDL | Partial | Oracle types, views/CTAS (SELECT body translated), `ALTER TABLE` forms, `COMMENT ON`, physical-clause stripping, ordinary + `BITMAP`→plain indexes, **function-based / expression indexes** (`CREATE [UNIQUE] INDEX … ON t (UPPER(c))`, `(NVL(a,0)+NVL(b,0))` — non-trivial keys parenthesised for PostgreSQL), basic materialized views with explicit refresh, `CREATE SYNONYM`→view, and global/private temp tables → `TEMP … IF NOT EXISTS` work. Oracle-managed MV refresh policies and full storage semantics are partial. |
| Triggers | Partial | `CREATE [OR REPLACE] TRIGGER` with `BEFORE`/`AFTER`/`INSTEAD OF`, row/statement level, multi-event (`INSERT OR UPDATE`), `WHEN (...)`, `:NEW`/`:OLD`, `REFERENCING NEW AS x OLD AS y` (aliases resolved, clause dropped), `INSTEAD OF` on views, and bodies using column assignment, an audit `INSERT`, `RAISE_APPLICATION_ERROR`, `IF … END IF`, and numeric `FOR` loops lower to a PostgreSQL trigger function. Compound triggers and package-scale PL/SQL bodies do not. |
| Transactions & locking | Partial | Explicit transaction control, savepoints, statement recovery, Oracle-style implicit DDL commits, `SET TRANSACTION READ ONLY / ISOLATION LEVEL`, `SELECT … FOR UPDATE` (`OF <cols>` list dropped, `WAIT n` → block, `NOWAIT`/`SKIP LOCKED` passthrough), and `WHERE CURRENT OF <cursor>` inside a PL/SQL cursor loop work. XA and autonomous transactions do not. |
| Session/NLS | Partial | Current schema, time zone, selected NLS settings, and harmless optimizer settings are handled; full Oracle implicit conversion/NLS behavior is not. |
| Errors | Partial | ~35 PostgreSQL SQLSTATE classes map to Oracle error numbers, including deadlock, serialization failure, cancellation, lock-not-available, timeout, and connection failures. The PostgreSQL statement character position is carried into the TTC `error_pos` field (python-oracledb `error.offset`; oracle-rs discards it). |
| Types | Partial | **NUMBER** — value-exact; declared `NUMBER(p,s)` precision/scale is reported for python-oracledb thin and oracle-rs (ojdbc / ODP.NET keep `(38,0)` — OAC parser limit). **DATE** 7-byte; **TIMESTAMP** native 11-byte, sub-second preserved (python-oracledb thin / OCI thick; ojdbc and ODP.NET get Oracle `DATE`, second precision — OAC parser limit); **TIMESTAMP WITH TIME ZONE** native 13-byte (offset fixed at `+00:00` — PostgreSQL stores TIMESTAMPTZ as UTC; same ojdbc/ODP.NET `DATE` fallback). **BINARY_FLOAT / BINARY_DOUBLE** and **INTERVAL YEAR TO MONTH / DAY TO SECOND** declared columns decode natively for python-oracledb thin; other drivers get NUMBER / an Oracle-style interval text rendering. Computed `float8` (`POWER`, `AVG`, …) stays NUMBER, matching Oracle. text (UTF-8), bytea, Boolean → NUMBER(1), `ROWID` → `ctid` text. `NCHAR`/`NVARCHAR2` map to VARCHAR2 (UTF-8 exact). |
| LOBs | Partial | CLOB/BLOB values are inline (limit: one ~64 MiB TTC packet). `DBMS_LOB.GETLENGTH/SUBSTR/INSTR` provided as SQL functions. TTC LOB locators and multi-gigabyte streaming are not implemented. |
| PL/SQL | Partial | Anonymous blocks and standalone function/procedure bodies translate: `DECLARE` sections, `%TYPE` and `%ROWTYPE` variables, numeric `FOR` loops, `WHILE`/`LOOP … EXIT WHEN`, `IF`/`CASE` (expression **and** statement forms), nested blocks, `EXECUTE IMMEDIATE`, explicit named cursors (`CURSOR c IS …` → `c CURSOR FOR …`; `OPEN`/`FETCH … INTO`/`CLOSE`, `FOR r IN c LOOP`, `FOR r IN (SELECT …) LOOP`, `WHERE CURRENT OF c`), `EXCEPTION WHEN …` handlers (Oracle predefined names — `DUP_VAL_ON_INDEX`, `NO_DATA_FOUND`, `ZERO_DIVIDE`, `VALUE_ERROR`, `INVALID_NUMBER`, `TOO_MANY_ROWS`, `OTHERS` — are mapped), user exceptions declared with `PRAGMA EXCEPTION_INIT` (handlers rerouted to `raise_exception`), and `SELECT … INTO` (rewritten to `INTO STRICT` for Oracle's single-row semantics). `DBMS_OUTPUT.PUT_LINE` → `RAISE NOTICE`; other pragmas are dropped. Packages, `BULK COLLECT`/`FORALL`, and pipelined functions do not. |
| Operations | Supported | TCP keepalive + configurable idle reaping, per-statement timeout → ORA-01013, OCIBreak/Ctrl-C cancels the in-flight statement, graceful drain on SIGINT/SIGTERM, dependency-free `/healthz` + `/readyz`, per-session tracing spans. Plaintext transport only (client and PG links). |

## Not implemented — PostgreSQL + `orafce` has no equivalent

These are out of scope for the foreseeable future because stock PostgreSQL with
only `orafce` cannot express them:

- **Flashback** (`AS OF SCN|TIMESTAMP`, `VERSIONS BETWEEN`) — no time-travel storage.
- **`MODEL` clause** — no spreadsheet/array computation primitive.
- **Full PL/SQL packages** — package state, overloading, and the PL/SQL type
  system are a compiler-scale effort, not a translation.
- **`DBMS_SCHEDULER` / `DBMS_JOB`** — no in-core job scheduler (would need
  `pg_cron` or similar, which is a second extension).
- **Advanced Queueing (AQ)** — no equivalent messaging substrate.
- **XA / distributed transactions**, **DRCP**, **Application Continuity**,
  **network compression**, and **client-side result cache** — Oracle
  transport/session features with no PostgreSQL counterpart.
- **Character sets other than AL32UTF8/UTF-8** on the wire.

`DBMS_PIPE` / `DBMS_ALERT` are provided by `orafce` and route through unchanged.

## Not implemented for `0.0.1` — planned, needs more protocol work

These are expressible on stock PostgreSQL but need per-client TTC
reverse-engineering (and a test oracle PgSaci does not have in CI). They are
deferred, not ruled out:

- **PL/SQL OUT parameters** (`BEGIN :x := … END`) and **`SYS_REFCURSOR`** — need
  a `DO` block wrapped as a returning function, and a client-drivable cursor
  handle backed by a real PostgreSQL cursor. (`RETURNING … INTO` DML OUT binds
  work for python-oracledb thin.)
- **OUT binds / native `BINARY_FLOAT|DOUBLE` / native `INTERVAL` / `NUMBER(p,s)`
  metadata for ojdbc and ODP.NET** — python-oracledb thin has all of these; the
  ojdbc and ODP.NET describe and OUT-bind framing for the corresponding TTC
  types is not written, so those drivers get NUMBER / a text interval /
  `(38,0)` / ORA-03001. oracle-rs 0.1.7 mis-decodes result types 100/101 (its
  tests only cover the raw functions).
- **Native `TIMESTAMP` describe for ojdbc and ODP.NET** — their column-metadata
  parser desyncs on the native datetime descriptor (a non-zero scale field
  shifts its reads and overruns an 8-byte scratch buffer). So
  PgSaci describes PostgreSQL `timestamp` / `timestamptz` as Oracle **DATE**
  (type 12) to those two drivers. This is exact for an Oracle `DATE` column
  (the common case — the DDL translator turns `DATE` into `timestamp(0)`, and
  ojdbc maps Oracle `DATE` straight to `java.sql.Timestamp`). An Oracle
  `TIMESTAMP(n>0)` column queried over ojdbc/ODP.NET loses its sub-second
  fractional digits and reports `getColumnTypeName() == "DATE"`. python-oracledb
  thin and OCI thick keep the native `TIMESTAMP` (180) / `TIMESTAMP WITH TIME
  ZONE` (181) types with sub-second precision.
- **TTC LOB locators** (open/read/write/length/close) and single LOB values
  larger than one ~64 MiB packet. `DBMS_LOB.*` over inline CLOB/BLOB works.
- **Single-byte request-charset transcoding** (`WE8ISO8859P1`, `WE8MSWIN1252`) —
  PgSaci assumes AL32UTF8 end to end. The transcoding is bounded (mapping
  tables) but every available thin client is UTF-8-only, so it cannot be
  validated; shipping it unverified would risk silent data corruption.
- **OCI thick-client handshake** — the Instant Client loads but the TNS/TTC
  negotiation differs from the thin drivers (`ORA-12592`); it needs its own
  reverse-engineering pass.
- **External auth** (Kerberos / wallet / proxy auth), a **warm
  backend-connection cache**, and a **listener connection rate limit**.

The canonical, executable compatibility claim is the corpus:

```text
cargo test --test corpus -- --test-threads=1
```

It runs with no ignored cases. New behavior must add Oracle-correct corpus
coverage rather than weakening an expectation.
