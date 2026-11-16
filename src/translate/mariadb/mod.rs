//! Oracle → MariaDB (`SQL_MODE=ORACLE`, 11.8+) translation.
//!
//! Pipeline, applied once per statement:
//!
//! 1. **Identifier semantics** — [`names::canonicalize`]: every unquoted word
//!    becomes its physical upper/lower spelling, quoted identifiers stay exact.
//!    All later rules see canonical text and never re-fold.
//! 2. **Structural lowerings** ([`structural`]) — MERGE, INSERT ALL/FIRST,
//!    legacy `(+)`, FULL OUTER JOIN, CONNECT BY, PIVOT/UNPIVOT, DDL statement
//!    shapes. They find query blocks with the token navigator, so subqueries
//!    are covered, and may emit a multi-statement batch.
//! 3. **Token/expression rules** ([`render`]) — one walk per statement over a
//!    lossless token stream: function calls, typed date arithmetic, NULL
//!    ordering, DDL types, reserved identifiers, dictionary routing. Literals,
//!    comments (and `/*+ hints */`), quoted names and binds are opaque tokens,
//!    and every byte no rule touches is copied verbatim.
//!
//! Where no exact MariaDB form exists the translator returns the Oracle error
//! (`Error::Ora`) instead of approximating; [`RULES`] lists every rule with its
//! kind. Translation is a pure function of the SQL text and the
//! [`MariaDbContext`] (identifier case, schema version via
//! [`SchemaInfo::version`], bind kinds, routing flag), so callers may cache it.
//!
//! **Batch contract**: statements are joined with
//! [`super::MARIADB_BATCH_SEP`]; binds are `?`, each belongs to the statement
//! it appears in, in order; a statement whose row count must not be reported
//! starts with `/* dbsaci:nocount */`. When a lowering would reorder or repeat
//! binds, the batch starts with `/* dbsaci:nocount */ SET @dbsaci_b0 = ?, …`
//! (all binds in Oracle order) and the statements use the variables.

pub mod datefmt;
pub mod helpers;
pub mod lex;
pub mod render;
pub mod rules;
pub mod structural;

use super::IdentifierCase;
use super::names;
use super::schema::{NoSchemaInfo, SchemaInfo, ValueKind};
use crate::error::{Error, Result};
use lex::{K, Ts};

pub use helpers::HELPERS;
pub use rules::{RULES, Rule, RuleKind};

/// Marker on a batch statement whose row count is not reported.
pub const NOCOUNT: &str = "/* dbsaci:nocount */";

/// Everything a MariaDB translation depends on besides the SQL text.
pub struct MariaDbContext<'a> {
    /// Physical spelling of Oracle-normal identifiers.
    pub case: IdentifierCase,
    /// Column types for type-dependent rules (`TRUNC`, `date - n`, `TO_CHAR`).
    pub schema: &'a dyn SchemaInfo,
    /// Kind of each `?` bind, in order of appearance.
    pub bind_kinds: &'a [ValueKind],
    /// Route Oracle dictionary relations / compatibility routines to
    /// `dbsaci_sys` (see [`crate::mariadb::dictionary`]).
    pub route_dictionary: bool,
}

impl Default for MariaDbContext<'_> {
    fn default() -> Self {
        MariaDbContext {
            case: IdentifierCase::default(),
            schema: &NoSchemaInfo,
            bind_kinds: &[],
            route_dictionary: false,
        }
    }
}

/// Statement class, for rules that depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtKind {
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    /// CREATE/ALTER/DROP … of tables, views, indexes, sequences.
    Ddl,
    /// Stored program definitions and anonymous blocks.
    Plsql,
    Trigger,
    Other,
}

