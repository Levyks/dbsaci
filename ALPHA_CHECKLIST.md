# PgSaci — road to alpha `0.0.1`

Goal of `0.0.1`: a real Oracle client (OCI / JDBC thin / ODP.NET / python-oracledb)
can point at PgSaci instead of an Oracle instance and run a **non-trivial but
bounded** workload — DML, rich SELECT, simple DDL, orafce-backed functions,
single-statement PL/SQL — against PostgreSQL, with a published list of what does
and does not work.

## Where we are (2026-08-29)

- SQL **translation** covers mainstream + much Oracle-specific SQL: joins /
  subqueries / CTEs (incl. recursive) / set ops / window & analytic funcs
  (`PERCENTILE_*`, `MEDIAN`, `CUME_DIST`, `NTH_VALUE`) / `ROWNUM` / legacy `(+)`
  / `CONNECT BY` (+`NOCYCLE`/`ISCYCLE`/`ISLEAF`) / `MERGE` (+matched `DELETE`) /
  `INSERT ALL` / `PIVOT`/`UNPIVOT` / DDL incl. synonyms, global temp tables,
  `COMMENT ON`, `ALTER TABLE`, MVs, function-based indexes, `VIRTUAL`→`STORED`
  generated columns / `CREATE TRIGGER` (incl. `INSTEAD OF`, multi-event) /
  `FOR UPDATE` variants / `SET TRANSACTION` / orafce / `DECODE`/`NVL*` / `q'[]'`
  / sequences / PL/SQL blocks, functions, procedures, triggers with explicit
  cursors, `%ROWTYPE`, statement `CASE`, `PRAGMA EXCEPTION_INIT`. **632 corpus
  cases pass, 0 ignored** (PG18/16; PG13 skips the 5 `MERGE` cases that need
  PG15+).
- **Streaming** (RowStream → Execute/Fetch, incl. a ≥ 1,000,000-row result and
  results larger than one 64 MiB TTC packet), **scalar binds** (real PG params +
  prepared-statement cache), **array binds / batch DML** (`executemany`, JDBC
  batch, ODP.NET `ArrayBindCount`), **`RETURNING … INTO` OUT binds**
  (python-oracledb thin), **`REEXECUTE` / `REEXECUTE_AND_FETCH`**, and
  **lifecycle** (keepalive, idle reaping, per-statement timeout → ORA-01013,
  OCIBreak cancel, graceful drain, `/healthz`+`/readyz`+`/metrics`, one-batch
  session init) are in place.
- **Type fidelity**: multi-user credentials (env/CLI/file), `NUMBER(p,s)` real
  precision/scale (python-oracledb, oracle-rs), native `TIMESTAMP` /
  `TIMESTAMP WITH TIME ZONE` sub-second wire encodings, native `BINARY_FLOAT` /
  `BINARY_DOUBLE` and `INTERVAL YEAR TO MONTH` / `DAY TO SECOND` for
  python-oracledb (Oracle-text fallback elsewhere), and PostgreSQL error
  character position → the TTC `error_pos` field.
- **All four target clients work end to end**, each verified against both the
  19c and 11g personas (`PGSACI_ORACLE_VERSION`):
  - `python-oracledb` 4.0.2 thin — `clients/python/probe.py`, 30/30.
  - Oracle JDBC thin `ojdbc11` 23.5 — `clients/java/JdbcCompat.java`, 12/12.
  - ODP.NET managed `Oracle.ManagedDataAccess.Core` 23.5 — `clients/dotnet`, 13/13.
  - `oracle-rs` 0.1 — the 632-case corpus.
  Each probe covers connect + 12c PBKDF2 auth with mutual server proof, metadata,
  SELECT, named + positional binds, INSERT + rowcount + same-txn visibility,
  rollback, a multi-thousand-row fetch loop, and array-bind batch DML.
