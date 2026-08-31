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

## Fairness: 2 CPU / 2 GiB

`run.sh` starts **both** database containers with `--cpus=2 --memory=2g`. Oracle
XE 21c is already hard-capped at 2 threads / 2 GB by its licence, so this makes
the two engines directly comparable. The pgSaci proxy itself runs unconstrained
on the host — it is the overhead being measured, not a third contestant.

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
pip install oracledb
bench/run.sh
BENCH_ITERS=500 BENCH_HEAVY_ITERS=10 BENCH_BIG_ROWS=50000 bench/run.sh   # quick
```

`run.sh` starts both containers, seeds a `bench` user + tables in each, runs the
workload twice, prints two markdown tables, and tears everything down. Raw
per-run JSON is left in a temp dir (path printed at the end).

## Reading the results

* **latency** table — pgSaci is expected to be slower; the `pgSaci / Oracle`
  column is how much. It is ~1 ms of fixed overhead, so a 0.5 ms Oracle read is
  ~1.5 ms (≈3x) but still ~1.5 ms.
* **throughput** table — the hop is amortised, so this mostly reflects the two
  database engines. `ratio < 1` means PostgreSQL-behind-pgSaci finished sooner.
  Under this 2 CPU cap, with the **stock, untuned** PostgreSQL container config
  (`shared_buffers` 128 MB, 4 MB `work_mem`, no parallel query room) and Oracle
  `NUMBER` → PostgreSQL `numeric` columns, Oracle XE's executor wins the
  CPU-bound scan/aggregate ops. Tune the config, use integer types, or add
  cores and it narrows or flips. The table shows the shape, not a winner.
* Single connection, single thread, loopback — no statement about concurrency.
* Numbers move a lot with the host. Re-run locally.
