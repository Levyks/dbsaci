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
The MariaDB lane — the primary backend from 0.3.0 — uses MariaDB 11.8+ with
`SQL_MODE=ORACLE`, plus dbSaci's targeted rewrites, Oracle's transaction model
and identifier rules, and a data dictionary installed once per server in
`dbsaci_sys`. Both lanes are exercised by the
same end-to-end Oracle-driver corpus wherever the backend can express the
feature.

## Feature matrix

| Oracle-facing feature | PostgreSQL | MariaDB | Notes |
| --- | :---: | :---: | --- |
| Oracle TNS/TTC listener | ✓ | ✓ | One protocol endpoint for both backends. |
| Oracle authentication (19c and 11g personas) | ✓ | ✓ | 12c PBKDF2 and 11g O5LOGON/MD5 paths. |
| Oracle client drivers | ✓ | ✓ | `python-oracledb`, JDBC, ODP.NET, `oracle-rs`; OCI thick is additionally covered on PostgreSQL. |
| Oracle schema == user mapping | ✓ | ✓ | PostgreSQL: per-user schema + `search_path`. MariaDB: login `APPUSER` → account `appuser`, database `APPUSER`. |
| Oracle identifier semantics | ✓ | ✓ | MariaDB: unquoted words upper-cased, quoted kept exact, for every identifier role; works with any `lower_case_table_names` (`--identifier-case upper`/`lower`). |
| Scalar binds | ✓ | ✓ | Typed values are passed as backend parameters; up to 65,535 binds per statement. |
| Large requests (> 8 KB) | ✓ | ✓ | Multi-packet requests (big `IN` lists, wide batches) are reassembled. |
| Array binds / batch DML | ✓ | ✓ | JDBC batches, Python `executemany`, and ODP.NET array binds. MariaDB sends array DML as multi-row `INSERT`s. |
| Result streaming and client fetches | ✓ | ✓ | Incremental batches, including large results; MariaDB results stream without full buffering. |
| Driver autocommit | ✓ | ✓ | The driver's autocommit (Execute commit bit) commits after each call — JDBC/HikariCP default, `connection.autocommit = True`. |
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
| Oracle data dictionary views | ✓ | ✓ | `USER_*`, `ALL_*`, objects, columns, constraints, indexes, sequences, comments, and version/session views. MariaDB: installed once per server in `dbsaci_sys` (`dbsaci install`), invoker-rights views granted to `PUBLIC`; nothing in application schemas. |
| `DBMS_OUTPUT`, `DBMS_METADATA.GET_DDL` | ✓ | ✓ | MariaDB versions live in `dbsaci_sys`. |
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
| Explicit transactions and savepoints | ✓ | ✓ | Includes statement recovery and Oracle-style DDL commit behavior. MariaDB: Oracle model (implicit transaction, `READ COMMITTED`, no stale snapshots), clean logoff commits, cursors fetchable across `COMMIT`; a lost backend connection with uncommitted work ends the session with `ORA-03113` (no silent reconnect). |
| `SET TRANSACTION` and locking clauses | ✓ | ✓ | `READ ONLY`, isolation, `WAIT`, `NOWAIT`, and `SKIP LOCKED` common forms. MariaDB: `SERIALIZABLE` gives a transaction snapshot; `ORA-01453` when not first in the transaction. |
| `WHERE CURRENT OF` | ✓ | ✗ | PostgreSQL PL/pgSQL cursor support. MariaDB leaves the clause as-is (no PK-guess rewrite). |
| Autonomous transactions | ✗ | ✗ | Requires an independent transaction context; MariaDB does not strip the pragma to fake success. |
| `PRAGMA EXCEPTION_INIT` | ✓ | ✗ | PostgreSQL maps user exceptions; MariaDB does not rewrite `WHEN e` → `WHEN OTHERS`. |
| `ALTER SESSION` and selected NLS settings | ✓ | ✓ | Selected session state is emulated; full Oracle NLS behavior is not. On MariaDB it is session (not transactional) state and survives reconnects. |
| Session time zones | ✓ | ✓ | Common named-region and fixed-offset forms; MariaDB has reduced Oracle time-zone semantics. |
| Oracle error mapping | ✓ | ✓ | Backend errors are mapped to Oracle error numbers where identifiable — PostgreSQL by `SQLSTATE`, MariaDB by errno (`ORA-00001`, `02291`, `02292`, `01400`/`01407`, `00060`, `00054`/`30006`, `12899`, `01438`, `01476`, `01722`, `01858`, `00942`, `00904`, `01013`, …). |
| `NUMBER` values | ✓ | ✓ | Value-exact common arithmetic and wire encoding. MariaDB: bare `NUMBER` → `DECIMAL(65,30)`, `NUMBER(p,s)` → `DECIMAL(p,s)`; unconstrained `NUMBER` described as precision 0 / scale -127. |
| Describe metadata | ✓ | ✓ | MariaDB reports real precision/scale and nullability; `CHAR` is blank-padded. |
| `DATE` and `TIMESTAMP` | ✓ | ✓ | Oracle-compatible wire encodings; MariaDB temporal text is normalized before encoding, and `TIMESTAMP` microseconds survive binds. |
| `TIMESTAMP WITH TIME ZONE` | ✓ | ✗ | PostgreSQL has the stronger native/session-zone path; MariaDB zone-aware semantics are incomplete. |
| `INTERVAL YEAR TO MONTH` | ✓ | ✗ | MariaDB date arithmetic is translated; result columns are not promoted to Oracle interval wire types 182/183. |
| `INTERVAL DAY TO SECOND` | ✓ | ✗ | Same: arithmetic works; native interval describe/value for thin clients is PostgreSQL-only today. |
| `BINARY_FLOAT` / `BINARY_DOUBLE` | ✓ | ✓ | Declared columns use domains (PG) / COMMENT markers (MariaDB); python-oracledb thin gets native wire types. |
| `ROWID` | ✓ | ✗ | PostgreSQL maps to `ctid`. MariaDB does not invent a fake ROWID from a column named `id`. |
| `CLOB` / `BLOB` inline values | ✓ | ✓ | Inline delivery only; no TTC locator streaming. LOB RPCs answer `ORA-22275` (not `ORA-03001`, which made HikariCP evict connections). |
| PL/SQL anonymous blocks | ✓ | ✓ | Basic declarations, control flow, cursors, exceptions, and `SELECT … INTO`. |
| Standalone PL/SQL routines | ✓ | ✓ | Common function/procedure bodies. |
| Packages and package state | ✗ | ✗ | Package-scale PL/SQL, overloading, and package state are not implemented. |
| `BULK COLLECT` / `FORALL` | ✗ | ✗ | Not currently translated. |
| PL/SQL OUT binds / `SYS_REFCURSOR` | ✗ | ✗ | `RETURNING` DML is supported only for the tested thin-driver path. |
| Flashback, `MODEL`, AQ, scheduler, XA/DRCP | ✗ | ✗ | No practical equivalent in the supported backend contract. |
| TCP keepalive, timeout, cancellation, health endpoints | ✓ | ✓ | Backend-independent operations layer. MariaDB per-call timeouts use `KILL QUERY`, covering streaming results and stored programs. |

## Test evidence

The PostgreSQL and MariaDB lanes use the same corpus harness and Oracle-correct
expected results. Known-red cases are listed in
`tests/corpus/expected-failures.<backend>` (ledger mode requires an exact
match). The MariaDB lane runs across MariaDB 11.8 and 12.3, `lower_case_table_names`
0 and 1, and identifier case `upper` and `lower`, all against one shared ledger
(a configuration-dependent result is a bug), as a least-privilege login.
MariaDB-specific positive cases live in `tests/corpus/mariadb_oracle_mode.sql`;
the `identifier_case` and `transactions_concurrency` groups cover the 0.3.0
identifier and transaction models. The MariaDB lane is not merely
`SQL_MODE=ORACLE`: that mode supplies useful lexical/basic semantics, while
dbSaci handles structural SQL differences, result metadata, Oracle wire
encodings, facade objects, and backend-specific gaps.

See [What works](/dbsaci/what-works/) for the narrative view and
[Limitations](/dbsaci/limitations/) for details on unsupported Oracle surface
area.
