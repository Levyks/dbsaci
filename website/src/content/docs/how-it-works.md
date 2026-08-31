---
title: How it works
description: The connection lifecycle, capability-driven wire negotiation, the translation pipeline, and how it is tested.
---

## The path of one query

```
Oracle driver ──TNS/TTC──▶ pgSaci ──libpq──▶ PostgreSQL + orafce
              ◀──────────         ◀────────
```

1. **Handshake & auth.** pgSaci terminates the Oracle Net connect, the TTC
   protocol / datatype negotiation, and the challenge/response authentication
   (12c PBKDF2 with a mutual server proof, or the 11g MD5 verifier). The client
   believes it authenticated against Oracle.
2. **Translate.** The Oracle SQL / PL-SQL text is parsed and rewritten to
   PostgreSQL SQL. Binds stay typed parameters — never string interpolation.
   Oracle scalar functions route to [`orafce`](https://github.com/orafce/orafce)
   rather than being reimplemented.
3. **Execute.** One backend round trip to an ordinary PostgreSQL release. The
   result is a `RowStream` — pgSaci never buffers the whole result set.
4. **Re-frame.** Each PostgreSQL row batch is re-encoded into Oracle's binary
   TTC framing (the column-describe, the row-data, the end-of-call trailer) and
   streamed back as the client's fetch loop asks for it.

One Oracle session holds one dedicated PostgreSQL connection for its lifetime, so
session-level state (current schema, transaction, temp tables, `SET` values)
behaves the way an Oracle session would.

## Capability-driven, not driver-sniffing

Different Oracle clients frame the same logical messages slightly differently —
the OCI thick client uses little-endian fixed-width integers where thin drivers
use a compact form; newer clients expect explicit end-of-response signals that
older ones do not; one client wants a shorter datatype-negotiation reply.

A real Oracle server resolves those differences from **what the client
negotiated in the TTC handshake** — the `TNS_CCAP_*` / `TNS_RCAP_*` capability
vectors and the protocol-version list — never from a driver-name string. pgSaci
does the same. Every wire divergence it makes is decided by a predicate over the
negotiated capabilities:

| Predicate | Reads | Selects |
| --- | --- | --- |
| `oci_dialect` | `TNS_CCAP_OCI1` | the OCI thick-client TTC dialect |
| `newer_describe_framing` | negotiated field version ≥ 20.1 | the newer row / describe / end-of-call shape |
| `response_completion` | field version / feature backport bits | explicit end-of-response signals |
| `na_without_version_list` | ran the NA exchange + sent an empty version list | the shorter datatype-negotiation reply + long-form auth chunks |

The driver-name banner is parsed for logging only. `grep -rE
'"(jdbc|odp\.net|oracle-rs)"' src/` is empty.

## The impersonated version

`PGSACI_ORACLE_VERSION` picks `19c` (default) or `11g`. It changes the product
banner, the `AUTH_VERSION_*` values, and which auth-verifier family is offered,
so both modern and 11g-era clients complete the handshake. The SQL translation
and result framing are the same either way.

## How compatibility is proven

The executable claim is the **golden corpus**: one real PostgreSQL/`orafce`
container and one real pgSaci proxy, every case driven over TNS, asserting
Oracle-correct **values, row counts and error text** — not merely "did not
error". It runs with no ignored cases (bar `MERGE`, which has a PostgreSQL 15
floor).

Supporting suites:

- `cargo test --lib` — auth crypto vectors, the `NUMBER` codec, translator units.
  No container.
- `cargo test --test translate_golden` — pure `oracle_to_postgres`
  string→string goldens.
- `clients/run.sh <python|java|dotnet>` — an end-to-end probe with a real
  third-party Oracle driver against both the 19c and 11g personas.
- `bench/run.sh` — the single-connection latency microbenchmark behind
  [Benchmarks](/pgsaci/benchmarks/).

CI runs the corpus against **PostgreSQL 18, 16 and 13**.
