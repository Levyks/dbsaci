---
title: What does not work
description: Split into "not yet — planned" and "structurally impossible on stock PostgreSQL + orafce".
---

Two categories: missing work that is on the roadmap, and things stock PostgreSQL
with `orafce` cannot do at all.

## Not yet — planned

Expressible on PostgreSQL, but the wire framing or translation is not written.

### Likely to bite first

- **Package-scale PL/SQL.** Packages (state, overloading, the PL/SQL type
  system), `BULK COLLECT` / `FORALL`, pipelined functions, and compound
  triggers. Anonymous blocks, standalone routines, and simple triggers translate
  — but the further a body strays from that, the more likely the translation is
  to be *wrong* rather than merely rejected.
- **OUT binds and `SYS_REFCURSOR`.** Scalar and array *input* binds pass through.
  `RETURNING <cols> INTO :out` DML works for **`python-oracledb` thin**
  (INSERT / UPDATE). PL/SQL OUT parameters (`BEGIN :x := … END`) and
  `SYS_REFCURSOR` are not implemented; ojdbc and ODP.NET get `ORA-03001` for
  `RETURNING … INTO` rather than a silent drop.
- **LOB streaming.** `CLOB` / `BLOB` values are delivered inline and capped at
  one ~64 MiB TTC packet. No TTC LOB locators, no multi-gigabyte streaming.
  `DBMS_LOB.GETLENGTH / SUBSTR / INSTR` exist as plain SQL functions.

### Type-metadata gaps

- **Declared `NUMBER` precision/scale in result metadata.** Reported for
  `python-oracledb` thin and `oracle-rs`; **ojdbc and ODP.NET** keep `(38,0)`
  (their column-metadata parser desyncs on a non-zero scale field). Values are
  always exact.
- **Native `TIMESTAMP` / `TIMESTAMP WITH TIME ZONE` describe for ojdbc and
  ODP.NET.** Those two drivers see PostgreSQL `timestamp` / `timestamptz`
  described as Oracle `DATE` (second precision) — exact for an Oracle `DATE`
  column (the common case), but an Oracle `TIMESTAMP(n>0)` loses its sub-second
  digits. `python-oracledb` thin keeps the native types with sub-second
  precision.
- **Native TTC encodings for `INTERVAL`, `BINARY_FLOAT` / `BINARY_DOUBLE`.**
  Complete for `python-oracledb` thin; other drivers get `NUMBER` / an
  Oracle-style interval text rendering.

### Other planned items

- **OCI thick-client handshake.** The Instant Client loads but the TNS/TTC
  negotiation differs from the thin drivers (`ORA-12592`); it needs its own
  reverse-engineering pass.
- **Non-UTF-8 wire character sets.** The server side is `AL32UTF8` only.
  Single-byte request-charset transcoding (`WE8ISO8859P1`, `WE8MSWIN1252`) is
  bounded but unvalidated — no non-UTF-8 thin client exists to test against.
- **External auth** (Kerberos / wallet / proxy auth), a **warm
  backend-connection cache**, and a **listener connection rate limit**.
- **Full Oracle implicit type conversion / NLS behaviour**, autonomous
  transactions, XA.

## Structurally impossible on stock PostgreSQL + `orafce`

Out of scope — the engine underneath can't express these:

- **Flashback** (`AS OF SCN|TIMESTAMP`, `VERSIONS BETWEEN`) — no time-travel
  storage.
- **`MODEL` clause** — no spreadsheet/array computation primitive.
- **Full PL/SQL packages** — package state, overloading, and the PL/SQL type
  system are a compiler-scale effort, not a translation.
- **`DBMS_SCHEDULER` / `DBMS_JOB`** — no in-core job scheduler.
- **Advanced Queueing (AQ)** — no equivalent messaging substrate.
- **XA / distributed transactions**, **DRCP**, **Application Continuity**,
  **network compression**, **client-side result cache** — Oracle transport /
  session features with no PostgreSQL counterpart.

`DBMS_PIPE` / `DBMS_ALERT` come from `orafce` and pass through unchanged.