pub fn classify(ts: &Ts) -> StmtKind {
    let n = ts.len();
    let mut i = ts.sig(0, n);
    while i < n && ts.k(i) == K::LParen {
        i = ts.next(i, n);
    }
    let w = |j: usize, k: &str| ts.is(j, k);
    if w(i, "SELECT") || w(i, "WITH") {
        return StmtKind::Select;
    }
    if w(i, "INSERT") {
        return StmtKind::Insert;
    }
    if w(i, "UPDATE") {
        return StmtKind::Update;
    }
    if w(i, "DELETE") {
        return StmtKind::Delete;
    }
    if w(i, "MERGE") {
        return StmtKind::Merge;
    }
    if w(i, "BEGIN") || w(i, "DECLARE") {
        return StmtKind::Plsql;
    }
    if w(i, "CREATE") {
        let mut j = ts.next(i, n);
        for _ in 0..6 {
            if ts.is_any(
                j,
                &["OR", "REPLACE", "EDITIONABLE", "NONEDITIONABLE", "DEFINER"],
            ) {
                j = ts.next(j, n);
            }
        }
        if w(j, "TRIGGER") {
            return StmtKind::Trigger;
        }
        if ts.is_any(j, &["PROCEDURE", "FUNCTION", "PACKAGE", "TYPE"]) {
            return StmtKind::Plsql;
        }
        return StmtKind::Ddl;
    }
    if ts.is_any(
        i,
        &["ALTER", "DROP", "TRUNCATE", "COMMENT", "RENAME", "REFRESH"],
    ) {
        return StmtKind::Ddl;
    }
    StmtKind::Other
}

/// Translate with default context (upper-case identifiers, no schema info,
/// no bind kinds, no dictionary routing).
pub fn oracle_to_mariadb(sql: &str) -> Result<String> {
    oracle_to_mariadb_ctx(sql, &MariaDbContext::default())
}

/// [`oracle_to_mariadb`] with an explicit identifier case.
pub fn oracle_to_mariadb_with_case(sql: &str, case: IdentifierCase) -> Result<String> {
    oracle_to_mariadb_ctx(
        sql,
        &MariaDbContext {
            case,
            ..MariaDbContext::default()
        },
    )
}

/// Translate one Oracle statement for MariaDB 11.8+ in `SQL_MODE=ORACLE`.
pub fn oracle_to_mariadb_ctx(sql: &str, cx: &MariaDbContext) -> Result<String> {
    let sql = trim_statement(sql);
    let canon = names::canonicalize(sql, cx.case);
    {
        // fast path: one lex, no structural rule applies
        let ts = Ts::new(&canon);
        if let Some(s) = alter_session(&ts) {
            return Ok(s);
        }
        let kind = classify(&ts);
        if !needs_structural(&ts, kind) {
            let r = render::Render::new(&ts, cx, kind, 0);
            let text = r.range(0, ts.len());
            render::take_err(&r)?;
            return Ok(text);
        }
    }
    let stmts = lower_statement(canon, cx)?;
    let mut out = Vec::with_capacity(stmts.len());
    let mut bind_base = 0u32;
    for s in &stmts {
        let ts = Ts::new(s);
        let kind = classify(&ts);
        let r = render::Render::new(&ts, cx, kind, bind_base);
        let text = r.range(0, ts.len());
        render::take_err(&r)?;
        bind_base += ts.t.iter().filter(|t| t.k == K::Bind).count() as u32;
        out.push(text);
    }
    Ok(out.join(super::MARIADB_BATCH_SEP))
}

static STRUCTURAL_WORDS: lex::KwSet = lex::KwSet::new(&[
    "PIVOT",
    "UNPIVOT",
    "GENERATE_SERIES",
    "CONNECT",
    "FULL",
    "LATERAL",
    "KEEP",
    "LISTAGG",
    "MEDIAN",
    "PERCENTILE_CONT",
    "PERCENTILE_DISC",
]);

/// Does any structural lowering apply to this statement?
fn needs_structural(ts: &Ts, kind: StmtKind) -> bool {
    if matches!(kind, StmtKind::Ddl | StmtKind::Trigger | StmtKind::Merge) {
        return true;
    }
    if kind == StmtKind::Insert {
        let i = ts.sig(0, ts.len());
        if ts.is_any(ts.next(i, ts.len()), &["ALL", "FIRST"]) {
            return true;
        }
    }
    (0..ts.len()).any(|i| match ts.k(i) {
        K::Word => ts.in_set(i, &STRUCTURAL_WORDS),
        K::LParen => {
            ts.is_op(ts.next(i, ts.len()), "+")
                && ts.next(ts.next(i, ts.len()), ts.len()) == ts.pair[i]
        }
        _ => false,
    })
}

