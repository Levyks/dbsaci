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
   functions map to `orafce` on PostgreSQL or to MariaDB's Oracle mode and the
   `dbsaci_sys` dictionary. On MariaDB, Oracle's identifier rules (unquoted →
   upper case, quoted → exact) are applied once, lexically, and translations are
   cached per session.
3. **Execute.** One backend round trip. The result is streamed —
   dbSaci does not buffer the whole result set. Array DML goes to MariaDB as
   multi-row `INSERT`s.
4. **Re-frame.** Each backend row batch is re-encoded into Oracle's binary TTC
   framing (column-describe, row-data, end-of-call trailer) and returned as the
   client's fetch loop asks for it.

One Oracle session holds one dedicated backend connection for its lifetime, so
session-level state (current schema, transaction, temp tables, `SET` values)
behaves as an Oracle session would. Connecting costs about two backend round
trips on MariaDB: the data dictionary is installed once per server in
`dbsaci_sys`, not per session.

Requests larger than 8 KB (big `IN` lists, wide batches) arrive split across
several TNS packets and are reassembled before translation.

## Transactions on MariaDB

dbSaci runs MariaDB sessions with `autocommit=0` and `READ COMMITTED`, which
gives Oracle's model: a transaction starts with the first statement and every
statement sees what was committed before it began, with no stale snapshots in
pooled sessions. DDL commits. The driver's autocommit (the Execute commit bit —
the JDBC/HikariCP default, `connection.autocommit = True`) commits after each
call on both backends. A clean logoff commits; a dropped connection rolls back.
`SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` gives a transaction snapshot,
`READ ONLY` rejects writes, and cursors stay fetchable across `COMMIT`.
`ALTER SESSION` state is session state, not transactional. If the backend
connection is lost with uncommitted work, the session ends with `ORA-03113`
instead of silently continuing on a new connection.

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
and 13, plus MariaDB 11.8 and 12.3 across `lower_case_table_names` 0/1 and
identifier case `upper`/`lower` — every MariaDB configuration against one
shared ledger of known reds, as a least-privilege login.

Alongside it:

- `cargo test --lib` — auth crypto vectors, the `NUMBER` codec, translator units.
- `cargo test --test translate_golden` — `oracle_to_postgres` and
  `oracle_to_mariadb` string→string goldens.
- `clients/run.sh <python|java|dotnet>` — an end-to-end probe with a real
  third-party Oracle driver against both the 19c and 11g personas and both
  backends, including driver autocommit, stale reads, fetch across `COMMIT`,
  and requests larger than 8 KB.
- `bench/run.sh` — the latency microbenchmark behind
  [Benchmarks](/dbsaci/benchmarks/).
