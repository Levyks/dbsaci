---
title: What works
description: The Oracle surface dbSaci covers today, verified by an end-to-end golden corpus.
---

Everything below has golden-corpus coverage. See the
[Compatibility matrix](/dbsaci/compatibility/) for per-area status and
[Limitations](/dbsaci/limitations/) for what is missing.

## Drivers & sessions

- `python-oracledb` (thin), the Oracle JDBC thin driver, and ODP.NET managed
  (`Oracle.ManagedDataAccess.Core`) each pass an end-to-end probe — connect, auth
  (12c PBKDF2 with mutual server proof), metadata, parametrised `SELECT`,
  `INSERT` with row counts, `ROLLBACK`, a 2 500-row fetch loop, and array-bind
  batch DML (`executemany` / JDBC batch / ODP.NET `ArrayBindCount`) — against
  both the 19c and 11g personas, against **PostgreSQL and MariaDB**.
  `NUMBER(p,s)` precision/scale is reported for python-oracledb thin and
  ojdbc and ODP.NET (real precision/scale; TIMESTAMP scale included).
- The **OCI thick client** (Instant Client, via `python-oracledb` thick mode)
  runs the golden corpus too: 634 / 637, the 3 skips being cases Oracle itself
  rejects over that transport (`ORA-01036` ×2, `ORA-00925`). It uses its own TTC
  dialect — little-endian fixed-width integers — which dbSaci emits from the
  negotiated `TNS_CCAP_OCI1` capability. Not run in CI (needs a licensed Instant
  Client).
- **Scalar and array binds** go across as backend parameters (up to 65,535 per
  statement). A cached statement re-run with new binds (`REEXECUTE`) is
  handled, including an ojdbc `PreparedStatement` re-execute. Requests larger
  than 8 KB (big `IN` lists, wide batches) are reassembled. Results stream
  batch by batch on both backends, including results past 1M rows or one
  64 MiB packet. On MariaDB, array DML runs as multi-row `INSERT`s (10,000
  rows in about 0.6 s in a debug build).
- **Describe metadata on MariaDB**: real precision/scale and nullability,
  blank-padded `CHAR`, `TIMESTAMP` microseconds kept through binds, and an
  unconstrained `NUMBER` described as precision 0 / scale -127. A bare `NUMBER`
  column is created as `DECIMAL(65,30)`, `NUMBER(p,s)` as `DECIMAL(p,s)`.

- `SELECT ... FROM DUAL`, `ROWNUM` (→ `LIMIT`), `FETCH FIRST n ROWS ONLY`,
  Oracle legacy `(+)` outer joins, `CONNECT BY` hierarchical queries
  (`NOCYCLE`, `CONNECT_BY_ISCYCLE` / `_ISLEAF` / `_ROOT`, `SYS_CONNECT_BY_PATH`),
  `MERGE` (incl. `WHEN MATCHED ... DELETE`), `INSERT ALL` / `INSERT FIRST`,
  `PIVOT` / `UNPIVOT`, analytic/window functions, recursive CTEs.
- `NVL` / `DECODE` / `NVL2` / `SYSDATE` / `ADD_MONTHS` / `TO_CHAR` / `SUBSTR` and
  the rest of the common Oracle scalar library, via `orafce`.
- `REGEXP_LIKE` / `REGEXP_REPLACE` (incl. back-references) / `REGEXP_SUBSTR`
  (nth match, capture group) / `REGEXP_INSTR` / `REGEXP_COUNT`.
- **Sequences** (`seq.NEXTVAL` / `.CURRVAL`), identity columns.
- `SYS_CONTEXT('USERENV', …)`, `USER`, `UID`.
- **Session time zone** — `ALTER SESSION SET TIME_ZONE` (IANA region or fixed
  offset), `SESSIONTIMEZONE` / `DBTIMEZONE`; `TIMESTAMPTZ` values are converted
  to the session zone on the wire, DST-correct for named regions.

## Data dictionary

