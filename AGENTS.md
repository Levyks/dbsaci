# PgSaci: Autonomous Agent Implementation Guide

Welcome to the **PgSaci** project. This file (`AGENTS.md`) provides the context, architectural boundaries, and implementation strategy required for AI agents to write, test, and refactor the PgSaci codebase.

## 1. Project Goal
**PgSaci** is a drop-in Oracle-to-PostgreSQL wire protocol and SQL translation proxy. 
It acts as a man-in-the-middle server, accepting connections from standard Oracle database clients (via the TNS/TTI wire protocol) and proxying them to a vanilla PostgreSQL backend. The goal is to allow legacy Oracle applications to run against PostgreSQL without modifying their source code or driver configurations.

## 2. Architecture Overview
PgSaci is written in **Rust** and utilizes the **Tokio** asynchronous runtime. It consists of three primary layers:
1. **The TNS/TTI Listener (Frontend):** A Tokio TCP listener (default port 1521) that accepts Oracle client connections, handles O5L authentication handshakes, and decodes binary TNS packets.
2. **The AST Translator (Middleware):** A SQL parser/transpiler (likely utilizing `sqlparser-rs`) that converts the Oracle SQL dialect into PostgreSQL-compatible syntax (e.g., converting `(+)` outer joins, `ROWNUM`, and implicit data type conversions).
3. **The PostgreSQL Proxy (Backend):** A standard Postgres driver (`tokio-postgres`) that maintains a connection pool to the backend database, submits the translated queries, and serializes the PostgreSQL response back into Oracle binary framing.

## 3. Protocol Reference: `oracle-rs`
The Oracle wire protocol (TNS/TTI) is proprietary and undocumented. You must **NOT** guess the byte framing. 

