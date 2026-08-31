# PgSaci compatibility corpus

Golden files describing how a real Oracle client (`oracle-rs`, over TNS, through
PgSaci, against PostgreSQL + orafce) should behave for a given piece of SQL.
Runner: [`tests/corpus.rs`](../corpus.rs). Pure string→string translation
goldens live in [`translate/`](translate), run by
[`tests/translate_golden.rs`](../translate_golden.rs).

```
cargo test --test corpus                       # whole corpus
cargo test --test corpus -- hierarchical       # one group (file stem)
cargo test --test corpus -- --exact oracle_dates::add_months_forward
cargo test --test translate_golden
```

One PostgreSQL/orafce container and one PgSaci proxy start once per run and are
shared by every case (custom `libtest-mimic` harness), so cases are cheap to
add. The image (`pgsaci-test-pg:18`, from
[`testcontainers/Dockerfile`](../../testcontainers/Dockerfile)) must exist and
Docker must be running.

## This is a compatibility ledger, not a pass/pass suite

Cases are written to the **Oracle-correct** result and grouped by **feature
area** — never by whether they currently pass. A run reports e.g.
`368 passed; 114 failed`; the failures are the Oracle-compatibility work still
outstanding, and the number should trend down as PgSaci improves. A case only
moves or changes when *the expectation itself* was wrong about Oracle, not to
make a red case green.

`-- tag: skip` exists only for a case that would hang or wedge the shared
connection in a way the reconnect can't recover; it is not for "known failing".

## Baseline data

Every `*.sql` case runs against this committed baseline (`BASELINE_SQL` in the
runner):

| teams |             | people |          |         |
|-------|-------------|--------|----------|---------|
| 1     | Engineering | 1      | Ada      | 1       |
| 2     | Sales       | 2      | Grace    | 1       |
| 3     | Marketing   | 3      | Linus    | 2       |
|       |             | 4      | Margaret | *NULL*  |

A case that changes state (DML/DDL keyword, `-- rowcount:`, `-- setup:`,
`-- teardown:`, `-- verify:`) is rolled back and its session reconnected
afterwards, so cases are order-independent. Client `SAVEPOINT`s can't be used
for this: PgSaci wraps every statement in `SAVEPOINT pgsaci_statement … RELEASE`,
and `RELEASE SAVEPOINT` also drops any savepoint established after it.

## File format

`#` lines are comments. `-- fixture: <SQL>` lines before the first case run once
on a **direct PostgreSQL connection** (no translation — use PostgreSQL DDL) to
build extra scaffolding.

```
-- case: <unique name within the file>
-- tag: skip                    (optional) do not run — reserve for connection-wedging cases
-- setup: <SQL>                 (optional, repeatable) run before the body, not asserted
-- bind: <type> <value>         (optional, repeatable) positional binds :1 :2 …
<SQL body — one statement, trailing ; optional>
<exactly one expectation>
-- teardown: <SQL>             (optional, repeatable) run after, on the direct connection, errors ignored
-- verify: <SQL> => <scalar>   (optional) run on an independent connection; asserts one committed scalar
-- end
```

### Expectations

| directive | meaning |
|-----------|---------|
| `-- expect:` … `-- end` | the rows returned, one per line, columns joined by ` \| `, `NULL` written as `NULL`, bytes as `0x…`; an empty block means no rows |
| `-- rowcount: <n>` | DML; assert *n* rows affected |
| `-- error: <token>` | fails with error text containing `<token>` (case-insensitive) — e.g. `ORA-00001` |
| `-- expect-regex: <pattern>` | rendered result matches a small regex (`^ $ . \d [...] * + \|`) |
| `-- ok` | succeeds; rows not inspected |

### Bind types

`int`, `float`, `str`, `null`, `bytes <hex>`, `date <YYYY-MM-DD[ HH:MM:SS]>`.

## Groups

Real-world Oracle surface, roughly by how often a migrated app hits it:

`pagination` `hierarchical` `sequences` `merge_upsert` `multi_table_insert`
`outer_join` (legacy `(+)`) · `oracle_nvl_decode` `oracle_strings` `regexp`
`oracle_dates` `intervals` `oracle_conversion` `oracle_numbers`
`numeric_semantics` `null_semantics` · `ansi_select` `ansi_joins`
`ansi_aggregates` `ansi_subqueries` `cte` `ansi_setops` `ansi_conditional`
`ansi_window` · `dml` `oracle_ddl` `data_dictionary` `pseudocolumns_session`
`transactions` `error_codes` `plsql` `quoting_identifiers` · `binds` `types`
`oracle_dialect`

## Fragile internals (adversarial probes)

Some cases pass only for the exact shape tested and break on a near neighbour.
Those neighbours are in the corpus too, next to the passing sibling:

| mechanism (source) | passing shape | breaks on — see cases |
|---|---|---|
| NUMERIC decoder (`backend.rs PgNumericText`) | `123.45`, `1.25` | `0.05` → `0.5`, `0.0001` → `0.1` — `numeric_semantics::small_fraction_*` |
| ROWNUM→LIMIT (`translate.rs strip_rownum_predicate`) | `WHERE ROWNUM <= n` | `BETWEEN`, reversed operands, 3-way AND, nested paren, `OR`, `+` — `pagination::rownum_*` |
| query vs DML routing (`server.rs is_query_statement`) | `SELECT …` | `(SELECT …)` returns 0 rows silently — `oracle_dialect::parenthesised_*` |
| bind framing (`wire.rs parse_execute_request`) | each `:n` once, counts match | `:1` twice / count mismatch drops the connection — `binds::*placeholder*` |
| empty string on the wire (`write_bytes_with_length`) | non-empty strings | `''` reads back as NULL anywhere — `types::empty_string_*` |
| Oracle DATE encoding (`backend.rs encode_oracle_date`) | second precision, no tz | fractional seconds / tz offset dropped — `types::timestamp*` |
| legacy `(+)` text rewrite (`translate.rs normalize_legacy_outer_join`) | one-line, 2 tables, `=` | newline before WHERE, ` from ` inside a literal, >2 tables, subquery in FROM — `outer_join::*` |

## Adding cases

Drop the case in the matching group file (or add `<area>.sql`). Author the
expected value to what Oracle does. If it fails, that's a finding — leave it red.
When you fix a passing case that "only sort-of works", add the neighbouring
shapes that would still break.
