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
| `--backend mariadb` | `DBSACI_BACKEND=mariadb` | `postgres` | Select MariaDB (11.4+) instead of PostgreSQL. dbSaci sets `SQL_MODE=ORACLE` per session — no server config needed. |
| `--db-host <HOST>` | `DBSACI_DB_HOST` | `localhost` | Backend host (PostgreSQL or MariaDB). |
| `--db-port <PORT>` | `DBSACI_DB_PORT` | `5432` | Backend port (`5432` PostgreSQL, `3306` MariaDB). |
| `--db-name <NAME>` | `DBSACI_DB_NAME` | `postgres` | Backend database. Shared fallback schema when a login has no database of its own name. PostgreSQL requires `orafce`. |
| `--oracle-version <V>` | `DBSACI_ORACLE_VERSION` | `19` | Impersonated Oracle release: `11` / `11g` / `11.2`, or `19` / `19c`. Changes the banner, `AUTH_VERSION_*`, and the auth-verifier family. |
| `--identifier-case <upper\|lower>` | `DBSACI_IDENTIFIER_CASE` | `upper` | MariaDB only: folds table identifiers to this case (AST-based) so `lower_case_table_names` does not matter. `upper` matches Oracle's own unquoted-identifier behaviour. No effect on PostgreSQL, which folds unquoted identifiers itself. |
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
| `--db-user <USER:PASSWORD>` | *(CLI only)* | One pair. **Repeatable.** Highest precedence. |
| `--db-users <LIST>` | `DBSACI_DB_USERS` | Comma-separated `user:password,user:password`. |
| `--db-users-file <PATH>` | `DBSACI_DB_USERS_FILE` | File of `user:password` lines (`#` comments and blank lines ignored). |
| `--db-password <PASSWORD>` | `DBSACI_DB_PASSWORD` | Fallback password for any user not named above. **No built-in default** — omit this and unnamed users get `ORA-01017`. |
| `--health-token <TOKEN>` | `DBSACI_HEALTH_TOKEN` | Required for `GET`/`DELETE /sessions` when `--health-addr` is not loopback. |
| `--tls-cert <PATH>` | `DBSACI_TLS_CERT` | PEM certificate to wrap the TNS listener in TLS (TCPS). |
| `--tls-key <PATH>` | `DBSACI_TLS_KEY` | PEM private key pairing `--tls-cert`. |
| `--db-ssl` | `DBSACI_DB_SSL` | Require TLS when connecting to the backend. |

Sources layer **file &lt; `DBSACI_DB_USERS` &lt; `--db-user`**. The Oracle
username is matched case-insensitively; the matched password drives both the
login challenge and the selected backend connection. An unmatched user with no
fallback is rejected with `ORA-01017`.

## Logging & debug

dbSaci uses `tracing`; set **`RUST_LOG`** (e.g. `RUST_LOG=dbsaci=debug`). Default
is `dbsaci=info`.

| Env | Effect |
| --- | --- |
| `RUST_LOG` | Standard `tracing` / `env_filter` directive. |
| `DBSACI_LOG_SQL` | Log each Oracle statement and its translated backend form. |
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
pooler (a transaction-mode pooler such as PgBouncer is *not* safe here — dbSaci
holds session-level state; use a session-mode pooler) between dbSaci and the
backend.
