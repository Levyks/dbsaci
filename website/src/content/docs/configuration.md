---
title: Configuration
description: Every pgSaci option — CLI flag, environment variable, and default.
---

Every option has both a **CLI flag** and a **`PGSACI_*` environment variable**.
The flag wins when both are set. Run `pgsaci --help` for the authoritative list.

## Core options

| Flag | Env | Default | Purpose |
| --- | --- | --- | --- |
| `--listen <ADDR>` | `PGSACI_LISTEN` | `0.0.0.0:1521` | Oracle TNS listen address. |
| `--pg-host <HOST>` | `PGSACI_PG_HOST` | `localhost` | Backend PostgreSQL host. |
| `--pg-port <PORT>` | `PGSACI_PG_PORT` | `5432` | Backend PostgreSQL port. |
| `--pg-db <NAME>` | `PGSACI_PG_DB` | `postgres` | Backend database. Must have `orafce` installed. |
| `--oracle-version <V>` | `PGSACI_ORACLE_VERSION` | `19` | Impersonated Oracle release: `11` / `11g` / `11.2`, or `19` / `19c`. Changes the banner, `AUTH_VERSION_*`, and the auth-verifier family. |
| `--health-addr <ADDR>` | `PGSACI_HEALTH_ADDR` | *(off)* | `host:port` for `/healthz` + `/readyz` + `/metrics`. Unset → no health server. |

## Timeouts & lifecycle

| Flag | Env | Default | Purpose |
| --- | --- | --- | --- |
| `--statement-timeout-ms <MS>` | `PGSACI_STATEMENT_TIMEOUT_MS` | *(none)* | Per-statement cap; `ORA-01013` on expiry. |
| `--idle-timeout-ms <MS>` | `PGSACI_IDLE_TIMEOUT_MS` | `900000` (15 min) | Idle-client reaping. `0` disables. |
| `--shutdown-grace-ms <MS>` | `PGSACI_SHUTDOWN_GRACE_MS` | `30000` | Drain window after `SIGINT` / `SIGTERM`. |

## Credentials

An Oracle login is a challenge/response — the password never crosses the wire —
so pgSaci must already hold each user's PostgreSQL password.

| Flag | Env | Purpose |
| --- | --- | --- |
| `--pg-user <USER:PASSWORD>` | *(CLI only)* | One pair. **Repeatable.** Highest precedence. |
| `--pg-users <LIST>` | `PGSACI_PG_USERS` | Comma-separated `user:password,user:password`. |
| `--pg-users-file <PATH>` | `PGSACI_PG_USERS_FILE` | File of `user:password` lines (`#` comments and blank lines ignored). |
| `--pg-password <PASSWORD>` | `PGSACI_PG_PASSWORD` | Fallback for any user not named above. Default fallback: `postgres`. |

Sources layer **file &lt; `PGSACI_PG_USERS` &lt; `--pg-user`**. The Oracle
username is matched case-insensitively; the matched password drives both the
login challenge and the backend PostgreSQL connection. An unmatched user with no
fallback is rejected with `ORA-01017`.

## Logging & debug

pgSaci uses `tracing`; set **`RUST_LOG`** (e.g. `RUST_LOG=pgsaci=debug`). Default
is `pgsaci=info`.

| Env | Effect |
| --- | --- |
| `RUST_LOG` | Standard `tracing` / `env_filter` directive. |
| `PGSACI_LOG_SQL` | Log each Oracle statement and its translated PostgreSQL form. |
| `PGSACI_WIRE_DUMP` | Hex-dump every TNS packet (very verbose; secrets in `DATA` payloads are still redacted). |
| `PGSACI_OCI_DEBUG` | Extra tracing on the OCI-dialect execute/re-execute path. |

## Health endpoints

Served on `PGSACI_HEALTH_ADDR` when set:

| Path | Meaning |
| --- | --- |
| `/healthz` | Process is up. |
| `/readyz` | Backend PostgreSQL is reachable. |
| `/metrics` | Prometheus text format (connection, statement, and error counters). |

## Sizing

**One Oracle session maps to one dedicated PostgreSQL connection.** Size
PostgreSQL `max_connections` for your expected concurrency, or put a session
pooler (PgBouncer in transaction mode is *not* safe here — pgSaci holds
session-level state; use a session-mode pooler) between pgSaci and PostgreSQL.
