---
title: What works
description: The Oracle surface pgSaci covers today, verified by an end-to-end golden corpus.
---

Everything below has golden-corpus coverage. See the
[Compatibility matrix](/pgsaci/compatibility/) for per-area status and
[Limitations](/pgsaci/limitations/) for what is missing.

## Drivers & sessions

- `python-oracledb` (thin), the Oracle JDBC thin driver, and ODP.NET managed
  (`Oracle.ManagedDataAccess.Core`) each pass an end-to-end probe — connect, auth
  (12c PBKDF2 with mutual server proof), metadata, parametrised `SELECT`,
  `INSERT` with row counts, `ROLLBACK`, a 2 500-row fetch loop, and array-bind
  batch DML (`executemany` / JDBC batch / ODP.NET `ArrayBindCount`) — against
  both the 19c and 11g personas.
- The **OCI thick client** (Instant Client, via `python-oracledb` thick mode)
  runs the golden corpus too: 634 / 637, the 3 skips being cases Oracle itself
  rejects over that transport (`ORA-01036` ×2, `ORA-00925`). It uses its own TTC
  dialect — little-endian fixed-width integers — which pgSaci emits from the
  negotiated `TNS_CCAP_OCI1` capability. Not run in CI (needs a licensed Instant
  Client).
- **Scalar and array binds** go across as PostgreSQL parameters. A cached
  statement re-run with new binds (`REEXECUTE`) is handled. Results stream
  batch by batch, including results past 1M rows or one 64 MiB packet.

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

## Data dictionary

Read-only catalog views the tooling and ORMs probe, returning UPPERCASE names
like a real Oracle database: `USER_*` / `ALL_*` for tables, columns, objects,
constraints, indexes, sequences, comments, triggers, users; `V$VERSION`;
`NLS_SESSION_PARAMETERS` (reflects the session's `NLS_*` settings).

An IDE schema browser — DataGrip / IntelliJ was the reference — introspects the
tree end to end against these, including `DBMS_METADATA.GET_DDL` for the "copy
DDL" action (rendered as PostgreSQL DDL). Objects appear under the connected
user's node: pgSaci gives each user a PostgreSQL schema of its own name
(Oracle's "schema == user"), and `ALL_*` / `USER_*` report honest owners
against it.

## Schemas

Each user owns a schema of its own name; unqualified names resolve there first,
then in `public` (the shared fallback, also reachable as `public.<name>`).
Cross-schema access is a qualified name plus the usual grants
(`SELECT * FROM hr.employees`); `ALTER SESSION SET CURRENT_SCHEMA` redirects
unqualified resolution.

## Transactions

Explicit control, `SAVEPOINT` / `ROLLBACK TO`, Oracle-style implicit commit on
DDL, `SET TRANSACTION READ ONLY`, `SELECT ... FOR UPDATE` (`OF <cols>` dropped,
`WAIT n` → blocking, `NOWAIT` / `SKIP LOCKED` passthrough).

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

Anonymous blocks and standalone function/procedure bodies translate — including
`%TYPE` / `%ROWTYPE`, explicit named cursors, `WHERE CURRENT OF`, expression and
statement `CASE`, `WHILE` / `LOOP`, `EXECUTE IMMEDIATE`, `EXCEPTION WHEN …`
handlers (Oracle predefined names mapped), user exceptions via
`PRAGMA EXCEPTION_INIT`, and `SELECT … INTO` (single-row `INTO STRICT`
semantics). `DBMS_OUTPUT.PUT_LINE` → `RAISE NOTICE`.

## Errors

~40 PostgreSQL `SQLSTATE`s map to real `ORA-` numbers — deadlock, serialization
failure, unique / foreign-key / not-null / check violations, `NOWAIT`
resource-busy, statement cancellation, connection loss, invalid number/date,
single-row subquery returning many rows, and more. The PostgreSQL statement
character position is carried into the TTC `error_pos` field (`python-oracledb`
`error.offset`).

## Operations

TCP keepalive, idle-session reaping, per-statement timeout → `ORA-01013`,
`OCIBreak` / Ctrl-C cancels the in-flight statement, graceful drain on
`SIGINT` / `SIGTERM`, and dependency-free `/healthz` + `/readyz` + `/metrics`
(Prometheus) endpoints.