- **OCI thick client** (`clients/oci/probe.py`, python-oracledb thick mode +
  Instant Client 19.32) — **a full functional query cycle works**: handshake →
  12c PBKDF2 auth → execute → fetch rows (scalar / single-row / multi-row /
  multi-column incl. NULL) → binds → DDL → simple DML → clean disconnect. The
  whole OCI TTC dialect uses **little-endian fixed-width integers** (not thin's
  compact `[len][BE bytes]` form) — reverse-engineered byte-for-byte from live
  Oracle XE 21c captures. Encoders: `build_query_response_oci`,
  `build_fetch_response_oci` + `parse_fetch_request_oci` (fetch is capped to one
  row per round trip — correct but slow for huge result sets),
  `build_error_response_oci`, `build_dml_response_oci`, `build_logoff_response_oci`;
  OCI bind decode in `parse_execute_request_oci`. `tests/wire.rs` locks the
  Execute reply against the capture; `clients/oci/corpus_runner.py` drives the
  golden corpus through python-oracledb thick (sample groups: ansi_select 27/27,
  oracle_strings 36/36, ansi_joins 15/15, pagination 20/20, oracle_dates 35/36,
  types 34/42, error_codes 12/21 — ~85-90% overall). **Still failing:** error
  edge cases (some kill the connection), a few OCI-framed type encodings
  (RAW/CLOB/interval/binary-float fall back to text/NUMBER), multi-row-per-fetch
  (speed), `INSERT…SELECT` rowcount. All OCI wire specifics are gated on an
  `is_oci` flag; oracle-rs corpus stays 632/0 and the thin/jdbc probes are
  unaffected.

Legend: `[ ]` not started · `[~]` partial · `[x]` done · `[D]` deferred with a
documented rationale (see `COMPATIBILITY.md` "Not implemented")

---

## P0 — blocks *any* real production use

### Result streaming & server-side cursors
`backend.rs::RowCursor` wraps `client.query_raw` → a `RowStream`. Rows are pulled
and encoded incrementally (`next_batch`), wrapped in the per-statement savepoint,
released on exhaustion / abandon / logoff. `server.rs` keeps at most one
`RowCursor` per session and has a real `Fetch` handler.

- [x] Stream rows from PostgreSQL with `query_raw` / `RowStream` instead of
      `query()` — the full backend result is no longer held in PgSaci.
- [x] Server-side cursor / statement handle: `RowCursor` keeps the stream +
      describe metadata alive between calls; `server.rs` tracks one per session.
- [x] Real `Fetch` (`0x05`) handler: pulls the next N rows, advances, signals
      exhaustion, drops the cursor when done.
- [x] Close cursors on logoff and on session drop; `RowCursor::finish` releases
      the savepoint.
- [x] TTC "more rows" flag + cursor id are emitted; Execute prefetch and Fetch
      array size bound each streamed batch.
- [x] Honour the client's prefetch / array size per Execute/Fetch.
- [x] Backpressure: batch sizes are bounded before row encoding; one `RowCursor`
      at a time, so an abandoned cursor cannot accumulate rows in PgSaci.
- [x] Cap concurrently open cursors per session: one active cursor.
- [x] Corpus: `result_streaming.sql` drives 50–1,000,000-row results through the
      streamed cursor, including a result larger than one 64 MiB TTC packet
      (`result_larger_than_one_packet`, `one_million_row_stream`).

### Real bind parameters (parse / bind / execute)
Scalar values are decoded from TTC, rewritten lexically from Oracle `:name`
syntax to PostgreSQL `$n` parameters, and passed separately to `tokio-postgres`.

- [x] Send scalar binds to PostgreSQL as real parameters (`$1..$n`), not
      interpolated text.
- [~] Map scalar Oracle bind descriptors to PostgreSQL parameter types. Text,
      NUMBER, bytes, DATE/TIMESTAMP, Boolean, and binary floats are supported;
      arbitrary NUMBER is safely sent as text then cast server-side.
- [x] Statement cache keyed by the translated SQL text
      (`backend.rs::prepare_cached`, cap 256/connection), with one transparent
      re-prepare on "cached plan must not change result type".
- [x] **Array binds / batch DML (`executemany`, TTC array execute).**
      `ExecuteRequest` carries `num_iters` + `bind_rows`; `parse_execute_strict`
      and `parse_execute_scan` read the iteration count from `al8i4[1]` and one
      `0x07`-prefixed value row per iteration. `server.rs` runs each row against
      the cached statement and replies with the summed row count. Verified with
      python `executemany`, JDBC `addBatch`/`executeBatch`, and ODP.NET
      `ArrayBindCount`. (No corpus case: `oracle-rs` has no array-bind API.)
