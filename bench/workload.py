#!/usr/bin/env python3
"""Fixed workload, run against one Oracle-wire endpoint.

The *same* script and SQL run against (a) a real Oracle XE instance and (b)
PostgreSQL fronted by pgSaci, both via python-oracledb thin, on one connection.

Two families of operations:

  * latency ops    — tiny statements; wall-clock is dominated by per-query
                     overhead (framing, translate, one round trip). Many iters.
  * throughput ops — scan / sort / aggregate / bulk-write over a large table;
                     wall-clock is dominated by the database engine. Few iters.

If an op raises (e.g. a 2 GiB-capped Oracle XE dropping the session on a big
fetch) it is recorded as an error and the run continues after reconnecting.

Env:
  BENCH_DSN            host:port/service   (default 127.0.0.1:1521/XEPDB1)
  BENCH_USER / BENCH_PASSWORD              (default bench / bench)
  BENCH_ITERS         latency-op iters    (default 2000)
  BENCH_WARMUP        latency-op warmup   (default 200)
  BENCH_HEAVY_ITERS   throughput-op iters (default 30)
  BENCH_HEAVY_WARMUP  throughput-op warmup(default 3)
  BENCH_BIG_ROWS      rows in bench_big   (default 100000)
  BENCH_OUT           also write results JSON here
"""
import json
import os
import statistics
import sys
import time

import oracledb

DSN = os.environ.get("BENCH_DSN", "127.0.0.1:1521/XEPDB1")
USER = os.environ.get("BENCH_USER", "bench")
PASSWORD = os.environ.get("BENCH_PASSWORD", "bench")
LIGHT_ITERS = int(os.environ.get("BENCH_ITERS", "2000"))
LIGHT_WARMUP = int(os.environ.get("BENCH_WARMUP", "200"))
HEAVY_ITERS = int(os.environ.get("BENCH_HEAVY_ITERS", "30"))
HEAVY_WARMUP = int(os.environ.get("BENCH_HEAVY_WARMUP", "3"))
BIG_ROWS = int(os.environ.get("BENCH_BIG_ROWS", "100000"))
OUT = os.environ.get("BENCH_OUT")

SMALL_ROWS = 5000
SCRATCH_SEED = 2000
SCRATCH_INSERT_BASE = 1_000_000
SCRATCH_BULK_BASE = 2_000_000

# One mutable holder so a mid-run reconnect is visible to every op closure.
DB = {}


def connect():
    conn = oracledb.connect(user=USER, password=PASSWORD, dsn=DSN)
    cur = conn.cursor()
    cur.arraysize = 1000
    cur.prefetchrows = 1000
    DB["conn"], DB["cur"] = conn, cur


def drop_if_exists(cur, table):
    # Oracle has no `DROP TABLE IF EXISTS` before 23c; the PL/SQL swallow is the
    # portable idiom (and avoids a bare-DDL error round trip).
    cur.execute(
        f"BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table}'; "
        f"EXCEPTION WHEN OTHERS THEN NULL; END;"
    )


GEN = max(1000, int(BIG_ROWS**0.5) + 2)  # bench_seed rows; GEN*GEN must exceed BIG_ROWS


