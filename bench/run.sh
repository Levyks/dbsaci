#!/usr/bin/env bash
# Benchmark orchestrator: run the identical workload (bench/workload.py,
# python-oracledb thin) against
#   1. a real Oracle XE 21c instance  (gvenzl/oracle-xe:21-slim)
#   2. PostgreSQL fronted by pgSaci    (testcontainers image + ./target/release/pgsaci)
# and print side-by-side latency + throughput tables.
#
# Both engines get 2 CPUs. Oracle XE 21c is licence-capped at 2 CPU threads and
# 2 GiB of *database* RAM; we give its container 3 GiB (2 GiB engine + OS / redo
# / server-process headroom, without which it OOM-kills at 2 GiB) and give the
# PostgreSQL container 2 GiB (it does not pre-allocate). Override with
# BENCH_CPUS / BENCH_ORA_MEM / BENCH_PG_MEM. The pgSaci proxy runs unconstrained
# on the host — it is the overhead under test, not a competitor.
#
#   bench/run.sh                          # full run
#   BENCH_ITERS=500 BENCH_HEAVY_ITERS=10 bench/run.sh   # quicker
#
# Requires: docker, cargo, python + `pip install oracledb`.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

py="${PYTHON:-python}"
ora_image="gvenzl/oracle-xe:21-slim"
pg_image="${PGSACI_TEST_PG_IMAGE:-pgsaci-test-pg:18}"
ora_port="${BENCH_ORA_PORT:-15521}"
pg_port="${BENCH_PG_PORT:-15543}"
cpus="${BENCH_CPUS:-2}"
ora_mem="${BENCH_ORA_MEM:-3g}"
pg_mem="${BENCH_PG_MEM:-2g}"
ora_limits=(--cpus="$cpus" --memory="$ora_mem" --memory-swap="$ora_mem")
pg_limits=(--cpus="$cpus" --memory="$pg_mem" --memory-swap="$pg_mem")
outdir="$(mktemp -d)"

# Workload knobs forwarded to bench/workload.py (defaults live there too).
export BENCH_USER=bench BENCH_PASSWORD=bench
export BENCH_ITERS="${BENCH_ITERS:-2000}"
export BENCH_WARMUP="${BENCH_WARMUP:-200}"
export BENCH_HEAVY_ITERS="${BENCH_HEAVY_ITERS:-30}"
export BENCH_HEAVY_WARMUP="${BENCH_HEAVY_WARMUP:-3}"
export BENCH_BIG_ROWS="${BENCH_BIG_ROWS:-100000}"