- [~] **`RETURNING ... INTO :x` OUT binds — done for python-oracledb thin.**
      `wire::split_returning_into` strips the `INTO <binds>` clause (byte-scan,
      quote-aware, multi-byte-safe); `backend::execute_returning` runs the plain
      `RETURNING` and encodes the returned column values;
      `wire::build_returning_response` emits the driver's expected sequence —
      `TNS_MSG_TYPE_IO_VECTOR` (per-bind direction: input 32, output 16),
      `TNS_MSG_TYPE_ROW_DATA` (per OUT bind: `ub4` row count, then each value +
      its trailing `sb4` actual-length that `_process_column_data` reads when not
      fetching), `TNS_MSG_TYPE_PARAMETER`, end-of-call. `parse_execute_strict`
      now tolerates a descriptor block with no RowData (an all-OUT-bind
      statement). Verified: python probe `returning_into_{value,rowcount,visible,update}`
      (INSERT and UPDATE, with and without input binds). ojdbc and ODP.NET
      frame OUT binds differently, so they get a clean ORA-03001 rather than a
      silent drop;
      PL/SQL OUT (`BEGIN :x := … END`) still needs wrapping the block as a
      returning function.
- [D] REF CURSOR OUT params (`SYS_REFCURSOR`). Needs the OUT-bind response path
      above plus a client-drivable cursor handle backed by a real PostgreSQL
      cursor (`DECLARE … CURSOR` in the session txn) with its own `Fetch` loop.
      Post-`0.0.1`.
- [x] Injection review: bind values are never included in the SQL string
      (`wire::tests::converts_binds_to_postgres_parameters_without_interpolation`,
      `does_not_substitute_binds_in_quoted_or_commented_sql`).

### Backend connection lifecycle (not multiplexed pooling)
1 Oracle session → 1 dedicated PostgreSQL connection. The connection ceiling is
PostgreSQL's `max_connections`; PgSaci translates the rejection.

- [x] Map PostgreSQL `53300` / `53400` / `08004` on backend connect → ORA-00018
      / ORA-12516 / ORA-12520 so client retry/backoff behaves.
- [x] Detect and reap vanished clients: TCP keepalive + `Config::idle_timeout`
      on every incomplete TNS frame.
- [x] Map a dropped backend PG connection to ORA-03113 / ORA-03135.
- [x] Per-statement timeout via `Config::statement_timeout`, scoped to the
      statement savepoint; cancellation → ORA-01013, session recovers.
- [x] Amortise session init: `BEGIN` + catalog-facade temp views + all
      `ORACLE_COMPAT_FACADE` functions go out as one simple-protocol batch with
      a piecewise fallback.
- [D] Warm cache of pre-initialised backend connections. Pure latency
      optimisation; the per-connect cost is one batch round trip. Not needed for
      `0.0.1`; documented as a future tuning item.

### Client compatibility proof

- [x] **`python-oracledb` 4.0.2 thin mode: 16/16 end-to-end.**
      `clients/python/probe.py` — connect + 12c auth, `SELECT 1 FROM DUAL`,
      seeded SELECT, named + positional binds, INSERT + rowcount + same-txn
      visibility, rollback, 2500-row `CONNECT BY` fetch loop, `NVL` translation,
      anonymous PL/SQL block, and `executemany` INSERT + UPDATE with rollback.
- [x] **JDBC thin (`ojdbc11` 23.5): 10/10 end-to-end.**
      `clients/java/JdbcCompat.java` — connect + `getDatabaseProductVersion`,
      SELECT, `PreparedStatement` bind, INSERT + `executeUpdate` + visibility,
      rollback, 2500-row `CONNECT BY`, and `addBatch`/`executeBatch` INSERT.
      Full handshake + auth + row/describe/fetch/end-of-call path reverse
      engineered from the driver (see `pgsaci-known-gaps` memory).
- [x] **ODP.NET managed (`Oracle.ManagedDataAccess.Core` 23.5): 11/11.**
      `clients/dotnet/Probe.cs` — the same coverage plus `ArrayBindCount` batch
      DML. Needs three negotiation-phase quirks (DataTypes TZ blob, `0x04` auth
      terminator, single-byte CLR chunks); the row path is shared with JDBC.
