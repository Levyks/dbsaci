# pgSaci benchmark

Two questions, one harness:

1. **How much latency does the proxy add per statement?** (the *latency* ops)
2. **When the database engine is the bottleneck, not the hop, how does
   PostgreSQL-via-pgSaci compare to Oracle XE?** (the *throughput* ops)

`workload.py` runs a fixed set of operations on a **single connection, single
thread**, over the Oracle TNS/TTC wire, using `python-oracledb` in thin mode.
The same script and SQL run against two endpoints:

| target | path under test |
| --- | --- |
| **Oracle XE 21c** (`gvenzl/oracle-xe:21-slim`) | client → real Oracle |
| **pgSaci** | client → pgSaci (TNS decode + translate + re-frame) → PostgreSQL + orafce |

Everything — the workload client, Oracle XE, PostgreSQL, and pgSaci — runs in
Docker on one user-defined bridge network. Every hop is a container-to-container
veth inside the Docker VM, so there is no host port-proxy in the path and both
targets are reached identically. pgSaci runs from its **published image**
(`levyks/pgsaci:0.0.5`, a static musl build), so the number reflects what you
ship. `PGSACI_IMAGE=…` overrides it.

## Fairness: 2 CPU / 2.5 GiB

`run.sh` starts **both** database containers with `--cpus=2` and
`--memory=$BENCH_MEM` (default `2560m`).

* Oracle XE 21c is hard-capped at 2 threads / 2 GiB of database RAM by its
  licence. The run spends the whole 2 GiB explicitly — `INIT_SGA_SIZE=1536` +
  `INIT_PGA_SIZE=512` — and the container ceiling is a little above that so
  background processes, redo, server processes and the container OS have
  headroom (it OOM-kills with much less). 2.5 GiB is proven sufficient with the
  full licence spent; run-to-run numbers are indistinguishable from a 3 GiB
  ceiling.
* PostgreSQL gets the same ceiling and a config **tuned to that envelope**
  (`shared_buffers=768MB`, `work_mem=64MB`, `max_parallel_workers_per_gather=2`,
  `jit=off`, …) instead of the stock 128 MB / 4 MB defaults. `synchronous_commit`
  stays `on` — the commit ops have to be as durable as Oracle's.

The pgSaci proxy container runs unconstrained — it is the overhead being
measured, not a third contestant.

## Operations

### latency — tiny statements, many iterations (default 2 000)

| name | shape |
| --- | --- |
| `select_1_from_dual` | bare round trip |
| `point_select_by_pk` | 1 bind, PK lookup, 1 row |
| `multi_bind_filter` | 3 binds (`BETWEEN` + `LIKE`), small result |
| `range_scan_100_rows` | 100-row fetch |
| `insert_commit` | `INSERT` + `COMMIT` (durable write) |
| `update_commit` | `UPDATE … WHERE id = :id` + `COMMIT` |
| `insert_then_rollback` | `INSERT` + `ROLLBACK` |

### throughput — scan / sort / aggregate / bulk, few iterations (default 30)

Run against `bench_big` (default **100 000** rows; `BENCH_BIG_ROWS` to change).

| name | shape |
| --- | --- |
| `big_scan_expr_count` | full scan, per-row `MOD(…)` expr, 1-row result |
| `big_full_aggregate` | `COUNT/SUM/AVG/MIN/MAX` over the whole table |
| `big_group_by_50` | `GROUP BY bucket` → 50 rows (hash aggregate) |
| `big_window_sort` | `MAX(ROW_NUMBER() OVER (ORDER BY n, label))` — full sort, 1-row result |
| `big_fetch_25k_rows` | `SELECT id, n, label … WHERE id <= 25000` — 25 k rows across the wire |
| `big_fetch_all_rows` | `SELECT id, n FROM bench_big` — the whole table across the wire |
| `bulk_insert_5000_setselect_commit` | 5 000-row `INSERT … SELECT` + `COMMIT` (+ cleanup) |

Each op reports p50 / p95 / mean of the per-call latency. The workload seeds with
set-based `INSERT … SELECT` (off a 1 000-row generator table); it is a
single-statement-throughput benchmark, so it does not use array binds even though
pgSaci now supports them.

## Running it

```bash
bench/run.sh
BENCH_ITERS=500 BENCH_HEAVY_ITERS=10 BENCH_BIG_ROWS=50000 bench/run.sh   # quick
BENCH_KEEP=1 bench/run.sh          # leave the network + containers up afterwards
```

Only `docker` is needed — the client (python-oracledb thin) runs in a throwaway
`python:3.12-slim` container. `run.sh` creates the network, starts the three
server containers, seeds a `bench` user + tables in each engine, runs the
workload twice, prints two markdown tables, and tears everything down. Raw
per-run JSON is left in a temp dir (path printed at the end).

## Reading the results

* **latency** table — pgSaci adds a fixed per-round-trip cost (TNS decode →
  translate → re-frame → a hop to PostgreSQL → and back). On this box that is
  ~0.45 ms, so a 0.1 ms Oracle point-read becomes ~0.55 ms (~5x) but is still
  sub-millisecond. On the commit ops the durable `fsync` dominates and the ratio
  falls to ~1.2x.
* **throughput** table — the per-call hop is amortised, so this splits two ways:
  * the **fetch** ops (`big_fetch_*`) still carry a proxy tax — every row is
    decoded from the PostgreSQL wire and re-encoded onto the Oracle wire.
  * the pure-engine ops (aggregate / sort / group-by) mostly reflect Oracle
    `NUMBER` → PostgreSQL `numeric` arithmetic, which is slower. Integer columns,
    more cores, or a bigger `work_mem` narrow it. The table shows the shape, not
    a winner.
* Single connection, single thread — no statement about concurrency.
* Numbers move a lot with the host. Re-run locally.
