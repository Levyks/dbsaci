---
title: Benchmarks
description: How much latency the proxy hop adds, measured against a real Oracle XE.
---

pgSaci sits in the path as an extra hop: it decodes the Oracle TNS/TTC frame,
translates the SQL, does **one** backend round trip to PostgreSQL, and re-frames
the answer. So per query it is slower than talking to Oracle directly — but both
are in the low-millisecond range on a laptop.

## Method

`bench/run.sh` runs an identical **single-connection, single-thread**
micro-workload (`python-oracledb` thin) against:

- a real **Oracle XE 21c** container, and
- **PostgreSQL 18 via pgSaci**.

Both database containers are pinned to **2 CPU** (Oracle XE 21c is licence-capped
there anyway; its container gets 3 GiB for headroom, PostgreSQL gets 2 GiB),
loopback, **stock / untuned PostgreSQL config** (`shared_buffers` 128 MB, 4 MB
`work_mem`, no parallel-query room). The pgSaci proxy itself runs unconstrained
on the host.

:::note
These are one sample run on a Windows laptop. Absolute numbers move a lot with
hardware — re-run `bench/run.sh` on yours.
:::

## Per-statement latency

Small ops — the wall-clock here **is** the proxy overhead.

| operation | Oracle XE p50 | pgSaci p50 | pgSaci / Oracle |
| --- | ---: | ---: | ---: |
| `select_1_from_dual` | 0.45 ms | 1.43 ms | 3.2× |
| `point_select_by_pk` (1 bind) | 0.47 ms | 1.47 ms | 3.1× |
| `multi_bind_filter` (3 binds) | 0.58 ms | 2.06 ms | 3.5× |
| `range_scan_100_rows` | 0.52 ms | 1.68 ms | 3.2× |
| `insert_commit` | 2.37 ms | 3.38 ms | 1.4× |
| `update_commit` | 2.34 ms | 3.37 ms | 1.4× |
| `insert_then_rollback` | 2.40 ms | 1.99 ms | 0.8× |

pgSaci adds **~1 ms of fixed overhead per statement** — a second hop
(client → pgSaci → PostgreSQL and back), plus SQL translation and re-encoding
the result into Oracle's wire format. A 0.5 ms Oracle read becomes ~1.5 ms —
~3× the ratio, but still ~1.5 ms in absolute terms. Committed writes add that hop
on top of a WAL fsync, so the ratio is only ~1.4×. `insert + rollback` is
*quicker* via pgSaci — Oracle XE's redo/undo path for that pattern is heavier.

## Throughput

Scan / sort / aggregate / transfer over `bench_big` (100 000 rows) — the
wall-clock here is dominated by the database engine, not the hop.

| operation | Oracle XE p50 | pgSaci p50 | pgSaci / Oracle |
| --- | ---: | ---: | ---: |
| `big_full_aggregate` (COUNT/SUM/AVG/MIN/MAX, `NUMBER` cols) | 3.6 ms | 10.8 ms | 3.0× |
| `big_scan_expr_count` (per-row `MOD` expr) | 16.2 ms | 21.9 ms | 1.4× |
| `big_group_by_50` (hash aggregate) | 8.4 ms | 16.3 ms | 1.9× |
| `big_window_sort` (full `ORDER BY` via window) | 16.5 ms | 42.3 ms | 2.6× |
| `big_fetch_25k_rows` (25 k rows across the wire) | 25.1 ms | 40.2 ms | 1.6× |
| `big_fetch_all_rows` (100 k rows across the wire) | 87.5 ms | 134 ms | 1.5× |
| `bulk_insert_5000` (`INSERT … SELECT` + commit) | 73 ms (p95 **2.9 s**) | 149 ms (p95 163 ms) | 2.0× |

On the CPU-bound analytics, PostgreSQL here is 1.4–3× slower than Oracle XE — but
the setup is stacked against it: the container config is untuned, the columns are
Oracle `NUMBER` → PostgreSQL `numeric` (software decimal arithmetic, much slower
than `bigint` / `double precision`), and the 2-CPU cap removes PostgreSQL's
parallel query. Tune the config, use integer types, or give it more cores and the
gap closes or reverses.

Bulk row transfer (`big_fetch_*`) is only ~1.5× — reasonable for a double hop
that re-encodes every row. The bulk write is ~2× slower but far **steadier**:
Oracle XE's p95 is ~2.9 s (redo-log-switch stalls) versus pgSaci's ~160 ms.

:::caution
This is a **single-connection latency benchmark**. It says nothing about
concurrency, mixed OLTP, or a tuned deployment — the areas where PostgreSQL
usually shines.
:::
