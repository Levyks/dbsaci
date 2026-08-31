---
title: Getting started
description: Run pgSaci in front of PostgreSQL and connect an Oracle client to it.
---

pgSaci needs a PostgreSQL server with the [`orafce`](https://github.com/orafce/orafce)
extension available, and a PostgreSQL login role for the proxy to use. Below,
"the client" is any supported Oracle driver.

:::note[Getting the image / binary]
`docker pull levyks/pgsaci:0.0.1` — a **~10 MB** image (a static musl binary on
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
    image: levyks/pgsaci:0.0.1
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
  levyks/pgsaci:0.0.1
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

## Which Oracle version to claim

`PGSACI_ORACLE_VERSION` (`--oracle-version`) picks which release pgSaci claims to
be — `19` (default) or `11`. It changes the banner, the `AUTH_VERSION_*` values,
and the auth-verifier family so that both modern and 11g-era clients negotiate
successfully.

## Health & metrics

With `PGSACI_HEALTH_ADDR` set, pgSaci serves three dependency-free endpoints:

| Path | Purpose |
| --- | --- |
| `/healthz` | process is up |
| `/readyz` | backend PostgreSQL reachable |
| `/metrics` | Prometheus text format |
