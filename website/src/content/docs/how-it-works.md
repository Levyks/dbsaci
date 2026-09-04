---
title: How it works
description: The connection lifecycle, capability-driven wire negotiation, the translation pipeline, and how it is tested.
---

## The path of one query

```
Oracle driver ──TNS/TTC──▶ dbSaci ──backend──▶ PostgreSQL + orafce
              ◀──────────              └────▶ MariaDB (Oracle mode)
```

1. **Handshake & auth.** dbSaci terminates the Oracle Net connect, the TTC
   protocol / datatype negotiation, and the challenge/response authentication
   (12c PBKDF2 with a mutual server proof, or the 11g MD5 verifier).
2. **Translate.** The Oracle SQL / PL-SQL text is parsed and rewritten to the
   selected backend's SQL. Binds are sent as backend parameters. Oracle scalar
   functions map to `orafce` on PostgreSQL or to MariaDB's Oracle mode/facade.
3. **Execute.** One backend round trip. The result is streamed —
   dbSaci does not buffer the whole result set.
4. **Re-frame.** Each PostgreSQL row batch is re-encoded into Oracle's binary TTC
   framing (column-describe, row-data, end-of-call trailer) and returned as the
   client's fetch loop asks for it.

One Oracle session holds one dedicated PostgreSQL connection for its lifetime, so
session-level state (current schema, transaction, temp tables, `SET` values)
behaves as an Oracle session would.

## Wire negotiation

Different Oracle clients frame the same logical messages slightly differently —
the OCI thick client uses little-endian fixed-width integers where thin drivers
use a compact form; newer clients expect explicit end-of-response signals that
older ones do not; one client wants a shorter datatype-negotiation reply.

dbSaci picks each of those from the capabilities the client sends in the TTC
handshake — the `TNS_CCAP_*` / `TNS_RCAP_*` vectors and the protocol-version
list. Every wire divergence is a predicate over those values:

| Predicate | Reads | Selects |
| --- | --- | --- |
| `oci_dialect` | `TNS_CCAP_OCI1` | the OCI thick-client TTC dialect |
| `newer_describe_framing` | negotiated field version ≥ 20.1 | the newer row / describe / end-of-call shape |
| `response_completion` | field version / feature backport bits | explicit end-of-response signals |
| `na_without_version_list` | ran the NA exchange + sent an empty version list | the shorter datatype-negotiation reply + long-form auth chunks |

## The impersonated version

`DBSACI_ORACLE_VERSION` picks `19c` (default) or `11g`. It changes the product
banner, the `AUTH_VERSION_*` values, and which auth-verifier family is offered,
so both modern and 11g-era clients complete the handshake. The SQL translation
and result framing are the same either way.

## Testing

The main suite is the **golden corpus**: real PostgreSQL/`orafce` and MariaDB
containers with a real dbSaci proxy, every case driven over TNS, checking
Oracle-correct values, row counts, and error text. CI runs PostgreSQL 18, 16,
and 13 plus MariaDB 11.4.

Alongside it:

- `cargo test --lib` — auth crypto vectors, the `NUMBER` codec, translator units.
- `cargo test --test translate_golden` — `oracle_to_postgres` string→string
  goldens.
- `clients/run.sh <python|java|dotnet>` — an end-to-end probe with a real
  third-party Oracle driver against both the 19c and 11g personas.
- `bench/run.sh` — the latency microbenchmark behind
  [Benchmarks](/dbsaci/benchmarks/).