/// Trim whitespace and a trailing `;` (outside literals/comments).
fn trim_statement(sql: &str) -> &str {
    let t = sql.trim();
    if t.ends_with(';') {
        let toks = lex::lex(t);
        if toks.last().is_some_and(|x| x.k == K::Semi) {
            // a PL/SQL block keeps its END; MariaDB accepts both
            return t[..t.len() - 1].trim_end();
        }
    }
    t
}

/// Statement-level structural lowerings; may return several statements.
fn lower_statement(sql: String, cx: &MariaDbContext) -> Result<Vec<String>> {
    let (kind, words, has_outer_marker) = {
        let ts = Ts::new(&sql);
        let words: Vec<String> = (0..ts.len())
            .filter(|&i| ts.k(i) == K::Word)
            .map(|i| ts.text(i).to_ascii_uppercase())
            .collect();
        let marker = (0..ts.len()).any(|i| {
            ts.k(i) == K::LParen
                && ts.is_op(ts.next(i, ts.len()), "+")
                && ts.next(ts.next(i, ts.len()), ts.len()) == ts.pair[i]
        });
        (classify(&ts), words, marker)
    };
    let has = |w: &str| words.iter().any(|x| x == w);
    let mut sql = sql;

    // DDL statement shapes (synonyms, temp tables, comments, triggers, …)
    if matches!(kind, StmtKind::Ddl | StmtKind::Trigger) {
        match ddl_statement(&sql)? {
            Ddl::Replaced(v) => return Ok(v),
            Ddl::Text(s) => sql = s,
        }
    }
    // query-shape lowerings (any statement kind: subqueries count)
    if has("PIVOT") || has("UNPIVOT") {
        sql = super::rewrite_unpivot_mariadb(&sql);
        sql = super::rewrite_pivot(&sql);
    }
    if has("GENERATE_SERIES") {
        sql = super::rewrite_generate_series(&sql)
            .replace(
                "SELECT (SELECT g FROM mariadb_series",
                "SELECT g FROM (SELECT g FROM mariadb_series",
            )
            .replace(") G FROM DUAL", ") _gs")
            .replace(") FROM DUAL) Q", ") _gs) Q")
            .replace(") FROM DUAL) q", ") _gs) q");
    }
    if has_outer_marker {
        sql = structural::lower_outer_joins(sql)?;
    }
    if has("CONNECT") {
        sql = structural::lower_connect_by(sql, cx)?;
    }
    if has("FULL") {
        sql = structural::lower_full_joins(sql)?;
    }
    if has("LATERAL") {
        sql = super::rewrite_mariadb_lateral(&sql);
    }
    if has("KEEP") {
        sql = super::rewrite_keep_first_last(&sql);
    }
    if has("LISTAGG") && has("OVER") {
        sql = super::rewrite_within_group_agg(&sql, "LISTAGG");
    }
    for agg in ["MEDIAN", "PERCENTILE_CONT", "PERCENTILE_DISC"] {
        if has(agg) {
            sql = super::rewrite_ordered_set_aggregate(&sql, agg);
        }
    }
    if kind == StmtKind::Merge
        && let Some(v) = structural::lower_merge(&sql, cx)?
    {
        return Ok(v);
    }
    if kind == StmtKind::Insert
        && let Some(v) = structural::lower_insert_multi(&sql)?
    {
        return Ok(v);
    }
    Ok(vec![sql])
}

enum Ddl {
    Text(String),
    Replaced(Vec<String>),
}

