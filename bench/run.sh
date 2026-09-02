#!/usr/bin/env bash
# Benchmark orchestrator: run the identical workload (bench/workload.py,
# python-oracledb thin) against
#   1. a real Oracle XE 21c instance  (gvenzl/oracle-xe:21-slim)
#   2. PostgreSQL fronted by pgSaci    (testcontainers image + the pgSaci image)
# and print side-by-side latency + throughput tables.
#
# EVERYTHING runs in Docker on one user-defined bridge network — the workload
# client, Oracle XE, PostgreSQL and pgSaci. Every hop is a container-to-container
# veth inside the Docker VM: no host port-proxy, and both targets are reached
# over an identical path. pgSaci runs from its published image
# (levyks/pgsaci:0.0.5 by default; PGSACI_IMAGE=... to override), so the number
# reflects what you actually ship — a static musl build.
#
# Fairness: both database containers get --cpus=2 and --memory=$BENCH_MEM
# (default 2560m).
#   * Oracle XE 21c is licence-capped at 2 CPU threads and 2 GiB of database RAM.
#     We spend the whole 2 GiB: INIT_SGA_SIZE=1536 + INIT_PGA_SIZE=512. The
#     container ceiling is a bit higher so background processes / redo / server
#     processes / the container OS have headroom (it OOM-kills with much less);
#     2.5 GiB is proven sufficient with the full licence spent.
#   * PostgreSQL gets the same ceiling and a config tuned to that envelope (the
#     -c flags below) instead of the stock 128 MB / 4 MB defaults.
# The pgSaci proxy container runs unconstrained — it is the overhead under test,
# not a third contestant.
#
#   bench/run.sh
#   BENCH_ITERS=500 BENCH_HEAVY_ITERS=10 BENCH_BIG_ROWS=50000 bench/run.sh   # quick
#   BENCH_KEEP=1 bench/run.sh          # leave the network + containers up afterwards
#   PGSACI_IMAGE=pgsaci:dev bench/run.sh
#
# Requires: docker. (No local cargo / python needed.)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# Docker Desktop is a Windows process; when this script runs under Git Bash/MSYS
# the shell rewrites POSIX-looking args ("/out", "1521/XEPDB1") into C:\... paths
# before docker.exe sees them. Disable that, and translate host paths for `-v`
# ourselves via cygpath. On a real Linux shell both are no-ops.
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
if command -v cygpath >/dev/null 2>&1; then hostpath() { cygpath -w "$1"; }
else hostpath() { printf '%s' "$1"; }; fi

ora_image="gvenzl/oracle-xe:21-slim"
pg_image="${PGSACI_TEST_PG_IMAGE:-pgsaci-test-pg:18}"
pgsaci_image="${PGSACI_IMAGE:-levyks/pgsaci:0.0.5}"
client_image="bench-client:local"

cpus="${BENCH_CPUS:-2}"
mem="${BENCH_MEM:-2560m}"
ora_sga="${BENCH_ORA_SGA:-1536}"   # MiB, INIT_SGA_SIZE
ora_pga="${BENCH_ORA_PGA:-512}"    # MiB, INIT_PGA_SIZE  (SGA + PGA = XE's 2 GiB cap)

sfx="$$"
net="bench-net-$sfx"
ora_name="bench-ora-$sfx"
pg_name="bench-pg-$sfx"
saci_name="bench-saci-$sfx"

# Workload knobs forwarded to bench/workload.py (defaults live there too).
export BENCH_USER=bench BENCH_PASSWORD=bench
export BENCH_ITERS="${BENCH_ITERS:-2000}"
export BENCH_WARMUP="${BENCH_WARMUP:-200}"
export BENCH_HEAVY_ITERS="${BENCH_HEAVY_ITERS:-30}"
export BENCH_HEAVY_WARMUP="${BENCH_HEAVY_WARMUP:-3}"
export BENCH_BIG_ROWS="${BENCH_BIG_ROWS:-100000}"

outdir="$(mktemp -d)"