- [x] OCI-based client (`clients/oci/probe.py` 3/3, `clients/run.sh oci`;
      python-oracledb **thick mode** + Instant Client 19.32). Full functional
      cycle: handshake → 12c PBKDF2 auth → execute → **fetch rows** (scalar /
      multi-row / multi-column / NULL) → binds → DDL → simple DML → disconnect.
      The OCI TTC dialect is **little-endian fixed-width ints**, reverse-
      engineered byte-for-byte from live Oracle XE 21c (`build_query_response_oci`,
      `build_fetch_response_oci`, `build_error_response_oci`,
      `build_dml_response_oci`, `build_logoff_response_oci`; OCI bind decode).
      `clients/oci/corpus_runner.py` runs the golden corpus via OCI — ~85-90%
      pass; the remainder is error edge cases, a few OCI-framed type encodings,
      multi-row-per-fetch speed, and `INSERT…SELECT` rowcount. Gated on `is_oci`
      so the four thin/`oracle-rs` clients (corpus still 632/0) are untouched.
- [x] `Config::oracle_version` (`OracleVersion::V11g | V19c`, env
      `PGSACI_ORACLE_VERSION`) drives the banner, `AUTH_VERSION_NO/STRING`, and
      the verifier family. Every probe runs against both.

### Authentication
- [x] 11g O5LOGON (MD5) verifier wired (`src/auth.rs`, `src/server.rs`) — used by
      python-oracledb against the 11g persona.
- [x] 12c PBKDF2 verifier wired and default. ojdbc and ODP.NET always use it (they
      hardwire O7L multi-round); python-oracledb uses it for the 19c persona.
      Mutual server proof verified by all three drivers.
- [x] **Per-user credential handling.** `src/credentials.rs` holds a
      pre-declared `pg_user → pg_password` map plus an optional fallback.
      `handle_connection` resolves the Oracle username (case-insensitively) to a
      password and runs *both* the login challenge and the backend PG connection
      with it, so an Oracle client authenticates with the same credentials it
      would use against PostgreSQL directly. Configured via `--pg-user u:p`
      (repeatable), `PGSACI_PG_USERS` (comma list), `--pg-users-file` /
      `PGSACI_PG_USERS_FILE`, layered file < env < CLI, with `--pg-password` /
      `PGSACI_PG_PASSWORD` as the fallback for users not in the list. A user with
      no match and no fallback is rejected with ORA-01017. Documented in
      `src/bin/pgsaci.rs` and `COMPATIBILITY.md`.
- [x] Reject malformed / invalid password proofs with ORA-01017, not a panic or
      generic protocol error.
- [D] External auth (Kerberos / wallet / proxy auth). Explicitly out of scope for
      `0.0.1`; listed as unsupported in `COMPATIBILITY.md`.

---

## P1 — needed before "works for *some* environments"

### DDL through the translator

- [x] Run the SELECT body of `CREATE [OR REPLACE] VIEW` / `CREATE MATERIALIZED
      VIEW` / `CREATE TABLE AS SELECT` through the SQL translator. Corpus:
      `views::view_over_dual`, `view_over_legacy_outer_join`, `view_over_rownum`,
      `view_over_decode`, `ctas_over_oracle_sql`.
- [~] `CREATE MATERIALIZED VIEW` + `REFRESH`: PostgreSQL MVs and explicit
      `REFRESH MATERIALIZED VIEW` work (`oracle_ddl::materialized_view_build_and_refresh`);
      Oracle FAST / ON COMMIT refresh policies and scheduler integration remain
      unsupported (documented — needs `pg_cron` or a second extension).
- [x] `CREATE [OR REPLACE] [PUBLIC] SYNONYM` → view; `DROP SYNONYM` → `DROP VIEW
      IF EXISTS`. Corpus: `oracle_ddl::*synonym*`.
- [x] `COMMENT ON TABLE/COLUMN` passes through. Corpus: `oracle_ddl::comment_on_*`.
- [x] `CREATE INDEX`: `BITMAP` → plain B-tree, physical clauses stripped,
      function-based / expression keys parenthesised for PostgreSQL,
      `COMPUTE STATISTICS` stripped. Corpus: `oracle_ddl::function_based_index_*`,
      `unique_function_based_index`.
- [x] Strip common Oracle physical clauses: `TABLESPACE`, `PCTFREE`, `STORAGE
      (...)`, `LOGGING`/`NOLOGGING`, `PARALLEL`, `ENABLE ROW MOVEMENT`, `SEGMENT
      CREATION`, inline `LOB (...) STORE AS`. Corpus:
      `oracle_ddl::physical_storage_clauses_ignored`.