fn ddl_statement(sql: &str) -> Result<Ddl> {
    let ts = Ts::new(sql);
    let n = ts.len();
    let first = ts.sig(0, n);
    let second = ts.next(first, n);
    // COMMENT ON TABLE t IS '…' / COMMENT ON COLUMN …
    if ts.is(first, "COMMENT") && ts.is(second, "ON") {
        let what = ts.next(second, n);
        if ts.is(what, "TABLE") {
            let is = ts
                .find_kw(what, n, &["IS"])
                .ok_or_else(|| Error::ora(905, "missing keyword"))?;
            return Ok(Ddl::Replaced(vec![format!(
                "ALTER TABLE {} COMMENT = {}",
                ts.tslice(what + 1, is),
                ts.tslice(is + 1, n)
            )]));
        }
        return Ok(Ddl::Replaced(vec!["DO 0".into()]));
    }
    // REFRESH MATERIALIZED VIEW / CREATE MATERIALIZED VIEW
    if ts.is(first, "REFRESH") {
        return Ok(Ddl::Replaced(vec!["DO 0".into()]));
    }
    let mut s = super::rewrite_oracle_ddl_common(sql);
    {
        let ts = Ts::new(&s);
        let n = ts.len();
        let c = ts.sig(0, n);
        let m = ts.next(c, n);
        if ts.is(c, "CREATE") && ts.is(m, "MATERIALIZED") && ts.is(ts.next(m, n), "VIEW") {
            let v = ts.next(m, n);
            let name = ts.next(v, n);
            let (_, nend) = render_path(&ts, name);
            let as_kw = ts
                .find_kw(nend, n, &["AS"])
                .ok_or_else(|| Error::ora(905, "missing keyword"))?;
            s = format!(
                "CREATE TABLE {} AS {}",
                ts.slice(name, nend),
                ts.tslice(as_kw + 1, n)
            );
        }
    }
    // ON COMMIT … ROWS is a temporary-table clause MariaDB lacks.
    {
        let ts = Ts::new(&s);
        let n = ts.len();
        if let Some(on) = (0..n).find(|&i| ts.is(i, "ON") && ts.is(ts.next(i, n), "COMMIT")) {
            let mut e = ts.next(ts.next(on, n), n);
            if ts.is_any(e, &["PRESERVE", "DELETE", "DROP"]) {
                e = ts.next(e, n);
                if ts.is_any(e, &["ROWS", "DEFINITION"]) {
                    s = format!("{}{}", ts.slice(0, on).trim_end(), ts.slice(e + 1, n));
                }
            }
        }
    }
    let s = super::rewrite_alter_modify_parens(&s);
    let s = super::rewrite_alter_drop_columns(&s);
    let s = super::rewrite_function_based_index(&s);
    // sequences: Oracle MAXVALUE beyond BIGINT, ORDER/NOORDER
    let s = sequence_options(&s);
    // triggers
    let s = super::rewrite_mariadb_trigger_when(&s);
    let s = super::rewrite_mariadb_trigger_referencing(&s);
    let parts: Vec<String> = super::rewrite_mariadb_multi_event_trigger(&s)
        .split(super::MARIADB_BATCH_SEP)
        .map(|p| p.to_string())
        .collect();
    if parts.len() > 1 {
        // per-event bodies: resolve INSERTING / UPDATING / DELETING
        let out = parts
            .iter()
            .map(|p| resolve_trigger_predicates(p))
            .collect::<Vec<_>>();
        return Ok(Ddl::Replaced(out));
    }
    Ok(Ddl::Text(resolve_trigger_predicates(&parts[0])))
}

fn render_path(ts: &Ts, i: usize) -> (Vec<usize>, usize) {
    let mut j = i;
    let mut v = vec![];
    while j < ts.len() && matches!(ts.k(j), K::Word | K::Quoted | K::Back) {
        v.push(j);
        if j + 2 < ts.len() && ts.k(j + 1) == K::Dot {
            j += 2;
        } else {
            return (v, j + 1);
        }
    }
    (v, j)
}

