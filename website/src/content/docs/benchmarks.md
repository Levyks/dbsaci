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

Everything — the workload client, Oracle XE, PostgreSQL and pgSaci — runs in
Docker on one user-defined bridge network, so every hop is a container-to-
container veth with no host port-proxy in the path and both targets are reached
identically. pgSaci runs from its **published image** (`levyks/pgsaci:0.0.5`, a
static musl build), so the number reflects what you ship.

Both database containers get **2 CPU / 2.5 GiB**:

- Oracle XE 21c is licence-capped at 2 threads / 2 GiB of database RAM, and the
  run spends the whole 2 GiB explicitly (`INIT_SGA_SIZE` 1536M + `INIT_PGA_SIZE`
  512M).
- PostgreSQL gets a config **tuned to that envelope** — `shared_buffers` 768 MB,
  `work_mem` 64 MB, `max_parallel_workers_per_gather` 2, `jit=off` — instead of
  the stock 128 MB / 4 MB defaults. `synchronous_commit` stays on.

The pgSaci proxy container runs unconstrained.

:::note
These are one sample run on a Windows laptop. Absolute numbers move a lot with
hardware — re-run `bench/run.sh` on yours.
:::

## Per-statement latency

Small ops — the wall-clock here **is** the proxy overhead.

| operation | Oracle XE p50 | pgSaci p50 | pgSaci / Oracle |
| --- | ---: | ---: | ---: |
| `select_1_from_dual` | 0.11 ms | 0.58 ms | 5.2× |
| `point_select_by_pk` (1 bind) | 0.11 ms | 0.55 ms | 4.8× |
| `multi_bind_filter` (3 binds) | 0.21 ms | 0.94 ms | 4.5× |
| `range_scan_100_rows` | 0.16 ms | 0.60 ms | 3.9× |
| `insert_commit` | 1.55 ms | 1.95 ms | 1.3× |
| `update_commit` | 1.52 ms | 1.92 ms | 1.3× |
| `insert_then_rollback` | 1.56 ms | 0.67 ms | 0.4× |

pgSaci adds **~0.45 ms of fixed overhead per round trip** — a second hop
(client → pgSaci → PostgreSQL and back), plus SQL translation and re-encoding
the result into Oracle's wire format. That is a large *ratio* on the sub-0.2 ms
reads but still sub-millisecond in absolute terms. On the commit ops the WAL
fsync dominates and the ratio falls to ~1.3×. `insert + rollback` is *quicker*
via pgSaci — Oracle XE's redo/undo path for that pattern is heavier.

## Throughput

Scan / sort / aggregate / transfer over `bench_big` (100 000 rows) — the
wall-clock here is dominated by the database engine, not the hop.

| operation | Oracle XE p50 | pgSaci p50 | pgSaci / Oracle |
| --- | ---: | ---: | ---: |
| `big_full_aggregate` (COUNT/SUM/AVG/MIN/MAX, `NUMBER` cols) | 2.8 ms | 9.2 ms | 3.3× |
| `big_scan_expr_count` (per-row `MOD` expr) | 15.3 ms | 21.1 ms | 1.4× |
| `big_group_by_50` (hash aggregate) | 8.8 ms | 14.3 ms | 1.6× |
| `big_window_sort` (full `ORDER BY` via window) | 15.8 ms | 33.1 ms | 2.1× |
| `big_fetch_25k_rows` (25 k rows across the wire) | 12.6 ms | 37.9 ms | 3.0× |
| `big_fetch_all_rows` (100 k rows across the wire) | 43.7 ms | 135.6 ms | 3.1× |
| `bulk_insert_5000` (`INSERT … SELECT` + commit) | 72 ms (p95 **2.9 s**) | 150 ms (p95 203 ms) | 2.1× |

The `big_fetch_*` ops sit at ~3× — that is the proxy decoding every row off the
PostgreSQL wire and re-encoding it onto the Oracle wire, which is structural for
a translating proxy. The pure-engine ops (aggregate / sort / group-by) mostly
reflect Oracle `NUMBER` → PostgreSQL `numeric` arithmetic (software decimal,
slower than `bigint` / `double precision`); even with the tuned config and
parallel query enabled, `big_full_aggregate` stays ~3× because the query is too
short to parallelise. Integer columns or more cores narrow it.

The bulk write is ~2× slower on p50 but far **steadier**: Oracle XE's p95 is
~2.9 s (redo-log-switch stalls) versus pgSaci's ~200 ms.

:::caution
This is a **single-connection latency benchmark**. It says nothing about
concurrency, mixed OLTP, or a tuned deployment — the areas where PostgreSQL
usually shines.
:::
