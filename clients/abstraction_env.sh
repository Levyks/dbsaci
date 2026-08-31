#!/usr/bin/env bash
# Bring up a persistent PostgreSQL + pgSaci pair for the `abstraction.mdc`
# Spring integration suite to run against (instead of a real Oracle container).
#
#   bash clients/abstraction_env.sh up      # start PG + pgSaci, seed app role
#   bash clients/abstraction_env.sh down     # stop + remove
#   bash clients/abstraction_env.sh psql ... # psql into the backing PG
#
# Coordinates the Java side must use (see ApplicationTests):
#   host 127.0.0.1  port 15301  sid/service XE  user hexing  pass hexing
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pg_image="pgsaci-test-pg:18"
cname="abstraction-pg"
listen_port="${ABS_PGSACI_PORT:-15301}"
health_port="${ABS_PGSACI_HEALTH:-15302}"
state_dir="$root/target/abstraction-env"
mkdir -p "$state_dir"

cmd="${1:-up}"; shift || true

pg_cid() { docker ps -aqf "name=^${cname}$"; }

case "$cmd" in
up)
  if [ -z "$(pg_cid)" ]; then
    echo "== starting postgres ($pg_image) =="
    docker run -d --name "$cname" -e POSTGRES_PASSWORD=postgres \
      -e POSTGRES_DB=postgres -P "$pg_image" >/dev/null
  else
    docker start "$cname" >/dev/null || true
  fi
  cid="$(pg_cid)"
  pg_port="$(docker inspect -f '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' "$cid")"
  echo "== waiting for postgres =="
  for _ in $(seq 1 60); do
    docker exec "$cid" pg_isready -U postgres >/dev/null 2>&1 && break
    sleep 0.5
  done
  echo "== seeding app role =="
  docker exec -i "$cid" psql -U postgres -v ON_ERROR_STOP=1 postgres <<'SQL'
CREATE EXTENSION IF NOT EXISTS orafce;
DO $$BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='hexing') THEN
    CREATE ROLE hexing WITH LOGIN PASSWORD 'hexing' SUPERUSER;
  END IF;
END$$;
SQL

  # kill a previous pgsaci for this env *before* the build (it holds the .exe)
  if [ -f "$state_dir/pgsaci.pid" ]; then
    kill "$(cat "$state_dir/pgsaci.pid")" 2>/dev/null || true
    sleep 1
  fi
  # belt-and-braces: any stray pgsaci bound to our port
  ( MSYS_NO_PATHCONV=1 taskkill //F //IM pgsaci.exe >/dev/null 2>&1 ) || true
  sleep 0.5

  echo "== building pgsaci (release) =="
  ( cd "$root" && cargo build --release --quiet --bin pgsaci )

  echo "== starting pgsaci on 127.0.0.1:${listen_port} -> PG :${pg_port} =="
  PGSACI_LISTEN="127.0.0.1:${listen_port}" \
  PGSACI_PG_HOST=127.0.0.1 PGSACI_PG_PORT="$pg_port" \
  PGSACI_PG_DB=postgres PGSACI_PG_PASSWORD=hexing \
  PGSACI_HEALTH_ADDR="127.0.0.1:${health_port}" \
  PGSACI_ORACLE_VERSION=11 \
  PGSACI_LOG_SQL="${PGSACI_LOG_SQL:-}" \
  RUST_LOG="${RUST_LOG:-pgsaci=info}" \
    "$root/target/release/pgsaci" > "$state_dir/pgsaci.log" 2>&1 &
  echo $! > "$state_dir/pgsaci.pid"
  echo "$pg_port" > "$state_dir/pg_port"

  echo "== waiting for pgsaci /readyz =="
  for _ in $(seq 1 40); do
    curl -fsS "http://127.0.0.1:${health_port}/readyz" >/dev/null 2>&1 && break
    sleep 0.25
  done

  echo "== loading abstraction.mdc schema (data.sql -> pgSaci) =="
  PGSACI_HOST=127.0.0.1 PGSACI_PORT="$listen_port" PGSACI_SERVICE=XE \
  PGSACI_USER=hexing PGSACI_PASS=hexing \
    "${PYTHON:-python}" "$root/clients/abstraction_schema.py"

  echo "READY  pgsaci=127.0.0.1:${listen_port}  pg_container=${cname}  pg_host_port=${pg_port}"
  ;;

down)
  if [ -f "$state_dir/pgsaci.pid" ]; then
    kill "$(cat "$state_dir/pgsaci.pid")" 2>/dev/null || true
    rm -f "$state_dir/pgsaci.pid"
  fi
  [ -n "$(pg_cid)" ] && docker rm -f "$cname" >/dev/null || true
  echo "down"
  ;;

psql)
  cid="$(pg_cid)"
  docker exec -i "$cid" psql -U postgres postgres "$@"
  ;;

log)
  tail -n "${1:-50}" "$state_dir/pgsaci.log"
  ;;

*)
  echo "usage: $0 {up|down|psql|log}" >&2; exit 2
  ;;
esac