fn sequence_options(sql: &str) -> String {
    let ts = Ts::new(sql);
    let n = ts.len();
    let c = ts.sig(0, n);
    if !(ts.is_any(c, &["CREATE", "ALTER"]) && ts.is(ts.next(c, n), "SEQUENCE")) {
        return sql.to_string();
    }
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        if ts.is_any(
            i,
            &[
                "ORDER", "NOORDER", "NOKEEP", "KEEP", "NOSCALE", "GLOBAL", "SESSION",
            ],
        ) {
            i += 1;
            continue;
        }
        if ts.is_any(i, &["MAXVALUE", "MINVALUE"]) {
            let v = ts.next(i, n);
            let neg = ts.is_op(v, "-");
            let num = if neg { ts.next(v, n) } else { v };
            if num < n && ts.k(num) == K::Num && ts.text(num).parse::<i64>().is_err() {
                out.push_str(if ts.is(i, "MAXVALUE") {
                    if neg { "NOMAXVALUE" } else { "NOMAXVALUE" }
                } else {
                    "NOMINVALUE"
                });
                i = num + 1;
                continue;
            }
        }
        out.push_str(ts.text(i));
        i += 1;
    }
    out
}

/// In a single-event trigger body, `INSERTING` / `UPDATING` / `DELETING` are
/// constants.
fn resolve_trigger_predicates(sql: &str) -> String {
    let ts = Ts::new(sql);
    let n = ts.len();
    let Some(t) = (0..n).find(|&i| ts.is(i, "TRIGGER")) else {
        return sql.to_string();
    };
    let ev = ts.find_kw(t, n, &["INSERT", "UPDATE", "DELETE"]);
    let Some(ev) = ev else {
        return sql.to_string();
    };
    // only when the header has exactly one event
    let on = ts.find_kw(ev, n, &["ON"]).unwrap_or(n);
    if ts.find_kw(ev, on, &["OR"]).is_some() {
        return sql.to_string();
    }
    let event = ts.text(ev).to_ascii_uppercase();
    let mut out = String::new();
    for i in 0..n {
        let w = ts.text(i);
        if ts.k(i) == K::Word && i > on {
            let e = match w.to_ascii_uppercase().as_str() {
                "INSERTING" => Some(event == "INSERT"),
                "UPDATING" => Some(event == "UPDATE"),
                "DELETING" => Some(event == "DELETE"),
                _ => None,
            };
            if let Some(v) = e {
                out.push_str(if v { "TRUE" } else { "FALSE" });
                continue;
            }
        }
        out.push_str(w);
    }
    out
}

/// `ALTER SESSION SET <name> = <value>`. The 0.3.0 adapter intercepts these
/// before translation; this keeps a self-contained mapping for callers that
/// do not: `TIME_ZONE` → `SET time_zone`, `CURRENT_SCHEMA` → session variable
/// + `USE`, `NLS_*` → the session NLS table, anything else → `DO 0`.
fn alter_session(ts: &Ts) -> Option<String> {
    let n = ts.len();
    let a = ts.sig(0, n);
    let s = ts.next(a, n);
    let set = ts.next(s, n);
    if !(ts.is(a, "ALTER") && ts.is(s, "SESSION") && ts.is(set, "SET")) {
        return None;
    }
    let name = ts.next(set, n);
    if name >= n {
        return Some("DO 0".into());
    }
    let mut v = ts.next(name, n);
    if ts.is_op(v, "=") {
        v = ts.next(v, n);
    }
    let value = ts.tslice(v, n);
    let unq = value.trim_matches('\'').trim_matches('"');
    let key = names::oracle_name(ts.text(name), IdentifierCase::Lower).to_ascii_uppercase();
    Some(match key.as_str() {
        "TIME_ZONE" => format!("SET time_zone = '{}'", unq.replace('\'', "''")),
        "CURRENT_SCHEMA" => {
            let schema = if value.starts_with('"') {
                unq.to_string()
            } else {
                unq.to_string()
            };
            format!(
                "SET @dbsaci_current_schema = '{}'{sep}USE `{}`",
                schema.replace('\'', "''"),
                schema.replace('`', "``"),
                sep = super::MARIADB_BATCH_SEP
            )
        }
        k if k.starts_with("NLS_") => format!(
            "UPDATE nls_session_parameters SET value = '{}' WHERE parameter = '{k}'",
            unq.replace('\'', "''")
        ),
        _ => "DO 0".into(),
    })
}

#[cfg(test)]
mod tests;
