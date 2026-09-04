#!/usr/bin/env bash
# Shared end-to-end harness for the client-compatibility probes.
#
#   clients/run.sh <python|java|dotnet> [oracle-version]
#
# Starts a real PostgreSQL+orafce container and a real DbSaci proxy, seeds the
# baseline schema, then runs the named client probe against it. `oracle-version`
# is passed through as DBSACI_ORACLE_VERSION (default: unset = 19c).
#
# Requires: docker, cargo, and the toolchain for the chosen client
# (python + `pip install oracledb`; a JDK; or the .NET SDK).
set -euo pipefail

client="${1:?usage: clients/run.sh <python|java|dotnet> [oracle-version]}"
oracle_version="${2:-}"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

pg_image="${DBSACI_TEST_PG_IMAGE:-dbsaci-test-pg:18}"
pg_major="${pg_image##*:}"
listen_port="${DBSACI_PORT:-15210}"
health_port="15280"
cid=""
dbsaci_pid=""

cleanup() {
  [ -n "$dbsaci_pid" ] && kill "$dbsaci_pid" 2>/dev/null || true
  [ -n "$cid" ] && docker rm -f "$cid" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if ! docker image inspect "$pg_image" >/dev/null 2>&1; then
  echo "== building $pg_image (postgres:${pg_major} + orafce) =="
  docker build --build-arg "PG_VERSION=${pg_major}" -t "$pg_image" "$root/testcontainers"
fi

echo "== starting postgres container =="
cid=$(docker run -d -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=postgres -P "$pg_image")
pg_port=$(docker inspect -f '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' "$cid")

echo "== waiting for postgres =="
# The official postgres image runs a throwaway server for its init scripts, then
# restarts. `pg_isready` (and a lone `psql`) can succeed against the throwaway
# one and then the socket vanishes mid-statement. Require several *consecutive*
# real connections over TCP so we only proceed once the final server is up.
ok=0
for _ in $(seq 1 120); do
  if docker exec "$cid" psql -h 127.0.0.1 -U postgres -tAc 'SELECT 1' postgres >/dev/null 2>&1; then
    ok=$((ok + 1))
    [ "$ok" -ge 5 ] && break
  else
    ok=0
  fi
  sleep 1
done

echo "== seeding baseline schema =="
# Retry the whole (idempotent) seed: DDL is transactional, so a connection drop
# mid-statement rolls back cleanly and the next attempt re-runs it.
seeded=0
for attempt in $(seq 1 15); do
  if docker exec -i "$cid" psql -h 127.0.0.1 -U postgres -v ON_ERROR_STOP=1 postgres <<'SQL'
CREATE EXTENSION IF NOT EXISTS orafce;
DROP ROLE IF EXISTS corpus;
CREATE ROLE corpus WITH LOGIN PASSWORD 'corpus' SUPERUSER;
DROP TABLE IF EXISTS people, teams CASCADE;
CREATE TABLE teams  (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, team_id INTEGER REFERENCES teams(id));
INSERT INTO teams  (id, name)          VALUES (1,'Engineering'),(2,'Sales'),(3,'Marketing');
INSERT INTO people (id, name, team_id) VALUES (1,'Ada',1),(2,'Grace',1),(3,'Linus',2),(4,'Margaret',NULL);
SQL
  then
    seeded=1
    break
  fi
  echo "   seed attempt $attempt failed; retrying"
  sleep 2
done
[ "$seeded" = 1 ] || { echo "!! could not seed schema" >&2; exit 1; }

echo "== building + starting dbsaci =="
cargo build --quiet --bin dbsaci
export DBSACI_LISTEN="127.0.0.1:${listen_port}"
export DBSACI_DB_HOST=127.0.0.1 DBSACI_DB_PORT="$pg_port"
export DBSACI_DB_NAME=postgres DBSACI_DB_PASSWORD=corpus
export DBSACI_HEALTH_ADDR="127.0.0.1:${health_port}"
# Mirror tests/corpus.rs: a 2 s per-statement cap so the corpus's
# `statement_timeout_is_user_cancel` case surfaces ORA-01013.
export DBSACI_STATEMENT_TIMEOUT_MS="${DBSACI_STATEMENT_TIMEOUT_MS:-2000}"
export RUST_LOG="${RUST_LOG:-dbsaci=info}"
[ -n "$oracle_version" ] && export DBSACI_ORACLE_VERSION="$oracle_version"
./target/debug/dbsaci > /tmp/dbsaci_stderr.log 2>&1 &
dbsaci_pid=$!

echo "== waiting for dbsaci /readyz =="
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:${health_port}/readyz" >/dev/null 2>&1 && break
  sleep 0.25
done

export DBSACI_HOST=127.0.0.1 DBSACI_PORT="$listen_port" \
       DBSACI_USER=corpus DBSACI_PASSWORD=corpus DBSACI_SERVICE=FREEPDB1

echo "== running $client probe =="
rc=0
case "$client" in
  python)
    "${PYTHON:-python}" "$root/clients/python/probe.py" || rc=$?
    ;;
  oci)
    "${PYTHON:-python}" "$root/clients/oci/probe.py" || rc=$?
    ;;
  oci-corpus)
    "${PYTHON:-python}" "$root/clients/oci/corpus_runner.py" || rc=$?
    ;;
  java)
    jar="$root/clients/java/lib/ojdbc11.jar"
    if [ ! -f "$jar" ]; then
      echo "== fetching ojdbc11 =="
      mkdir -p "$(dirname "$jar")"
      curl -fsSL -o "$jar" \
        "https://repo1.maven.org/maven2/com/oracle/database/jdbc/ojdbc11/23.5.0.24.07/ojdbc11-23.5.0.24.07.jar"
    fi
    case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) sep=';' ;; *) sep=':' ;; esac
    ( cd "$root/clients/java" \
        && javac -cp "lib/ojdbc11.jar" JdbcCompat.java \
        && java -cp "lib/ojdbc11.jar${sep}." JdbcCompat ) || rc=$?
    ;;
  dotnet)
    dotnet run --project "$root/clients/dotnet/Probe.csproj" -c Release || rc=$?
    ;;
  *)
    echo "unknown client '$client' (want python|oci|java|dotnet)" >&2
    exit 2
    ;;
esac
exit "$rc"