cleanup() {
  if [ -n "${BENCH_KEEP:-}" ]; then
    echo "BENCH_KEEP set — leaving up: network $net, containers $ora_name / $pg_name / $saci_name"
    return
  fi
  docker rm -f "$saci_name" "$pg_name" "$ora_name" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker network create "$net" >/dev/null

# PostgreSQL config, scaled to the 3 GiB / 2-CPU / single-connection envelope.
# Passed as server args (no file mount). synchronous_commit stays on —
# insert_commit has to be as durable as Oracle's for the comparison to mean
# anything. jit off: at ~100k rows it costs more than it saves and adds p95 noise.
pg_conf=(
  -c shared_buffers=768MB
  -c effective_cache_size=2304MB
  -c work_mem=64MB
  -c maintenance_work_mem=256MB
  -c max_worker_processes=4
  -c max_parallel_workers=2
  -c max_parallel_workers_per_gather=2
  -c random_page_cost=1.1
  -c effective_io_concurrency=200
  -c jit=off
  -c checkpoint_completion_target=0.9
  -c max_wal_size=2GB
  -c synchronous_commit=on
)

run_client() {  # $1 = dsn, $2 = out basename
  docker run --rm --network "$net" -v "$(hostpath "$outdir"):/out" -v "$(hostpath "$root/bench"):/bench:ro" \
    -e BENCH_DSN="$1" -e BENCH_OUT="/out/$2" \
    -e BENCH_USER -e BENCH_PASSWORD -e BENCH_ITERS -e BENCH_WARMUP \
    -e BENCH_HEAVY_ITERS -e BENCH_HEAVY_WARMUP -e BENCH_BIG_ROWS \
    "$client_image" python /bench/workload.py >/dev/null
}

# --------------------------------------------------------------- images ------
if ! docker image inspect "$client_image" >/dev/null 2>&1; then
  echo "== building $client_image (python + oracledb thin) =="
  docker build -q -t "$client_image" - >/dev/null <<'EOF'
FROM python:3.12-slim
RUN pip install --no-cache-dir oracledb
EOF
fi
if ! docker image inspect "$pg_image" >/dev/null 2>&1; then
  echo "== building $pg_image =="
  docker build --build-arg "PG_VERSION=${pg_image##*:}" -t "$pg_image" "$root/testcontainers"
fi
docker image inspect "$pgsaci_image" >/dev/null 2>&1 || {
  echo "== pulling $pgsaci_image =="; docker pull -q "$pgsaci_image" >/dev/null; }

# ---------------------------------------------------------------- Oracle XE ---
echo "== starting Oracle XE ($ora_image) — SGA ${ora_sga}M + PGA ${ora_pga}M, ${cpus} CPU / $mem — first boot takes a few minutes =="
docker run -d --name "$ora_name" --network "$net" \
  --cpus="$cpus" --memory="$mem" --memory-swap="$mem" \
  -e ORACLE_PASSWORD=bench -e APP_USER=bench -e APP_USER_PASSWORD=bench \
  -e INIT_SGA_SIZE="$ora_sga" -e INIT_PGA_SIZE="$ora_pga" \
  "$ora_image" >/dev/null

echo "== waiting for Oracle XE ('DATABASE IS READY TO USE!' in the log) =="
ready=0
for _ in $(seq 1 180); do
  if docker logs "$ora_name" 2>&1 | grep -q "DATABASE IS READY TO USE!"; then ready=1; break; fi
  [ "$(docker inspect -f '{{.State.Status}}' "$ora_name")" = running ] || {
    echo "Oracle XE container exited early"; docker logs --tail 60 "$ora_name"; exit 1;
  }
  sleep 5
done
[ "$ready" = 1 ] || { echo "Oracle XE not ready after ~15min"; docker logs --tail 80 "$ora_name"; exit 1; }

echo "== workload vs real Oracle XE =="
run_client "$ora_name:1521/XEPDB1" oracle.json

# ------------------------------------------------------- PostgreSQL + pgSaci ---
echo "== starting PostgreSQL ($pg_image), ${cpus} CPU / $mem, tuned =="
docker run -d --name "$pg_name" --network "$net" \
  --cpus="$cpus" --memory="$mem" --memory-swap="$mem" --shm-size=512m \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=postgres \
  "$pg_image" "${pg_conf[@]}" >/dev/null
for _ in $(seq 1 60); do docker exec "$pg_name" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 0.5; done
docker exec -i "$pg_name" psql -U postgres -v ON_ERROR_STOP=1 postgres <<'SQL'
CREATE EXTENSION IF NOT EXISTS orafce;
DROP ROLE IF EXISTS bench;
CREATE ROLE bench WITH LOGIN PASSWORD 'bench' SUPERUSER;
SQL

echo "== starting pgSaci ($pgsaci_image), unconstrained =="
docker run -d --name "$saci_name" --network "$net" \
  -e PGSACI_LISTEN=0.0.0.0:1521 \
  -e PGSACI_PG_HOST="$pg_name" -e PGSACI_PG_PORT=5432 \
  -e PGSACI_PG_DB=postgres -e PGSACI_PG_PASSWORD=bench \
  -e PGSACI_HEALTH_ADDR=0.0.0.0:9500 -e RUST_LOG=pgsaci=warn \
  "$pgsaci_image" >/dev/null
# scratch image: no shell to exec into. Poll the listener from a throwaway client.
docker run --rm --network "$net" "$client_image" python -c "
import socket, sys, time
for _ in range(120):
    s = socket.socket(); s.settimeout(1)
    if s.connect_ex(('$saci_name', 1521)) == 0:
        sys.exit(0)
    time.sleep(0.5)
sys.exit(1)
" || { echo 'pgSaci listener never came up'; docker logs --tail 60 "$saci_name"; exit 1; }

echo "== workload vs PostgreSQL via pgSaci =="
run_client "$saci_name:1521/FREEPDB1" pgsaci.json

# ------------------------------------------------------------------- report ---
echo
docker run --rm -i -e BENCH_MEM="$mem" -v "$(hostpath "$outdir"):/out" "$client_image" python - <<'PY'
import json, os
MEM = os.environ.get("BENCH_MEM", "?")
O = json.load(open("/out/oracle.json"))
P = json.load(open("/out/pgsaci.json"))
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

print(f"Oracle XE 21c vs PostgreSQL via pgSaci — all in Docker on one bridge network, "
      f"{O.get('light_iters', '?')} latency iters. Both DB containers: 2 CPU / {MEM} "
      f"(XE spends its full 2 GiB licence: 1536M SGA + 512M PGA; PostgreSQL tuned to "
      f"that envelope). Single connection. bench_big = {O['big_rows']} rows.")
table("latency", "(small statements — wall-clock is per-query overhead; lower ratio = pgSaci closer)")
table("throughput", "(scan / sort / aggregate / bulk write — wall-clock is the DB engine; ratio < 1 = pgSaci+PostgreSQL faster)")
PY
echo
echo "raw JSON in $outdir (oracle.json, pgsaci.json)"