Instead, use the source code of the open-source client driver [oracle-rs](https://github.com/stiang/oracle-rs) as your ultimate "clean room" reference. 
* Look at `oracle-rs` to understand how a client constructs TNS packets, authenticates, and parses rows.
* **Your task is to reverse this state machine:** PgSaci must parse what `oracle-rs` sends, and construct the byte-level server responses that `oracle-rs` expects to receive.

## 4. Backend Strategy: `orafce`
Do **not** attempt to translate every Oracle function into native Postgres SQL manually. 
Assume the backend PostgreSQL database has the [orafce extension](https://github.com/orafce/orafce) installed. 
* Pass Oracle-specific date math (e.g., `ADD_MONTHS`), string manipulation (`NVL`, `DECODE`), and system variables (`SYSDATE`) through to Postgres natively. 
* Only use the Rust AST transpiler for structural dialect differences (e.g., joins, subqueries, and table name mapping).

## 5. Test Suites & Validation Strategy
Agents must validate PgSaci's logic against the following open-source test corpora. Ensure you fetch these repositories or reference their known SQL test cases when writing regression tests:

1. **Client Compatibility Tests (`oracle-rs` / `diesel-oci`):**
   * **Goal:** Verify wire protocol stability.
   * **Method:** Run the native integration test suites of these Rust client drivers against the PgSaci proxy (`localhost:1521`). If they connect and successfully parse the dummy responses, the TNS framing is correct.
2. **Procedural & Edge-Case Tests ([IvorySQL](https://github.com/IvorySQL/IvorySQL)):**
   * **Goal:** Test PL/SQL translation and advanced Oracle SQL semantics.
   * **Method:** Use the `src/test/regress/` test cases from IvorySQL to understand the expected behavior and mapping of Oracle syntax to Postgres.
3. **Function & Type Tests ([Orafce](https://github.com/orafce/orafce)):**
   * **Goal:** Ensure `orafce` functions are being correctly parsed and routed.
   * **Method:** Execute the `sql/` regression files from the `orafce` repository through PgSaci, verifying the outputs match their `expected/` fixtures.
4. **Integration Schemas ([Oracle db-sample-schemas](https://github.com/oracle-samples/db-sample-schemas)):**
   * **Goal:** End-to-end integration.
   * **Method:** PgSaci must be able to parse and execute the official Oracle `hr_install.sql` (Human Resources sample schema), migrating the DDL and inserting the sample DML without crashing.
5. **AST Dialect Parsers ([Apache Calcite / Babel](https://github.com/apache/calcite)):**
   * **Goal:** Validate the Rust AST parser.
   * **Method:** Use the Oracle fixtures from Calcite's `babel` module to test your parser's ability to handle permissive Oracle edge cases.

### Current test layout

* `cargo test --lib` — fast unit tests (auth crypto vectors, NUMBER codec, bind
  substitution, translator) with no container.
* `tests/translate_golden.rs` (+ `tests/corpus/translate/*.txt`) — pure
  `oracle_to_postgres` string→string goldens, no container. `oracle SQL => pg SQL`,
  or `oracle SQL => !` to assert rejection.
* `tests/corpus.rs` (+ `tests/corpus/*.sql`) — end-to-end golden corpus. **One**
  PostgreSQL/orafce container and **one** PgSaci proxy are started per run and
  shared by every case (custom `libtest-mimic` harness). Each case asserts
  returned values / row counts / error text, not just "did not error". Cases are
  written to the **Oracle-correct** result and grouped by feature area, never by
  whether they currently pass — a run reports e.g. `368 passed; 118 failed`, and
  the failures ARE the Oracle-compatibility backlog (`cargo test --test corpus
  2>&1 | grep FAILED | sed 's/::.*//' | sort | uniq -c` gives the per-feature
  breakdown). The number should trend down; don't move or soften a red case to
  make it green. Format: `tests/corpus/README.md`.
  The `PGSACI_TEST_PG_IMAGE=pgsaci-test-pg:<major>` env var points the corpus at
  a different PostgreSQL major; a group can set `# requires-pg: N` to run as
  ignored below a hard version floor (`MERGE` needs PG 15).
* Porting more of the suites above means adding `tests/corpus/*.sql` groups
  (values authored to the Oracle-correct answer), not new bespoke test binaries.
* `clients/run.sh <python|java|dotnet> [oracle-version]` — real third-party
  Oracle driver against a real container + real PgSaci. Probes live under
  `clients/{python,java,dotnet}/` (the .NET probe targets `net10.0`).
  ODP.NET dialect quirks live behind `is_odpnet` in `src/server.rs` + the
  `*_odpnet` builders in `src/wire.rs`.
* `bench/run.sh` — latency microbenchmark, real Oracle XE 21c
  (`gvenzl/oracle-xe`) vs PostgreSQL-via-PgSaci; feeds the README's *How slow is
  this?* table. `bench/workload.py` + `bench/README.md`.
* CI is one workflow, `.github/workflows/ci.yml`: `lint` (fmt + clippy + `--lib`
  + `translate_golden`), `corpus` as a matrix over PostgreSQL 18 / 16 / 13, and
  `client-{python,java,dotnet}` (each against the 19c and 11g personas).

## 6. Implementation Guidelines for Agents
* **Safe Rust:** Stick strictly to Safe Rust unless directly handling FFI or highly optimized byte buffers. 
* **Async I/O:** All network operations must be non-blocking using `tokio::net::TcpStream` and `tokio::io`. Avoid blocking the Tokio executor. 
* **Byte Parsing:** Use the `bytes` crate (`Bytes`, `BytesMut`, `Buf`, `BufMut`) for all TNS packet construction and decoding. Do not manually manipulate `Vec<u8>` if `bytes` provides a safer abstraction.
* **Auth Mapping:** When authenticating, intercept the Oracle user credentials, establish the Postgres backend connection using those credentials, and immediately issue a `SET search_path TO <username>;` to emulate Oracle's User=Schema paradigm.

## 7. Roadmap / Milestones
1. **M1 (Handshake):** TCP Listener accepts connection, negotiates TNS version, and successfully logs in a dummy client.
2. **M2 (Mocking):** Hardcode a server response to `SELECT 1 FROM DUAL`. Prove `oracle-rs` can execute it via PgSaci.
3. **M3 (Proxying):** Hook up `tokio-postgres`. Pass through a basic `SELECT` statement and serialize the Postgres results into TNS row data.
4. **M4 (Translation):** Integrate the AST parser. Intercept Oracle system catalog queries (e.g., `ALL_TABLES`) and rewrite them to query `pg_class` on the backend.
