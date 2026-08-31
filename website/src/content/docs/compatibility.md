---
title: Compatibility matrix
description: The detailed per-area breakdown of what pgSaci supports.
---

The detailed per-area breakdown. See [What works](/pgsaci/what-works/) and
[Limitations](/pgsaci/limitations/) for the narrative version. Everything marked
"Supported" or "Partial" below has golden-corpus coverage
(`cargo test --test corpus`).

## Per-area status

| Area | Status | Notes |
| --- | --- | --- |
| **Connection / authentication** | Supported | Full TNS/TTC handshake + auth for `python-oracledb` thin, the Oracle JDBC thin driver, ODP.NET managed, and `oracle-rs` — each verified end to end against both the 19c and 11g personas. 11g clients that only speak O5LOGON get the MD5 verifier; JDBC and ODP.NET 11g use 12c PBKDF2. One Oracle session ↔ one dedicated PostgreSQL connection. Credential model: a pre-declared `pg_user → pg_password` list with an optional fallback; the Oracle username is matched case-insensitively and an unmatched user with no fallback is `ORA-01017`. |
| **SQL SELECT / DML** | Supported subset | Joins, CTEs (incl. recursive), windows/analytics, `ROWNUM`, legacy `(+)`, `CONNECT BY` (incl. `NOCYCLE`, `CONNECT_BY_ISCYCLE/ISLEAF`), `MERGE` (incl. matched `DELETE`), `INSERT ALL`, `PIVOT`/`UNPIVOT`, sequences, and common `orafce` functions. |
| **Result delivery** | Supported | Results stream through Execute/Fetch batches without whole-result buffering; multi-packet results (>64 MiB) stream via the client fetch loop. `REEXECUTE` / `REEXECUTE_AND_FETCH` (a cached statement re-run with new binds — `python-oracledb`'s default) are handled. |
| **Scalar binds** | Supported | Text, `NUMBER`, bytes, temporal values, Boolean, binary-float input values passed as PostgreSQL parameters, never interpolated. |
| **Array binds / batch DML** | Supported | `executemany` (`python-oracledb`), `addBatch`/`executeBatch` (JDBC), `ArrayBindCount` (ODP.NET) run one statement over an N-row value matrix; the summed row count is returned. |
| **OUT binds** | Partial | `RETURNING <cols> INTO :out` DML works for **`python-oracledb` thin**. ojdbc and ODP.NET get `ORA-03001`. PL/SQL OUT parameters and `SYS_REFCURSOR` are not implemented. |
| **DDL** | Partial | Oracle types, views/CTAS (SELECT body translated), `ALTER TABLE` forms, `COMMENT ON`, physical-clause stripping, ordinary + `BITMAP`→plain + function-based/expression indexes, basic materialized views with explicit refresh, `CREATE SYNONYM`→view, global/private temp tables. Oracle-managed MV refresh policies and full storage semantics are partial. |
| **Triggers** | Partial | `BEFORE`/`AFTER`/`INSTEAD OF`, row/statement, multi-event, `WHEN (...)`, `:NEW`/`:OLD`, `REFERENCING`, and bodies using column assignment, an audit `INSERT`, `RAISE_APPLICATION_ERROR`, `IF … END IF`, numeric `FOR` loops → a PostgreSQL trigger function. Compound triggers and package-scale bodies do not. |
| **Transactions & locking** | Partial | Explicit control, savepoints, statement recovery, implicit DDL commits, `SET TRANSACTION READ ONLY / ISOLATION LEVEL`, `SELECT … FOR UPDATE` (`OF` list dropped, `WAIT n`→block, `NOWAIT`/`SKIP LOCKED` passthrough), `WHERE CURRENT OF` in a PL/SQL cursor loop. XA and autonomous transactions do not. |
| **Session / NLS** | Partial | Current schema, time zone, selected NLS settings, harmless optimizer settings. Full Oracle implicit conversion / NLS behaviour is not. |
| **Errors** | Partial | ~35 PostgreSQL SQLSTATE classes → Oracle error numbers (deadlock, serialization failure, cancellation, lock-not-available, timeout, connection failures). The PostgreSQL statement character position is carried into the TTC `error_pos` field. |
| **Types** | Partial | **NUMBER** value-exact; declared `NUMBER(p,s)` precision/scale reported for `python-oracledb` thin and `oracle-rs` (ojdbc / ODP.NET keep `(38,0)` — column-metadata parser limit). **DATE** 7-byte. **TIMESTAMP** native 11-byte with sub-second (`python-oracledb` thin / OCI thick; ojdbc and ODP.NET get Oracle `DATE`, second precision). **TIMESTAMP WITH TIME ZONE** native 13-byte, offset fixed at `+00:00`. **BINARY_FLOAT / BINARY_DOUBLE** and **INTERVAL YEAR TO MONTH / DAY TO SECOND** decode natively for `python-oracledb` thin; other drivers get `NUMBER` / interval text. `text` (UTF-8), `bytea`, Boolean → `NUMBER(1)`, `ROWID` → `ctid` text. `NCHAR`/`NVARCHAR2` → `VARCHAR2` (UTF-8 exact). |
| **LOBs** | Partial | `CLOB`/`BLOB` inline (limit: one ~64 MiB TTC packet). `DBMS_LOB.GETLENGTH/SUBSTR/INSTR` as SQL functions. TTC LOB locators and multi-gigabyte streaming are not implemented. |
| **PL/SQL** | Partial | Anonymous blocks and standalone routines translate: `DECLARE`, `%TYPE`/`%ROWTYPE`, numeric `FOR`, `WHILE`/`LOOP`, `IF`/`CASE` (expression **and** statement), nested blocks, `EXECUTE IMMEDIATE`, explicit named cursors, `EXCEPTION WHEN …` (predefined names mapped), `PRAGMA EXCEPTION_INIT`, `SELECT … INTO`. `DBMS_OUTPUT.PUT_LINE` → `RAISE NOTICE`. Packages, `BULK COLLECT`/`FORALL`, pipelined functions do not. |
| **Operations** | Supported | TCP keepalive + configurable idle reaping, per-statement timeout → `ORA-01013`, `OCIBreak`/Ctrl-C cancels the in-flight statement, graceful drain on `SIGINT`/`SIGTERM`, dependency-free `/healthz` + `/readyz` + `/metrics`, per-session tracing spans. Plaintext transport only. |

## Not implemented — PostgreSQL + `orafce` has no equivalent

Flashback, the `MODEL` clause, full PL/SQL packages, `DBMS_SCHEDULER` /
`DBMS_JOB`, Advanced Queueing, XA / distributed transactions, DRCP, Application
Continuity, network compression, client-side result cache, and non-UTF-8 wire
character sets. `DBMS_PIPE` / `DBMS_ALERT` are provided by `orafce` and route
through unchanged. See [Limitations](/pgsaci/limitations/) for the full list and
the "planned, not yet" set.