def seed():
    # 1 000 single-row inserts into a generator table, then everything else via
    # set-based `INSERT … SELECT` off its self cross join. Avoids array binds
    # (pgSaci implements them only partially) and `CONNECT BY` inside an INSERT.
    cur = DB["cur"]
    for t in ("bench", "bench_big", "bench_scratch", "bench_seed"):
        drop_if_exists(cur, t)
    cur.execute("CREATE TABLE bench_seed (k NUMBER PRIMARY KEY)")
    cur.execute("CREATE TABLE bench (id NUMBER PRIMARY KEY, n NUMBER, label VARCHAR2(40))")
    cur.execute(
        "CREATE TABLE bench_big "
        "(id NUMBER PRIMARY KEY, n NUMBER, label VARCHAR2(40), bucket NUMBER)"
    )
    cur.execute(
        "CREATE TABLE bench_scratch (id NUMBER PRIMARY KEY, n NUMBER, label VARCHAR2(40))"
    )
    for k in range(1, GEN + 1):
        cur.execute("INSERT INTO bench_seed (k) VALUES (:1)", [k])
    DB["conn"].commit()

    rid = f"((a.k - 1) * {GEN} + b.k)"
    cur.execute(
        f"INSERT INTO bench (id, n, label) "
        f"SELECT {rid}, MOD({rid} * 7, 1000), 'row-' || {rid} "
        f"FROM bench_seed a, bench_seed b WHERE {rid} <= {SMALL_ROWS}"
    )
    cur.execute(
        f"INSERT INTO bench_big (id, n, label, bucket) "
        f"SELECT {rid}, MOD({rid} * 7919, 100000), 'lbl-' || MOD({rid}, 997), MOD({rid}, 50) "
        f"FROM bench_seed a, bench_seed b WHERE {rid} <= {BIG_ROWS}"
    )
    cur.execute(
        f"INSERT INTO bench_scratch (id, n, label) "
        f"SELECT {rid}, {rid}, 's-' || {rid} "
        f"FROM bench_seed a, bench_seed b WHERE {rid} <= {SCRATCH_SEED}"
    )
    DB["conn"].commit()
    cur.execute("SELECT COUNT(*) FROM bench_big")
    got = cur.fetchone()[0]
    if got != BIG_ROWS:
        raise RuntimeError(f"seed: bench_big has {got} rows, expected {BIG_ROWS}")


def latency_ops():
    def select_dual(_):
        c = DB["cur"]; c.execute("SELECT 1 FROM DUAL"); c.fetchone()

    def point_by_pk(i):
        c = DB["cur"]
        c.execute("SELECT label FROM bench WHERE id = :id", id=(i % SMALL_ROWS) + 1)
        c.fetchone()

    def multi_bind_filter(i):
        c = DB["cur"]
        lo = (i * 13) % 900
        c.execute(
            "SELECT id FROM bench WHERE n BETWEEN :lo AND :hi AND label LIKE :pat",
            lo=lo, hi=lo + 50, pat="row-%",
        )
        c.fetchall()

    def range_100(i):
        c = DB["cur"]
        lo = (i % (SMALL_ROWS - 100)) + 1
        c.execute("SELECT id, n, label FROM bench WHERE id BETWEEN :a AND :b", a=lo, b=lo + 99)
        c.fetchall()

    def insert_commit(i):
        c = DB["cur"]
        c.execute(
            "INSERT INTO bench_scratch (id, n, label) VALUES (:1, :2, :3)",
            [SCRATCH_INSERT_BASE + i, i % 1000, "ins"],
        )
        DB["conn"].commit()

    def update_commit(i):
        c = DB["cur"]
        c.execute(
            "UPDATE bench_scratch SET n = :n WHERE id = :id",
            n=i % 5000, id=(i % SCRATCH_SEED) + 1,
        )
        DB["conn"].commit()

    def insert_rollback(i):
        c = DB["cur"]
        c.execute(
            "INSERT INTO bench_scratch (id, n, label) VALUES (:1, :2, :3)",
            [SCRATCH_INSERT_BASE + 500_000 + i, i, "rbk"],
        )
        DB["conn"].rollback()

    return [
        ("select_1_from_dual", select_dual),
        ("point_select_by_pk", point_by_pk),
        ("multi_bind_filter", multi_bind_filter),
        ("range_scan_100_rows", range_100),
        ("insert_commit", insert_commit),
        ("update_commit", update_commit),
        ("insert_then_rollback", insert_rollback),
    ]