ora_cid="" pg_cid="" pgsaci_pid=""
cleanup() {
  [ -n "$pgsaci_pid" ] && kill "$pgsaci_pid" 2>/dev/null || true
  [ -n "$ora_cid" ] && docker rm -f "$ora_cid" >/dev/null 2>&1 || true
  [ -n "$pg_cid" ] && docker rm -f "$pg_cid" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ---------------------------------------------------------------- Oracle XE ---
echo "== starting Oracle XE ($ora_image), 2 CPU — first boot takes a few minutes =="
ora_cid=$(docker run -d "${ora_limits[@]}" -p "${ora_port}:1521" \
  -e ORACLE_PASSWORD=bench -e APP_USER=bench -e APP_USER_PASSWORD=bench \
  "$ora_image")
sleep 2
[ "$(docker inspect -f '{{.State.Status}}' "$ora_cid")" = running ] || {
  echo "Oracle XE container did not start"; docker logs --tail 40 "$ora_cid"; exit 1;
}

echo "== waiting for Oracle XE ('DATABASE IS READY TO USE!' in the log) =="
ready=0
for _ in $(seq 1 180); do
  if docker logs "$ora_cid" 2>&1 | grep -q "DATABASE IS READY TO USE!"; then ready=1; break; fi
  [ "$(docker inspect -f '{{.State.Status}}' "$ora_cid")" = running ] || {
    echo "Oracle XE container exited early"; docker logs --tail 60 "$ora_cid"; exit 1;
  }
  sleep 5
done
[ "$ready" = 1 ] || { echo "Oracle XE not ready after ~15min"; docker logs --tail 60 "$ora_cid"; exit 1; }

echo "== workload vs real Oracle XE =="
BENCH_DSN="127.0.0.1:${ora_port}/XEPDB1" BENCH_OUT="$outdir/oracle.json" \
  "$py" bench/workload.py >/dev/null

# ------------------------------------------------------- PostgreSQL + pgSaci ---
echo "== building pgSaci (release) =="
cargo build --release --quiet --bin pgsaci

if ! docker image inspect "$pg_image" >/dev/null 2>&1; then
  echo "== building $pg_image =="
  docker build --build-arg "PG_VERSION=${pg_image##*:}" -t "$pg_image" "$root/testcontainers"
fi

echo "== starting PostgreSQL container, 2 CPU =="
pg_cid=$(docker run -d "${pg_limits[@]}" --shm-size=256m \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=postgres -p "${pg_port}:5432" "$pg_image")
for _ in $(seq 1 60); do docker exec "$pg_cid" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 0.5; done
docker exec -i "$pg_cid" psql -U postgres -v ON_ERROR_STOP=1 postgres <<'SQL'
CREATE EXTENSION IF NOT EXISTS orafce;
DROP ROLE IF EXISTS bench;
CREATE ROLE bench WITH LOGIN PASSWORD 'bench' SUPERUSER;
SQL

echo "== starting pgSaci proxy =="
PGSACI_LISTEN=127.0.0.1:15599 \
  PGSACI_PG_HOST=127.0.0.1 PGSACI_PG_PORT="$pg_port" \
  PGSACI_PG_DB=postgres PGSACI_PG_PASSWORD=bench \
  PGSACI_HEALTH_ADDR=127.0.0.1:15598 RUST_LOG=pgsaci=warn \
  ./target/release/pgsaci &
pgsaci_pid=$!
for _ in $(seq 1 40); do curl -fsS http://127.0.0.1:15598/readyz >/dev/null 2>&1 && break; sleep 0.25; done

echo "== workload vs PostgreSQL via pgSaci =="
BENCH_DSN="127.0.0.1:15599/FREEPDB1" BENCH_OUT="$outdir/pgsaci.json" \
  "$py" bench/workload.py >/dev/null

# ------------------------------------------------------------------- report ---
echo
"$py" - "$outdir/oracle.json" "$outdir/pgsaci.json" <<'PY'
import json, sys
O = json.load(open(sys.argv[1]))
P = json.load(open(sys.argv[2]))
o, p = O["results"], P["results"]

def cell(d, key):
    return f"{d[key]:.3f} ms" if "error" not in d else "err"

def table(kind, note):
    print(f"\n**{kind}** {note}\n")
    print("| operation | Oracle XE p50 | pgSaci p50 | Oracle p95 | pgSaci p95 | pgSaci / Oracle (p50) |")
    print("| --- | ---: | ---: | ---: | ---: | ---: |")
    for k, a in o.items():
        if a.get("kind") != kind:
            continue
        b = p.get(k, {"error": "not run"})
        if "error" in a or "error" in b:
            ratio = "n/a"
        else:
            ratio = f"{b['p50_ms'] / a['p50_ms']:.2f}x" if a["p50_ms"] else "n/a"
        print(f"| `{k}` | {cell(a,'p50_ms')} | {cell(b,'p50_ms')} "
              f"| {cell(a,'p95_ms')} | {cell(b,'p95_ms')} | {ratio} |")
    errs = {k: v['error'] for k, v in list(o.items()) + list(p.items())
            if v.get('kind') == kind and 'error' in v}
    for k, msg in errs.items():
        print(f"\n> `{k}` errored: {msg}")

print(f"Oracle XE 21c vs PostgreSQL via pgSaci — 2 CPU each (XE container 3 GiB / "
      f"2 GiB engine cap, PostgreSQL 2 GiB), single connection. "
      f"bench_big = {O['big_rows']} rows.")
table("latency", "(small statements — wall-clock is per-query overhead; lower ratio = pgSaci closer)")
table("throughput", "(scan / sort / aggregate / bulk write — wall-clock is the DB engine; ratio < 1 = pgSaci+PostgreSQL faster)")
PY
echo
echo "raw JSON: $outdir/{oracle,pgsaci}.json"