- [~] `ALTER TABLE`: `RENAME COLUMN`, parenthesised `DROP`, `ADD/DROP
      CONSTRAINT`, multi-column `MODIFY` work. `SET UNUSED` maps to an immediate
      `DROP COLUMN` (Corpus: `oracle_ddl::set_unused_hides_columns`), not
      Oracle's deferred physical drop — behavioural note, not a gap.
- [x] `NUMBER(p,s)` in describe metadata. `tokio_postgres::Column::type_modifier()`
      *is* exposed in 0.7.18 (the old "not exposed" note was stale); PgSaci now
      decodes the RowDescription `atttypmod` (`((p<<16)|s)+4`) for `NUMERIC`
      columns and reports the real `NUMBER(p,s)`. python-oracledb thin and
      oracle-rs take it (corpus + `number_ps_precision`/`_scale` in the python
      probe); ojdbc / ODP.NET keep `(38,0)` because their column-metadata parser
      desyncs on a non-zero scale field (`DescribeCaps::report_number_scale`,
      gated on `!newer_describe_framing`) — the value is exact regardless.

### Type & wire fidelity
- [x] **`TIMESTAMP` fractional seconds on the result path.** A PostgreSQL
      `timestamp` column is now described as Oracle TIMESTAMP (type 180) and
      encoded as the native 11-byte form (7-byte DATE + 4-byte big-endian
      nanoseconds); `timestamptz` is Oracle TIMESTAMP WITH TIME ZONE (type 181),
      the 13-byte form (offset `+00:00`, since PostgreSQL stores TIMESTAMPTZ as
      UTC). Plain `date` stays the 7-byte DATE. Corpus:
      `types::timestamp_native_fractional_seconds`, `timestamp_native_millis`,
      `timestamptz_native_subsecond_survives`; python probe asserts
      `datetime.microsecond`. Verified against oracle-rs, python-oracledb thin,
      JDBC, and ODP.NET.
- [~] **`INTERVAL YEAR TO MONTH` / `DAY TO SECOND` result columns.** New
      `backend::PgInterval` (`FromSql`, parses the 16-byte PostgreSQL binary);
      `interval_is_year_to_month(type_modifier)` picks the Oracle family from the
      column's field mask. python-oracledb thin gets the native TTC forms
      (type 182 = 5 bytes, type 183 = 11 bytes; corpus + python probe
      `interval_ds_value` / `interval_ds_negative` / `interval_ym_value`);
      oracle-rs / JDBC / ODP.NET get an Oracle-style text rendering
      (`+DD HH:MI:SS.ffffff` / `±YY-MM`) — corpus
      `intervals::*_result_column` — where a raw interval column previously
      decoded as NULL. (`DescribeCaps::native_intervals`, gated to thin-strict.)
- [~] `BINARY_FLOAT` / `BINARY_DOUBLE` native result encoding. The DDL
      translator maps them to transparent domains `pgsaci.binary_float` /
      `pgsaci.binary_double` (over `real`/`float8`, committed once per
      connection); at describe time a catalog lookup on `table_oid`/`column_id`
      recovers a *declared* column's Oracle-ness and emits type 100/101 in the
      native sign-adjusted IEEE wire form. A computed `float8` (`POWER`, `SQRT`,
      `CAST … AS DOUBLE PRECISION`) has no `table_oid` and correctly stays NUMBER
      — corpus `types::computed_double_stays_number`. **Works end to end for
      python-oracledb thin** (`clients/python/probe.py`: `DB_TYPE_BINARY_DOUBLE`
      + exact value). Gated off (`DescribeCaps::native_binary_floats`) for the
      others: oracle-rs 0.1.7 mis-decodes a BINARY_DOUBLE result column (its
      tests only cover the raw encode/decode fns), and ojdbc / ODP.NET need
      per-driver OAC describe framing for types 100/101 — they get NUMBER
      (value-exact). Arithmetic over a BINARY_DOUBLE column also degrades to
      NUMBER (PostgreSQL erases the domain on expressions).
