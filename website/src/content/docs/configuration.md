---
title: Configuration
description: Every dbSaci option — CLI flag, environment variable, and default.
---

Every option has both a **CLI flag** and a **`DBSACI_*` environment variable**.
The flag wins when both are set. Run `dbsaci --help` for the authoritative list.

## Core options

| Flag | Env | Default | Purpose |
| --- | --- | --- | --- |
| `--listen <ADDR>` | `DBSACI_LISTEN` | `0.0.0.0:1521` | Oracle TNS listen address. |
| `--backend mariadb` | `DBSACI_BACKEND=mariadb` | `postgres` | Select MariaDB instead of PostgreSQL. MariaDB 11.4+ should use `SQL_MODE=ORACLE`. |
| `--pg-host <HOST>` | `DBSACI_PG_HOST` | `localhost` | Selected backend host (the flag name is retained for compatibility). |
| `--pg-port <PORT>` | `DBSACI_PG_PORT` | `5432` | Selected backend port. |
| `--pg-db <NAME>` | `DBSACI_PG_DB` | `postgres` | Selected backend database/schema. PostgreSQL requires `orafce`. |
| `--oracle-version <V>` | `DBSACI_ORACLE_VERSION` | `19` | Impersonated Oracle release: `11` / `11g` / `11.2`, or `19` / `19c`. Changes the banner, `AUTH_VERSION_*`, and the auth-verifier family. |
| `--health-addr <ADDR>` | `DBSACI_HEALTH_ADDR` | *(off)* | `host:port` for `/healthz` + `/readyz` + `/metrics`. Unset → no health server. |

## Timeouts & lifecycle

| Flag | Env | Default | Purpose |
| --- | --- | --- | --- |
| `--statement-timeout-ms <MS>` | `DBSACI_STATEMENT_TIMEOUT_MS` | *(none)* | Per-statement cap; `ORA-01013` on expiry. |
| `--idle-timeout-ms <MS>` | `DBSACI_IDLE_TIMEOUT_MS` | `900000` (15 min) | Idle-client reaping. `0` disables. |
| `--shutdown-grace-ms <MS>` | `DBSACI_SHUTDOWN_GRACE_MS` | `30000` | Drain window after `SIGINT` / `SIGTERM`. |

## Credentials

An Oracle login is a challenge/response — the password never crosses the wire —
so dbSaci must already hold each user's backend password.

| Flag | Env | Purpose |
| --- | --- | --- |
| `--pg-user <USER:PASSWORD>` | *(CLI only)* | One pair. **Repeatable.** Highest precedence. |
| `--pg-users <LIST>` | `DBSACI_PG_USERS` | Comma-separated `user:password,user:password`. |
| `--pg-users-file <PATH>` | `DBSACI_PG_USERS_FILE` | File of `user:password` lines (`#` comments and blank lines ignored). |
| `--pg-password <PASSWORD>` | `DBSACI_PG_PASSWORD` | Fallback for any user not named above. Default fallback: `postgres`. |

Sources layer **file &lt; `DBSACI_PG_USERS` &lt; `--pg-user`**. The Oracle
username is matched case-insensitively; the matched password drives both the
login challenge and the selected backend connection. An unmatched user with no
fallback is rejected with `ORA-01017`.

## Logging & debug

dbSaci uses `tracing`; set **`RUST_LOG`** (e.g. `RUST_LOG=dbsaci=debug`). Default
is `dbsaci=info`.

| Env | Effect |
| --- | --- |
| `RUST_LOG` | Standard `tracing` / `env_filter` directive. |
| `DBSACI_LOG_SQL` | Log each Oracle statement and its translated PostgreSQL form. |
| `DBSACI_WIRE_DUMP` | Hex-dump every TNS packet (very verbose; secrets in `DATA` payloads are still redacted). |
| `DBSACI_OCI_DEBUG` | Extra tracing on the OCI-dialect execute/re-execute path. |

## Health endpoints

Served on `DBSACI_HEALTH_ADDR` when set:

| Path | Meaning |
| --- | --- |
| `/healthz` | Process is up. |
| `/readyz` | The selected backend is reachable. |
| `/metrics` | Prometheus text format (connection, statement, and error counters). |

## Sizing

**One Oracle session maps to one dedicated backend connection.** Size the
selected backend's connection capacity for your expected concurrency, or put a session
pooler (PgBouncer in transaction mode is *not* safe here — dbSaci holds
session-level state; use a session-mode pooler) between dbSaci and PostgreSQL.