Read-only catalog views the tooling and ORMs probe, returning UPPERCASE names
like a real Oracle database: `USER_*` / `ALL_*` for tables, columns, objects,
constraints, indexes, sequences, comments, triggers, users; `V$VERSION`;
`NLS_SESSION_PARAMETERS` (reflects the session's `NLS_*` settings).

An IDE schema browser — DataGrip / IntelliJ was the reference — introspects the
tree end to end against these, including `DBMS_METADATA.GET_DDL` for the "copy
DDL" action (rendered as PostgreSQL DDL). Objects appear under the connected
user's node: dbSaci gives each user a PostgreSQL schema of its own name
(Oracle's "schema == user"), and `ALL_*` / `USER_*` report honest owners
against it.

On **MariaDB** the dictionary — including `SYS_CONTEXT`, `DBMS_OUTPUT`,
`DBMS_METADATA.GET_DDL` and a few Oracle functions MariaDB lacks — is installed
once per server in the `dbsaci_sys` database (`dbsaci install`). The views run
with the caller's rights, so `ALL_TABLES` shows what that account can see;
nothing is created in application schemas.

## Schemas

Each user owns a schema of its own name; unqualified names resolve there first,
then in `public` (the shared fallback, also reachable as `public.<name>`).
Cross-schema access is a qualified name plus the usual grants
(`SELECT * FROM hr.employees`); `ALTER SESSION SET CURRENT_SCHEMA` redirects
unqualified resolution. On **MariaDB** the login `APPUSER` starts in the
database `APPUSER` (otherwise in `--db-name`), and Oracle's
identifier rules — unquoted upper-cased, quoted exact — apply to every name
with any `lower_case_table_names`.

## Transactions

Explicit control, `SAVEPOINT` / `ROLLBACK TO`, Oracle-style implicit commit on
DDL, `SET TRANSACTION READ ONLY`, `SELECT ... FOR UPDATE` (`OF <cols>` dropped,
`WAIT n` → blocking, `NOWAIT` / `SKIP LOCKED` passthrough). The driver's
autocommit (JDBC/HikariCP default, `connection.autocommit = True`) commits after
each call on both backends.

On **MariaDB**, Oracle's model: every statement sees what was committed before
it began (`READ COMMITTED`, no stale snapshots in pooled sessions), a clean
logoff commits, cursors stay fetchable across `COMMIT`,
`SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` gives a transaction snapshot
(`ORA-01453` if it is not the first statement), and a lost backend connection
with uncommitted work ends the session with `ORA-03113` rather than silently
reconnecting. `ALTER SESSION` state is session state and survives reconnects.

## DDL

Oracle column types, `CREATE VIEW` / `CREATE TABLE AS` (the SELECT body is
translated), many `ALTER TABLE` forms, `COMMENT ON`, `CREATE SYNONYM` → view,
global/private temp tables → `TEMP ... IF NOT EXISTS`, ordinary and `BITMAP`
(→ plain) indexes, function-based / expression indexes, basic materialized views
with explicit refresh.

## Triggers

`CREATE [OR REPLACE] TRIGGER`, `BEFORE` / `AFTER` / `INSTEAD OF`, row/statement
level, multi-event (`INSERT OR UPDATE`), `WHEN (...)`, `:NEW` / `:OLD`,
`REFERENCING NEW AS x`, and bodies that do column assignment, an audit `INSERT`,
`RAISE_APPLICATION_ERROR`, `IF … END IF`, or numeric `FOR` loops — lowered to a
PostgreSQL trigger function.

## PL/SQL (the smaller end)

Anonymous blocks and standalone function/procedure bodies translate on
**PostgreSQL** — including `%TYPE` / `%ROWTYPE`, explicit named cursors,
`WHERE CURRENT OF`, expression and statement `CASE`, `WHILE` / `LOOP`,
`EXECUTE IMMEDIATE`, `EXCEPTION WHEN …` handlers (Oracle predefined names
mapped), user exceptions via `PRAGMA EXCEPTION_INIT`, and `SELECT … INTO`
(single-row `INTO STRICT` semantics). `DBMS_OUTPUT.PUT_LINE` → `RAISE NOTICE`.
On **MariaDB**, anonymous blocks and simple routines run where Oracle mode
accepts them; `ROWID`, `WHERE CURRENT OF`, autonomous transactions, and
`EXCEPTION_INIT` are not emulated (no column-named-`id` or drop-the-pragma
shims). Interval result columns stay text on MariaDB (no type 182/183).

## Errors

On MariaDB, errors map by errno — `ORA-00001`, `02291`, `02292`,
`01400` / `01407`, `00060`, `00054` / `30006`, `12899`, `01438`, `01476`,
`01722`, `01858`, `00942`, `00904`, `01013`, and more. On PostgreSQL,
~40 `SQLSTATE`s map to real `ORA-` numbers — deadlock, serialization
failure, unique / foreign-key / not-null / check violations, `NOWAIT`
resource-busy, statement cancellation, connection loss, invalid number/date,
single-row subquery returning many rows, and more. The PostgreSQL statement
character position is carried into the TTC `error_pos` field (`python-oracledb`
`error.offset`).

## Operations

TCP keepalive, idle-session reaping, `--max-sessions`, per-call timeout →
`ORA-01013` (on MariaDB via `KILL QUERY`, covering streaming results and stored
programs),
`OCIBreak` / Ctrl-C cancels the in-flight statement, graceful drain on
`SIGINT` / `SIGTERM`, and dependency-free `/healthz` + `/readyz` + `/metrics`
(Prometheus) endpoints.
