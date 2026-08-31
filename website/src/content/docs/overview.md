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
server* that builds Oracle SQL / PL-SQL compatibility — an Oracle parse mode,
PL/iSQL, packages, `NUMBER`/`DATE` semantics, `DUAL` — **into the database
engine**. It is far deeper than pgSaci on the *dialect*. But IvorySQL speaks the
**PostgreSQL wire protocol**: your application connects to it with a PostgreSQL
driver. An unmodified Oracle client (ojdbc, ODP.NET, `python-oracledb`, an
OCI-linked tool) cannot connect to IvorySQL at all.

pgSaci is the inverse trade. It speaks Oracle's **TNS/TTC wire protocol**, so the
Oracle client connects exactly as it would to Oracle — no driver swap, no
connection-string rewrite — while the database behind it is an ordinary,
unmodified PostgreSQL. The cost is coverage: pgSaci only supports the slice of
Oracle SQL / PL-SQL its translation layer plus `orafce` can express.

Use IvorySQL if you can repoint the application at a new database and want broad
Oracle SQL semantics. Use pgSaci if the application **and its Oracle driver** have
to stay exactly as they are.

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

Each wire-level behaviour is chosen from the capabilities the client negotiates
in the TTC handshake. See [How it works](/pgsaci/how-it-works/).
