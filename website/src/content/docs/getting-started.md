---
title: Getting started
description: Run pgSaci in front of PostgreSQL and connect an Oracle client to it.
---

pgSaci needs a PostgreSQL server with the [`orafce`](https://github.com/orafce/orafce)
extension available, and a PostgreSQL login role for the proxy to use. Below,
"the client" is any supported Oracle driver.

:::note[Getting the image / binary]
`docker pull levyks/pgsaci:0.0.9` — a **~12 MB** image (a static musl binary on
`scratch`; pgSaci has no C dependencies). It is also buildable from the repo's
[`Dockerfile`](https://github.com/Levyks/pgsaci/blob/main/Dockerfile), and
[building from source](#option-d--build-from-source) is a single `cargo build`.
Pre-built stand-alone binaries, when published, land on the
[GitHub releases page](https://github.com/Levyks/pgsaci/releases).
:::

## Option A — Docker Compose (proxy + database)

The fastest way to try it. This brings up PostgreSQL 18 with `orafce` and pgSaci
in front of it.

Build the PostgreSQL + `orafce` image once (or point `image:` at any PostgreSQL
image where `CREATE EXTENSION orafce` works):

```bash
docker build -t pgsaci-postgres:18 \
  https://github.com/Levyks/pgsaci.git#main:testcontainers --build-arg PG_VERSION=18
```

```yaml title="docker-compose.yml"
services:
  postgres:
    image: pgsaci-postgres:18
    environment:
      POSTGRES_PASSWORD: pgpw
      POSTGRES_DB: appdb
    ports: ["5432:5432"]

  pgsaci:
    image: levyks/pgsaci:0.0.9
    depends_on: [postgres]
    environment:
      PGSACI_LISTEN: 0.0.0.0:1521
      PGSACI_PG_HOST: postgres
      PGSACI_PG_PORT: "5432"
      PGSACI_PG_DB: appdb
      PGSACI_PG_PASSWORD: pgpw
      PGSACI_ORACLE_VERSION: "19"     # or "11"
      PGSACI_HEALTH_ADDR: 0.0.0.0:9500
    ports:
      - "1521:1521"   # Oracle listener
      - "9500:9500"   # /healthz /readyz /metrics
```

```bash
docker compose up -d
```

Then point any Oracle client at `//localhost:1521/FREEPDB1`, logging in with the
PostgreSQL role and password (here `postgres` / `pgpw` — see
[Credentials](#credentials-multi-user)).

## Option B — `docker run` against your own PostgreSQL

```bash
docker run --rm -p 1521:1521 -p 9500:9500 \
  -e PGSACI_LISTEN=0.0.0.0:1521 \
  -e PGSACI_PG_HOST=host.docker.internal \
  -e PGSACI_PG_PORT=5432 \
  -e PGSACI_PG_DB=appdb \
  -e PGSACI_PG_PASSWORD=pgpw \
  -e PGSACI_ORACLE_VERSION=19 \
  -e PGSACI_HEALTH_ADDR=0.0.0.0:9500 \
  levyks/pgsaci:0.0.9
```

Your PostgreSQL must have `orafce` installed in the target database
(`CREATE EXTENSION IF NOT EXISTS orafce;`).

## Option C — Binary from GitHub Releases

Download the archive for your platform from the
[releases page](https://github.com/Levyks/pgsaci/releases), extract it, and run:

```bash
./pgsaci \
  --listen 0.0.0.0:1521 \
  --pg-host 127.0.0.1 --pg-port 5432 \
  --pg-db appdb --pg-password pgpw \
  --oracle-version 19 \
  --health-addr 127.0.0.1:9500
```

Every option has both a CLI flag and a `PGSACI_*` environment variable; the flag
wins. Run `pgsaci --help` for the full list. See [Configuration](/pgsaci/configuration/).

## Option D — Build from source

```bash
git clone https://github.com/Levyks/pgsaci
cd pgsaci

# a PostgreSQL + orafce image for local use
docker build -t pgsaci-test-pg:18 testcontainers
docker run -d -e POSTGRES_PASSWORD=postgres -P pgsaci-test-pg:18

PGSACI_LISTEN=0.0.0.0:1521 \
PGSACI_PG_HOST=127.0.0.1 PGSACI_PG_PORT=<mapped-port> \
PGSACI_PG_DB=postgres PGSACI_PG_PASSWORD=postgres \
cargo run --release --bin pgsaci
```

Requires a stable Rust toolchain (2024 edition).

## Connecting a client

The service name in the connect string is cosmetic — use `FREEPDB1` (or
`XEPDB1`, or anything). The host/port are pgSaci's listener.

```python title="python-oracledb (thin)"
import oracledb
conn = oracledb.connect(user="appuser", password="apppw",
                        dsn="localhost:1521/FREEPDB1")
print(conn.cursor().execute("SELECT 'hello' FROM dual").fetchone())
```

```java title="Oracle JDBC thin"
var ds = new oracle.jdbc.pool.OracleDataSource();
ds.setURL("jdbc:oracle:thin:@//localhost:1521/FREEPDB1");
ds.setUser("appuser");
ds.setPassword("apppw");
try (var c = ds.getConnection();
     var rs = c.createStatement().executeQuery("SELECT sysdate FROM dual")) {
    rs.next();
    System.out.println(rs.getTimestamp(1));
}
```

```csharp title="ODP.NET managed"
using Oracle.ManagedDataAccess.Client;
using var c = new OracleConnection(
    "User Id=appuser;Password=apppw;Data Source=localhost:1521/FREEPDB1");
c.Open();
using var cmd = new OracleCommand("SELECT 1 FROM dual", c);
Console.WriteLine(cmd.ExecuteScalar());
```

## Credentials (multi-user)

An Oracle login is a challenge/response — the password never crosses the wire —
so pgSaci must already hold each user's PostgreSQL password. Declare them up
front, and an Oracle client then authenticates with the **same** user/password it
would use against PostgreSQL directly:

```bash
pgsaci \
  --pg-user alice:s3cret --pg-user bob:hunter2 \   # repeatable, CLI only
  --pg-users-file /etc/pgsaci/users              \ # file of user:password lines
  --pg-password postgres                           # fallback for anyone not listed
```

Environment equivalents: `PGSACI_PG_USERS="alice:s3cret,bob:hunter2"`,
`PGSACI_PG_USERS_FILE=...`, `PGSACI_PG_PASSWORD=...`.

Sources layer **file &lt; `PGSACI_PG_USERS` &lt; `--pg-user`**. The username is
matched case-insensitively; a user with no match and no fallback is rejected with
`ORA-01017`. The matched password drives both the login challenge and the backend
PostgreSQL connection.

## Schemas

pgSaci follows Oracle's **schema == user** model. On connect it ensures a
PostgreSQL schema named after the user exists (`CREATE SCHEMA IF NOT EXISTS
"<user>"`) and sets `search_path` to `"<user>", oracle, public`:

- unqualified `CREATE TABLE` / `SELECT` resolve in the user's own schema first;
- other users' schemas are reached by qualifying (`SELECT * FROM hr.employees`,
  with the usual `GRANT USAGE ON SCHEMA` / `GRANT SELECT`), exactly as in
  Oracle;
- `ALTER SESSION SET CURRENT_SCHEMA = hr` redirects unqualified resolution;
- **`public`** is the shared fallback — objects there resolve unqualified for
  every user, and are always reachable explicitly as `public.<name>`. Point
  pgSaci at an existing PostgreSQL database whose tables live in `public` and
  they work unchanged.

If the backend role lacks `CREATE` on the database the connection still
succeeds; objects then resolve via `oracle` / `public` and a warning is logged.

## Provisioning (read-only / least-privilege roles)

pgSaci installs a set of **global** objects once: the `pgsaci` and `sys`
schemas, the `SYS.*` catalog views, and `public.*` / `dbms_*` helper functions.
Creating them needs `CREATE` on the database and on `public` — more than a
read-only integration role has.

It is a one-time install, not per-session. When those objects are already
present at the running version, **every login role skips the install entirely
and just uses them** — and the installing connection grants read / execute on
them to `PUBLIC`, so an unprivileged role needs no per-role grants.

So for a least-privilege setup: make **one** connection with a role that can
create them (any superuser, or a role with `CREATE` on the database + `public`),
then point your read-only roles at pgSaci. If the objects are missing and the
connecting role cannot create them, pgSaci logs one actionable line and the
session still serves plain queries (`orafce` functions included) — only the
`SYS.*` catalog and a few helpers are unavailable until an admin provisions.

`PGSACI_ORACLE_VERSION` (`--oracle-version`) picks which release pgSaci claims to
be — `19` (default) or `11`. It changes the banner, the `AUTH_VERSION_*` values,
and the auth-verifier family so that both modern and 11g-era clients negotiate
successfully.

## Health & metrics

With `PGSACI_HEALTH_ADDR` set, pgSaci serves these dependency-free endpoints:

| Path | Purpose |
| --- | --- |
| `GET /healthz` | process is up |
| `GET /readyz` | backend PostgreSQL reachable |
| `GET /metrics` | Prometheus text format |
| `GET /sessions` | JSON list of live Oracle sessions — `id`, `addr`, `user`, `age_seconds` |
| `DELETE /sessions/<id>` | abort one session (drops its backend PostgreSQL connection, releasing any locks it held) |

Idle clients are reaped after `--idle-timeout-ms` (`PGSACI_IDLE_TIMEOUT_MS`,
default 15 min; `0` disables). The effective value is logged at startup.

## Preflight diagnostics

On its first backend connection pgSaci logs a one-time check of the target
database: whether `orafce` is installed, the effective `search_path`, and
whether the login user's schema contains any upper/mixed-case identifiers (see
below). A missing assumption shows up here as one line instead of an opaque
PostgreSQL error later.

## Identifier case

PostgreSQL folds unquoted identifiers to **lower** case; Oracle folds them to
**UPPER**. pgSaci bridges this by mapping an Oracle client's quoted ALL-CAPS
name (`"EMPLOYEES"`) to its lower-case form (`"employees"`) — so it lines up
with a PostgreSQL schema that uses ordinary lower-case identifiers, which is the
normal PostgreSQL convention. Quoted **mixed**-case names (`"MixedCase"`) stay
case-sensitive on both sides, as they are in Oracle.

The practical rule: **use lower-case identifiers in the target schema.** A
table created in PostgreSQL as `"Employees"` (quoted, mixed case) is not
reachable from an Oracle client, exactly as it would not be in Oracle itself.

## Time zones

pgSaci follows Oracle's model. `ALTER SESSION SET TIME_ZONE = …` sets the
session zone — a named IANA region (`'America/Sao_Paulo'`), a fixed offset
(`'-03:00'`), or `'local'`. `SESSIONTIMEZONE`, `DBTIMEZONE` and
`SYS_CONTEXT('USERENV', 'SESSIONTIMEZONE')` report it. `CURRENT_TIMESTAMP` /
`CURRENT_DATE` are in the session zone; `SYSTIMESTAMP` / `SYSDATE` are in the
database zone (UTC).

PostgreSQL `timestamptz` columns are converted to the session zone on the wire
— with per-instant, DST-correct offsets for named regions — so a view returning
one no longer needs an explicit `AT TIME ZONE` / `::timestamp` cast, and the
Oracle JDBC thin driver does not need `oracle.jdbc.timezoneAsRegion=false`. PG
`timestamp` (no zone) is returned unchanged.