def throughput_ops():
    def scan_expr_count(_):
        c = DB["cur"]
        c.execute("SELECT COUNT(*) FROM bench_big WHERE MOD(n * 7 + LENGTH(label), 13) = 0")
        c.fetchone()

    def full_aggregate(_):
        c = DB["cur"]
        c.execute("SELECT COUNT(*), SUM(n), AVG(n), MIN(n), MAX(n) FROM bench_big")
        c.fetchone()

    def group_by_bucket(_):
        c = DB["cur"]
        c.execute(
            "SELECT bucket, COUNT(*), AVG(n) FROM bench_big GROUP BY bucket ORDER BY bucket"
        )
        c.fetchall()

    def window_sort(_):
        # full ORDER BY of the table, forced by the window, 1-row result.
        c = DB["cur"]
        c.execute(
            "SELECT MAX(rn) FROM "
            "(SELECT ROW_NUMBER() OVER (ORDER BY n, label) AS rn FROM bench_big)"
        )
        if c.fetchone()[0] != BIG_ROWS:
            raise RuntimeError("window_sort row count mismatch")

    def fetch_25k(_):
        c = DB["cur"]
        c.execute("SELECT id, n, label FROM bench_big WHERE id <= 25000")
        if len(c.fetchall()) != 25000:
            raise RuntimeError("fetch_25k row count mismatch")

    def fetch_all_big(_):
        c = DB["cur"]
        c.execute("SELECT id, n FROM bench_big")
        rows = c.fetchall()
        if len(rows) != BIG_ROWS:
            raise RuntimeError(f"fetch_all_big got {len(rows)} rows, expected {BIG_ROWS}")

    rid = f"((a.k - 1) * {GEN} + b.k)"

    def bulk_insert_5000(_):
        c = DB["cur"]
        c.execute(
            f"INSERT INTO bench_scratch (id, n, label) "
            f"SELECT {SCRATCH_BULK_BASE} + {rid}, MOD({rid}, 1000), 'bulk' "
            f"FROM bench_seed a, bench_seed b WHERE {rid} <= 5000"
        )
        DB["conn"].commit()
        c.execute("DELETE FROM bench_scratch WHERE id > :a", a=SCRATCH_BULK_BASE)
        DB["conn"].commit()

    return [
        ("big_scan_expr_count", scan_expr_count),
        ("big_full_aggregate", full_aggregate),
        ("big_group_by_50", group_by_bucket),
        ("big_window_sort", window_sort),
        ("big_fetch_25k_rows", fetch_25k),
        ("big_fetch_all_rows", fetch_all_big),
        ("bulk_insert_5000_setselect_commit", bulk_insert_5000),
    ]


def measure(fn, iters, warmup):
    for i in range(warmup):
        fn(i)
    samples = []
    for i in range(iters):
        t0 = time.perf_counter()
        fn(warmup + i)
        samples.append((time.perf_counter() - t0) * 1000.0)
    samples.sort()
    return {
        "n": len(samples),
        "mean_ms": round(statistics.fmean(samples), 4),
        "p50_ms": round(samples[len(samples) // 2], 4),
        "p95_ms": round(samples[min(len(samples) - 1, int(len(samples) * 0.95))], 4),
        "min_ms": round(samples[0], 4),
    }


def main():
    print(f"connecting: {DSN} as {USER}", file=sys.stderr)
    connect()
    print(f"seeding (bench_big = {BIG_ROWS} rows)...", file=sys.stderr)
    seed()

    results = {}
    plan = [
        ("latency", latency_ops(), LIGHT_ITERS, LIGHT_WARMUP),
        ("throughput", throughput_ops(), HEAVY_ITERS, HEAVY_WARMUP),
    ]
    for kind, ops, iters, warmup in plan:
        for name, fn in ops:
            try:
                r = measure(fn, iters, warmup)
                r["kind"] = kind
                print(
                    f"{name:<26} p50={r['p50_ms']:.3f}ms  p95={r['p95_ms']:.3f}ms  "
                    f"mean={r['mean_ms']:.3f}ms  ({1000.0 / r['mean_ms']:.0f} ops/s)",
                    file=sys.stderr,
                )
            except Exception as e:  # noqa: BLE001 — record and keep going
                r = {"kind": kind, "error": f"{type(e).__name__}: {e}"[:200]}
                print(f"{name:<26} ERROR: {r['error']}", file=sys.stderr)
                try:
                    connect()
                except Exception as re:  # noqa: BLE001
                    print(f"  reconnect failed: {re}", file=sys.stderr)
            results[name] = r

    try:
        DB["cur"].close()
        DB["conn"].close()
    except Exception:  # noqa: BLE001
        pass

    payload = {
        "dsn": DSN,
        "light_iters": LIGHT_ITERS,
        "heavy_iters": HEAVY_ITERS,
        "small_rows": SMALL_ROWS,
        "big_rows": BIG_ROWS,
        "results": results,
    }
    print(json.dumps(payload, indent=2))
    if OUT:
        with open(OUT, "w") as f:
            json.dump(payload, f, indent=2)


if __name__ == "__main__":
    main()
