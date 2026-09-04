---
title: Compatibility matrix
description: The detailed per-area breakdown of what dbSaci supports.
---

The detailed per-area breakdown. See [What works](/dbsaci/what-works/) and
[Limitations](/dbsaci/limitations/) for the narrative version. Everything marked
"Supported" or "Partial" below has golden-corpus coverage
(`cargo test --test corpus`).

dbSaci presents the same Oracle TNS/TTC interface in both modes. The backend
columns below describe the compatibility layer behind that interface. A `✓`
means that the feature is supported in the tested/common case; an `✗` means it
is not currently available. `Partial` in the notes is intentional: the common
path works, but Oracle's complete semantics do not.

The PostgreSQL lane uses PostgreSQL plus [orafce](https://github.com/orafce/orafce).
The MariaDB lane uses MariaDB 11.4 with `SQL_MODE=ORACLE`, plus dbSaci's
targeted rewrites and compatibility facade. Both lanes are exercised by the
same end-to-end Oracle-driver corpus wherever the backend can express the
feature.

## Feature matrix

| Oracle-facing feature | PostgreSQL | MariaDB | Notes |
| --- | :---: | :---: | --- |
| Oracle TNS/TTC listener | ✓ | ✓ | One protocol endpoint for both backends. |
| Oracle authentication (19c and 11g personas) | ✓ | ✓ | 12c PBKDF2 and 11g O5LOGON/MD5 paths. |
| Oracle client drivers | ✓ | ✓ | `python-oracledb`, JDBC, ODP.NET, `oracle-rs`; OCI thick is additionally covered on PostgreSQL. |
| Oracle schema == user mapping | ✓ | ✓ | Per-session schema/search-path handling. |
| Scalar binds | ✓ | ✓ | Typed values are passed as backend parameters. |
| Array binds / batch DML | ✓ | ✓ | JDBC batches, Python `executemany`, and ODP.NET array binds. |
| Result streaming and client fetches | ✓ | ✓ | Incremental batches, including large results. |
| `DUAL`, `USER`, `UID`, `SYS_CONTEXT` | ✓ | ✓ | Facade implementations. |
| ANSI joins, subqueries, CTEs | ✓ | ✓ | Recursive CTEs included. |
| Legacy `(+)` outer joins | ✓ | ✓ | Structural rewrite where MariaDB needs one. |
| `FULL OUTER JOIN` | ✓ | ✓ | MariaDB is lowered to a union-based equivalent. |
| `LATERAL` joins | ✓ | ✓ | Supported common forms. |
| `ROWNUM` and row limiting | ✓ | ✓ | `LIMIT`/window rewrites as appropriate. |
| `FETCH FIRST … WITH TIES` | ✓ | ✓ | Common ordered-query forms. |
| Analytic/window functions | ✓ | ✓ | Backend-specific expression rewrites may apply. |
| Analytic `LISTAGG` | ✓ | ✓ | MariaDB uses ordered `GROUP_CONCAT` forms. |
| `CONNECT BY` hierarchy | ✓ | ✓ | Including `NOCYCLE`, `CONNECT_BY_ISCYCLE`, `ISLEAF`, `ROOT`, and path expressions. |
| `PIVOT` | ✓ | ✓ | MariaDB uses conditional aggregation. |
| `UNPIVOT` | ✓ | ✓ | MariaDB uses `UNION ALL`. |
| `MERGE` | ✓ | ✓ | Common matched/not-matched forms, including matched delete. |
| `INSERT ALL` / `INSERT FIRST` | ✓ | ✓ | MariaDB uses set-based lowering and statement batching. |
| Oracle scalar functions | ✓ | ✓ | `NVL`, `DECODE`, `NVL2`, `ADD_MONTHS`, `SUBSTR`, `INSTR`, `TRANSLATE`, and related functions. |
| Regular expressions | ✓ | ✓ | `REGEXP_LIKE`, `REPLACE`, `SUBSTR`, `INSTR`, and `COUNT` common forms. |
| Sequences (`NEXTVAL` / `CURRVAL`) | ✓ | ✓ | MariaDB uses its compatibility sequence facade. |
| Identity columns | ✓ | ✓ | Native or translated according to backend. |
| Oracle data dictionary views | ✓ | ✓ | `USER_*`, `ALL_*`, objects, columns, constraints, indexes, sequences, comments, and version/session views. |
| Oracle DDL types | ✓ | ✓ | Common `VARCHAR2`, `NUMBER`, date/time, raw, LOB, and identifier forms. |
| Views and CTAS | ✓ | ✓ | SELECT bodies are translated. |
| Temporary tables | ✓ | ✓ | Global/private temporary tables are mapped to backend temporary tables. |
| Ordinary and bitmap indexes | ✓ | ✓ | Bitmap indexes become ordinary indexes on MariaDB. |
| Function-based indexes | ✓ | ✓ | Generated-column/index lowering on MariaDB. |
| Comments and synonyms | ✓ | ✓ | Synonyms are represented through compatibility objects. |
| Basic materialized views | ✓ | ✓ | Explicit-refresh forms only. |
| Simple triggers | ✓ | ✓ | `BEFORE`/`AFTER`, row/statement, `WHEN`, `:NEW`/`:OLD`, assignments, audit DML, and application errors. |
| Multi-event triggers | ✓ | ✓ | MariaDB splits compatible multi-event definitions into backend triggers. |
| Trigger `REFERENCING` | ✓ | ✓ | Common alias forms. |
| `INSTEAD OF` triggers | ✓ | ✗ | PostgreSQL views can express the common path; MariaDB has no equivalent view-trigger mechanism. |
| Explicit transactions and savepoints | ✓ | ✓ | Includes statement recovery and Oracle-style DDL commit behavior. |
| `SET TRANSACTION` and locking clauses | ✓ | ✓ | `READ ONLY`, isolation, `WAIT`, `NOWAIT`, and `SKIP LOCKED` common forms. |
| `WHERE CURRENT OF` | ✓ | ✗ | PostgreSQL PL/pgSQL cursor support. MariaDB leaves the clause as-is (no PK-guess rewrite). |
| Autonomous transactions | ✗ | ✗ | Requires an independent transaction context; MariaDB does not strip the pragma to fake success. |
| `PRAGMA EXCEPTION_INIT` | ✓ | ✗ | PostgreSQL maps user exceptions; MariaDB does not rewrite `WHEN e` → `WHEN OTHERS`. |
| `ALTER SESSION` and selected NLS settings | ✓ | ✓ | Selected session state is emulated; full Oracle NLS behavior is not. |
| Session time zones | ✓ | ✓ | Common named-region and fixed-offset forms; MariaDB has reduced Oracle time-zone semantics. |
| Oracle error mapping | ✓ | ✓ | Backend errors are mapped to Oracle error numbers where identifiable. |
| `NUMBER` values | ✓ | ✓ | Value-exact common arithmetic and wire encoding. |
| `DATE` and `TIMESTAMP` | ✓ | ✓ | Oracle-compatible wire encodings; MariaDB temporal text is normalized before encoding. |
| `TIMESTAMP WITH TIME ZONE` | ✓ | ✗ | PostgreSQL has the stronger native/session-zone path; MariaDB zone-aware semantics are incomplete. |
| `INTERVAL YEAR TO MONTH` | ✓ | ✗ | MariaDB date arithmetic is translated; result columns are not promoted to Oracle interval wire types 182/183. |
| `INTERVAL DAY TO SECOND` | ✓ | ✗ | Same: arithmetic works; native interval describe/value for thin clients is PostgreSQL-only today. |
| `BINARY_FLOAT` / `BINARY_DOUBLE` | ✓ | ✓ | Declared columns use domains (PG) / COMMENT markers (MariaDB); python-oracledb thin gets native wire types. |
| `ROWID` | ✓ | ✗ | PostgreSQL maps to `ctid`. MariaDB does not invent a fake ROWID from a column named `id`. |
| `CLOB` / `BLOB` inline values | ✓ | ✓ | Inline delivery only; no TTC locator streaming. |
| PL/SQL anonymous blocks | ✓ | ✓ | Basic declarations, control flow, cursors, exceptions, and `SELECT … INTO`. |
| Standalone PL/SQL routines | ✓ | ✓ | Common function/procedure bodies. |
| Packages and package state | ✗ | ✗ | Package-scale PL/SQL, overloading, and package state are not implemented. |
| `BULK COLLECT` / `FORALL` | ✗ | ✗ | Not currently translated. |
| PL/SQL OUT binds / `SYS_REFCURSOR` | ✗ | ✗ | `RETURNING` DML is supported only for the tested thin-driver path. |
| Flashback, `MODEL`, AQ, scheduler, XA/DRCP | ✗ | ✗ | No practical equivalent in the supported backend contract. |
| TCP keepalive, timeout, cancellation, health endpoints | ✓ | ✓ | Backend-independent operations layer. |

## Test evidence

The PostgreSQL and MariaDB lanes use the same corpus harness and Oracle-correct
expected results. Known-red cases are listed in
`tests/corpus/expected-failures.<backend>` (ledger mode requires an exact
match). MariaDB-specific positive cases live in
`tests/corpus/mariadb_oracle_mode.sql`. The MariaDB lane is not merely
`SQL_MODE=ORACLE`: that mode supplies useful lexical/basic semantics, while
dbSaci handles structural SQL differences, result metadata, Oracle wire
encodings, facade objects, and backend-specific gaps.

See [What works](/dbsaci/what-works/) for the narrative view and
[Limitations](/dbsaci/limitations/) for details on unsupported Oracle surface
area.