- [~] `NCHAR` / `NVARCHAR2` map to VARCHAR2 and round-trip UTF-8 correctly
      (corpus `types::nvarchar2_declared_column_roundtrip`, `non_ascii_*`).
      Reporting them as a distinct N-type was tried (a `pgsaci.nvarchar2` domain
      + `charset_form=2`) and reverted: python-oracledb decodes an NCHAR column
      against the *negotiated* national charset (AL16UTF16), so UTF-8 bytes
      tagged N-type are mis-decoded. Making it work needs UTF-16 on the wire for
      NCHAR columns or an ncharset renegotiation — disproportionate to a label
      change, and the value is exact as VARCHAR2. Original note about the
      national charset id below still applies but is now moot.
- [~] (superseded) the national charset id is reported as the DB
      charset. Cosmetic for AL32UTF8-end-to-end.
- [~] `ROWID` → `ctid` as text. Round-trips for in-transaction re-fetch and
      `COUNT(DISTINCT ROWID)` (corpus `types::rowid_*`); not a stable physical
      address across row moves (neither is Oracle's under row movement).
- [x] Boolean (23c): PostgreSQL `bool` result columns described as `NUMBER(1)`,
      encoded 1/0. Corpus: `types::boolean_result_maps_to_number_one_zero`.
- [x] Explicitly unsupported, listed in `COMPATIBILITY.md`: `XMLType`, native
      `JSON`, object types, `VARRAY` / nested tables, `BFILE`, `ANYDATA`,
      `SDO_GEOMETRY`.

### LOBs
- [~] CLOB / BLOB travel inline as TEXT / BYTEA, up to one ~64 MiB TTC packet.
- [x] `DBMS_LOB.GETLENGTH` / `SUBSTR` / `INSTR` as `dbms_lob.*` SQL functions
      over inline text/bytea. Corpus: `types::dbms_lob_*`.
- [D] TTC LOB locators (open/read/write/length/close) and multi-gigabyte
      streaming LOB read. A distinct protocol sub-machine; documented unsupported
      for `0.0.1` in `COMPATIBILITY.md`.

### Session settings / NLS
- [~] `ALTER SESSION SET NLS_DATE_FORMAT / NLS_TIMESTAMP_FORMAT /
      NLS_NUMERIC_CHARACTERS / NLS_SORT / NLS_COMP` — tracked per session and
      reflected in `nls_session_parameters`; full Oracle implicit
      conversion/NLS semantics are not emulated.
- [x] `ALTER SESSION SET CURRENT_SCHEMA` → `SET search_path`.
- [x] `ALTER SESSION SET TIME_ZONE` → `SET timezone`.
- [x] Ignore harmlessly: `ALTER SESSION SET ... optimizer / events / sql_trace`.
- [x] `nls_session_parameters` reflects tracked current session settings.

### Transactions & locking
- [x] Oracle implicit DDL commits before and after execution; a later ROLLBACK
      does not undo prior DML.
- [~] `SELECT ... FOR UPDATE [OF <cols>] [SKIP LOCKED | NOWAIT | WAIT n]`:
      `OF <cols>` list dropped to an unqualified row lock, `WAIT n` → plain
      blocking wait, `NOWAIT` / `SKIP LOCKED` pass through. Positioned `UPDATE
      ... WHERE CURRENT OF <cursor>` works inside a PL/SQL cursor loop but not
      for a client TTC cursor handle. Corpus: `transactions::select_for_update_*`.
- [x] `SET TRANSACTION READ ONLY` / `ISOLATION LEVEL {READ COMMITTED |
      SERIALIZABLE}` map 1:1. Corpus: `transactions::set_transaction_*`.
- [x] Autonomous transactions (`PRAGMA AUTONOMOUS_TRANSACTION`) documented
      unsupported.
- [x] No XA / distributed transactions — documented unsupported.

### Error mapping
- [x] `server.rs::oracle_error_for` maps ~35 SQLSTATE classes; unmapped falls
      back to ORA-00900. Corpus: `error_codes.sql`.
- [x] Error text delivered as the bare message; the client renders `ORA-nnnnn:`
      from the numeric code (`error_codes::error_message_has_no_sqlstate_prefix`).
- [x] **Preserve error character position.** `Error::PgStatement` carries the
      PostgreSQL `ErrorPosition::Original` offset up from `recover_statement_error`;
      `oracle_error_for_pos` returns it and `build_error_response_at` writes it
      into the TTC `error_pos` field of all three end-of-call layouts
      (`write_end_of_call` / `_ext` / `_jdbc`). python-oracledb thin surfaces it
      as `error.offset` (python probe `error_position_nonzero`). oracle-rs still
      discards the field; JDBC/ODP.NET carry it but the probes don't assert.
- [x] Deadlock (40P01) → ORA-00060, serialization failure (40001) → ORA-08177,
      query canceled (57014) → ORA-01013, lock not available (55P03) → ORA-00054,
      `division_by_zero` (22012) → ORA-01476.

### Concurrency & robustness
- [x] Fuzz / malformed-packet resistance in `wire.rs` / `tns.rs` — a bad client
      gets an `Err`, never a panic. `ReadBuffer` bounds-checks every read
      (`ensure_remaining`); no production `unwrap`/`expect`/`panic!` in the
      parsers. Test:
      `wire::tests::request_parsers_reject_malformed_input_without_panicking`
      (truncated, oversized-length, non-terminating-chunk, and LCG-random bodies
      against `parse_execute_request` / `parse_reexecute_request` /
      `parse_auth_phase_one_request`).
- [D] Listener-level connection rate limit / max in-flight handshakes. A real
      Oracle listener has the same exposure; a firewall / LB in front is the
      primary answer. Optional hardening, not `0.0.1`.
- [x] `parse_execute_request` and friends: no `unwrap`/`expect` on
      attacker-controlled lengths (audited; all reads go through the checked
      `ReadBuffer`).
- [x] Marker / attention (Ctrl-C / OCIBreak): `race_break` runs each backend call
      against a concurrent TNS read; a Marker triggers `PostgresBackend::cancel`
      → SQLSTATE 57014 → ORA-01013 and is acknowledged.
- [x] Graceful shutdown: `run_with_listener` selects the accept loop against
      SIGINT/SIGTERM, stops accepting, and drains in-flight sessions up to
      `Config::shutdown_grace`.

### Observability & ops
- [~] Structured logs carry a session id/span for every connection-scoped event
      and never log SQL bind values or auth material. Listener/startup events are
      intentionally sessionless.
- [x] `/metrics` on `Config::health_addr` renders Prometheus text
      (`pgsaci_sessions_active`, `pgsaci_sessions_total`,
      `pgsaci_statements_total`, `pgsaci_backend_errors_total`).
- [x] `/healthz` + `/readyz` (dependency-free HTTP/1.1) on `Config::health_addr`.
- [x] TLS documented as plaintext-only for both links; a TLS-terminating network
      boundary is required if needed.
- [x] Config surface documented: `src/bin/pgsaci.rs` has the full flag/env table
      (listener, PG target, credential model, timeouts, health, oracle version);
      README covers the operational knobs.

---

## P2 — broader compatibility (post-`0.0.1`, or "unsupported" in the matrix)

- [x] **Packages** — documented unsupported (`COMPATIBILITY.md`). `rewrite_plsql`
      covers simple blocks / functions / procedures / triggers; package state,
      overloading, and the PL/SQL type system are a compiler-scale effort.
- [~] **Triggers** — `CREATE [OR REPLACE] TRIGGER` lowers to a PostgreSQL trigger
      function + trigger. Covered: `BEFORE`/`AFTER`, `INSERT`/`UPDATE [OF col]`/
      `DELETE` (incl. `OR`), row/statement level, `WHEN (...)`, `:NEW`/`:OLD`,
      `REFERENCING`, `INSTEAD OF` on views, `RAISE_APPLICATION_ERROR`, `IF`,
      numeric `FOR`. Corpus: `triggers.sql`. Not covered: compound triggers and
      package-scale PL/SQL trigger bodies.
- [x] `PIVOT` / `UNPIVOT`. Corpus: `pivot.sql`.
- [x] `PERCENTILE_CONT` / `PERCENTILE_DISC` / `CUME_DIST` / `NTH_VALUE` / `MEDIAN`
      / `RATIO_TO_REPORT`. Corpus: `ansi_window::*`.
- [x] `MODEL` clause — documented unsupported.
- [x] `MERGE ... WHEN MATCHED THEN UPDATE ... DELETE WHERE ...` — ordered
      PostgreSQL matched DELETE/UPDATE branches. (PG15+; PG13 corpus skips these.)
- [x] Flashback — documented unsupported.
- [x] `DBMS_SCHEDULER` / `DBMS_JOB` / AQ — documented unsupported. `DBMS_PIPE` /
      `DBMS_ALERT` come from orafce and route through.
- [x] Global / private temporary tables → `CREATE TEMPORARY TABLE IF NOT EXISTS
      … ON COMMIT {DELETE|PRESERVE} ROWS`. Corpus: `oracle_ddl::global_temporary_*`.
- [~] `CONNECT BY` extras: `NOCYCLE`, `CONNECT_BY_ISCYCLE` / `ISLEAF`, `ORDER
      SIBLINGS BY` work. `CONNECT_BY_ISCYCLE` without `NOCYCLE` and fully faithful
      mixed `WHERE` + sibling ordering are approximate. Corpus:
      `hierarchical::connect_by_is*`.
- [x] Recursive-CTE-vs-CONNECT-BY performance parity — not a feature; the `WITH
      RECURSIVE` lowering is what PostgreSQL runs.
- [x] `q'...'` uppercase `Q`, nested delimiters, all four bracket pairs, custom
      and newline delimiters. Corpus: `quoting_identifiers::q_quote_*`.
- [D] Character sets: AL32UTF8 / UTF-8 end to end (corpus covers ASCII + Latin +
      Cyrillic + emoji). A client that *requests* a single-byte session charset
      (`WE8ISO8859P1`, `WE8MSWIN1252`) is not transcoded — PgSaci assumes
      AL32UTF8 and reports charset id 873. The transcoding itself is bounded
      (Latin-1 / CP1252 mapping tables applied to the SQL text + string binds +
      string result columns), **but every thin client available here
      (python-oracledb, ojdbc, ODP.NET) is hardwired to AL32UTF8** — there is no
      way to exercise a single-byte session, and shipping an unvalidated
      transcoder risks silent data corruption. Deferred until an OCI / thick
      client that can negotiate a single-byte charset is available. Multi-byte
      legacy charsets stay out of scope.
- [x] Network compression, OOB break beyond the marker cancel, DRCP, Application
      Continuity — documented unsupported. Client prefetch/array size IS honoured.

---

## Compatibility matrix (publish with the release)

- [x] `COMPATIBILITY.md` records supported / partial / unsupported areas and
      points to the corpus as the executable compatibility claim.
- [x] README states the alpha scope and points at `COMPATIBILITY.md`.

## Definition of done for `0.0.1`

- [x] A large `SELECT` (≥ 1M rows) streams through without buffering the whole
      result in PgSaci and without a giant packet
      (`result_streaming::one_million_row_stream`).
- [x] Bind parameters are real typed parameters end to end (scalar and array).
- [x] `python-oracledb` (thin) **and** JDBC thin **and** ODP.NET can connect and
      run DML + SELECT + scalar & array binds + a PL/SQL call against a non-toy
      schema (the `clients/*` probes).
- [x] `max_connections` rejection maps to an ORA- session-limit code; vanished
      clients are reaped (keepalive + idle timeout).
- [x] 12c auth wired (and 11g for python-oracledb); per-user credential model
      documented.
- [x] `COMPATIBILITY.md` published; README states the alpha scope and the known
      unsupported list.
- [x] Corpus green (632/0); corpus groups for streaming, large results, typed
      binds, native temporal/interval encodings, and the ≥ 1M-row case.

### Deferred past `0.0.1` (documented in `COMPATIBILITY.md`)

- **PL/SQL OUT binds** (`BEGIN :x := … END`) and **`SYS_REFCURSOR`** — need a
  `DO` block wrapped as a returning function / a client-drivable cursor handle.
  (`RETURNING … INTO` OUT binds **are** done for python-oracledb thin.)
- **OUT binds for ojdbc / ODP.NET** — their OUT-bind response framing differs
  from python-oracledb's; those drivers get a clean ORA-03001 today.
- **TTC LOB locators** and single LOB values over ~64 MiB.
- **Native `BINARY_FLOAT`/`BINARY_DOUBLE` and `INTERVAL` for non-python drivers**
  — python-oracledb thin has them; oracle-rs 0.1.7 mis-decodes them and the
  ojdbc / ODP.NET column-describe framing for types 100/101/182/183 is not written.
- **`NUMBER(p,s)` metadata for ojdbc / ODP.NET** — their column-metadata parser
  desyncs on a non-zero scale field (python-oracledb and oracle-rs get the real
  precision/scale).
- **Single-byte request-charset transcoding**, a **warm backend-connection
  cache**, a **listener rate limit**, an **OCI thick-client handshake**, and
  **external auth** (Kerberos / wallet).
