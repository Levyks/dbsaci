---
title: What pgSaci is
description: The project overview — what pgSaci does, what it is not, and what you need to run it.
---

pgSaci is a proxy that speaks Oracle's **TNS/TTC wire protocol** on the front and
**stock PostgreSQL** on the back. An unmodified Oracle client points at pgSaci
instead of at an Oracle database, and pgSaci:

1. terminates the Oracle client handshake and authentication,
2. translates the Oracle SQL dialect it understands into PostgreSQL SQL,
3. runs the query against a normal PostgreSQL server that has the
   [`orafce`](https://github.com/orafce/orafce) extension installed,
4. re-encodes the PostgreSQL result back into Oracle's binary framing.

The application keeps its Oracle driver, its connection string shape, and most of
its SQL. Nothing about the database it actually talks to is Oracle.

:::caution[Status: very early development. Mostly written by AI.]
There are hundreds of passing end-to-end tests, but this has never run a real
workload. **Do not point it at data you care about.** Bug reports, failing-case
contributions, and design feedback are very welcome.
:::

## What pgSaci is not

It is **not an Oracle database implementation**. It supports the slice of Oracle
that its SQL translation layer plus `orafce` can express — see
[What works](/pgsaci/what-works/) and [Limitations](/pgsaci/limitations/).

### How this differs from IvorySQL

[IvorySQL](https://github.com/IvorySQL/IvorySQL) is a *fork of the PostgreSQL
server* that builds Oracle compatibility — an Oracle parse mode, PL/iSQL,
packages, `NUMBER`/`DATE` semantics, `DUAL` — **into the database engine**. You
deploy IvorySQL as your database.

pgSaci changes nothing in the database. It is a **man-in-the-middle process** in
front of an ordinary, current PostgreSQL release. That buys you a smaller blast
radius (your DB is just Postgres) and lets Oracle *drivers* connect unchanged,
but it also means pgSaci only supports what its translation layer plus `orafce`
can express — a much narrower slice of Oracle than IvorySQL's engine-level
compatibility.

If you want broad Oracle SQL/PL-SQL fidelity, use IvorySQL. If you specifically
need Oracle *clients* to connect to *plain PostgreSQL*, that is what pgSaci is
for.

## Requirements

- **PostgreSQL** (a recent release) with the **`orafce`** extension available
  (`CREATE EXTENSION orafce`). A ready-to-use image is built from
  [`testcontainers/Dockerfile`](https://github.com/Levyks/pgsaci/blob/main/testcontainers/Dockerfile).
- A login role on PostgreSQL for the proxy to connect as. **One Oracle session
  maps to one dedicated PostgreSQL connection**, so size `max_connections` (or
  put a session pooler behind pgSaci) accordingly.
- Transport is **plaintext on both sides** today. Run pgSaci behind a
  TLS-terminating boundary if you need encryption in transit.
- To build from source: **Rust** (stable, 2024 edition).

## Which clients

Verified end to end (connect, auth, metadata, parametrised `SELECT`/DML, array
binds, rollback, multi-thousand-row fetch loop) against both the 19c and 11g
personas:

| Client | Mode | Status |
| --- | --- | --- |
| `python-oracledb` | thin | Supported |
| Oracle JDBC thin driver (ojdbc8 / ojdbc11) | thin | Supported |
| ODP.NET (`Oracle.ManagedDataAccess.Core`) | managed | Supported |
| `oracle-rs` | thin | Supported |
| Instant Client / OCI thick | thick | Handshake differs (`ORA-12592`) — [not yet](/pgsaci/limitations/) |

pgSaci chooses every wire-level behaviour from the capabilities the client
negotiates in the TTC handshake, the way a real Oracle server does — never from a
driver-name string. See [How it works](/pgsaci/how-it-works/).
