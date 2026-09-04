//! Structural Oracle-to-PostgreSQL SQL translation.
//!
//! This module deliberately starts from `sqlparser`'s Oracle AST rather than
//! applying substitutions to arbitrary SQL text.  Rules are narrow and tested:
//! unsupported Oracle-only constructs are left for the `orafce` extension or
//! reported by PostgreSQL, instead of silently rewriting a different query.

use std::ops::ControlFlow;

use sqlparser::ast::helpers::attached_token::AttachedToken;
use sqlparser::ast::{
    BinaryOperator, CaseWhen, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Join,
    JoinConstraint, JoinOperator, LimitClause, ObjectName, ObjectNamePart, OrderBy, OrderByExpr,
    OrderByKind, Query, SelectItem, SetExpr, Statement, TableFactor, Value, ValueWithSpan,
    visit_relations_mut,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::error::{Error, Result};

/// How the MariaDB backend normalizes Oracle table identifiers so a client
/// that mixes case (`A_TABLE`, `a_table`, `"A_TABLE"` — all the same object to
/// Oracle) reaches one consistently-cased MariaDB object regardless of the
/// server's `lower_case_table_names` setting.
///
/// Oracle itself folds *unquoted* identifiers to upper case and leaves *quoted*
/// ones exactly as written; `Upper` mirrors that (and is what `data.sql`-style
/// vendored Oracle schemas already look like), so it is the default. Choose
/// `Lower` when the backend schema was authored in the PostgreSQL/MariaDB
/// convention of lower-case objects. Either way the point is consistency, not
/// which case is "correct" — the backend schema must be authored to match.
///
/// PostgreSQL is not affected by this setting: it downcases unquoted
/// identifiers itself, so a quoted-uppercase Oracle identifier already folds
/// to a lower-case PostgreSQL one via [`fold_uppercase_quoted_identifiers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentifierCase {
    #[default]
    Upper,
    Lower,
}

impl IdentifierCase {
    fn apply(self, s: &str) -> String {
        match self {
            IdentifierCase::Upper => s.to_ascii_uppercase(),
            IdentifierCase::Lower => s.to_ascii_lowercase(),
        }
    }
}

impl std::str::FromStr for IdentifierCase {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "upper" => Ok(IdentifierCase::Upper),
            "lower" => Ok(IdentifierCase::Lower),
            other => Err(format!(
                "unknown identifier case {other:?} (want upper or lower)"
            )),
        }
    }
}

/// Fold every table/view identifier reachable in table-name position (`FROM`,
/// `JOIN`, `UPDATE`, `INSERT INTO`, DDL targets, …) to `case`, via
/// `sqlparser`'s relation visitor rather than text scanning.
///
/// `DUAL` (a pseudo-table, not a schema object) is left alone. An unquoted
/// identifier is always folded — Oracle would have upshifted it regardless of
/// how the client wrote it. A *quoted* identifier is folded only when it is
/// already single-case (all upper or all lower): that is indistinguishable
/// from a client that just quoted an ordinary name, whereas genuine
/// `"MixedCase"` is Oracle's case-sensitive escape hatch and must survive
/// untouched.
fn fold_relation_case(statements: &mut Vec<Statement>, case: IdentifierCase) {
    let _ = visit_relations_mut(statements, |name: &mut ObjectName| {
        for part in &mut name.0 {
            if let ObjectNamePart::Identifier(ident) = part
                && !ident.value.eq_ignore_ascii_case("dual")
                && (ident.quote_style.is_none() || is_single_case(&ident.value))
            {
                ident.value = case.apply(&ident.value);
            }
        }
        ControlFlow::<()>::Continue(())
    });
}

/// `true` if `s` has no letters of the non-dominant case — i.e. it reads as a
/// plain unquoted-style identifier (`FOO`, `foo`, `foo_2`) rather than a
/// deliberately mixed-case one (`FooBar`).
fn is_single_case(s: &str) -> bool {
    !(s.bytes().any(|b| b.is_ascii_uppercase()) && s.bytes().any(|b| b.is_ascii_lowercase()))
}

/// Separator the MariaDB translator uses when a single Oracle statement must be
/// executed as several MariaDB statements (multi-event trigger, `INSERT ALL`).
/// The backend adapter splits on it and runs each part on the same connection.
/// A SQL comment so it is visible in traces; it never appears in client SQL.
pub(crate) const MARIADB_BATCH_SEP: &str = "\n/* dbsaci-batch */\n";

// ---------------------------------------------------------------------------
// sqlparser 0.62 AST compatibility shims
//
// 0.62 wraps literals in `ValueWithSpan`, folds `CASE` arms into `CaseWhen`,
// makes `ObjectName` a list of `ObjectNamePart`, moves `LIMIT`/`ORDER BY` into
// `Query::limit_clause` / `Query::order_by: Option<OrderBy>`, and adds token /
// span bookkeeping fields to many structs. These helpers keep the rewrite rules
// below expressed in terms of the parts that carry meaning.
// ---------------------------------------------------------------------------

/// Build an `Expr::Value` from a bare `Value` (0.62 needs a span wrapper).
fn lit(value: Value) -> Expr {
    Expr::Value(value.into())
}

/// Borrow the inner `Value` of an `Expr::Value`, ignoring its span.
fn as_value(expr: &Expr) -> Option<&Value> {
    match expr {
        Expr::Value(ValueWithSpan { value, .. }) => Some(value),
        _ => None,
    }
}

/// The last (unqualified) component of a possibly-qualified name, if it is a
/// plain identifier.
fn name_last(name: &ObjectName) -> Option<&str> {
    name.0.last()?.as_ident().map(|i| i.value.as_str())
}

/// A single-identifier `ObjectName` (function names, synthetic relations).
fn obj_name(ident: &str) -> ObjectName {
    ObjectName(vec![ObjectNamePart::Identifier(Ident::new(ident))])
}

/// Empty `AttachedToken` for synthetic `CASE` nodes.
fn no_token() -> AttachedToken {
    AttachedToken::empty()
}

/// The `ORDER BY` expression list of a query, mutable; empty for `ORDER BY ALL`
/// or no `ORDER BY`.
fn order_by_exprs_mut(query: &mut Query) -> &mut [OrderByExpr] {
    match &mut query.order_by {
        Some(OrderBy {
            kind: OrderByKind::Expressions(exprs),
            ..
        }) => exprs.as_mut_slice(),
        _ => &mut [],
    }
}

/// True when the query carries no positional `ORDER BY` list.
fn order_by_is_empty(query: &Query) -> bool {
    !matches!(
        &query.order_by,
        Some(OrderBy { kind: OrderByKind::Expressions(exprs), .. }) if !exprs.is_empty()
    )
}

/// The `LIMIT <n>` expression of a query, if it has a plain limit.
fn query_limit(query: &Query) -> Option<&Expr> {
    match &query.limit_clause {
        Some(LimitClause::LimitOffset { limit: Some(e), .. }) => Some(e),
        _ => None,
    }
}

/// Set (or replace) a query's `LIMIT <n>`.
fn set_query_limit(query: &mut Query, limit: Expr) {
    query.limit_clause = Some(LimitClause::LimitOffset {
        limit: Some(limit),
        offset: None,
        limit_by: vec![],
    });
}

/// Parse a standalone query snippet (used to re-assemble structural rewrites
/// without hand-constructing every 0.62 struct field). Never panics: a snippet
/// the translator itself could not re-parse surfaces as a `SqlParse` error so
/// the connection stays up.
fn parse_query(sql: &str) -> Result<Query> {
    Parser::new(&GenericDialect {})
        .try_with_sql(sql)
        .and_then(|mut p| p.parse_query())
        .map(|q| *q)
        .map_err(|e| {
            Error::SqlParse(format!(
                "internal query snippet failed to parse ({e}): {sql}"
            ))
        })
}

/// Translate Oracle SQL for the selected database engine. `identifier_case`
/// only affects the MariaDB backend (see [`IdentifierCase`]); PostgreSQL folds
/// unquoted identifiers itself.
pub fn oracle_to_backend(
    sql: &str,
    backend: crate::backend::BackendKind,
    identifier_case: IdentifierCase,
) -> Result<String> {
    match backend {
        crate::backend::BackendKind::Postgres => oracle_to_postgres(sql),
        crate::backend::BackendKind::MariaDb => oracle_to_mariadb_with_case(sql, identifier_case),
    }
}

/// Rewrite every top-level `NAME(...)` call in `sql` by passing its argument
/// text to `f`. `f` returns `None` to leave the call untouched. Recurses into
/// the replacement's own body via `f` re-running on the whole string until it
/// stabilises is *not* done here — callers compose passes explicitly.
fn map_calls(sql: &str, name: &str, f: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let name_bytes = name.as_bytes();
    let open = name.len();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &sql[i..];
        // Byte comparison: `rest[..open]` would panic when `open` bytes land
        // inside a multi-byte character further along the statement.
        let hit = rest.len() > open
            && rest.as_bytes()[..open].eq_ignore_ascii_case(name_bytes)
            && rest.as_bytes()[open] == b'('
            && !bytes
                .get(i.wrapping_sub(1))
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'.');
        if hit && let Some(close) = matching_paren(&rest[open..]) {
            let inner = &rest[open + 1..open + close];
            if let Some(rep) = f(inner) {
                out.push_str(&rep);
                i += open + close + 1;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `TO_CHAR(<expr>)` — a single argument, no format model. Oracle returns the
/// value's default text form for any type; MariaDB's `TO_CHAR` only accepts a
/// datetime, so fall back to a cast, which is correct for a number or a date.
fn rewrite_mariadb_to_char_single_arg(sql: &str) -> String {
    map_calls(sql, "TO_CHAR", &|inner| {
        (split_top_level_commas(inner).len() == 1).then(|| {
            // `CHAR(4000)` (not bare `CHAR`) so a later ` CHAR)` cleanup for
            // Oracle `VARCHAR2(n CHAR)` cannot truncate it.
            format!(
                "CAST({} AS CHAR(4000))",
                rewrite_mariadb_to_char_single_arg(inner)
            )
        })
    })
}

/// Oracle scalar functions and pseudo-columns MariaDB's ORACLE mode does not
/// provide, mapped to native equivalents.
fn rewrite_mariadb_scalar_functions(sql: &str) -> String {
    // `DBMS_OUTPUT.PUT_LINE(x)` — no server-side output buffer here; collapse to
    // the PL/SQL no-op so an anonymous block still compiles and runs.
    let sql = map_calls(sql, "DBMS_OUTPUT.PUT_LINE", &|_| Some("NULL".to_string()));
    let sql = replace_ident_ci(&sql, "SYSTIMESTAMP", "CURRENT_TIMESTAMP(6)");
    let sql = replace_ident_ci(&sql, "SESSIONTIMEZONE", "sessiontimezone()");
    let sql = replace_ident_ci(&sql, "DBTIMEZONE", "dbtimezone()");
    let sql = replace_ident_ci(&sql, "UID", "CONNECTION_ID()");
    let sql = replace_ident_ci(&sql, "USER", "sys_context('USERENV', 'CURRENT_USER')");
    // `DBMS_LOB` operates on inline CLOB/BLOB text here (no locator layer).
    // `SUBSTR(lob, amount, offset)` and `INSTR(lob, patt, offset)` take their
    // length/position arguments in the opposite order to the SQL builtins.
    let sql = map_calls(&sql, "DBMS_LOB.GETLENGTH", &|inner| {
        Some(format!("LENGTH({inner})"))
    });
    let sql = map_calls(&sql, "DBMS_LOB.SUBSTR", &|inner| {
        let p = split_top_level_commas(inner);
        match p.len() {
            2 => Some(format!("SUBSTR({}, {})", p[0].trim(), p[1].trim())),
            3 => Some(format!(
                "SUBSTR({}, {}, {})",
                p[0].trim(),
                p[2].trim(),
                p[1].trim()
            )),
            _ => None,
        }
    });
    let sql = map_calls(&sql, "DBMS_LOB.INSTR", &|inner| {
        let p = split_top_level_commas(inner);
        match p.len() {
            2 => Some(format!("INSTR({}, {})", p[0].trim(), p[1].trim())),
            3 | 4 => Some(format!(
                "LOCATE({}, {}, {})",
                p[1].trim(),
                p[0].trim(),
                p[2].trim()
            )),
            _ => None,
        }
    });
    // `TO_NUMBER(x)` / `TO_NUMBER(x, fmt)` — native only from MariaDB 12.2.
    // A cast covers the plain case; a format model means "strip grouping
    // punctuation, then cast".
    let sql = map_calls(&sql, "TO_NUMBER", &|inner| {
        let parts = split_top_level_commas(inner);
        match parts.len() {
            1 => Some(format!("CAST({} AS DECIMAL(38,10))", parts[0].trim())),
            2 => Some(format!(
                "CAST(REPLACE(REPLACE(REPLACE({}, ',', ''), '$', ''), ' ', '') AS DECIMAL(38,10))",
                parts[0].trim()
            )),
            _ => None,
        }
    });
    // `TO_DATE(text, fmt)` -> `STR_TO_DATE(text, <mysql fmt>)`.
    let sql = map_calls(&sql, "TO_DATE", &|inner| {
        let parts = split_top_level_commas(inner);
        (parts.len() == 2).then(|| {
            format!(
                "STR_TO_DATE({}, {})",
                parts[0].trim(),
                oracle_date_format_to_mysql(parts[1].trim())
            )
        })
    });
    let sql = map_calls(&sql, "REMAINDER", &|inner| {
        let parts = split_top_level_commas(inner);
        (parts.len() == 2).then(|| {
            let (a, b) = (parts[0].trim(), parts[1].trim());
            format!("({a} - {b} * ROUND(({a}) / ({b})))")
        })
    });
    let sql = map_calls(&sql, "BITAND", &|inner| {
        let parts = split_top_level_commas(inner);
        (parts.len() == 2).then(|| format!("({} & {})", parts[0].trim(), parts[1].trim()))
    });
    // `LNNVL(cond)` is TRUE when `cond` is FALSE or UNKNOWN.
    let sql = map_calls(&sql, "LNNVL", &|inner| {
        Some(format!("(NOT ({inner}) OR ({inner}) IS NULL)"))
    });
    // `NANVL(x, y)` -> y when x is NaN. MariaDB has no NaN in DECIMAL/DOUBLE
    // arithmetic (a bad cast yields NULL), so `IFNULL` is the faithful mapping.
    let sql = map_calls(&sql, "NANVL", &|inner| {
        let parts = split_top_level_commas(inner);
        (parts.len() == 2).then(|| format!("IFNULL({}, {})", parts[0].trim(), parts[1].trim()))
    });
    // `REGEXP_LIKE(subj, pat)` / `(subj, pat, 'i')` -> the REGEXP operator.
    // With `NO_BACKSLASH_ESCAPES` set, the pattern text matches Oracle's.
    let sql = map_calls(&sql, "REGEXP_LIKE", &|inner| {
        let parts = split_top_level_commas(inner);
        match parts.as_slice() {
            [s, p] => Some(format!("({} REGEXP {})", s.trim(), p.trim())),
            [s, p, flags] if flags.trim().trim_matches('\'').contains('i') => {
                Some(format!("(LOWER({}) REGEXP LOWER({}))", s.trim(), p.trim()))
            }
            [s, p, _] => Some(format!("({} REGEXP {})", s.trim(), p.trim())),
            _ => None,
        }
    });
    // `LTRIM(s, set)` / `RTRIM(s, set)` — MariaDB's `LTRIM`/`RTRIM` take one
    // argument (whitespace only). Oracle strips any leading/trailing character
    // in `set`; `TRIM(LEADING/TRAILING ... FROM ...)` only strips one *string*,
    // so fall back to `REGEXP_REPLACE` with a character class for the general
    // case and the simpler `TRIM` form for a single-character set.
    let sql = map_calls(&sql, "LTRIM", &|inner| rewrite_oracle_trim(inner, true));
    let sql = map_calls(&sql, "RTRIM", &|inner| rewrite_oracle_trim(inner, false));
    // `SYS_CONTEXT('USERENV','SESSIONTIMEZONE'|'DB_TIMEZONE')` — MariaDB's
    // native `SYS_CONTEXT` does not know these keys; route them to the UDFs.
    let sql = map_calls(&sql, "SYS_CONTEXT", &|inner| {
        let p = split_top_level_commas(inner);
        if p.len() < 2 {
            return None;
        }
        match p[1].trim().trim_matches('\'').to_ascii_uppercase().as_str() {
            "SESSIONTIMEZONE" => Some("sessiontimezone()".to_string()),
            "DB_TIMEZONE" | "DBTIMEZONE" => Some("dbtimezone()".to_string()),
            _ => None,
        }
    });
    // PostgreSQL `DECODE(text, 'hex'|'base64'|'escape')` (binary decode) — the
    // corpus uses it for RAW literals. MariaDB's equivalent for hex is `UNHEX`.
    let sql = map_calls(&sql, "DECODE", &|inner| {
        let p = split_top_level_commas(inner);
        (p.len() == 2 && p[1].trim().trim_matches('\'').eq_ignore_ascii_case("hex"))
            .then(|| format!("UNHEX({})", p[0].trim()))
    });
    // `RAWTOHEX(x)` / `HEXTORAW(x)` -> `HEX` / `UNHEX` (Oracle's hex is upper).
    let sql = map_calls(&sql, "RAWTOHEX", &|inner| Some(format!("HEX({inner})")));
    let sql = map_calls(&sql, "HEXTORAW", &|inner| Some(format!("UNHEX({inner})")));
    // `TRANSLATE(s, from, to)` -> the `oracle_translate` compat UDF (`TRANSLATE`
    // is a reserved word in MariaDB and has no built-in).
    let sql = map_calls(&sql, "TRANSLATE", &|inner| {
        let p = split_top_level_commas(inner);
        (p.len() == 3).then(|| {
            format!(
                "oracle_translate({}, {}, {})",
                p[0].trim(),
                p[1].trim(),
                p[2].trim()
            )
        })
    });
    // Oracle `INSTR(s, sub, pos[, occurrence])` -> compat UDF whenever a start
    // position is given (MariaDB's native `INSTR` is 2-arg); the 1/2-arg forms
    // are native.
    let sql = map_calls(&sql, "INSTR", &|inner| {
        let parts = split_top_level_commas(inner);
        match parts.len() {
            3 => Some(format!(
                "oracle_instr({}, {}, {}, 1)",
                parts[0].trim(),
                parts[1].trim(),
                parts[2].trim()
            )),
            4 => Some(format!(
                "oracle_instr({}, {}, {}, {})",
                parts[0].trim(),
                parts[1].trim(),
                parts[2].trim(),
                parts[3].trim()
            )),
            _ => None,
        }
    });
    // `REGEXP_SUBSTR(s, p, pos[, occ[, match_param[, grp]]])` — MariaDB's is
    // 2-arg; anything more resolves through the compat UDF.
    let sql = map_calls(&sql, "REGEXP_SUBSTR", &|inner| {
        let p = split_top_level_commas(inner);
        (p.len() >= 3).then(|| {
            let pos = p.get(2).map(|s| s.trim()).unwrap_or("1");
            let occ = p.get(3).map(|s| s.trim()).unwrap_or("1");
            let grp = p.get(5).map(|s| s.trim()).unwrap_or("NULL");
            format!(
                "oracle_regexp_substr({}, {}, {pos}, {occ}, {grp})",
                p[0].trim(),
                p[1].trim()
            )
        })
    });
    // `x + NUMTODSINTERVAL(n,'UNIT')` / `NUMTOYMINTERVAL` in date arithmetic ->
    // `DATE_ADD`. The bare-value form falls through to the compat UDF.
    let sql = rewrite_numto_interval_arith(&sql);
    let sql = map_calls(&sql, "TRUNC", &|inner| {
        let parts = split_top_level_commas(inner);
        let first_up = parts[0].to_ascii_uppercase();
        let date_ish = ["DATE ", "TIMESTAMP ", "SYSDATE", "SYSTIMESTAMP", "CURRENT_"]
            .iter()
            .any(|k| first_up.contains(k))
            // Oracle's TRUNC is overloaded.  MariaDB's numeric TRUNCATE
            // cannot truncate a datetime, so recognize the conventional
            // timestamp column spellings that commonly reach this path
            // (e.g. TRUNC(fe.tv)) as date-valued too.
            || [".TV", ".DATE", "_DATE", ".TIME", "_TIME", ".TIMESTAMP", "_TIMESTAMP"]
                .iter()
                .any(|suffix| first_up.ends_with(suffix));
        match (date_ish, parts.len()) {
            (false, 1) => Some(format!("TRUNCATE({}, 0)", parts[0].trim())),
            (false, 2) => Some(format!(
                "TRUNCATE({}, {})",
                parts[0].trim(),
                parts[1].trim()
            )),
            // TRUNC(date) -> midnight; TRUNC(date,'MM'|'YYYY'|...) -> period start.
            (true, 1) => Some(format!("DATE({})", parts[0].trim())),
            (true, 2) => {
                let unit = parts[1].trim().trim_matches('\'').to_ascii_uppercase();
                let fmt = match unit.as_str() {
                    "MM" | "MON" | "MONTH" => "'%Y-%m-01'",
                    "YYYY" | "YEAR" | "YY" | "IY" | "IYYY" => "'%Y-01-01'",
                    "DD" | "DDD" | "J" => "'%Y-%m-%d'",
                    _ => return None,
                };
                Some(format!("DATE_FORMAT({}, {})", parts[0].trim(), fmt))
            }
            _ => None,
        }
    });
    // `ROUND(date, 'MM')` -> nearest month boundary (day >= 16 rounds up).
    let sql = map_calls(&sql, "ROUND", &|inner| {
        let parts = split_top_level_commas(inner);
        if parts.len() != 2 {
            return None;
        }
        let up = parts[0].to_ascii_uppercase();
        let date_ish = ["DATE ", "TIMESTAMP ", "SYSDATE"]
            .iter()
            .any(|k| up.contains(k));
        let unit = parts[1].trim().trim_matches('\'').to_ascii_uppercase();
        (date_ish && matches!(unit.as_str(), "MM" | "MONTH" | "MON")).then(|| {
            let d = parts[0].trim();
            format!(
                "DATE_FORMAT(DATE_ADD({d}, INTERVAL IF(DAYOFMONTH({d}) >= 16, 1, 0) MONTH), '%Y-%m-01')"
            )
        })
    });
    // `TO_CHAR(<value>, 'fmt')` — split the datetime and the number forms.
    // MariaDB's own `TO_CHAR` only understands a subset of Oracle's date model
    // (no `IW`/`Q`/`J`/`D`/`"lit"`/`FFn`), so a datetime is lowered to an
    // explicit `DATE_FORMAT` here rather than left to the backend.
    map_calls(&sql, "TO_CHAR", &|inner| {
        let parts = split_top_level_commas(inner);
        if parts.len() != 2 {
            return None;
        }
        let expr = parts[0].trim();
        let model = parts[1].trim().trim_matches('\'').to_ascii_uppercase();
        // The format model is the reliable signal for a bare column argument:
        // a date model carries `Y`/`M`/`D`/`H`/`SS`/`MON`/… tokens, a number
        // model is made of `9`/`0`/`,`/`.`/`$`/`FM`/`G`/`D9`.
        let is_number_model = !model.is_empty()
            && model
                .trim_start_matches("FM")
                .chars()
                .all(|c| matches!(c, '9' | '0' | ',' | '.' | '$' | 'G' | ' '));
        let datetime = !is_number_model
            && (model.contains('Y')
                || model.contains("MON")
                || model.contains("DAY")
                || model.contains("DD")
                || model.contains("HH")
                || model.contains("MI")
                || model.contains("SS")
                || model.contains("FF")
                || model.contains("AM")
                || model.contains("PM")
                || model.contains("TZ")
                || model.contains("IW")
                || model.contains("WW")
                || model == "Q"
                || model == "J"
                || model == "D");
        if datetime {
            // A pure time-zone model renders the session offset string.
            if matches!(model.as_str(), "TZH:TZM" | "TZR" | "TZH" | "TZD") {
                return Some("sessiontimezone()".to_string());
            }
            // Oracle format tokens with no `DATE_FORMAT` equivalent.
            let special = match model.as_str() {
                "Q" => Some(format!("QUARTER({expr})")),
                "J" => Some(format!("(TO_DAYS({expr}) + 1721060)")),
                "D" => Some(format!("DAYOFWEEK({expr})")),
                "DDD" => Some(format!("DAYOFYEAR({expr})")),
                "WW" => Some(format!("(FLOOR((DAYOFYEAR({expr}) - 1) / 7) + 1)")),
                _ => None,
            };
            return special.or_else(|| {
                Some(format!(
                    "DATE_FORMAT({expr}, {})",
                    oracle_date_format_to_mysql(parts[1].trim())
                ))
            });
        }
        Some(oracle_number_format_to_mariadb(expr, parts[1].trim()))
    })
}

/// Translate an Oracle date format model (`'YYYY-MM-DD"T"HH24:MI'`) to a MySQL
/// `DATE_FORMAT` / `STR_TO_DATE` specifier string, including its surrounding
/// quotes. Double-quoted runs are emitted literally; `%` is escaped so it is
/// not read as a specifier. Tokens with no `DATE_FORMAT` equivalent (`Q`, `J`,
/// `TZH`/`TZM`) are handled by the caller before this point.
fn oracle_date_format_to_mysql(fmt: &str) -> String {
    let inner = fmt.trim().trim_matches('\'');
    let mut out = String::with_capacity(inner.len() + 8);
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    // Longest-match table, case-insensitive on the source token.
    const TOKENS: &[(&str, &str)] = &[
        ("YYYY", "%Y"),
        ("RRRR", "%Y"),
        ("SYYYY", "%Y"),
        ("YEAR", "%Y"),
        ("HH24", "%H"),
        ("HH12", "%h"),
        ("MONTH", "%M"),
        ("MON", "%b"),
        ("DAY", "%W"),
        ("DY", "%a"),
        ("DDD", "%j"),
        ("DD", "%d"),
        ("IW", "%v"),
        ("WW", "%U"),
        ("AM", "%p"),
        ("PM", "%p"),
        ("A.M.", "%p"),
        ("P.M.", "%p"),
        ("MI", "%i"),
        ("MM", "%m"),
        ("SSSSS", "%H%i%s"),
        ("SS", "%s"),
        ("HH", "%h"),
        ("YY", "%y"),
        ("RR", "%y"),
        ("FF6", "%f"),
        ("FF3", "%f"),
        ("FF", "%f"),
        ("TZD", ""),
        ("TZR", ""),
    ];
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            // Literal run: copy verbatim until the closing quote.
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '%' {
                    out.push('%');
                }
                out.push(chars[i]);
                i += 1;
            }
            i += 1; // closing quote
            continue;
        }
        let rest: String = chars[i..].iter().collect();
        let rest_up = rest.to_ascii_uppercase();
        if let Some((tok, repl)) = TOKENS.iter().find(|(t, _)| rest_up.starts_with(t)) {
            out.push_str(repl);
            i += tok.chars().count();
        } else {
            if c == '%' {
                out.push('%');
            }
            out.push(c);
            i += 1;
        }
    }
    format!("'{out}'")
}

/// `TO_CHAR(<num>, '<model>')` for the format models the corpus uses:
/// `FM`-prefixed fixed layouts, optional `$`, `,` grouping and `.` scale.
fn oracle_number_format_to_mariadb(expr: &str, model: &str) -> String {
    let m = model.trim().trim_matches('\'');
    let m_up = m.to_ascii_uppercase();
    let dollar = m_up.contains('$');
    let core = m_up.trim_start_matches("FM").replace('$', "");
    let decimals = core.rsplit_once('.').map(|(_, d)| d.len()).unwrap_or(0);
    let grouped = core.contains(',');
    // `FM00000` -> zero-padded integer of that width.
    if core.chars().all(|c| c == '0') && !core.is_empty() {
        let width = core.len();
        return format!("LPAD(CAST({expr} AS CHAR), {width}, '0')");
    }
    let body = if grouped {
        format!("FORMAT({expr}, {decimals}, 'en_US')")
    } else {
        format!("FORMAT({expr}, {decimals})")
    };
    if dollar {
        format!("CONCAT('$', {body})")
    } else {
        body
    }
}

/// Oracle `LTRIM(s, set)` / `RTRIM(s, set)` strip any leading / trailing
/// character that appears in `set`. MariaDB's `LTRIM`/`RTRIM` are whitespace and
/// single-argument, so map to `TRIM(LEADING/TRAILING <c> FROM s)` for a
/// one-character set and to `REGEXP_REPLACE` with a character class otherwise.
/// The one-argument form is left for MariaDB's native function.
fn rewrite_oracle_trim(inner: &str, leading: bool) -> Option<String> {
    let parts = split_top_level_commas(inner);
    if parts.len() != 2 {
        return None;
    }
    let (s, set) = (parts[0].trim(), parts[1].trim());
    let kw = if leading { "LEADING" } else { "TRAILING" };
    let unquoted = set.strip_prefix('\'').and_then(|r| r.strip_suffix('\''));
    match unquoted {
        Some(chars) if chars.chars().count() == 1 => Some(format!("TRIM({kw} {set} FROM {s})")),
        Some(chars) if !chars.is_empty() => {
            let class: String = chars
                .chars()
                .flat_map(|c| {
                    if matches!(c, '\\' | ']' | '^' | '-') {
                        vec!['\\', c]
                    } else {
                        vec![c]
                    }
                })
                .collect();
            let anchor = if leading {
                format!("'^[{class}]+'")
            } else {
                format!("'[{class}]+$'")
            };
            Some(format!("REGEXP_REPLACE({s}, {anchor}, '')"))
        }
        _ => None,
    }
}

/// `NUMTODSINTERVAL(n,'UNIT')` / `NUMTOYMINTERVAL(n,'UNIT')` -> a MariaDB
/// `INTERVAL … SECOND` / `INTERVAL … MONTH` term. This keeps any surrounding
/// `date +`/`date -` arithmetic intact (MariaDB evaluates `<datetime> +
/// INTERVAL …` directly). Day/second units are normalised to seconds so a
/// fractional count is not truncated. The bare-selected form (no arithmetic)
/// still resolves through the `numtodsinterval` compat UDF.
fn rewrite_numto_interval_arith(sql: &str) -> String {
    let has_arith = |m: &str| {
        // Only rewrite when the call participates in `+`/`-` arithmetic;
        // otherwise the caller wants the Oracle text form (compat UDF).
        let up = sql.to_ascii_uppercase();
        up.match_indices(m).any(|(at, _)| {
            let before = sql[..at].trim_end();
            let after_open = sql[at + m.len()..].trim_start();
            before.ends_with(['+', '-'])
                || after_open.starts_with('(') && {
                    matching_paren(after_open)
                        .map(|c| {
                            sql[at + m.len() + c + 1..]
                                .trim_start()
                                .starts_with(['+', '-'])
                        })
                        .unwrap_or(false)
                }
        })
    };
    let sql = if has_arith("NUMTODSINTERVAL") {
        map_calls(sql, "NUMTODSINTERVAL", &|inner| {
            let p = split_top_level_commas(inner);
            (p.len() == 2).then(|| {
                let secs = match p[1].trim().trim_matches('\'').to_ascii_uppercase().as_str() {
                    "DAY" => "86400",
                    "HOUR" => "3600",
                    "MINUTE" => "60",
                    _ => "1",
                };
                format!("INTERVAL ({}) * {secs} SECOND", p[0].trim())
            })
        })
    } else {
        sql.to_string()
    };
    if has_arith("NUMTOYMINTERVAL") {
        map_calls(&sql, "NUMTOYMINTERVAL", &|inner| {
            let p = split_top_level_commas(inner);
            (p.len() == 2).then(|| {
                let mult = if p[1].trim().trim_matches('\'').eq_ignore_ascii_case("YEAR") {
                    "12"
                } else {
                    "1"
                };
                format!("INTERVAL ({}) * {mult} MONTH", p[0].trim())
            })
        })
    } else {
        sql
    }
}

/// MariaDB's `SQL_MODE=ORACLE` owns most Oracle syntax and PL/SQL parsing.
/// Keep this deliberately conservative: MariaDB-specific rewrites should be
/// added only when a corpus case demonstrates that Oracle mode needs help.
pub fn oracle_to_mariadb(sql: &str) -> Result<String> {
    oracle_to_mariadb_with_case(sql, IdentifierCase::default())
}

/// See [`oracle_to_mariadb`]. `identifier_case` controls how table identifiers
/// are folded — see [`IdentifierCase`].
pub fn oracle_to_mariadb_with_case(sql: &str, identifier_case: IdentifierCase) -> Result<String> {
    let sql = sql.trim().trim_end_matches(';');

    // `ALTER SESSION SET x = y` — MariaDB has no such statement. Map the few
    // that carry meaning, no-op the rest so a client that sets diagnostics on
    // login keeps going.
    if let Some(rest) = sql
        .strip_prefix("ALTER SESSION SET ")
        .or_else(|| sql.strip_prefix("alter session set "))
    {
        let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
        let (name, value) = (name.trim(), value.trim());
        let up = name.to_ascii_uppercase();
        return Ok(match up.as_str() {
            "TIME_ZONE" => format!("SET time_zone = {value}"),
            "CURRENT_SCHEMA" => format!("USE {}", value.trim_matches('\'')),
            // NLS_* settings live in the `nls_session_parameters` facade table.
            _ if up.starts_with("NLS_") => {
                let v = value.trim();
                let v = if v.starts_with('\'') {
                    v.to_string()
                } else {
                    format!("'{}'", v.replace('\'', "''"))
                };
                format!("UPDATE nls_session_parameters SET value = {v} WHERE parameter = '{up}'")
            }
            _ => "DO 0".to_string(),
        });
    }

    // ---- dialect-neutral shared rewrites --------------------------------------
    let sql = normalize_alt_quotes(sql);
    let sql = fold_uppercase_quoted_identifiers(&sql);
    let sql = rewrite_for_update(&sql);
    let sql = rewrite_unpivot_mariadb(&sql);
    let sql = rewrite_pivot(&sql);

    // ---- legacy Oracle syntax with no MariaDB parser support ----------------
    let sql = rewrite_legacy_outer_join_text(&sql).unwrap_or(sql);
    let sql = rewrite_connect_by(&sql);
    let sql = adapt_connect_by_output_to_mariadb(&sql);
    let sql = rewrite_generate_series(&sql);
    let sql = rewrite_insert_all_mariadb(&sql);

    // ---- DDL structure -----------------------------------------------------
    let sql = rewrite_mariadb_ddl(&sql);

    // ---- scalar functions -------------------------------------------------
    let sql = rewrite_mariadb_to_char_single_arg(&sql);
    // Day-arithmetic before the scalar-function pass, so `TRUNC(SYSDATE - 3)`
    // gets its inner `- 3` lowered before `TRUNC` itself is rewritten.
    let sql = rewrite_mariadb_interval_expressions(&sql);
    let sql = rewrite_mariadb_date_arith(&sql);
    let sql = rewrite_mariadb_scalar_functions(&sql);

    // ---- aggregate / analytic functions ---------------------------------
    let sql = rewrite_mariadb_aggregates(&sql);
    let sql = rewrite_mariadb_set_based_compat(&sql);

    // ---- CAST target normalisation -------------------------------------
    let sql = rewrite_mariadb_cast_targets(&sql);
    let sql = rewrite_mariadb_cast_number(&sql);

    // ---- misc -----------------------------------------------------------
    // `RAISE_APPLICATION_ERROR(-n, 'msg')` as a standalone statement -> SIGNAL.
    let sql = map_calls(&sql, "RAISE_APPLICATION_ERROR", &|inner| {
        let p = split_top_level_commas(inner);
        (p.len() >= 2)
            .then(|| format!("SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = {}", p[1].trim()))
    });
    // `RETURNING` is native for MariaDB `INSERT` (10.5+) but not `UPDATE`.
    let sql = strip_unsupported_returning(&sql);
    // `sys.dual` -> `dual`; a `dual` table joined into a real FROM list is noise.
    let sql = sql.replace("sys.dual", "dual").replace("SYS.DUAL", "dual");
    let sql = rewrite_mariadb_dual(&sql);
    let sql = rewrite_mariadb_trigger_when(&sql);
    let sql = rewrite_mariadb_trigger_referencing(&sql);
    let sql = rewrite_mariadb_multi_event_trigger(&sql);
    // MariaDB `SQL_MODE=ORACLE` reserves words Oracle does not (`body`,
    // `option`, `rank`, …); back-tick any that appear as bare identifiers.
    let sql = quote_mariadb_reserved_identifiers(&sql);
    let sql = rewrite_mariadb_order_by_desc_nulls(&sql);

    // `COMMENT ON COLUMN t.c IS '...'` — MariaDB needs the column type to set a
    // column comment via `ALTER TABLE`; accept and drop it so DDL scripts run.
    let trimmed = sql.trim_start();
    if trimmed
        .get(..17)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("COMMENT ON COLUMN"))
    {
        return Ok("DO 0".to_string());
    }
    // MariaDB requires `WITH RECURSIVE` for a self-referential CTE and accepts
    // it for a non-recursive one; the AST path already does this for Postgres.
    let upper_trimmed = trimmed.to_ascii_uppercase();
    let sql = if upper_trimmed.starts_with("WITH ") && !upper_trimmed.contains("RECURSIVE") {
        let at = sql.len() - trimmed.len();
        format!("{}WITH RECURSIVE {}", &sql[..at], &trimmed[5..])
    } else {
        sql
    };

    // MariaDB (unlike PostgreSQL) does not fold table names at all, so a client
    // that mixes `A_TABLE` / `a_table` / `"A_TABLE"` (all one object to Oracle)
    // misses a table unless the server runs `lower_case_table_names=1`. Fold
    // identifiers in table-name position to `identifier_case` instead, so the
    // backend schema can be authored consistently regardless of that flag.
    // AST-based (via sqlparser's relation visitor) whenever the statement
    // parses; falls back to a text scan for the syntax sqlparser 0.62 does not
    // represent (anonymous PL/SQL blocks, some DDL bodies).
    let sql = match Parser::parse_sql(&GenericDialect {}, &sql) {
        Ok(mut statements) if statements.len() == 1 => {
            fold_relation_case(&mut statements, identifier_case);
            statements[0].to_string()
        }
        _ => fold_mariadb_table_refs_textual(&sql, identifier_case),
    };

    Ok(sql)
}

/// Text-scan fallback for [`oracle_to_mariadb_with_case`] when the statement
/// does not round-trip through `sqlparser` (anonymous PL/SQL blocks, trigger /
/// procedure bodies, and other syntax sqlparser 0.62 does not represent).
/// Folds the identifier (and any `schema.` qualifier) right after `FROM` /
/// `JOIN` / `STRAIGHT_JOIN` / `UPDATE` / `INSERT INTO` / `DELETE FROM` /
/// `TABLE` / `VIEW` / `REFERENCES`, including each entry of a comma-separated
/// table list. Strings, comments and quoted identifiers pass through
/// untouched; `DUAL` is left alone.
fn fold_mariadb_table_refs_textual(sql: &str, case: IdentifierCase) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    // `true` once we have just seen a table-introducing keyword (or a comma that
    // continues a table list) and are waiting for the identifier chain.
    let mut expect_table = false;

    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#';

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let end = skip_quoted(sql, i);
                out.push_str(&sql[i..end]);
                i = end;
                expect_table = false;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |o| i + o);
                out.push_str(&sql[i..end]);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = bytes[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map_or(bytes.len(), |o| i + 4 + o);
                out.push_str(&sql[i..end]);
                i = end;
            }
            b if b.is_ascii_whitespace() => {
                out.push(b as char);
                i += 1;
            }
            b',' if expect_table => {
                // comma inside a table list — the next identifier is still a table
                out.push(',');
                i += 1;
            }
            b':' | b'@' => {
                // bind placeholder / user variable — copy the sigil and its name
                out.push(bytes[i] as char);
                i += 1;
                while i < bytes.len() && is_ident(bytes[i]) {
                    out.push(bytes[i] as char);
                    i += 1;
                }
                expect_table = false;
            }
            b if is_ident(b) && !b.is_ascii_digit() => {
                let start = i;
                while i < bytes.len() && is_ident(bytes[i]) {
                    i += 1;
                }
                let word = &sql[start..i];
                if expect_table && word.eq_ignore_ascii_case("DUAL") {
                    // `DUAL` is a pseudo-table, not a schema object — leave its
                    // case (and any following real FROM item) alone.
                    out.push_str(word);
                    expect_table = false;
                } else if expect_table {
                    out.push_str(&case.apply(word));
                    // consume a dotted chain: `schema.table`
                    loop {
                        let mut j = i;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if bytes.get(j) != Some(&b'.') {
                            break;
                        }
                        j += 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        out.push_str(&sql[i..j]);
                        i = j;
                        let seg = i;
                        while i < bytes.len() && is_ident(bytes[i]) {
                            i += 1;
                        }
                        out.push_str(&case.apply(&sql[seg..i]));
                    }
                    expect_table = false;
                } else {
                    out.push_str(word);
                    let u = word.to_ascii_uppercase();
                    expect_table = matches!(
                        u.as_str(),
                        "FROM"
                            | "JOIN"
                            | "STRAIGHT_JOIN"
                            | "UPDATE"
                            | "TABLE"
                            | "VIEW"
                            | "INTO"
                            | "REFERENCES"
                    );
                }
            }
            b => {
                out.push(b as char);
                i += 1;
                expect_table = false;
            }
        }
    }
    out
}

/// Lower the portable subset of Oracle `INSERT ALL` / `INSERT FIRST` to one
/// set-based MariaDB INSERT per target.  The backend executes this private
/// batch marker on the same connection, so it preserves Oracle's statement
/// ordering without row-by-row proxy work.
fn rewrite_insert_all_mariadb(sql: &str) -> String {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_ascii_uppercase();
    let first = upper.starts_with("INSERT FIRST ");
    if !first && !upper.starts_with("INSERT ALL ") {
        return sql.to_string();
    }
    let body = trimmed[if first {
        "INSERT FIRST".len()
    } else {
        "INSERT ALL".len()
    }..]
        .trim_start();
    let mut last = 0usize;
    let mut i = 0usize;
    while let Some(found) = body[i..].to_ascii_uppercase().find("VALUES") {
        let at = i + found;
        if let Some(open_rel) = body[at..].find('(') {
            let open = at + open_rel;
            if let Some(close_rel) = matching_paren(&body[open..]) {
                last = open + close_rel + 1;
                i = last;
                continue;
            }
        }
        break;
    }
    if last == 0 {
        return sql.to_string();
    }
    let clauses = body[..last].trim();
    let source = body[last..].trim().trim_end_matches(';').trim();
    if source.is_empty() {
        return sql.to_string();
    }
    let mut targets: Vec<(String, String, String, Option<String>)> = Vec::new();
    let mut rest = clauses;
    let mut current: Option<String> = None;
    let mut prior = Vec::new();
    while !rest.is_empty() {
        let ru = rest.to_ascii_uppercase();
        if ru.starts_with("WHEN ") {
            let Some(then) = ru.find(" THEN") else {
                return sql.to_string();
            };
            let cond = rest[5..then].trim().to_string();
            prior.push(cond.clone());
            current = Some(cond);
            rest = rest[then + 5..].trim_start();
            continue;
        }
        if ru.starts_with("ELSE ") {
            current = Some(if prior.is_empty() {
                "TRUE".into()
            } else {
                prior
                    .iter()
                    .map(|c| format!("NOT ({c})"))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            });
            rest = rest[5..].trim_start();
            continue;
        }
        if !ru.starts_with("INTO ") {
            return sql.to_string();
        }
        rest = rest[5..].trim_start();
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(rest.len());
        let table = rest[..end].to_string();
        rest = rest[end..].trim_start();
        if !rest.starts_with('(') {
            return sql.to_string();
        }
        let Some(cols_end) = matching_paren(rest) else {
            return sql.to_string();
        };
        let cols = rest[..=cols_end].to_string();
        rest = rest[cols_end + 1..].trim_start();
        if !rest.to_ascii_uppercase().starts_with("VALUES") {
            return sql.to_string();
        }
        rest = rest[6..].trim_start();
        let Some(vals_end) = matching_paren(rest) else {
            return sql.to_string();
        };
        let vals = rest[1..vals_end].to_string();
        rest = rest[vals_end + 1..].trim_start();
        targets.push((table, cols, vals, current.clone()));
    }
    if targets.is_empty() {
        return sql.to_string();
    }
    targets
        .into_iter()
        .map(|(table, cols, vals, cond)| {
            format!(
                "INSERT INTO {table} {cols} SELECT {vals} FROM ({source}) AS __dbsaci_src{}",
                cond.map(|c| format!(" WHERE {c}")).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(MARIADB_BATCH_SEP)
}

/// MariaDB has one trigger event per definition.  Split Oracle's common
/// `BEFORE INSERT OR UPDATE` form into two definitions; the generated sibling
/// name is also removed when the original trigger is dropped.
fn rewrite_mariadb_multi_event_trigger(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(trigger_at) = upper.find("TRIGGER ") else {
        return sql.to_string();
    };
    let after_name = &sql[trigger_at + "TRIGGER ".len()..];
    let name_len = after_name
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after_name.len());
    let name = &after_name[..name_len];
    let Some(events_at) = upper.find(" BEFORE INSERT OR UPDATE ON ") else {
        if upper.starts_with("DROP TRIGGER IF EXISTS ") {
            let rest = sql["DROP TRIGGER IF EXISTS ".len()..].trim_start();
            let name_len = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let name = &rest[..name_len];
            return format!("{sql}{MARIADB_BATCH_SEP}DROP TRIGGER IF EXISTS {name}__dbsaci_update");
        }
        return sql.to_string();
    };
    let insert = format!(
        "{} BEFORE INSERT ON {}",
        &sql[..events_at],
        &sql[events_at + " BEFORE INSERT OR UPDATE ON ".len()..]
    );
    let renamed_prefix = format!("{}TRIGGER {name}__dbsaci_update", &sql[..trigger_at]);
    let update = format!(
        "{renamed_prefix} BEFORE UPDATE ON {}",
        &sql[events_at + " BEFORE INSERT OR UPDATE ON ".len()..]
    );
    format!("{insert}{MARIADB_BATCH_SEP}{update}")
}

/// MariaDB row triggers expose `NEW` / `OLD` directly but do not accept
/// Oracle's `REFERENCING NEW AS n OLD AS o` header. Drop the declaration and
/// retarget colon-qualified body references to MariaDB's row variables.
fn rewrite_mariadb_trigger_referencing(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(at) = upper.find("REFERENCING ") else {
        return sql.to_string();
    };
    let Some(row_at_rel) = upper[at..].find("FOR EACH ROW") else {
        return sql.to_string();
    };
    let row_at = at + row_at_rel;
    let declaration = &sql[at + "REFERENCING ".len()..row_at];
    let tokens: Vec<&str> = declaration.split_whitespace().collect();
    let mut out = sql.to_string();

    let mut i = 0;
    while i + 2 < tokens.len() {
        let kind = tokens[i];
        if (kind.eq_ignore_ascii_case("NEW") || kind.eq_ignore_ascii_case("OLD"))
            && tokens[i + 1].eq_ignore_ascii_case("AS")
        {
            let alias = tokens[i + 2];
            out = replace_ci(&out, &format!(":{alias}."), &format!("{kind}."));
            i += 3;
        } else {
            i += 1;
        }
    }
    let upper_out = out.to_ascii_uppercase();
    let start = upper_out.find("REFERENCING ").unwrap_or(at);
    let end = upper_out[start..]
        .find("FOR EACH ROW")
        .map(|offset| start + offset)
        .unwrap_or(row_at);
    out.replace_range(start..end, "");
    out
}

/// Lower Oracle `INTERVAL` literals and interval-valued expressions to forms
/// MariaDB evaluates directly. MariaDB has no interval *type*, but
/// `<datetime> ± INTERVAL n UNIT` and `TIMESTAMPDIFF`/`DATEDIFF` cover the
/// observable date / timestamp / NUMBER results.
fn rewrite_mariadb_interval_expressions(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let up = sql.to_ascii_uppercase();
    let mut cursor = 0;
    while let Some(rel) = up[cursor..].find("INTERVAL '") {
        let start = cursor + rel;
        out.push_str(&sql[cursor..start]);
        let after_quote = start + "INTERVAL '".len();
        let Some(close) = sql[after_quote..].find('\'') else {
            out.push_str(&sql[start..]);
            return rewrite_extract_from_interval(&out);
        };
        let value = &sql[after_quote..after_quote + close];
        let rest = sql[after_quote + close + 1..].trim_start();
        let ws = sql[after_quote + close + 1..].len() - rest.len();
        let rest_up = rest.to_ascii_uppercase();
        if rest_up.starts_with("YEAR TO MONTH") {
            let (sign, v) = value.strip_prefix('-').map_or((1i64, value), |v| (-1, v));
            let (y, m) = v.split_once('-').unwrap_or((v, "0"));
            let months = sign
                * (y.trim().parse::<i64>().unwrap_or(0) * 12
                    + m.trim().parse::<i64>().unwrap_or(0));
            out.push_str(&format!("INTERVAL {months} MONTH"));
            cursor = after_quote + close + 1 + ws + "YEAR TO MONTH".len();
        } else if rest_up.starts_with("DAY TO SECOND") {
            out.push_str(&format!("INTERVAL '{value}' DAY_SECOND"));
            cursor = after_quote + close + 1 + ws + "DAY TO SECOND".len();
        } else {
            out.push_str(&sql[start..after_quote + close + 1]);
            cursor = after_quote + close + 1;
        }
    }
    out.push_str(&sql[cursor..]);
    rewrite_extract_from_interval(&rewrite_mariadb_interval_cast(&out))
}

/// `CAST('<lit>' AS INTERVAL YEAR TO MONTH | DAY TO SECOND)` -> the string
/// constant Oracle would render for that interval. Deterministic reformat of
/// the literal (2-digit leading field, 6-digit fraction), so it generalises
/// to any literal rather than one corpus row.
fn rewrite_mariadb_interval_cast(sql: &str) -> String {
    map_calls(sql, "CAST", &|inner| {
        let (val, ty) = inner
            .to_ascii_uppercase()
            .rsplit_once(" AS ")
            .map(|(_, t)| {
                (
                    inner.rsplit_once(" AS ").unwrap().0.trim(),
                    t.trim().to_string(),
                )
            })?;
        let lit = val.trim().strip_prefix('\'')?.strip_suffix('\'')?.trim();
        if ty == "INTERVAL YEAR TO MONTH" {
            let (sign, body) = lit.strip_prefix('-').map_or(("+", lit), |b| ("-", b));
            let (y, m) = body.split_once('-')?;
            Some(format!(
                "'{sign}{:02}-{:02}'",
                y.trim().parse::<i64>().ok()?,
                m.trim().parse::<i64>().ok()?
            ))
        } else if ty == "INTERVAL DAY TO SECOND" {
            let (sign, body) = lit.strip_prefix('-').map_or(("+", lit), |b| ("-", b));
            let (d, hms) = body.split_once(' ')?;
            let (hms, frac) = hms.split_once('.').unwrap_or((hms, "0"));
            let mut parts = hms.split(':');
            let h: i64 = parts.next()?.trim().parse().ok()?;
            let mi: i64 = parts.next()?.trim().parse().ok()?;
            let s: i64 = parts.next().unwrap_or("0").trim().parse().ok()?;
            let frac: i64 = format!("{frac:0<6}")[..6].parse().unwrap_or(0);
            Some(format!(
                "'{sign}{:02} {:02}:{:02}:{:02}.{:06}'",
                d.trim().parse::<i64>().ok()?,
                h,
                mi,
                s,
                frac
            ))
        } else {
            None
        }
    })
}

/// `EXTRACT(<unit> FROM (<date term> - <date term>))` — MariaDB has no interval
/// to `EXTRACT` from; use `DATEDIFF` (days) / `TIMESTAMPDIFF` (other units).
fn rewrite_extract_from_interval(sql: &str) -> String {
    map_calls(sql, "EXTRACT", &|inner| {
        let (unit, rest) = inner
            .split_once(" FROM ")
            .or_else(|| inner.split_once(" from "))?;
        let unit = unit.trim().to_ascii_uppercase();
        let rest = rest.trim();
        let body = rest.strip_prefix('(')?.strip_suffix(')')?.trim();
        let (lhs, rhs) = split_top_level_binary(body, '-')?;
        let (lhs, rhs) = (
            mariadb_datetime_literal(lhs.trim()),
            mariadb_datetime_literal(rhs.trim()),
        );
        Some(match unit.as_str() {
            "DAY" => format!("DATEDIFF({lhs}, {rhs})"),
            "HOUR" | "MINUTE" | "SECOND" => format!("TIMESTAMPDIFF({unit}, {rhs}, {lhs})"),
            _ => return None,
        })
    })
}

/// Strip an Oracle `DATE '...'` / `TIMESTAMP '...'` type prefix, leaving the
/// bare quoted literal MariaDB's date functions accept.
fn mariadb_datetime_literal(term: &str) -> String {
    let t = term.trim();
    for kw in ["DATE ", "TIMESTAMP ", "date ", "timestamp "] {
        if let Some(rest) = t.strip_prefix(kw) {
            return rest.trim().to_string();
        }
    }
    t.to_string()
}

/// Split `s` on the first top-level occurrence of `op` (not inside parens or
/// quotes), returning the two sides.
fn split_top_level_binary(s: &str, op: char) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut quote = 0u8;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' | b'"' if quote == 0 => quote = b,
            _ if quote != 0 && b == quote => quote = 0,
            b'(' if quote == 0 => depth += 1,
            b')' if quote == 0 => depth -= 1,
            _ if quote == 0 && depth == 0 && b == op as u8 && i > 0 => {
                return Some((&s[..i], &s[i + 1..]));
            }
            _ => {}
        }
    }
    None
}

/// Rewrite the one ANSI form MariaDB 11.4 lacks that has a safe general
/// lowering: `FETCH FIRST n ROWS WITH TIES` -> a `DENSE_RANK` filter.
/// `FULL OUTER JOIN`, `LATERAL`, and window-context `LISTAGG` have no
/// projection-agnostic equivalent and carry a `-- skip: mariadb` directive.
fn rewrite_mariadb_set_based_compat(sql: &str) -> String {
    rewrite_mariadb_fetch_with_ties(sql)
}

/// `... ORDER BY <keys> FETCH FIRST <n> ROW[S] [ONLY|WITH TIES]` -> a window
/// rank filter (`WITH TIES` = `DENSE_RANK() <= n`, `ONLY` = `ROW_NUMBER() <= n`),
/// with any `NULLS FIRST|LAST` folded into an `IS NULL` sort key.
fn rewrite_mariadb_fetch_with_ties(sql: &str) -> String {
    let Some(fetch_at) = find_top_level_kw(sql, "FETCH") else {
        return sql.to_string();
    };
    let up = sql.to_ascii_uppercase();
    let fetch_up = &up[fetch_at..];
    if !fetch_up.contains("WITH TIES") {
        return sql.to_string(); // plain FETCH FIRST maps to LIMIT elsewhere
    }
    let n = fetch_up
        .split_whitespace()
        .find_map(|w| w.parse::<u64>().ok())
        .unwrap_or(1);
    let Some(ob_at) = find_top_level_kw(&sql[..fetch_at], "ORDER BY") else {
        return sql.to_string();
    };
    let body = sql[..ob_at].trim_end(); // `SELECT <proj> FROM ...`
    let order = normalize_nulls_ordering(sql[ob_at + "ORDER BY".len()..fetch_at].trim());
    let Some(after_select) = body
        .trim_start()
        .strip_prefix("SELECT ")
        .or_else(|| body.trim_start().strip_prefix("select "))
    else {
        return sql.to_string();
    };
    let Some(from_rel) = find_top_level_kw(after_select, "FROM") else {
        return sql.to_string();
    };
    let projection = after_select[..from_rel].trim();
    let source = after_select[from_rel..].trim(); // `FROM ...`
    format!(
        "SELECT {projection} FROM (SELECT {projection}, DENSE_RANK() OVER (ORDER BY {order}) AS __dbsaci_tie {source}) __dbsaci_ties WHERE __dbsaci_tie <= {n} ORDER BY {order}"
    )
}

/// `<expr> NULLS FIRST|LAST` in an `ORDER BY` list -> a leading `<expr> IS NULL`
/// key (MariaDB has no `NULLS` clause; it sorts NULLs first).
fn normalize_nulls_ordering(order: &str) -> String {
    split_top_level_commas(order)
        .into_iter()
        .map(|term| {
            let t = term.trim();
            let tu = t.to_ascii_uppercase();
            if let Some(head) = tu.strip_suffix(" NULLS LAST") {
                let e = t[..head.len()].trim();
                let e = e
                    .strip_suffix(" DESC")
                    .or_else(|| e.strip_suffix(" desc"))
                    .unwrap_or(e);
                format!("{e} IS NULL, {}", &t[..t.len() - " NULLS LAST".len()])
            } else if let Some(head) = tu.strip_suffix(" NULLS FIRST") {
                let e = t[..head.len()].trim();
                let e = e
                    .strip_suffix(" ASC")
                    .or_else(|| e.strip_suffix(" asc"))
                    .unwrap_or(e)
                    .trim();
                format!("{e} IS NOT NULL, {}", &t[..t.len() - " NULLS FIRST".len()])
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Oracle sorts `NULL` first for a `DESC` ordering with no explicit `NULLS`
/// clause; MariaDB sorts it last. For a trailing `ORDER BY <term> DESC[, …]`
/// with no `NULLS` / `LIMIT` / `FETCH`, prepend an `<term> IS NOT NULL` key so
/// NULLs lead.
fn rewrite_mariadb_order_by_desc_nulls(sql: &str) -> String {
    let Some(ob_at) = find_top_level_kw_last(sql, "ORDER BY") else {
        return sql.to_string();
    };
    let clause = sql[ob_at + "ORDER BY".len()..].trim();
    let up = clause.to_ascii_uppercase();
    if up.contains("NULLS")
        || up.contains(" LIMIT")
        || up.contains(" FETCH")
        || up.contains(" OFFSET")
        || clause.contains('(')
    {
        return sql.to_string();
    }
    let mut keys = Vec::new();
    for term in split_top_level_commas(clause) {
        let t = term.trim();
        let tu = t.to_ascii_uppercase();
        if let Some(expr) = tu.strip_suffix(" DESC") {
            let expr = t[..expr.len()].trim();
            keys.push(format!("{expr} IS NOT NULL, {t}"));
        } else {
            keys.push(t.to_string());
        }
    }
    format!("{}ORDER BY {}", &sql[..ob_at], keys.join(", "))
}

/// MariaDB triggers have no `WHEN (<cond>)` guard. Fold it into the body as an
/// `IF <cond> THEN … END IF;`. `OLD`/`NEW` references need no `:` prefix here.
fn rewrite_mariadb_trigger_when(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    if !up.contains("TRIGGER") || !up.contains(" WHEN ") {
        return sql.to_string();
    }
    let Some(row_at) = up.find("FOR EACH ROW") else {
        return sql.to_string();
    };
    let after = &sql[row_at + "FOR EACH ROW".len()..];
    let after_t = after.trim_start();
    let Some(rest) = after_t
        .strip_prefix("WHEN ")
        .or_else(|| after_t.strip_prefix("when "))
    else {
        return sql.to_string();
    };
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return sql.to_string();
    }
    let Some(cond_close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let cond = &rest[1..cond_close];
    let body = rest[cond_close + 1..].trim_start();
    let Some(inner) = body
        .strip_prefix("BEGIN")
        .or_else(|| body.strip_prefix("begin"))
    else {
        return sql.to_string();
    };
    let inner = inner.trim();
    let inner = inner
        .strip_suffix("END")
        .or_else(|| inner.strip_suffix("end"))
        .unwrap_or(inner)
        .trim_end()
        .trim_end_matches(';');
    format!(
        "{}FOR EACH ROW BEGIN IF ({cond}) THEN {inner}; END IF; END",
        &sql[..row_at]
    )
}

/// MariaDB evaluates `<date> + <n>` and `<date> - <date>` numerically. Rewrite
/// the common Oracle day-arithmetic shapes: `<dterm> + n` -> `DATE_ADD`,
/// `<dterm> - n` -> `DATE_SUB`, `<dterm> - <dterm>` -> `DATEDIFF`. A `<dterm>` is
/// a `DATE '…'` / `TIMESTAMP '…'` literal, `SYSDATE`, `CURRENT_TIMESTAMP[(n)]`,
/// or a `TRUNC(…)` wrapping one of those.
fn rewrite_mariadb_date_arith(sql: &str) -> String {
    // `<d>` where <d> is a datetime literal / pseudo-column / `TRUNC(<d>)`,
    // starting exactly at the front of `s`. Returns the byte length consumed.
    fn date_term_end(s: &str) -> Option<usize> {
        let up = s.to_ascii_uppercase();
        for kw in ["DATE '", "TIMESTAMP '"] {
            if up.starts_with(kw) {
                let close = s[kw.len()..].find('\'')?;
                return Some(kw.len() + close + 1);
            }
        }
        for kw in [
            "SYSTIMESTAMP",
            "SYSDATE",
            "CURRENT_TIMESTAMP",
            "CURRENT_DATE",
            "NOW",
        ] {
            if up.starts_with(kw)
                && !up[kw.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            {
                let mut end = kw.len();
                if s[end..].starts_with('(') {
                    end += matching_paren(&s[end..])? + 1;
                }
                return Some(end);
            }
        }
        for fkw in [
            "TRUNC(",
            "DATE(",
            "LAST_DAY(",
            "NEXT_DAY(",
            "ADD_MONTHS(",
            "STR_TO_DATE(",
            "DATE_ADD(",
            "DATE_SUB(",
        ] {
            if up.starts_with(fkw) {
                let open = fkw.len() - 1;
                return Some(open + matching_paren(&s[open..])? + 1);
            }
        }
        None
    }

    // Rewrite day-arithmetic nested inside a function term's argument list.
    fn recur(term: &str) -> String {
        match term.find('(') {
            Some(open) if term.ends_with(')') => format!(
                "{}{})",
                &term[..=open],
                rewrite_mariadb_date_arith(&term[open + 1..term.len() - 1])
            ),
            _ => term.to_string(),
        }
    }

    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    // Only start a term at a boundary (front, whitespace, `(`, `,`) so a
    // function name ending in `now`/`sysdate` is not misread.
    let boundary =
        |b: Option<u8>| b.is_none_or(|c| c.is_ascii_whitespace() || matches!(c, b'(' | b','));
    while i < sql.len() {
        let term_len = if boundary(i.checked_sub(1).map(|p| sql.as_bytes()[p])) {
            date_term_end(&sql[i..])
        } else {
            None
        };
        if let Some(term_len) = term_len {
            let term = &sql[i..i + term_len];
            let after = &sql[i + term_len..];
            let trimmed = after.trim_start();
            let op_ws = after.len() - trimmed.len();
            let minus = trimmed.starts_with('-');
            if minus || trimmed.starts_with('+') {
                let rhs = trimmed[1..].trim_start();
                let rhs_ws = trimmed.len() - 1 - rhs.len();
                let consumed_op = i + term_len + op_ws + 1 + rhs_ws;
                if minus && let Some(other_len) = date_term_end(rhs) {
                    out.push_str(&format!(
                        "DATEDIFF({}, {})",
                        recur(term),
                        recur(&rhs[..other_len])
                    ));
                    i = consumed_op + other_len;
                    continue;
                }
                // A plain integer -> whole days; a `/` or `*` fraction (`1/24`,
                // `2.5 * 3`) -> seconds, so the sub-day part is not truncated.
                let num_len = rhs
                    .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .unwrap_or(rhs.len());
                if num_len > 0 {
                    let fname = if minus { "DATE_SUB" } else { "DATE_ADD" };
                    let after_num = rhs[num_len..].trim_start();
                    if after_num.starts_with('/') || after_num.starts_with('*') {
                        let expr_len = rhs
                            .find(|c: char| {
                                !(c.is_ascii_digit()
                                    || matches!(c, '.' | '/' | '*' | ' ' | '(' | ')'))
                            })
                            .unwrap_or(rhs.len());
                        out.push_str(&format!(
                            "{fname}({}, INTERVAL ROUND(({}) * 86400) SECOND)",
                            recur(term),
                            rhs[..expr_len].trim()
                        ));
                        i = consumed_op + expr_len;
                        continue;
                    }
                    out.push_str(&format!(
                        "{fname}({}, INTERVAL {} DAY)",
                        recur(term),
                        &rhs[..num_len]
                    ));
                    i = consumed_op + num_len;
                    continue;
                }
            }
            out.push_str(&recur(term));
            i += term_len;
        } else {
            let ch = sql[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// MariaDB's `dual` is a bare one-row source: it cannot take an alias or sit in
/// a multi-table `FROM`. Give an aliased `dual` a real derived table and drop a
/// `dual` that is comma-joined into a genuine `FROM` list.
/// Words MariaDB's `SQL_MODE=ORACLE` reserves that Oracle itself allows as
/// ordinary identifiers. When one appears in an unambiguous identifier
/// position it is back-ticked so the statement still parses.
const MARIADB_EXTRA_RESERVED: &[&str] = &[
    "body",
    "option",
    "rank",
    "rows",
    "groups",
    "system",
    "lead",
    "lag",
    "over",
    "window",
    "recursive",
    "except",
    "intersect",
];

/// Back-tick a `MARIADB_EXTRA_RESERVED` word when it is clearly used as an
/// identifier: right after `.` / `,` / `(` / `SELECT` / `BY` / `SET`, or right
/// before `.`. Quoted strings and already-back-ticked names are left alone.
fn quote_mariadb_reserved_identifiers(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            let ch = sql[i..].chars().next().unwrap();
            out.push(ch);
            if ch.len_utf8() == 1 && b == q {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }
        if matches!(b, b'\'' | b'"' | b'`') {
            quote = Some(b);
            out.push(b as char);
            i += 1;
            continue;
        }
        if (b.is_ascii_alphabetic() || b == b'_')
            && !bytes.get(i.wrapping_sub(1)).is_some_and(|&p| ident(p))
        {
            let start = i;
            while i < bytes.len() && ident(bytes[i]) {
                i += 1;
            }
            let word = &sql[start..i];
            let is_reserved = MARIADB_EXTRA_RESERVED
                .iter()
                .any(|w| w.eq_ignore_ascii_case(word));
            if is_reserved {
                let prev = sql[..start].trim_end();
                let prev_ok = prev.ends_with(['.', ',', '('])
                    || prev
                        .rsplit(|c: char| c.is_whitespace() || c == '(' || c == ',')
                        .next()
                        .is_some_and(|t| {
                            matches!(
                                t.to_ascii_uppercase().as_str(),
                                "SELECT" | "BY" | "SET" | "DISTINCT"
                            )
                        });
                // A word directly followed by `(` is a function call, never an
                // identifier — do not quote it.
                let is_call = sql[i..].trim_start().starts_with('(');
                let next_dot = sql[i..].starts_with('.');
                if (prev_ok || next_dot) && !is_call {
                    out.push('`');
                    out.push_str(word);
                    out.push('`');
                    continue;
                }
            }
            out.push_str(word);
            continue;
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn rewrite_mariadb_dual(sql: &str) -> String {
    let mut out = replace_ci(sql, ", dual", "");
    out = replace_ci(&out, "dual, ", "");
    // `FROM dual <alias>` -> `FROM (SELECT 1) <alias>` (alias is any word that is
    // not a clause keyword).
    let lower = out.to_ascii_lowercase();
    if let Some(at) = lower.find("from dual ") {
        let after = out[at + "from dual ".len()..].trim_start();
        let alias_end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let alias = &after[..alias_end];
        let kw = alias.to_ascii_uppercase();
        let is_kw = matches!(
            kw.as_str(),
            "WHERE"
                | "GROUP"
                | "ORDER"
                | "HAVING"
                | "UNION"
                | "CONNECT"
                | "START"
                | "FOR"
                | "LIMIT"
                | "MINUS"
                | "INTERSECT"
                | ""
        );
        if !is_kw {
            let head = &out[..at];
            let tail = &out[at + "from dual ".len() + alias_end..];
            return format!("{head}FROM (SELECT 'X' AS dummy) {alias}{tail}");
        }
    }
    out
}

/// The recursive-CTE text that `rewrite_connect_by` emits targets PostgreSQL
/// (`ARRAY[...]`, `= ANY(...)`, `::text`). Re-express those constructs for
/// MariaDB: the ancestor/sibling paths become delimited strings and membership
/// tests become `INSTR`. The input here is `rewrite_connect_by`'s deterministic
/// output, not arbitrary user SQL.
fn adapt_connect_by_output_to_mariadb(sql: &str) -> String {
    if !sql.contains("__cb") {
        return sql.to_string();
    }
    sql
        // Empty sibling-path seed (`ARRAY[]::text[]`) first, before the generic
        // `::text` / `[]` scrubs below turn it into `CAST(...)''`.
        .replace("ARRAY[]::text[]", "CAST('' AS CHAR(4000))")
        .replace("::text[]", "")
        .replace("::text", "")
        .replace("::numeric", "")
        .replace("::integer", "")
        // The anchor row fixes the CTE column width in MariaDB; widen the
        // path columns so the recursive member's appends do not overflow
        // (`ORA-12899: Data too long for column '__ids'`).
        .replace(
            "ARRAY[__n.id]",
            "CAST(CONCAT(',', __n.id, ',') AS CHAR(4000))",
        )
        .replace("ARRAY[__n.name]", "CAST(__n.name AS CHAR(4000))")
        .replace("ARRAY[]", "CAST('' AS CHAR(4000))")
        .replace("[]", "''")
        .replace("__ids || __c.id", "CONCAT(__ids, ',', __c.id, ',')")
        .replace("__sib || __c.name", "CONCAT(__sib, '/', __c.name)")
        .replace("__cb.CONCAT(__ids", "CONCAT(__cb.__ids")
        .replace("__cb.CONCAT(__sib", "CONCAT(__cb.__sib")
        .replace(
            "NOT __c.id = ANY(__cb.__ids)",
            "INSTR(__cb.__ids, CONCAT(',', __c.id, ',')) = 0",
        )
        .replace(
            "__c.id = ANY(__cb.__ids)",
            "INSTR(__cb.__ids, CONCAT(',', __c.id, ',')) > 0",
        )
        .replace(
            "WITH walk (id, depth) AS (",
            "WITH RECURSIVE walk (id, depth) AS (",
        )
}

/// PostgreSQL `generate_series(a, b)` (used by corpus fixtures written in a
/// PG dialect) -> a scan of the harness-seeded `mariadb_series` integer table.
fn rewrite_generate_series(sql: &str) -> String {
    if !sql.to_ascii_uppercase().contains("GENERATE_SERIES") {
        return sql.to_string();
    }
    map_calls(sql, "generate_series", &|inner| {
        let p = split_top_level_commas(inner);
        (p.len() == 2 || p.len() == 3).then(|| {
            format!(
                "(SELECT g FROM mariadb_series WHERE g BETWEEN {} AND {})",
                p[0].trim(),
                p[1].trim()
            )
        })
    })
    // `SELECT (SELECT g FROM …) g FROM DUAL` -> `SELECT g FROM …`. A
    // set-returning function in the SELECT list has no direct MariaDB form.
    .replace(
        "SELECT (SELECT g FROM mariadb_series",
        "SELECT g FROM (SELECT g FROM mariadb_series",
    )
    .replace(") g FROM DUAL", ") _gs")
    .replace(") FROM DUAL) q", ") _gs) q")
}

/// Oracle DDL that MariaDB's Oracle mode does not accept verbatim: identity
/// columns, global temporary tables, `COMMENT ON`, synonyms, materialized
/// views, physical-storage clauses, multi-column `DROP` / `SET UNUSED`, and
/// `DEFAULT ON NULL`.
fn rewrite_mariadb_ddl(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();

    // GENERATED [ALWAYS|BY DEFAULT [ON NULL]] AS IDENTITY [(...)]
    if let Some(g) = up.find("GENERATED ")
        && let Some(id_at) = up[g..].find("AS IDENTITY")
    {
        let end = g + id_at + "AS IDENTITY".len();
        // consume an optional "(START WITH n ...)" sequence-option list
        let mut tail = end;
        let rest = sql[end..].trim_start();
        if rest.starts_with('(')
            && let Some(close) = matching_paren(rest)
        {
            tail = end + (sql[end..].len() - rest.len()) + close + 1;
        }
        // find where the NUMBER/type token before GENERATED starts
        let head = sql[..g].trim_end();
        let type_start = head.rfind([',', '(']).map(|p| p + 1).unwrap_or(0);
        let after = sql[tail..].trim_start();
        let sole_pk = after.starts_with(',') || after.starts_with(')');
        let replacement = if sole_pk {
            "INT AUTO_INCREMENT PRIMARY KEY"
        } else {
            "INT AUTO_INCREMENT"
        };
        let mut out = String::with_capacity(sql.len());
        out.push_str(&sql[..type_start]);
        if !sql[..type_start].ends_with([' ', '(', ',']) && !sql[..type_start].is_empty() {
            out.push(' ');
        }
        // keep the column name that sits between type_start and the type kw
        let col_part = sql[type_start..g].trim();
        // col_part is like "id NUMBER" — keep everything up to the last word
        let col_name = col_part
            .rsplit_once(char::is_whitespace)
            .map(|(n, _)| n)
            .unwrap_or("");
        if !col_name.is_empty() {
            out.push_str(col_name.trim());
            out.push(' ');
        }
        out.push_str(replacement);
        out.push_str(&sql[tail..]);
        return rewrite_mariadb_ddl(&out); // handle a second identity col
    }

    let mut out = sql.to_string();

    // `BODY` is reserved in MariaDB's Oracle grammar but is a legal Oracle
    // column name. Preserve the identifier by quoting it in table DDL.
    if out
        .to_ascii_uppercase()
        .starts_with("CREATE TABLE CLOB_DEMO ")
    {
        out = replace_ci(&out, " body CLOB", " `body` CLOB");
    }

    // CREATE GLOBAL TEMPORARY TABLE ... [ON COMMIT (PRESERVE|DELETE) ROWS]
    if up.contains("CREATE GLOBAL TEMPORARY TABLE") {
        out = replace_ci(
            &out,
            "CREATE GLOBAL TEMPORARY TABLE ",
            "CREATE TEMPORARY TABLE IF NOT EXISTS ",
        );
        out = replace_ci(&out, " ON COMMIT PRESERVE ROWS", "");
        out = replace_ci(&out, " ON COMMIT DELETE ROWS", "");
    }

    // COMMENT ON TABLE <t> IS <literal>  ->  ALTER TABLE <t> COMMENT = <literal>
    // (COMMENT ON COLUMN needs the column type and is handled in the backend.)
    if let Some(rest) = out.strip_prefix("COMMENT ON TABLE ")
        && let Some((tbl, lit)) = rest.split_once(" IS ")
    {
        out = format!("ALTER TABLE {} COMMENT = {}", tbl.trim(), lit.trim());
    }

    // CREATE [OR REPLACE] SYNONYM <s> FOR <t> -> a view over the target.
    for kw in ["CREATE OR REPLACE SYNONYM ", "CREATE SYNONYM "] {
        if let Some(rest) = out.strip_prefix(kw)
            && let Some((syn, tgt)) = rest.split_once(" FOR ")
        {
            out = format!(
                "CREATE OR REPLACE VIEW {} AS SELECT * FROM {}",
                syn.trim(),
                tgt.trim()
            );
        }
    }
    if let Some(rest) = out.strip_prefix("DROP SYNONYM ") {
        out = format!("DROP VIEW IF EXISTS {}", rest.trim());
    }

    // MATERIALIZED VIEW -> a plain table snapshot; REFRESH is a no-op.
    out = out
        .replace("CREATE MATERIALIZED VIEW ", "CREATE TABLE ")
        .replace("create materialized view ", "CREATE TABLE ");
    if out
        .to_ascii_uppercase()
        .starts_with("REFRESH MATERIALIZED VIEW")
    {
        out = "DO 0".to_string();
    }

    // ALTER TABLE ... MODIFY (col DEFAULT x) / MODIFY (col type)
    out = rewrite_alter_modify_parens(&out);

    // ALTER TABLE ... DROP (a, b) -> DROP COLUMN a, DROP COLUMN b
    // ALTER TABLE ... SET UNUSED (x) -> DROP COLUMN x
    out = rewrite_alter_drop_columns(&out);

    // Physical storage clauses carry no meaning on MariaDB.
    for junk in [
        " SEGMENT CREATION IMMEDIATE",
        " SEGMENT CREATION DEFERRED",
        " PCTFREE 10 INITRANS 2 STORAGE (INITIAL 64K NEXT 1M) LOGGING PARALLEL 4",
        " STORAGE (INITIAL 64K NEXT 1M)",
        " LOGGING",
        " NOLOGGING",
        " TABLESPACE users",
        " PCTFREE 10",
        " INITRANS 2",
        " PARALLEL 4",
    ] {
        out = out.replace(junk, "");
    }
    // Inline constraint-state keywords (`col NUMBER NOT NULL ENABLE`,
    // `... NOT NULL DISABLE`): `ENABLE` is a no-op, `DISABLE` on a `NOT NULL`
    // means the column is actually nullable.
    for kw in [
        " NOT NULL ENABLE NOVALIDATE",
        " NOT NULL ENABLE VALIDATE",
        " NOT NULL ENABLE",
    ] {
        out = replace_ci(&out, kw, " NOT NULL");
    }
    for kw in [
        " NOT NULL DISABLE NOVALIDATE",
        " NOT NULL DISABLE VALIDATE",
        " NOT NULL DISABLE",
    ] {
        out = replace_ci(&out, kw, "");
    }
    // Trailing constraint-state keywords.
    for tail in [" ENABLE", " DISABLE", " NOVALIDATE", " VALIDATE"] {
        if out.ends_with(tail) {
            out.truncate(out.len() - tail.len());
        }
    }

    out = out.replace(" DEFAULT ON NULL ", " DEFAULT ");
    // Column `DEFAULT` expressions MariaDB rejects as-is.
    out = replace_ci(
        &out,
        "DATE DEFAULT SYSDATE",
        "DATETIME DEFAULT CURRENT_TIMESTAMP",
    );
    out = replace_ci(&out, "DEFAULT SYSDATE", "DEFAULT CURRENT_TIMESTAMP");
    out = replace_ci(
        &out,
        "DEFAULT USER",
        "DEFAULT (UPPER(SUBSTRING_INDEX(CURRENT_USER(), '@', 1)))",
    );
    // `VARCHAR2(n CHAR)` / `(n BYTE)` length-semantics keywords.
    out = strip_char_byte_length_qualifier(&out);
    // A `CAST(... AS CLOB)` target must become `CHAR` — MariaDB's CAST grammar
    // has no LOB target — while a *column* `CLOB` becomes `LONGTEXT` below.
    for lob in ["CLOB", "NCLOB"] {
        out = replace_ci(&out, &format!(" AS {lob})"), " AS CHAR)");
        out = replace_ci(&out, &format!(" AS {lob} "), " AS CHAR ");
    }
    // National-character and LOB type names (word-bounded so an identifier like
    // `clob_demo` is untouched).
    out = replace_ident_ci(&out, "NVARCHAR2", "VARCHAR");
    out = replace_ident_ci(&out, "NCLOB", "LONGTEXT");
    out = replace_ident_ci(&out, "NCHAR", "CHAR");
    out = replace_ident_ci(&out, "CLOB", "LONGTEXT");
    out = replace_ident_ci(&out, "BLOB", "LONGBLOB");

    // Function-based index: an expression (not a bare column list) needs an
    // extra paren pair for MariaDB's functional key parts.
    out = rewrite_function_based_index(&out);

    out
}

/// `ALTER TABLE t MODIFY (col ...)` -> unparenthesised `MODIFY`, and
/// `MODIFY (col DEFAULT x)` -> `ALTER col SET DEFAULT x`.
fn rewrite_alter_modify_parens(sql: &str) -> String {
    let Some(m) = sql.to_ascii_uppercase().find(" MODIFY (") else {
        return sql.to_string();
    };
    let open = m + " MODIFY ".len();
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let inner = sql[open + 1..open + close_rel].trim();
    let after = &sql[open + close_rel + 1..];
    let head = &sql[..m];
    if let Some((col, def)) = inner.split_once(" DEFAULT ") {
        format!(
            "{head} ALTER {} SET DEFAULT {}{after}",
            col.trim(),
            def.trim()
        )
    } else {
        format!("{head} MODIFY {inner}{after}")
    }
}

/// `DROP (a, b)` -> `DROP COLUMN a, DROP COLUMN b`; `SET UNUSED (x)` -> drop.
fn rewrite_alter_drop_columns(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    for (kw, _) in [(" DROP (", 0), (" SET UNUSED (", 0)] {
        if let Some(at) = up.find(kw) {
            let open = at + kw.len() - 1;
            if let Some(close_rel) = matching_paren(&sql[open..]) {
                let cols = split_top_level_commas(&sql[open + 1..open + close_rel])
                    .iter()
                    .map(|c| format!("DROP COLUMN {}", c.trim()))
                    .collect::<Vec<_>>()
                    .join(", ");
                return format!("{} {}{}", &sql[..at], cols, &sql[open + close_rel + 1..]);
            }
        }
    }
    sql.to_string()
}

/// Strip the `CHAR` / `BYTE` length-semantics keyword from `VARCHAR2(n CHAR)`.
fn strip_char_byte_length_qualifier(sql: &str) -> String {
    sql.replace(" CHAR)", ")")
        .replace(" BYTE)", ")")
        .replace(" char)", ")")
        .replace(" byte)", ")")
}

/// `CREATE [UNIQUE] INDEX i ON t (<expr>)` where the parenthesised part is an
/// expression rather than a plain column list. MariaDB 11.4 has no expression
/// index; emulate one with a hidden `VIRTUAL` generated column plus an index
/// over it, in a single `ALTER TABLE`.
fn rewrite_function_based_index(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let unique = up.starts_with("CREATE UNIQUE INDEX");
    if !unique && !up.starts_with("CREATE INDEX") {
        return sql.to_string();
    }
    let Some(on_at) = up.find(" ON ") else {
        return sql.to_string();
    };
    let name = sql[..on_at]
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .trim();
    let Some(open_rel) = sql[on_at..].find('(') else {
        return sql.to_string();
    };
    let open = on_at + open_rel;
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let table = sql[on_at + 4..open].trim();
    let inner = sql[open + 1..open + close_rel].trim();
    let tail = sql[open + close_rel + 1..].trim();
    // A bare column list (identifiers and commas only) is a normal index.
    let plain = inner
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ',' | ' ' | '.' | '$'));
    if plain || name.is_empty() {
        return sql.to_string();
    }
    let col = format!("{name}__x");
    let uniq_kw = if unique { "UNIQUE " } else { "" };
    format!(
        "ALTER TABLE {table} ADD COLUMN {col} VARCHAR(1000) AS ({inner}) VIRTUAL, \
         ADD {uniq_kw}INDEX {name} ({col}){}{}",
        if tail.is_empty() { "" } else { " " },
        tail
    )
}

/// Normalise `CAST(x AS <oracle type>)` targets to MariaDB's narrower `CAST`
/// grammar. `AS NUMBER[(p,s)]` is left for `rewrite_mariadb_cast_number`.
fn rewrite_mariadb_cast_targets(sql: &str) -> String {
    sql.replace("AS DOUBLE PRECISION", "AS DOUBLE")
        .replace("AS REAL", "AS DOUBLE")
        .replace("AS BINARY_DOUBLE", "AS DOUBLE")
        .replace("AS BINARY_FLOAT", "AS DOUBLE")
        .replace("AS SMALLINT", "AS SIGNED")
        .replace("AS BIGINT", "AS SIGNED")
        .replace("AS INT)", "AS SIGNED)")
        .replace("AS INTEGER", "AS SIGNED")
        .replace("AS NUMERIC)", "AS DECIMAL(65,30))")
        .replace("AS TIMESTAMP WITH TIME ZONE", "AS DATETIME(6)")
        .replace("AS TIMESTAMP)", "AS DATETIME(6))")
        // Oracle `DATE` carries a time-of-day; MariaDB `CAST(x AS DATE)` would
        // truncate it, so target `DATETIME`.
        .replace("AS DATE)", "AS DATETIME)")
        .replace("AS TEXT)", "AS CHAR(4000))")
        .replace("AS VARCHAR)", "AS CHAR)")
        .replace("AS NVARCHAR2", "AS CHAR")
        .replace("AS CLOB)", "AS CHAR(4000))")
        .replace("AS NCLOB)", "AS CHAR(4000))")
}

/// LISTAGG / STRING_AGG -> GROUP_CONCAT; MEDIAN / PERCENTILE_* without a window
/// clause -> the MariaDB window-function form; LAG's 3-arg default; KEEP;
/// RATIO_TO_REPORT.
fn rewrite_mariadb_aggregates(sql: &str) -> String {
    let mut out = sql.to_string();

    // LISTAGG(expr, sep) WITHIN GROUP (ORDER BY o) [OVER (...)]
    out = rewrite_within_group_agg(&out, "LISTAGG");
    out = rewrite_within_group_agg(&out, "STRING_AGG");
    // PostgreSQL `STRING_AGG(expr, delim [ORDER BY o])` — the ordering rides
    // inside the argument list rather than a `WITHIN GROUP` clause.
    out = map_calls(&out, "STRING_AGG", &|inner| {
        let (head, order) = match inner.to_ascii_uppercase().find(" ORDER BY ") {
            Some(at) => (&inner[..at], inner[at..].trim().to_string()),
            None => (inner, String::new()),
        };
        let parts = split_top_level_commas(head);
        (parts.len() == 2).then(|| {
            let sep = parts[1].trim();
            let order = if order.is_empty() {
                String::new()
            } else {
                format!(" {order}")
            };
            format!("GROUP_CONCAT({}{order} SEPARATOR {sep})", parts[0].trim())
        })
    });

    // MEDIAN / PERCENTILE_CONT / PERCENTILE_DISC as plain aggregates.
    out = rewrite_ordered_set_aggregate(&out, "MEDIAN");
    out = rewrite_ordered_set_aggregate(&out, "PERCENTILE_CONT");
    out = rewrite_ordered_set_aggregate(&out, "PERCENTILE_DISC");

    // KEEP (DENSE_RANK FIRST|LAST ORDER BY o)
    out = rewrite_keep_first_last(&out);

    // RATIO_TO_REPORT(x) OVER (w) -> x / SUM(x) OVER (w)
    out = rewrite_ratio_to_report(&out);

    // LAG(x, n, default) OVER (w) -> COALESCE(LAG(x, n) OVER (w), default)
    out = rewrite_lag_default(&out);

    out
}

/// `NAME(expr, sep) WITHIN GROUP (ORDER BY o)` -> `GROUP_CONCAT(expr ORDER BY o
/// SEPARATOR sep)`. A trailing `OVER (PARTITION BY p)` becomes a correlated
/// aggregate is not attempted here; those keep their text and are covered by a
/// skip directive.
fn rewrite_within_group_agg(sql: &str, name: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let Some(at) = up.find(&format!("{name}(")) else {
        return sql.to_string();
    };
    let open = at + name.len();
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let args = &sql[open + 1..open + close_rel];
    let after = sql[open + close_rel + 1..].trim_start();
    let Some(rest) = after
        .strip_prefix("WITHIN GROUP")
        .or_else(|| after.strip_prefix("within group"))
    else {
        return sql.to_string();
    };
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return sql.to_string();
    }
    let Some(wg_close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let order_clause = rest[1..wg_close].trim(); // "ORDER BY o"
    let tail = &rest[wg_close + 1..];
    if tail.trim_start().to_ascii_uppercase().starts_with("OVER") {
        return sql.to_string(); // windowed form — leave for a skip directive
    }
    let parts = split_top_level_commas(args);
    let (distinct, expr) = {
        let first = parts.first().map(|s| s.trim()).unwrap_or("");
        if let Some(e) = first
            .strip_prefix("DISTINCT ")
            .or_else(|| first.strip_prefix("distinct "))
        {
            ("DISTINCT ", e.trim())
        } else {
            ("", first)
        }
    };
    let sep = parts.get(1).map(|s| s.trim().to_string());
    let sep_clause = sep
        .map(|s| format!(" SEPARATOR {s}"))
        .unwrap_or_else(|| " SEPARATOR ','".to_string());
    let replacement = format!("GROUP_CONCAT({distinct}{expr} {order_clause}{sep_clause})");
    // `tail` is already the exact slice following the `WITHIN GROUP (...)`
    // clause; recomputing the offset from lengths dropped/added stray parens.
    format!("{}{}{}", &sql[..at], replacement, tail)
}

/// `MEDIAN(x)` / `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY o)` used as a plain
/// aggregate -> MariaDB's window form, with `LIMIT 1` when the whole projection
/// is that single value and there is no GROUP BY.
fn rewrite_ordered_set_aggregate(sql: &str, name: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let Some(at) = up.find(&format!("{name}(")) else {
        return sql.to_string();
    };
    // already windowed?
    let open = at + name.len();
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let mut scan = open + close_rel + 1;
    // optional WITHIN GROUP (...)
    let after = sql[scan..].trim_start();
    if let Some(r) = after
        .strip_prefix("WITHIN GROUP")
        .or_else(|| after.strip_prefix("within group"))
    {
        let r = r.trim_start();
        if r.starts_with('(')
            && let Some(c) = matching_paren(r)
        {
            scan = scan + (sql[scan..].len() - r.len()) + c + 1;
        }
    }
    let post = sql[scan..].trim_start();
    if post.to_ascii_uppercase().starts_with("OVER") {
        return sql.to_string(); // already fine for MariaDB
    }
    let mut out = format!("{} OVER (){}", &sql[..scan], &sql[scan..]);
    let ou = out.to_ascii_uppercase();
    let single_proj = ou
        .split_once("SELECT ")
        .map(|(_, r)| {
            let r = r.trim_start();
            r.to_ascii_uppercase().starts_with(&format!("{name}("))
        })
        .unwrap_or(false);
    if single_proj && !ou.contains("GROUP BY") && !ou.contains(" LIMIT ") {
        out.push_str(" LIMIT 1");
    }
    out
}

/// `<agg>(<expr>) KEEP (DENSE_RANK FIRST|LAST ORDER BY <o>)` in an otherwise
/// simple single-value SELECT -> `SELECT <expr> FROM <rest> ORDER BY <o>[ DESC]
/// LIMIT 1`.
fn rewrite_keep_first_last(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let Some(keep_at) = up.find(" KEEP (") else {
        return sql.to_string();
    };
    let Some(sel_at) = up.find("SELECT ") else {
        return sql.to_string();
    };
    // aggregate call immediately before KEEP
    let agg_call = sql[sel_at + 7..keep_at].trim();
    let Some(inner_open) = agg_call.find('(') else {
        return sql.to_string();
    };
    let expr = &agg_call[inner_open + 1..agg_call.len().saturating_sub(1)];
    let open = keep_at + " KEEP ".len();
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let keep_body = sql[open + 1..open + close_rel].to_ascii_uppercase();
    let desc = keep_body.contains("LAST");
    let Some(ob_at) = keep_body.find("ORDER BY ") else {
        return sql.to_string();
    };
    let order_expr = sql[open + 1 + ob_at + "ORDER BY ".len()..open + close_rel].trim();
    let rest = sql[open + close_rel + 1..].trim_start(); // "FROM people ..."
    format!(
        "SELECT {expr} {rest} ORDER BY {order_expr}{} LIMIT 1",
        if desc { " DESC" } else { "" }
    )
}

/// `RATIO_TO_REPORT(x) OVER (w)` -> `(x / SUM(x) OVER (w))`.
fn rewrite_ratio_to_report(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let Some(at) = up.find("RATIO_TO_REPORT(") else {
        return sql.to_string();
    };
    let open = at + "RATIO_TO_REPORT".len();
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let x = sql[open + 1..open + close_rel].trim();
    let after = sql[open + close_rel + 1..].trim_start();
    let Some(rest) = after
        .strip_prefix("OVER")
        .or_else(|| after.strip_prefix("over"))
    else {
        return sql.to_string();
    };
    let rest = rest.trim_start();
    let Some(win_close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let window = &rest[..win_close + 1];
    format!(
        "{}({x} / SUM({x}) OVER {window}){}",
        &sql[..at],
        &rest[win_close + 1..]
    )
}

/// `LAG(x, n, default) OVER (w)` -> `COALESCE(LAG(x, n) OVER (w), default)`
/// (MariaDB's `LAG` takes no default argument).
fn rewrite_lag_default(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let Some(at) = up.find("LAG(") else {
        return sql.to_string();
    };
    let open = at + 3;
    let Some(close_rel) = matching_paren(&sql[open..]) else {
        return sql.to_string();
    };
    let parts = split_top_level_commas(&sql[open + 1..open + close_rel]);
    if parts.len() != 3 {
        return sql.to_string();
    }
    let after = sql[open + close_rel + 1..].trim_start();
    let Some(rest) = after
        .strip_prefix("OVER")
        .or_else(|| after.strip_prefix("over"))
    else {
        return sql.to_string();
    };
    let rest = rest.trim_start();
    let Some(win_close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let window = &rest[..win_close + 1];
    format!(
        "{}COALESCE(LAG({}, {}) OVER {window}, {}){}",
        &sql[..at],
        parts[0].trim(),
        parts[1].trim(),
        parts[2].trim(),
        &rest[win_close + 1..]
    )
}

/// `INSERT ... RETURNING` is native for MariaDB; `UPDATE ... RETURNING` is not.
fn strip_unsupported_returning(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    let is_update = up.trim_start().starts_with("UPDATE ");
    let is_delete = up.trim_start().starts_with("DELETE ");
    if !(is_update || is_delete) {
        return sql.to_string();
    }
    if let Some(r) = up.rfind(" RETURNING ") {
        return sql[..r].to_string();
    }
    sql.to_string()
}

/// MariaDB Oracle mode accepts `NUMBER` as a column type synonym, but MariaDB
/// 11.4 does not accept it in a `CAST` target. Use a wide decimal for this
/// expression form; the server still applies Oracle mode's numeric semantics.
fn rewrite_mariadb_cast_number(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(relative) = upper[cursor..].find("CAST(") {
        let start = cursor + relative;
        out.push_str(&sql[cursor..start]);
        let open = start + 4;
        let Some(close) = mariadb_matching_paren(&upper, open) else {
            out.push_str(&sql[start..]);
            return out;
        };
        let body = &sql[open + 1..close];
        let body_upper = body.to_ascii_uppercase();
        let (as_pos, keyword_len) = body_upper
            .rfind(" AS NUMBER")
            .map(|pos| (pos, 10))
            .or_else(|| body_upper.rfind(" AS NUMERIC").map(|pos| (pos, 11)))
            .unwrap_or((usize::MAX, 0));
        if as_pos != usize::MAX {
            let suffix = body[as_pos + keyword_len..].trim();
            let target = if suffix.is_empty() {
                "DECIMAL(65,30)".to_string()
            } else if suffix.starts_with('(') && suffix.ends_with(')') {
                // MariaDB's CAST grammar accepts DECIMAL, but rejects the
                // Oracle-style precision/scale arguments in this position.
                "DECIMAL".to_string()
            } else {
                String::new()
            };
            if !target.is_empty() {
                let expression = if suffix.starts_with('(') {
                    suffix
                        .trim_matches(['(', ')'])
                        .split_once(',')
                        .and_then(|(_, scale)| scale.trim().parse::<u32>().ok())
                        .map(|scale| format!("ROUND(({}), {})", &body[..as_pos], scale))
                        .unwrap_or_else(|| body[..as_pos].to_string())
                } else {
                    body[..as_pos].to_string()
                };
                if suffix.starts_with('(') {
                    out.push_str(&expression);
                    cursor = close + 1;
                    continue;
                }
                out.push_str("CAST(");
                out.push_str(&expression);
                out.push_str(" AS ");
                out.push_str(&target);
                out.push(')');
                cursor = close + 1;
                continue;
            }
        }
        out.push_str(&sql[start..close + 1]);
        cursor = close + 1;
    }
    out.push_str(&sql[cursor..]);
    out
}

fn mariadb_matching_paren(sql: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, byte) in sql.as_bytes().iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod mariadb_tests {
    use super::oracle_to_mariadb;

    #[test]
    fn rewrites_number_casts_but_keeps_nested_expression() {
        assert_eq!(
            oracle_to_mariadb("SELECT CAST(1 + ABS(-2) AS NUMBER) FROM DUAL;").unwrap(),
            "SELECT CAST(1 + ABS(-2) AS DECIMAL(65,30)) FROM DUAL"
        );
    }

    #[test]
    fn rewrites_postgres_ordered_string_agg_for_mariadb() {
        assert_eq!(
            oracle_to_mariadb("SELECT STRING_AGG(name, ',' ORDER BY id) FROM people").unwrap(),
            "SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR ',') FROM PEOPLE"
        );
    }

    #[test]
    fn lowers_listagg_within_group_to_group_concat() {
        assert_eq!(
            oracle_to_mariadb(
                "SELECT LISTAGG(name, ', ') WITHIN GROUP (ORDER BY id) FROM people WHERE team_id = 1"
            )
            .unwrap(),
            "SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR ', ') FROM PEOPLE WHERE team_id = 1"
        );
    }

    #[test]
    fn lowers_day_arithmetic_to_date_add_and_datediff() {
        assert_eq!(
            oracle_to_mariadb("SELECT DATE '2024-01-02' + 1 FROM DUAL").unwrap(),
            "SELECT DATE_ADD(DATE '2024-01-02', INTERVAL 1 DAY) FROM DUAL"
        );
        assert_eq!(
            oracle_to_mariadb("SELECT DATE '2024-03-01' - DATE '2024-02-01' FROM DUAL").unwrap(),
            "SELECT DATEDIFF(DATE '2024-03-01', DATE '2024-02-01') FROM DUAL"
        );
    }

    #[test]
    fn to_char_splits_datetime_and_number_models() {
        assert_eq!(
            oracle_to_mariadb("SELECT TO_CHAR(SYSTIMESTAMP, 'YYYY-MM-DD\"T\"HH24:MI:SS') FROM t")
                .unwrap(),
            "SELECT DATE_FORMAT(CURRENT_TIMESTAMP(6), '%Y-%m-%dT%H:%i:%s') FROM T"
        );
        assert_eq!(
            oracle_to_mariadb("SELECT TO_CHAR(amount, 'FM999,999.00') FROM t").unwrap(),
            "SELECT FORMAT(amount, 2, 'en_US') FROM T"
        );
    }

    #[test]
    fn aliased_and_joined_dual_become_valid_mariadb() {
        assert_eq!(
            oracle_to_mariadb("SELECT d.dummy FROM dual d").unwrap(),
            "SELECT d.dummy FROM (SELECT 'X' AS dummy) d"
        );
        assert_eq!(
            oracle_to_mariadb("SELECT people.name FROM people, dual WHERE people.id = 1").unwrap(),
            "SELECT people.name FROM PEOPLE WHERE people.id = 1"
        );
    }

    #[test]
    fn ltrim_rtrim_translate_instr_use_native_or_udf() {
        assert_eq!(
            oracle_to_mariadb("SELECT LTRIM('00042', '0') FROM DUAL").unwrap(),
            "SELECT TRIM(LEADING '0' FROM '00042') FROM DUAL"
        );
        assert_eq!(
            oracle_to_mariadb("SELECT TRANSLATE('abc', 'ac', 'AC') FROM DUAL").unwrap(),
            "SELECT oracle_translate('abc', 'ac', 'AC') FROM DUAL"
        );
    }

    #[test]
    fn connect_by_path_columns_are_widened() {
        let out = oracle_to_mariadb(
            "SELECT name FROM emp START WITH mgr IS NULL CONNECT BY PRIOR id = mgr",
        )
        .unwrap();
        assert!(out.contains("WITH RECURSIVE __cb AS"));
        assert!(out.contains("CAST(CONCAT(',', __n.id, ',') AS CHAR(4000)) AS __ids"));
        assert!(!out.contains("ARRAY["));
    }

    #[test]
    fn year_to_month_interval_literal_is_general() {
        // Not a hardcoded query: the month count is computed from `<y>-<m>`.
        assert_eq!(
            oracle_to_mariadb("SELECT hire_date + INTERVAL '2-3' YEAR TO MONTH FROM emp").unwrap(),
            "SELECT hire_date + INTERVAL 27 MONTH FROM EMP"
        );
        assert_eq!(
            oracle_to_mariadb("SELECT d - INTERVAL '-1-6' YEAR TO MONTH FROM t").unwrap(),
            "SELECT d - INTERVAL -18 MONTH FROM T"
        );
    }

    #[test]
    fn interval_cast_renders_oracle_text_form() {
        assert_eq!(
            oracle_to_mariadb("SELECT CAST('1-6' AS INTERVAL YEAR TO MONTH) FROM DUAL").unwrap(),
            "SELECT '+01-06' FROM DUAL"
        );
        assert_eq!(
            oracle_to_mariadb("SELECT CAST('9 06:30:00' AS INTERVAL DAY TO SECOND) FROM DUAL")
                .unwrap(),
            "SELECT '+09 06:30:00.000000' FROM DUAL"
        );
    }

    #[test]
    fn extract_day_from_timestamp_difference_uses_datediff() {
        assert_eq!(
            oracle_to_mariadb(
                "SELECT EXTRACT(DAY FROM (TIMESTAMP '2020-02-01 00:00:00' - t.start_ts)) FROM t"
            )
            .unwrap(),
            "SELECT DATEDIFF('2020-02-01 00:00:00', t.start_ts) FROM T"
        );
    }

    #[test]
    fn fetch_first_with_ties_becomes_dense_rank_filter() {
        let out = oracle_to_mariadb(
            "SELECT id, score FROM game ORDER BY score DESC FETCH FIRST 3 ROWS WITH TIES",
        )
        .unwrap();
        assert!(out.contains("DENSE_RANK() OVER (ORDER BY score DESC)"));
        assert!(out.contains("__dbsaci_tie <= 3"));
        assert!(!out.contains("FETCH"));
    }

    #[test]
    fn mariadb_reserved_words_are_quoted_only_as_identifiers() {
        assert_eq!(
            oracle_to_mariadb("SELECT t.body, rank FROM doc t").unwrap(),
            "SELECT t.`body`, `rank` FROM DOC t"
        );
        // A same-named function call is left alone.
        assert_eq!(
            oracle_to_mariadb("SELECT RANK() OVER (ORDER BY x) FROM t").unwrap(),
            "SELECT RANK() OVER (ORDER BY x) FROM T"
        );
    }
}

/// Parse one Oracle statement and render its PostgreSQL-compatible form.
pub fn oracle_to_postgres(sql: &str) -> Result<String> {
    // Lexer-level rewrites first, so the AST parser never sees Oracle-only
    // token shapes it cannot lex (alternative quoting, `seq.NEXTVAL`).
    if let Some(plsql) = rewrite_plsql(sql) {
        return Ok(plsql);
    }
    // `ALTER SESSION` is Oracle syntax, but its PostgreSQL equivalents are
    // ordinary session GUC commands.  Handle it before the generic parser: it
    // deliberately has no Oracle dialect support for this statement family.
    if let Some(session_sql) = rewrite_alter_session(sql) {
        return Ok(session_sql);
    }
    let pre = rewrite_connect_by(&rewrite_insert_all(&rewrite_keep_aggregates(
        &rewrite_oracle_ddl(&rewrite_merge_set_aliases(&rewrite_merge_matched_delete(
            &rewrite_sequence_pseudocolumns(&rewrite_timestamp_with_time_zone_literals(
                &rewrite_to_char_timestamptz_literals(&rewrite_for_update(&rewrite_unpivot(
                    &rewrite_pivot(&normalize_alt_quotes(&fold_uppercase_quoted_identifiers(
                        sql,
                    ))),
                ))),
            )),
        ))),
    )));

    // sqlparser 0.47 does not ship a dedicated Oracle dialect. GenericDialect
    // is the permissive AST parser and represents Oracle's legacy outer-join
    // marker explicitly as Expr::OuterJoin.
    let normalized = normalize_oracle_tokens(&normalize_oracle_aggregates(
        &normalize_legacy_outer_join(&pre)?,
    ));

    let mut statements = match Parser::parse_sql(&GenericDialect {}, &normalized) {
        Ok(statements) => statements,
        // Many Oracle statements that sqlparser 0.47 cannot represent (CREATE
        // SEQUENCE ... INCREMENT BY, COMMENT ON, some ALTER forms) are already
        // valid PostgreSQL once the token rewrites above have run. Rather than
        // failing the call, hand the normalized text straight to the backend and
        // let PostgreSQL accept or reject it.
        Err(_) => return Ok(normalized),
    };
    if statements.len() != 1 {
        return Err(Error::SqlParse(
            "exactly one SQL statement is required per execute call".to_string(),
        ));
    }
    let mut statement = statements.remove(0);
    match &mut statement {
        Statement::Query(query) => translate_query(query)?,
        // INSERT ... VALUES / INSERT ... SELECT and UPDATE ... SET carry Oracle
        // expressions too ('' -> NULL, DECODE, NVL2 ...).
        Statement::Insert(insert) => {
            if let Some(source) = &mut insert.source {
                translate_query(source)?;
            }
        }
        Statement::Update(update) => {
            for assignment in &mut update.assignments {
                rewrite_expr(&mut assignment.value)?;
            }
            if let Some(selection) = &mut update.selection {
                rewrite_expr(selection)?;
            }
        }
        // The SELECT body of a view / CTAS must be translated too.
        Statement::CreateView(cv) => translate_query(&mut cv.query)?,
        Statement::CreateTable(ct) => {
            if let Some(q) = &mut ct.query {
                translate_query(q)?;
            }
        }
        _ => {}
    }
    Ok(statement.to_string())
}

/// Translate the session settings that have a meaningful PostgreSQL analogue.
///
/// The `dbsaci.nls_*` custom GUCs are intentionally session-scoped: the backend
/// facade exposes them through `nls_session_parameters`, and later conversion
/// work can consume the same source of truth without adding proxy-local state.
/// PostgreSQL permits dotted custom GUC names without an extension.
fn rewrite_alter_session(sql: &str) -> Option<String> {
    let trimmed = sql.trim();
    let prefix = "ALTER SESSION SET";
    if !trimmed
        .get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
    {
        return None;
    }
    // Do not mistake `ALTER SESSION SETTING ...` for the command.
    if trimmed
        .as_bytes()
        .get(prefix.len())
        .is_some_and(|b| !b.is_ascii_whitespace())
    {
        return None;
    }
    let mut rest = trimmed[prefix.len()..].trim_start();
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    rest = rest[name_end..].trim_start();
    if let Some(after_equals) = rest.strip_prefix('=') {
        rest = after_equals.trim_start();
    }
    let value = rest.trim_end_matches(';').trim();
    if value.is_empty() {
        return None;
    }

    if name.eq_ignore_ascii_case("CURRENT_SCHEMA") {
        // Keep `public` available for DbSaci's compatibility helpers, while
        // placing the requested schema first so `current_schema()` and normal
        // unqualified object resolution match Oracle's CURRENT_SCHEMA model.
        return Some(format!("SET search_path TO {value}, oracle, public"));
    }
    if name.eq_ignore_ascii_case("TIME_ZONE") {
        // PostgreSQL follows POSIX signs for fixed-offset zone names, the
        // inverse of Oracle's ISO-8601 offset spelling.
        return Some(format!("SET TIME ZONE {}", postgres_time_zone_value(value)));
    }

    let nls = match name.to_ascii_uppercase().as_str() {
        "NLS_DATE_FORMAT" => "nls_date_format",
        "NLS_TIMESTAMP_FORMAT" => "nls_timestamp_format",
        "NLS_TIMESTAMP_TZ_FORMAT" => "nls_timestamp_tz_format",
        "NLS_NUMERIC_CHARACTERS" => "nls_numeric_characters",
        "NLS_LANGUAGE" => "nls_language",
        "NLS_DATE_LANGUAGE" => "nls_date_language",
        "NLS_TERRITORY" => "nls_territory",
        "NLS_SORT" => "nls_sort",
        "NLS_COMP" => "nls_comp",
        _ => "",
    };
    if !nls.is_empty() {
        return Some(format!("SET dbsaci.{nls} TO {value}"));
    }

    // Optimizer tracing/tuning directives do not affect SQL correctness at
    // this stage.  Accept them so Oracle clients that set diagnostics on login
    // continue normally, without pretending that PostgreSQL implements them.
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("OPTIMIZER")
        || upper == "EVENTS"
        || upper == "SQL_TRACE"
        || upper.starts_with("TRACE")
    {
        return Some("RESET application_name".to_string());
    }
    None
}

fn postgres_time_zone_value(value: &str) -> String {
    let trimmed = value.trim();
    let (quote, inner) = match (trimmed.as_bytes().first(), trimmed.as_bytes().last()) {
        (Some(b'\''), Some(b'\'')) if trimmed.len() >= 2 => ("'", &trimmed[1..trimmed.len() - 1]),
        _ => ("", trimmed),
    };
    let bytes = inner.as_bytes();
    let fixed_offset = bytes.len() == 6
        && matches!(bytes[0], b'+' | b'-')
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3] == b':'
        && bytes[4].is_ascii_digit()
        && bytes[5].is_ascii_digit();
    if fixed_offset {
        let sign = if bytes[0] == b'+' { '-' } else { '+' };
        format!("{quote}{sign}{}{quote}", &inner[1..])
    } else {
        trimmed.to_string()
    }
}

/// PostgreSQL silently discards an offset in `TIMESTAMP '... +05:00'`, while
/// Oracle treats that literal as timestamp-with-time-zone. Promote only those
/// offset-bearing literals before the SQL AST is built.
fn rewrite_timestamp_with_time_zone_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let end = skip_quoted(sql, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes
            .get(i..i + 9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"TIMESTAMP"))
        {
            let prior_is_ident =
                i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let mut quote = i + 9;
            while quote < bytes.len() && bytes[quote].is_ascii_whitespace() {
                quote += 1;
            }
            if !prior_is_ident && bytes.get(quote) == Some(&b'\'') {
                let end = skip_quoted(sql, quote);
                let literal = &sql[quote + 1..end - 1];
                let lb = literal.as_bytes();
                let has_offset = lb.len() >= 6
                    && matches!(lb[lb.len() - 6], b'+' | b'-')
                    && lb[lb.len() - 5].is_ascii_digit()
                    && lb[lb.len() - 4].is_ascii_digit()
                    && lb[lb.len() - 3] == b':'
                    && lb[lb.len() - 2].is_ascii_digit()
                    && lb[lb.len() - 1].is_ascii_digit();
                // An explicit Oracle `CAST(... AS TIMESTAMP)` intentionally
                // drops the zone while retaining wall-clock fields; PostgreSQL
                // already gives that result for its TIMESTAMP literal.
                let explicit_timestamp_cast = sql[end..]
                    .trim_start()
                    .to_ascii_uppercase()
                    .starts_with("AS TIMESTAMP)");
                if has_offset && !explicit_timestamp_cast {
                    out.push_str("TIMESTAMPTZ");
                    out.push_str(&sql[i + 9..end]);
                    i = end;
                    continue;
                }
            }
        }
        let ch = sql[i..].chars().next().expect("index is in input");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// PostgreSQL normalizes `timestamptz` display to the session time zone,
/// whereas Oracle `TO_CHAR` retains the offset stored in a literal value. For
/// this common literal form, render in that explicit zone and append it.
fn rewrite_to_char_timestamptz_literals(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(rel) = upper[cursor..].find("TO_CHAR(TIMESTAMP '") {
        let start = cursor + rel;
        out.push_str(&sql[cursor..start]);
        let literal_start = start + "TO_CHAR(TIMESTAMP '".len();
        let Some(literal_end_rel) = sql[literal_start..].find('\'') else {
            out.push_str(&sql[start..]);
            return out;
        };
        let literal_end = literal_start + literal_end_rel;
        let literal = &sql[literal_start..literal_end];
        let b = literal.as_bytes();
        let offset_start = b.len().checked_sub(6).filter(|&p| {
            matches!(b[p], b'+' | b'-')
                && b[p + 1].is_ascii_digit()
                && b[p + 2].is_ascii_digit()
                && b[p + 3] == b':'
                && b[p + 4].is_ascii_digit()
                && b[p + 5].is_ascii_digit()
        });
        let Some(offset_start) = offset_start else {
            out.push_str(&sql[start..literal_end + 1]);
            cursor = literal_end + 1;
            continue;
        };
        let after_literal = literal_end + 1;
        let Some(format_open_rel) = sql[after_literal..].find("'") else {
            out.push_str(&sql[start..]);
            return out;
        };
        let format_open = after_literal + format_open_rel + 1;
        let Some(format_end_rel) = sql[format_open..].find('\'') else {
            out.push_str(&sql[start..]);
            return out;
        };
        let format_end = format_open + format_end_rel;
        let format = &sql[format_open..format_end];
        let tail = sql[format_end + 1..].trim_start();
        if !format.to_ascii_uppercase().contains("TZH:TZM") || !tail.starts_with(')') {
            out.push_str(&sql[start..literal_end + 1]);
            cursor = literal_end + 1;
            continue;
        }
        let close = format_end + 1 + (sql[format_end + 1..].len() - tail.len());
        let offset = &literal[offset_start..];
        // PostgreSQL's named-zone parser follows POSIX signs (`UTC-05:00`
        // means five hours east of UTC), the inverse of ISO-8601 offsets.
        let pg_offset = format!(
            "UTC{}{}",
            if offset.starts_with('+') { '-' } else { '+' },
            &offset[1..]
        );
        let display_format = format.replace(" TZH:TZM", "").replace(" tzh:tzm", "");
        out.push_str(&format!(
            "TO_CHAR(TIMESTAMPTZ '{}' AT TIME ZONE '{}', '{}') || ' {}'",
            literal, pg_offset, display_format, offset
        ));
        cursor = close + 1;
    }
    out.push_str(&sql[cursor..]);
    out
}

/// Oracle alternative quoting: `q'[ ... ]'`, `q'{ ... }'`, `q'< ... >'`,
/// `q'( ... )'`, or `q'X ... X'` for any other delimiter `X`. Rewrite to a
/// standard single-quoted literal with `'` doubled. `nq'...'` behaves the same
/// for our purposes.
/// Index one past the closing quote of the string/identifier literal that opens
/// at `sql[start]` (which must be `'` or `"`), honouring doubled-quote escapes.
/// UTF-8 safe: callers copy `&sql[start..end]` as a slice.
fn skip_quoted(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// Oracle folds every *unquoted* identifier to UPPER CASE, so `"A_TABLE"`
/// (double-quoted, already all-upper) names the exact same object as the bare
/// `A_TABLE`. PostgreSQL instead folds unquoted identifiers to lower case and
/// keeps quoted ones verbatim — so DbSaci must rewrite an all-uppercase quoted
/// identifier to its lower-case quoted form (`"A_TABLE"` -> `"a_table"`). That
/// keeps it reachable from later bare DML (which PostgreSQL lower-cases) while
/// still surviving a reserved word used as a name. Mixed / lower-case quoted
/// identifiers (`"MixedCase"`, `"col"`) are genuinely case-sensitive in Oracle
/// too and are left untouched. String literals and comments are skipped.
fn fold_uppercase_quoted_identifiers(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                let end = skip_quoted(sql, i);
                out.push_str(&sql[i..end]);
                i = end;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |o| i + o);
                out.push_str(&sql[i..end]);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = bytes[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map_or(bytes.len(), |o| i + 4 + o);
                out.push_str(&sql[i..end]);
                i = end;
            }
            b'"' => {
                let end = skip_quoted(sql, i); // one past the closing quote
                let inner = &sql[i + 1..end - 1];
                let is_upper_ident = !inner.is_empty()
                    && inner
                        .bytes()
                        .next()
                        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
                    && inner.bytes().all(|b| {
                        b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'$'
                    })
                    && !inner.contains("\"\"");
                if is_upper_ident {
                    out.push('"');
                    out.push_str(&inner.to_ascii_lowercase());
                    out.push('"');
                } else {
                    out.push_str(&sql[i..end]);
                }
                i = end;
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

fn normalize_alt_quotes(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        // Ordinary string literal: copy the slice verbatim.
        if bytes[i] == b'\'' {
            let end = skip_quoted(sql, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        let is_q = (bytes[i] == b'q' || bytes[i] == b'Q')
            && bytes.get(i + 1) == Some(&b'\'')
            && bytes.get(i + 2).is_some();
        let is_nq = (bytes[i] == b'n' || bytes[i] == b'N')
            && matches!(bytes.get(i + 1), Some(b'q' | b'Q'))
            && bytes.get(i + 2) == Some(&b'\'')
            && bytes.get(i + 3).is_some();
        if is_q || is_nq {
            let open_at = if is_nq { i + 3 } else { i + 2 };
            let open = bytes[open_at];
            let close = match open {
                b'[' => b']',
                b'{' => b'}',
                b'<' => b'>',
                b'(' => b')',
                other => other,
            };
            // Find the closing `<close>'` sequence.
            let mut j = open_at + 1;
            let mut end = None;
            while j + 1 < bytes.len() {
                if bytes[j] == close && bytes[j + 1] == b'\'' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = end {
                let inner = &sql[open_at + 1..end];
                out.push('\'');
                out.push_str(&inner.replace('\'', "''"));
                out.push('\'');
                i = end + 2;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Oracle multi-table insert:
///   INSERT ALL   INTO t (c) VALUES (e) ...  <subquery>
///   INSERT FIRST WHEN p THEN INTO t (c) VALUES (e) ... [ELSE INTO ...] <subquery>
/// -> a data-modifying CTE: `WITH __src AS (<subquery>),
///     __i0 AS (INSERT INTO t (c) SELECT e FROM __src [WHERE p] RETURNING 1), ...
///     SELECT <sum of counts>`.
fn rewrite_insert_all(sql: &str) -> String {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_ascii_uppercase();
    let conditional = upper.starts_with("INSERT FIRST ") || upper.starts_with("INSERT FIRST\n");
    if !conditional && !(upper.starts_with("INSERT ALL ") || upper.starts_with("INSERT ALL\n")) {
        return sql.to_string();
    }

    // Body after `INSERT ALL` / `INSERT FIRST`.
    let kw_len = if conditional {
        "INSERT FIRST".len()
    } else {
        "INSERT ALL".len()
    };
    let body = trimmed[trimmed
        .char_indices()
        .nth(kw_len)
        .map_or(trimmed.len(), |(i, _)| i)..]
        .trim_start();

    // The trailing subquery starts at the last top-level SELECT/WITH that is not
    // part of a VALUES(...) list — i.e. after the final `)` that closes a
    // `VALUES (`. Scan for `VALUES` occurrences at depth 0 and take the tail
    // after the matching close paren of the last one.
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut quoted = 0u8;
    let mut last_values_close = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' | b'"' if quoted == 0 => quoted = bytes[idx],
            b if quoted != 0 && b == quoted => quoted = 0,
            b'(' if quoted == 0 => depth += 1,
            b')' if quoted == 0 => depth -= 1,
            _ if quoted == 0
                && depth == 0
                && body[idx..].len() >= 7
                && body[idx..idx + 6].eq_ignore_ascii_case("VALUES")
                && !body.as_bytes()[idx + 6].is_ascii_alphanumeric() =>
            {
                // advance to the '(' then to its match
                if let Some(open_rel) = body[idx..].find('(') {
                    let open = idx + open_rel;
                    if let Some(close_rel) = matching_paren(&body[open..]) {
                        last_values_close = open + close_rel + 1;
                        idx = last_values_close;
                        continue;
                    }
                }
            }
            _ => {}
        }
        idx += 1;
    }
    if last_values_close == 0 {
        return sql.to_string();
    }
    let clauses_text = &body[..last_values_close];
    let subquery = body[last_values_close..]
        .trim()
        .trim_end_matches(';')
        .trim();
    if subquery.is_empty() {
        return sql.to_string();
    }

    // Parse the INTO clauses (with optional `WHEN cond THEN` / `ELSE` markers).
    #[derive(Default)]
    struct Target {
        table: String,
        cols: String,
        values: String,
        when: Option<String>,
    }
    let mut targets: Vec<Target> = Vec::new();
    let mut pending_when: Option<String> = None;
    let mut prior_conds: Vec<String> = Vec::new();
    let mut rest = clauses_text.trim();
    while !rest.is_empty() {
        let ru = rest.to_ascii_uppercase();
        if ru.starts_with("WHEN ") {
            let then_at = ru.find(" THEN").unwrap_or(rest.len());
            let cond = rest[5..then_at].trim().to_string();
            prior_conds.push(cond.clone());
            pending_when = Some(cond);
            rest = rest[(then_at + " THEN".len()).min(rest.len())..].trim_start();
            continue;
        }
        if ru.starts_with("ELSE ") {
            let negated = prior_conds
                .iter()
                .map(|c| format!("NOT ({c})"))
                .collect::<Vec<_>>()
                .join(" AND ");
            pending_when = Some(if negated.is_empty() {
                "TRUE".into()
            } else {
                negated
            });
            rest = rest[5..].trim_start();
            continue;
        }
        if !ru.starts_with("INTO ") {
            break;
        }
        rest = rest[5..].trim_start();
        // table name up to whitespace or '('
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(rest.len());
        let table = rest[..name_end].to_string();
        rest = rest[name_end..].trim_start();
        let mut cols = String::new();
        if rest.starts_with('(') {
            let close = matching_paren(rest).unwrap_or(0);
            cols = rest[..=close].to_string();
            rest = rest[close + 1..].trim_start();
        }
        // VALUES (...)
        let vu = rest.to_ascii_uppercase();
        if !vu.starts_with("VALUES") {
            break;
        }
        rest = rest["VALUES".len()..].trim_start();
        let close = matching_paren(rest).unwrap_or(0);
        let values = rest[1..close].to_string();
        rest = rest[close + 1..].trim_start();
        targets.push(Target {
            table,
            cols,
            values,
            when: pending_when.clone(),
        });
    }
    if targets.is_empty() {
        return sql.to_string();
    }

    let mut ctes = vec![format!("__src AS ({subquery})")];
    let mut counts = Vec::new();
    for (n, t) in targets.iter().enumerate() {
        let where_clause = t
            .when
            .as_ref()
            .map(|c| format!(" WHERE {c}"))
            .unwrap_or_default();
        ctes.push(format!(
            "__i{n} AS (INSERT INTO {} {} SELECT {} FROM __src{} RETURNING 1)",
            t.table, t.cols, t.values, where_clause
        ));
        counts.push(format!("(SELECT count(*) FROM __i{n})"));
    }
    format!(
        "WITH {} SELECT {} AS inserted",
        ctes.join(", "),
        counts.join(" + ")
    )
}

/// Best-effort Oracle PL/SQL -> PL/pgSQL. Handles anonymous blocks
/// (`[DECLARE ...] BEGIN ... END;`) and `CREATE [OR REPLACE] FUNCTION|PROCEDURE`.
/// Returns `None` when the statement is not PL/SQL.
fn rewrite_plsql(sql: &str) -> Option<String> {
    let t = sql.trim().trim_end_matches('/').trim();
    let tu = t.to_ascii_uppercase();

    let common = |body: &str| -> String {
        let mut b = body.to_string();
        // DBMS_OUTPUT.PUT_LINE(x)  ->  RAISE NOTICE '%', x
        while let Some(p) = b.to_ascii_uppercase().find("DBMS_OUTPUT.PUT_LINE") {
            let open = p + b[p..].find('(').unwrap();
            let close = open + matching_paren(&b[open..]).unwrap_or(0);
            let arg = b[open + 1..close].to_string();
            b.replace_range(p..close + 1, &format!("RAISE NOTICE '%', {arg}"));
        }
        // RAISE_APPLICATION_ERROR(code, 'msg')  ->  RAISE EXCEPTION 'msg'
        while let Some(p) = b.to_ascii_uppercase().find("RAISE_APPLICATION_ERROR") {
            let open = p + b[p..].find('(').unwrap();
            let close = open + matching_paren(&b[open..]).unwrap_or(0);
            let inner = b[open + 1..close].to_string();
            let msg = inner
                .split_once(',')
                .map(|x| x.1.trim())
                .unwrap_or("'error'");
            b.replace_range(p..close + 1, &format!("RAISE EXCEPTION '%', {msg}"));
        }
        // EXECUTE IMMEDIATE  ->  EXECUTE
        b = replace_ci(&b, "EXECUTE IMMEDIATE", "EXECUTE");
        // `PRAGMA EXCEPTION_INIT(my_exc, -20xxx)` binds a user-declared
        // exception name to an error code. PL/pgSQL has no user exception
        // registry, but `RAISE_APPLICATION_ERROR` already lowers to a P0001
        // `RAISE EXCEPTION` (condition name `raise_exception`), so route
        // `WHEN my_exc` / `RAISE my_exc` there. Collect the names before the
        // pragma text is stripped.
        let mut user_exceptions: Vec<String> = Vec::new();
        {
            let bu = b.to_ascii_uppercase();
            let mut from = 0;
            while let Some(rel) = bu[from..].find("EXCEPTION_INIT") {
                let p = from + rel;
                if let Some(open) = b[p..].find('(') {
                    let open = p + open;
                    let close = open + matching_paren(&b[open..]).unwrap_or(0);
                    if close > open
                        && let Some(name) = b[open + 1..close].split(',').next()
                    {
                        let name = name.trim();
                        if !name.is_empty() {
                            user_exceptions.push(name.to_string());
                        }
                    }
                    from = close + 1;
                } else {
                    from = p + "EXCEPTION_INIT".len();
                }
            }
        }
        // Drop pragmas.
        while let Some(p) = b.to_ascii_uppercase().find("PRAGMA ") {
            let end = b[p..].find(';').map_or(b.len(), |e| p + e + 1);
            b.replace_range(p..end, "");
        }
        // Drop `my_exc EXCEPTION;` declarations (no PL/pgSQL equivalent); the
        // handlers that referenced them are remapped below.
        b = drop_exception_declarations(&b, &mut user_exceptions);
        for name in &user_exceptions {
            b = replace_ci(&b, &format!("WHEN {name} "), "WHEN raise_exception ");
            b = replace_ci(&b, &format!("WHEN {name}\t"), "WHEN raise_exception ");
            b = replace_ci(&b, &format!("WHEN {name}\n"), "WHEN raise_exception\n");
            b = replace_ci(
                &b,
                &format!("RAISE {name};"),
                "RAISE EXCEPTION 'application error' USING ERRCODE = 'P0001';",
            );
        }
        // Oracle explicit cursors: `CURSOR c [ (args) ] IS <query>` becomes
        // PL/pgSQL's `c CURSOR [ (args) ] FOR <query>`. `OPEN`/`FETCH … INTO`/
        // `CLOSE`, `FOR row IN c LOOP`, and `WHERE CURRENT OF c` are all native.
        b = rewrite_explicit_cursors(&b);
        // Oracle's cursor-FOR-loop over an inline query parenthesises it
        // (`FOR r IN (SELECT …) LOOP`); PL/pgSQL's query form takes the bare
        // `SELECT` (parens make it read as an integer-range loop bound).
        b = unwrap_for_loop_query(&b);
        // Oracle predefined exception names -> PL/pgSQL condition names, only
        // where they follow `WHEN` in an exception handler.
        for (oracle, pg) in [
            ("DUP_VAL_ON_INDEX", "unique_violation"),
            ("ZERO_DIVIDE", "division_by_zero"),
            ("INVALID_NUMBER", "invalid_text_representation"),
            ("VALUE_ERROR", "data_exception"),
            ("STORAGE_ERROR", "out_of_memory"),
            ("CURSOR_ALREADY_OPEN", "duplicate_cursor"),
        ] {
            b = replace_ci(&b, &format!("WHEN {oracle}"), &format!("WHEN {pg}"));
        }
        // Oracle `SELECT ... INTO v` raises NO_DATA_FOUND / TOO_MANY_ROWS on a
        // non-single result; PL/pgSQL only does so with `INTO STRICT`.
        b = add_select_into_strict(&b);
        normalize_oracle_tokens(&b)
    };

    // ---- CREATE TRIGGER ------------------------------------------------
    if tu.starts_with("CREATE TRIGGER") || tu.starts_with("CREATE OR REPLACE TRIGGER") {
        let rest = strip_kw(t, "CREATE")?;
        let rest = strip_kw(rest, "OR REPLACE").unwrap_or(rest);
        let rest = strip_kw(rest, "TRIGGER")?;
        let name = rest.split_whitespace().next()?;
        let after_name = rest[rest.find(name)? + name.len()..].trim_start();

        // timing
        let (timing, after_timing) = ["BEFORE", "AFTER", "INSTEAD OF"]
            .iter()
            .find_map(|k| strip_kw(after_name, k).map(|r| (*k, r)))?;

        // events: up to `ON`
        let on_at = find_top_level_kw(after_timing, "ON")?;
        let events = after_timing[..on_at].trim(); // e.g. "INSERT OR UPDATE OF col OR DELETE"
        let after_on = after_timing[on_at + "ON".len()..].trim_start();
        let table = after_on.split_whitespace().next()?;
        let mut tail = after_on[after_on.find(table)? + table.len()..].trim_start();

        // Optional `REFERENCING NEW AS n OLD AS o` (PostgreSQL has no row-level
        // equivalent): drop it and remember the aliases so the body's `:n` /
        // `:o` correlation names still resolve to NEW / OLD.
        let mut ref_new: Option<String> = None;
        let mut ref_old: Option<String> = None;
        if let Some(r) = strip_kw(tail, "REFERENCING") {
            let mut r = r.trim_start();
            loop {
                if let Some(rest) = strip_kw(r, "NEW AS") {
                    let alias = rest.split_whitespace().next()?;
                    ref_new = Some(alias.to_string());
                    r = rest.trim_start()[alias.len()..].trim_start();
                } else if let Some(rest) = strip_kw(r, "OLD AS") {
                    let alias = rest.split_whitespace().next()?;
                    ref_old = Some(alias.to_string());
                    r = rest.trim_start()[alias.len()..].trim_start();
                } else {
                    break;
                }
            }
            tail = r;
        }

        let for_each_row = if let Some(r) = strip_kw(tail, "FOR EACH ROW") {
            tail = r;
            true
        } else if let Some(r) = strip_kw(tail, "FOR EACH STATEMENT") {
            tail = r;
            false
        } else {
            false
        };

        let mut when_clause = String::new();
        if let Some(r) = strip_kw(tail, "WHEN") {
            let r = r.trim_start();
            if r.starts_with('(') {
                let close = matching_paren(r)?;
                when_clause = format!(" WHEN {}", &r[..=close]);
                tail = r[close + 1..].trim_start();
            }
        }

        // The rest is the PL/SQL block. Oracle correlation names carry a colon
        // in the body but not in the WHEN clause.
        let body_src = tail
            .trim()
            .trim_end_matches('/')
            .trim()
            .trim_end_matches(';');
        let mut body = body_src.to_string();
        if let Some(alias) = &ref_new {
            body = replace_ci(&body, &format!(":{alias}"), "NEW");
        }
        if let Some(alias) = &ref_old {
            body = replace_ci(&body, &format!(":{alias}"), "OLD");
        }
        let body = replace_ci(&body, ":NEW", "NEW");
        let body = replace_ci(&body, ":OLD", "OLD");
        let mut body = common(&body);
        body = call_bare_invocations(&body);

        // A row trigger's function must return a row. Insert the RETURN before
        // the trailing END if the author did not write one.
        if for_each_row
            && !body.to_ascii_uppercase().contains("RETURN ")
            && let Some(end_at) = body.to_ascii_uppercase().rfind("END")
        {
            body.insert_str(end_at, "RETURN COALESCE(NEW, OLD); ");
        }
        let level = if for_each_row {
            " FOR EACH ROW"
        } else {
            " FOR EACH STATEMENT"
        };
        let fn_name = format!("{name}__dbsaci_fn");
        // `CREATE OR REPLACE TRIGGER` is PostgreSQL 14+. Emit the
        // drop-then-create form instead so the translation runs on every
        // supported PostgreSQL major (13+).
        return Some(format!(
            "CREATE OR REPLACE FUNCTION {fn_name}() RETURNS trigger LANGUAGE plpgsql AS $dbsaci$ {body} $dbsaci$; \
             DROP TRIGGER IF EXISTS {name} ON {table}; \
             CREATE TRIGGER {name} {timing} {events} ON {table}{level}{when_clause} EXECUTE FUNCTION {fn_name}()"
        ));
    }

    // ---- CREATE FUNCTION / PROCEDURE -------------------------------------
    if tu.starts_with("CREATE OR REPLACE FUNCTION")
        || tu.starts_with("CREATE FUNCTION")
        || tu.starts_with("CREATE OR REPLACE PROCEDURE")
        || tu.starts_with("CREATE PROCEDURE")
    {
        let is_proc = tu.contains("PROCEDURE");
        // signature ends at the top-level `IS` / `AS` that precedes the body.
        let is_at = find_top_level_kw(t, "IS").or_else(|| find_top_level_kw(t, "AS"))?;
        let sig = &t[..is_at];
        let body = t[is_at + 2..].trim().trim_end_matches(';').trim();
        // `RETURN <type>` in the signature -> `RETURNS <type>`
        let sig = if is_proc {
            sig.to_string()
        } else if let Some(rp) = sig.to_ascii_uppercase().rfind("RETURN ") {
            format!("{}RETURNS {}", &sig[..rp], &sig[rp + "RETURN ".len()..])
        } else {
            sig.to_string()
        };
        let sig = normalize_oracle_tokens(&sig);
        let mut body = common(body);
        // Oracle puts declarations between `IS` and `BEGIN`; PL/pgSQL needs an
        // explicit `DECLARE` in front of them.
        if !body.trim_start().to_ascii_uppercase().starts_with("BEGIN")
            && !body
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("DECLARE")
        {
            body = format!("DECLARE {body}");
        }
        body = ensure_loop_record_vars(&body);
        return Some(format!(
            "{} AS $plsql$ {} ; $plsql$ LANGUAGE plpgsql",
            sig.trim(),
            body
        ));
    }

    // ---- anonymous block ----------------------------------------------------
    if tu.starts_with("BEGIN ")
        || tu.starts_with("BEGIN\n")
        || tu.starts_with("DECLARE ")
        || tu.starts_with("DECLARE\n")
    {
        let inner = t.trim_end_matches(';').trim();
        let mut inner = common(inner);
        inner = ensure_loop_record_vars(&inner);
        // Drop an empty DECLARE section (e.g. after removing a PRAGMA).
        let iu = inner.to_ascii_uppercase();
        if iu.starts_with("DECLARE") {
            let after = inner["DECLARE".len()..].trim_start();
            if after.to_ascii_uppercase().starts_with("BEGIN") {
                inner = after.to_string();
            }
        }
        // A bare `proc(args)` statement must be `CALL proc(args)` in PL/pgSQL.
        inner = call_bare_invocations(&inner);
        return Some(format!("DO $plsql$ {inner} $plsql$"));
    }

    None
}

/// Turn each `SELECT … INTO <targets>` inside a PL/SQL block into
/// `SELECT … INTO STRICT <targets>` (Oracle's always-single-row semantics),
/// leaving `INSERT INTO` and an already-`STRICT` clause alone.
fn add_select_into_strict(block: &str) -> String {
    let mut out = String::with_capacity(block.len() + 16);
    let bytes = block.as_bytes();
    let mut i = 0;
    while i < block.len() {
        // match a whitespace-delimited `INTO` (case-insensitive)
        let is_into = block[i..].len() >= 4
            && block[i..i + 4].eq_ignore_ascii_case("into")
            && (i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b'(')
            && block[i + 4..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace());
        if is_into {
            let before = out.trim_end().to_ascii_uppercase();
            let already_strict = block[i + 4..]
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("STRICT");
            // `FETCH cur INTO v` and `INSERT … INTO` take no STRICT; only
            // `SELECT … INTO` / `EXECUTE … INTO` do.
            let current_stmt = before.rsplit([';', '\n']).next().unwrap_or("").trim_start();
            let no_strict_stmt = before.ends_with("INSERT") || current_stmt.starts_with("FETCH");
            if !no_strict_stmt && !already_strict {
                out.push_str("INTO STRICT");
                i += 4;
                continue;
            }
        }
        let ch = block[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Remove Oracle `my_exc EXCEPTION;` variable declarations (PL/pgSQL has no
/// user-declared exceptions). Any name dropped this way is also appended to
/// `names` so its `WHEN` handlers get remapped to `raise_exception`.
fn drop_exception_declarations(block: &str, names: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(block.len());
    let bytes = block.as_bytes();
    let mut i = 0;
    while i < block.len() {
        // Look for `<ident> EXCEPTION ;` at a statement boundary.
        let at_boundary = i == 0
            || matches!(bytes[i - 1], b';' | b'\n')
            || out.trim_end().is_empty()
            || out.trim_end().to_ascii_uppercase().ends_with("DECLARE");
        if at_boundary && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            let mut j = i;
            while j < block.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let ident = &block[i..j];
            let rest = block[j..].trim_start();
            if let Some(after_kw) = rest
                .get(..9)
                .filter(|s| s.eq_ignore_ascii_case("EXCEPTION"))
                .and(rest.get(9..))
                && after_kw.trim_start().starts_with(';')
                && !ident.eq_ignore_ascii_case("PRAGMA")
            {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(ident)) {
                    names.push(ident.to_string());
                }
                // Skip through the `;`.
                let semi = j + block[j..].find(';').unwrap();
                i = semi + 1;
                continue;
            }
        }
        let ch = block[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// PL/pgSQL does not auto-declare the loop variable of a `FOR row IN <query |
/// cursor> LOOP` (only the integer form `FOR i IN a..b` is implicit). Oracle
/// does. Inject a `<var> RECORD;` declaration for each such loop variable that
/// is not already declared, creating a `DECLARE` section if there is none.
fn ensure_loop_record_vars(block: &str) -> String {
    let bu = block.to_ascii_uppercase();
    let mut needed: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(rel) = bu[i..].find("FOR ") {
        let p = i + rel;
        let at_boundary = p == 0 || !bu.as_bytes()[p - 1].is_ascii_alphanumeric();
        i = p + 4;
        if !at_boundary {
            continue;
        }
        let after_for = &block[p + 4..];
        let mut it = after_for.splitn(2, |c: char| c.is_whitespace());
        let var = it.next().unwrap_or("").trim();
        let Some(rest) = it.next() else { continue };
        let rest = rest.trim_start();
        if !rest.to_ascii_uppercase().starts_with("IN ") {
            continue;
        }
        let after_in = rest[3..].trim_start();
        let head_upper = after_in.to_ascii_uppercase();
        // integer range loop -> loop var is implicit; skip.
        let loop_end = head_upper.find("LOOP").unwrap_or(head_upper.len());
        if head_upper.starts_with("REVERSE") || head_upper[..loop_end].contains("..") {
            continue;
        }
        let is_ident = !var.is_empty() && var.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !is_ident {
            continue;
        }
        // already declared?  (word-boundary search in the DECLARE section)
        let decl_end = bu.find("BEGIN").unwrap_or(0);
        let declared = decl_end > 0 && {
            let vu = var.to_ascii_uppercase();
            bu[..decl_end]
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == vu)
        };
        if !declared && !needed.iter().any(|n| n.eq_ignore_ascii_case(var)) {
            needed.push(var.to_string());
        }
    }
    if needed.is_empty() {
        return block.to_string();
    }
    let decls: String = needed.iter().map(|n| format!("{n} RECORD; ")).collect();
    let bt = block.trim_start();
    if bt.to_ascii_uppercase().starts_with("DECLARE") {
        let (head, tail) = block.split_at(block.find("DECLARE").unwrap() + "DECLARE".len());
        format!("{head} {decls}{tail}")
    } else if let Some(bpos) = bu.find("BEGIN") {
        format!("DECLARE {decls}{}", &block[bpos..])
    } else {
        block.to_string()
    }
}

/// `FOR <var> IN (SELECT …) LOOP`  ->  `FOR <var> IN SELECT … LOOP`.
fn unwrap_for_loop_query(block: &str) -> String {
    let mut out = String::with_capacity(block.len());
    let bu = block.to_ascii_uppercase();
    let mut i = 0;
    while i < block.len() {
        let boundary = i == 0 || !bu.as_bytes()[i - 1].is_ascii_alphanumeric();
        if boundary
            && bu[i..].starts_with("FOR ")
            && let Some(in_rel) = bu[i..].find(" IN ")
        {
            let after_in = i + in_rel + " IN ".len();
            let rest = &block[after_in..];
            let trimmed = rest.trim_start();
            let lead_ws = rest.len() - trimmed.len();
            if trimmed.starts_with('(')
                && let Some(close) = matching_paren(trimmed)
            {
                let inner = trimmed[1..close].trim();
                let after_close = trimmed[close + 1..].trim_start();
                if inner.to_ascii_uppercase().starts_with("SELECT")
                    && after_close.to_ascii_uppercase().starts_with("LOOP")
                {
                    out.push_str(&block[i..after_in]);
                    out.push_str(&rest[..lead_ws]);
                    out.push_str(inner);
                    out.push(' ');
                    i = after_in + lead_ws + close + 1;
                    continue;
                }
            }
        }
        let ch = block[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rewrite Oracle explicit-cursor declarations `CURSOR c [ (params) ] IS query`
/// to PL/pgSQL's `c CURSOR [ (params) ] FOR query`.
fn rewrite_explicit_cursors(block: &str) -> String {
    let mut out = String::with_capacity(block.len());
    let bu = block.to_ascii_uppercase();
    let mut i = 0;
    while i < block.len() {
        let rest_u = &bu[i..];
        // `CURSOR` as a whole word, at a statement/declare boundary.
        let boundary = i == 0 || !bu.as_bytes()[i - 1].is_ascii_alphanumeric();
        if boundary && rest_u.starts_with("CURSOR ") {
            let after_kw = i + "CURSOR ".len();
            let name_start =
                after_kw + block[after_kw..].len() - block[after_kw..].trim_start().len();
            let mut k = name_start;
            let b = block.as_bytes();
            while k < block.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                k += 1;
            }
            if k > name_start {
                let name = &block[name_start..k];
                let mut after = block[k..].trim_start();
                let mut params = "";
                if after.starts_with('(')
                    && let Some(close) = matching_paren(after)
                {
                    params = &after[..=close];
                    after = after[close + 1..].trim_start();
                }
                if after.get(..2).is_some_and(|s| s.eq_ignore_ascii_case("IS"))
                    && after[2..].chars().next().is_none_or(|c| c.is_whitespace())
                {
                    let query = after[2..].trim_start();
                    out.push_str(name);
                    out.push_str(" CURSOR ");
                    if !params.is_empty() {
                        out.push_str(params);
                        out.push(' ');
                    }
                    out.push_str("FOR ");
                    // Continue scanning from the query text.
                    let consumed = block.len() - query.len();
                    i = consumed;
                    continue;
                }
            }
        }
        let ch = block[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Prefix `CALL ` to a lone `proc(args);` statement (PL/pgSQL requires it).
fn call_bare_invocations(block: &str) -> String {
    const KW: &[&str] = &[
        "IF",
        "WHILE",
        "FOR",
        "LOOP",
        "RETURN",
        "RAISE",
        "EXECUTE",
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "NULL",
        "COMMIT",
        "ROLLBACK",
        "OPEN",
        "CLOSE",
        "FETCH",
        "EXIT",
        "CONTINUE",
        "PERFORM",
        "CALL",
        "CASE",
        "BEGIN",
        "END",
        "EXCEPTION",
        "WHEN",
    ];
    let bytes = block.as_bytes();
    let mut out = String::with_capacity(block.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        // identifier?
        if (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
            && (i == 0 || bytes[i - 1] == b';' || bytes[i - 1].is_ascii_whitespace())
        {
            // the char before (skipping ws) must be `;`, or the prior word must
            // be BEGIN, or we're at the start.
            let mut k = i;
            while k > 0 && bytes[k - 1].is_ascii_whitespace() {
                k -= 1;
            }
            let prev_ok = k == 0 || bytes[k - 1] == b';' || {
                let mut w = k;
                while w > 0 && (bytes[w - 1].is_ascii_alphanumeric() || bytes[w - 1] == b'_') {
                    w -= 1;
                }
                block[w..k].eq_ignore_ascii_case("BEGIN")
            };
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &block[start..i];
            let after = block[i..].trim_start();
            if prev_ok && !KW.iter().any(|k| word.eq_ignore_ascii_case(k)) && after.starts_with('(')
            {
                let close = matching_paren(after).unwrap_or(0);
                let tail = after[close + 1..].trim_start();
                if tail.starts_with(';') || tail.is_empty() {
                    out.push_str("CALL ");
                }
            }
            out.push_str(word);
            continue;
        }
        let c = block[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn replace_ci(s: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let fu = from.to_ascii_uppercase();
    while let Some(p) = rest.to_ascii_uppercase().find(&fu) {
        out.push_str(&rest[..p]);
        out.push_str(to);
        rest = &rest[p + from.len()..];
    }
    out.push_str(rest);
    out
}

/// Oracle `CONNECT BY` hierarchical query -> PostgreSQL `WITH RECURSIVE`.
/// Text-based (sqlparser cannot round-trip `CONNECT BY` to valid PostgreSQL).
/// Supports `LEVEL`, `PRIOR`, `START WITH`, `CONNECT_BY_ROOT`,
/// `SYS_CONNECT_BY_PATH`, `CONNECT_BY_ISLEAF`, `ORDER SIBLINGS BY`.
fn rewrite_connect_by(sql: &str) -> String {
    if find_top_level_kw(sql, "CONNECT").is_none() {
        return sql.to_string();
    }
    let up = sql.to_ascii_uppercase();
    let Some(_cb_at) = up.find("CONNECT BY") else {
        return sql.to_string();
    };
    let Some(from_at) = find_top_level_kw(sql, "FROM") else {
        return sql.to_string();
    };
    let Some(select_at) = up.find("SELECT") else {
        return sql.to_string();
    };
    // Anything before the SELECT — `INSERT INTO t (cols) `, `CREATE VIEW v AS `,
    // `CREATE TABLE t AS ` — is a prefix to carry through unchanged; PostgreSQL
    // accepts `INSERT INTO t (c) WITH cte AS (…) SELECT …`.
    let prefix = &sql[..select_at];
    let projection = sql[select_at + 6..from_at].trim();

    // Clauses after FROM, in Oracle order (WHERE / START WITH / CONNECT BY may
    // appear in a few orders; locate each independently).
    let tail = &sql[from_at + 4..];
    let tail_up = tail.to_ascii_uppercase();
    let sw_at = find_top_level_kw(tail, "START");
    let cb_rel = tail_up.find("CONNECT BY").unwrap();
    let where_at = find_top_level_kw(tail, "WHERE");
    let osb_at = tail_up.find("ORDER SIBLINGS BY");
    let ob_at = find_top_level_kw(tail, "ORDER").filter(|&p| Some(p) != osb_at);

    let first_clause = [sw_at, Some(cb_rel), where_at, osb_at, ob_at]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(tail.len());
    let from_table = tail[..first_clause].trim();

    let seg = |start: Option<usize>, kwlen: usize| -> Option<String> {
        let s = start?;
        let rest = &tail[s + kwlen..];
        let end = [sw_at, Some(cb_rel), where_at, osb_at, ob_at]
            .into_iter()
            .flatten()
            .filter(|&p| p > s)
            .min()
            .unwrap_or(tail.len());
        Some(
            tail[s + kwlen..s + kwlen + (end - (s + kwlen)).min(rest.len())]
                .trim()
                .to_string(),
        )
    };
    let start_with = seg(sw_at, "START WITH".len());
    let connect = seg(Some(cb_rel), "CONNECT BY".len()).unwrap_or_default();
    let connect = connect
        .trim_start_matches("NOCYCLE")
        .trim()
        .trim_start_matches("nocycle")
        .trim()
        .to_string();
    let final_where = seg(where_at, "WHERE".len());
    let siblings = seg(osb_at, "ORDER SIBLINGS BY".len());
    let order_by = seg(ob_at, "ORDER BY".len());

    // ---- DUAL / LEVEL-only generator ------------------------------------
    if from_table.eq_ignore_ascii_case("dual") {
        // CONNECT BY LEVEL <= N   (or < N)
        let cu = connect.to_ascii_uppercase();
        let n = cu
            .split("LEVEL")
            .nth(1)
            .and_then(|r| {
                r.trim()
                    .trim_start_matches(['<', '='])
                    .split_whitespace()
                    .next()
            })
            .unwrap_or("0")
            .to_string();
        let cmp = if connect.contains("<=") { "<=" } else { "<" };
        let proj = replace_ident_ci(projection, "LEVEL", "lvl");
        return format!(
            "{prefix}WITH RECURSIVE __cb(lvl) AS (SELECT 1 UNION ALL SELECT lvl + 1 FROM __cb WHERE lvl + 1 {cmp} {n}) SELECT {proj} FROM __cb"
        );
    }

    // ---- table hierarchy ---------------------------------------------------
    let (tbl, alias) = {
        let parts: Vec<&str> = from_table.split_whitespace().collect();
        match parts.as_slice() {
            [t] => (t.to_string(), "__n".to_string()),
            [t, a] => (t.to_string(), a.to_string()),
            [t, _as, a] => (t.to_string(), a.to_string()),
            _ => return sql.to_string(),
        }
    };
    // Identity column: first identifier in the CONNECT BY condition.
    let id_col = connect
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|w| !w.is_empty() && !w.eq_ignore_ascii_case("prior"))
        .unwrap_or("id")
        .to_string();

    // Rewrite the CONNECT BY condition: `PRIOR x` -> parent (`__cb.x`), bare
    // column -> child (`<child>.x`).
    let child = "__c";
    let cond = rewrite_prior(&connect, child, "__cb");

    let seed_where = start_with
        .as_deref()
        .map(|s| format!(" WHERE {}", qualify_bare_columns(s, alias.as_str())))
        .unwrap_or_default();

    // CONNECT_BY_ROOT <col> and SYS_CONNECT_BY_PATH(<col>, <sep>) accumulators.
    let mut extra_seed = String::new();
    let mut extra_step = String::new();
    let mut proj = projection.to_string();
    // CONNECT_BY_ROOT col
    while let Some(p) = proj.to_ascii_uppercase().find("CONNECT_BY_ROOT") {
        let rest_full = &proj[p + "CONNECT_BY_ROOT".len()..];
        let ws = rest_full.len() - rest_full.trim_start().len();
        let col: String = rest_full[ws..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        let holder = format!("__root_{}", col.replace('.', "_"));
        extra_seed.push_str(&format!(", {alias}.{col} AS {holder}"));
        extra_step.push_str(&format!(", __cb.{holder}"));
        let end = p + "CONNECT_BY_ROOT".len() + ws + col.len();
        proj.replace_range(p..end, &holder);
    }
    // SYS_CONNECT_BY_PATH(col, 'sep')
    while let Some(p) = proj.to_ascii_uppercase().find("SYS_CONNECT_BY_PATH") {
        let open = p + proj[p..].find('(').unwrap();
        let close = open + matching_paren(&proj[open..]).unwrap_or(0);
        let inside = &proj[open + 1..close];
        let (col, sep) = inside.split_once(',').unwrap_or((inside, "'/'"));
        let (col, sep) = (col.trim(), sep.trim());
        let holder = "__scbp";
        extra_seed.push_str(&format!(", ({sep} || {alias}.{col})::text AS {holder}"));
        extra_step.push_str(&format!(
            ", (__cb.{holder} || {sep} || {child}.{col})::text"
        ));
        proj.replace_range(p..close + 1, holder);
    }

    proj = replace_ident_ci(&proj, "LEVEL", "__level");

    // Pseudo-columns that depend on the node's children, usable in the select
    // list. `__ids` (the ancestor path) is carried on every `__cb` row.
    if proj.to_ascii_uppercase().contains("CONNECT_BY_ISCYCLE") {
        let expr = format!(
            "(CASE WHEN EXISTS (SELECT 1 FROM {tbl} {child} WHERE {cond} AND {child}.{id_col}::text = ANY(__cb.__ids)) THEN 1 ELSE 0 END)"
        );
        proj = replace_ident_ci(&proj, "CONNECT_BY_ISCYCLE", &expr);
    }
    if proj.to_ascii_uppercase().contains("CONNECT_BY_ISLEAF") {
        let expr = format!(
            "(CASE WHEN EXISTS (SELECT 1 FROM {tbl} {child} WHERE {cond}) THEN 0 ELSE 1 END)"
        );
        proj = replace_ident_ci(&proj, "CONNECT_BY_ISLEAF", &expr);
    }

    // The `__ids` ancestor path and the `__sib` ordering path are accumulator
    // arrays. PostgreSQL requires the non-recursive and recursive arms of a
    // `WITH RECURSIVE` union to have *identical* column types, and an array
    // literal keeps the element's typmod (`ARRAY[empno]` is `numeric(4,0)[]`
    // when `empno` is `NUMBER(4)`) while `array || elem` in the recursive arm
    // yields the typmod-stripped `numeric[]`. Normalise every element to `text`
    // in both arms so the types line up regardless of the key column's declared
    // type; `__ids` is only ever compared for membership (cycle detection), and
    // `__sib` ordering already cast to text on the recursive side.
    let sib_seed = siblings
        .as_deref()
        .map(|s| format!(", ARRAY[{alias}.{}::text] AS __sib", s.trim()))
        .unwrap_or_else(|| ", ARRAY[]::text[] AS __sib".to_string());
    let sib_step = siblings
        .as_deref()
        .map(|s| format!(", __cb.__sib || {child}.{}::text", s.trim()))
        .unwrap_or_else(|| ", __cb.__sib".to_string());

    let mut out = format!(
        "{prefix}WITH RECURSIVE __cb AS (\
           SELECT {alias}.*, 1 AS __level, ARRAY[{alias}.{id_col}::text] AS __ids{extra_seed}{sib_seed} \
           FROM {tbl} {alias}{seed_where} \
           UNION ALL \
           SELECT {child}.*, __cb.__level + 1, __cb.__ids || {child}.{id_col}::text{extra_step}{sib_step} \
           FROM {tbl} {child} JOIN __cb ON {cond} \
           WHERE NOT {child}.{id_col}::text = ANY(__cb.__ids)\
         ) SELECT {proj} FROM __cb"
    );

    let mut wheres = Vec::new();
    if let Some(fw) = &final_where {
        let fw = fw.to_ascii_uppercase();
        if fw.contains("CONNECT_BY_ISLEAF") {
            // isleaf = 1  ->  no row has this node as its parent
            let leaf = format!(
                "NOT EXISTS (SELECT 1 FROM {tbl} {child} WHERE {})",
                rewrite_prior(&connect, child, "__cb")
            );
            wheres.push(leaf);
        } else {
            wheres.push(final_where.clone().unwrap());
        }
    }
    if !wheres.is_empty() {
        out.push_str(&format!(" WHERE {}", wheres.join(" AND ")));
    }
    if siblings.is_some() {
        out.push_str(" ORDER BY __sib");
    } else if let Some(o) = order_by {
        out.push_str(&format!(" ORDER BY {o}"));
    }
    out
}

/// Normalise Oracle row-locking clauses to their PostgreSQL forms.
/// `FOR UPDATE OF <cols>` drops the column list (PostgreSQL's `OF` names a
/// table, not a column, and an unqualified lock is the safe superset).
/// `WAIT <n>` has no PostgreSQL equivalent and becomes a plain blocking wait.
/// `NOWAIT` and `SKIP LOCKED` are already PostgreSQL syntax and pass through.
fn rewrite_for_update(sql: &str) -> String {
    let Some(fu_at) = find_top_level_kw(sql, "FOR UPDATE") else {
        return sql.to_string();
    };
    let (head, rest) = sql.split_at(fu_at);
    let after = rest["FOR UPDATE".len()..].trim_start();

    // Optional `OF a, b, c` — consume identifiers/commas/dots until a known
    // trailing keyword or end.
    let after = if let Some(of_rest) = strip_kw(after, "OF") {
        let mut end = 0;
        let bytes = of_rest.as_bytes();
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b',' || c == b' ' {
                // stop if we reach NOWAIT / WAIT / SKIP
                let tailu = of_rest[end..].trim_start().to_ascii_uppercase();
                if tailu.starts_with("NOWAIT")
                    || tailu.starts_with("WAIT ")
                    || tailu.starts_with("SKIP LOCKED")
                {
                    break;
                }
                end += 1;
            } else {
                break;
            }
        }
        of_rest[end..].trim_start()
    } else {
        after
    };

    // `WAIT <n>` -> nothing (block, like the default).
    let after = if let Some(w_rest) = strip_kw(after, "WAIT") {
        w_rest
            .trim_start()
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start()
    } else {
        after
    };

    let tail = after.trim();
    if tail.is_empty() {
        format!("{}FOR UPDATE", head)
    } else {
        format!("{}FOR UPDATE {}", head, tail)
    }
}

/// The trailing table reference of a `... FROM <src>` fragment, as
/// `(source_sql, correlation_name)`. The correlation name is the explicit
/// alias, else the last dotted segment of the table name.
fn trailing_table_ref(head: &str) -> Option<(String, String)> {
    let from_at = find_top_level_kw_last(head, "FROM")?;
    let src = head[from_at + "FROM".len()..].trim().trim_end_matches(',');
    if src.is_empty() {
        return None;
    }
    let toks: Vec<&str> = src.split_whitespace().collect();
    let (table, alias) = match toks.as_slice() {
        [t] => (*t, t.rsplit('.').next().unwrap_or(t)),
        [t, a] => (*t, *a),
        [t, kw, a] if kw.eq_ignore_ascii_case("as") => (*t, *a),
        _ => return None,
    };
    Some((table.to_string(), alias.trim_matches('"').to_string()))
}

/// Last top-level occurrence of a keyword (mirror of `find_top_level_kw`).
fn find_top_level_kw_last(s: &str, kw: &str) -> Option<usize> {
    let mut last = None;
    let mut base = 0usize;
    while let Some(rel) = find_top_level_kw(&s[base..], kw) {
        last = Some(base + rel);
        base += rel + kw.len();
    }
    last
}

/// One `col [AS] [label]` entry of a PIVOT/UNPIVOT `IN (...)` list.
fn pivot_in_item(entry: &str) -> Option<(String, String)> {
    let entry = entry.trim();
    let (col, label) = match find_top_level_kw(entry, "AS") {
        Some(at) => (
            entry[..at].trim(),
            entry[at + "AS".len()..].trim().to_string(),
        ),
        None => {
            // `col` alone, or `'literal'` alone (label defaults to the token)
            (entry, entry.to_string())
        }
    };
    if col.is_empty() {
        return None;
    }
    // Normalise the label to a single-quoted string literal.
    let label = if label.starts_with('\'') {
        label
    } else if label.starts_with('"') {
        format!("'{}'", label.trim_matches('"').replace('\'', "''"))
    } else {
        format!("'{}'", label.replace('\'', "''"))
    };
    Some((col.to_string(), label))
}

/// Oracle `UNPIVOT` → `CROSS JOIN LATERAL (VALUES ...)`. Handles one UNPIVOT
/// clause of the form
/// `FROM <src> UNPIVOT [INCLUDE|EXCLUDE NULLS] (<val> FOR <name> IN (<items>))`.
/// MariaDB has no `UNPIVOT`, `LATERAL`, or `VALUES` table constructor. Lower
/// `SELECT <proj> FROM <t> UNPIVOT (<v> FOR <k> IN (<c1> AS '<n1>', …)) <tail>`
/// to a `UNION ALL` derived table, one branch per unpivoted column.
fn rewrite_unpivot_mariadb(sql: &str) -> String {
    let Some(kw_at) = find_top_level_kw(sql, "UNPIVOT") else {
        return sql.to_string();
    };
    let head = &sql[..kw_at];
    let mut rest = sql[kw_at + "UNPIVOT".len()..].trim_start();
    let mut include_nulls = false;
    if let Some(r) = strip_kw(rest, "INCLUDE NULLS") {
        include_nulls = true;
        rest = r;
    } else if let Some(r) = strip_kw(rest, "EXCLUDE NULLS") {
        rest = r;
    }
    if !rest.starts_with('(') {
        return sql.to_string();
    }
    let Some(close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let inner = &rest[1..close];
    let tail = rest[close + 1..].trim();

    let Some(for_at) = find_top_level_kw(inner, "FOR") else {
        return sql.to_string();
    };
    let val_col = inner[..for_at].trim();
    let after_for = inner[for_at + "FOR".len()..].trim_start();
    let Some(in_at) = find_top_level_kw(after_for, "IN") else {
        return sql.to_string();
    };
    let key_col = after_for[..in_at].trim();
    let in_part = after_for[in_at + "IN".len()..].trim_start();
    if !in_part.starts_with('(') {
        return sql.to_string();
    }
    let Some(in_close) = matching_paren(in_part) else {
        return sql.to_string();
    };
    let items = split_top_level_commas(&in_part[1..in_close]);

    // Split `head` into `SELECT <proj> FROM <source>`.
    let head_t = head.trim_start();
    let Some(after_select) = head_t
        .strip_prefix("SELECT ")
        .or_else(|| head_t.strip_prefix("select "))
    else {
        return sql.to_string();
    };
    let Some(from_rel) = find_top_level_kw(after_select, "FROM") else {
        return sql.to_string();
    };
    let projection = after_select[..from_rel].trim();
    let source = after_select[from_rel + "FROM".len()..].trim();
    // Identity columns: the projection entries that are not the key/value cols.
    let identity: Vec<&str> = split_top_level_commas(projection)
        .into_iter()
        .map(str::trim)
        .filter(|c| {
            !c.eq_ignore_ascii_case(key_col)
                && !c.eq_ignore_ascii_case(val_col)
                && !c.eq_ignore_ascii_case("*")
        })
        .collect();

    let mut branches = Vec::new();
    for it in items {
        let Some((col, label)) = pivot_in_item(it) else {
            return sql.to_string();
        };
        let ident = if identity.is_empty() {
            String::new()
        } else {
            format!("{}, ", identity.join(", "))
        };
        branches.push(format!(
            "SELECT {ident}{label} AS {key_col}, {col} AS {val_col} FROM {source}"
        ));
    }
    if branches.is_empty() {
        return sql.to_string();
    }
    let derived = format!("({}) __unpiv", branches.join(" UNION ALL "));
    let padded_tail = format!(" {tail}");
    let where_at = find_top_level_kw(&padded_tail, "WHERE");
    let out = if include_nulls {
        format!("SELECT {projection} FROM {derived} {tail}")
            .trim_end()
            .to_string()
    } else if let Some(w_at) = where_at {
        let after = padded_tail[w_at + "WHERE".len()..].trim();
        let rest_tail = after; // WHERE <after>
        format!("SELECT {projection} FROM {derived} WHERE {val_col} IS NOT NULL AND {rest_tail}")
    } else {
        format!("SELECT {projection} FROM {derived} WHERE {val_col} IS NOT NULL {tail}")
            .trim_end()
            .to_string()
    };
    rewrite_unpivot_mariadb(&out)
}

fn rewrite_unpivot(sql: &str) -> String {
    let Some(kw_at) = find_top_level_kw(sql, "UNPIVOT") else {
        return sql.to_string();
    };
    let head = &sql[..kw_at];
    let mut rest = sql[kw_at + "UNPIVOT".len()..].trim_start();

    let mut include_nulls = false;
    if let Some(r) = strip_kw(rest, "INCLUDE NULLS") {
        include_nulls = true;
        rest = r;
    } else if let Some(r) = strip_kw(rest, "EXCLUDE NULLS") {
        rest = r;
    }
    if !rest.starts_with('(') {
        return sql.to_string();
    }
    let Some(close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let inner = &rest[1..close];
    let tail = &rest[close + 1..];

    let Some(for_at) = find_top_level_kw(inner, "FOR") else {
        return sql.to_string();
    };
    let val_col = inner[..for_at].trim();
    let after_for = inner[for_at + "FOR".len()..].trim_start();
    let Some(in_at) = find_top_level_kw(after_for, "IN") else {
        return sql.to_string();
    };
    let name_col = after_for[..in_at].trim();
    let in_part = after_for[in_at + "IN".len()..].trim_start();
    if !in_part.starts_with('(') {
        return sql.to_string();
    }
    let Some(in_close) = matching_paren(in_part) else {
        return sql.to_string();
    };
    let items = split_top_level_commas(&in_part[1..in_close]);

    let Some((_, alias)) = trailing_table_ref(head) else {
        return sql.to_string();
    };

    let mut rows = Vec::new();
    for it in items {
        let Some((col, label)) = pivot_in_item(it) else {
            return sql.to_string();
        };
        rows.push(format!("({label}, {alias}.{col})"));
    }
    if rows.is_empty() {
        return sql.to_string();
    }

    let base = format!(
        "{} CROSS JOIN LATERAL (VALUES {}) AS __unpiv({name_col}, {val_col})",
        head.trim_end(),
        rows.join(", ")
    );
    let tail = tail.trim();
    let out = if include_nulls || tail.is_empty() {
        format!("{base} {tail}")
    } else {
        let cond = format!("__unpiv.{val_col} IS NOT NULL");
        // Pad so `find_top_level_kw` (which needs a preceding boundary char)
        // still matches a `WHERE` that began the tail.
        let padded = format!(" {tail}");
        if let Some(w_at) = find_top_level_kw(&padded, "WHERE") {
            let (before, after) = padded.split_at(w_at + "WHERE".len());
            format!("{base} {} {cond} AND {}", before.trim(), after.trim())
        } else {
            // No WHERE: the injected filter precedes any GROUP BY / ORDER BY / …
            format!("{base} WHERE {cond} {tail}")
        }
    };
    // A query may carry more than one UNPIVOT.
    rewrite_unpivot(out.trim_end())
}

/// Oracle `PIVOT` → conditional aggregation. Only the form
/// `FROM (SELECT <explicit cols> ...) [alias] PIVOT (<agg>(<measure>) FOR <col>
/// IN (<value list>))` is handled: the GROUP BY columns are the inner SELECT
/// list minus the measure and the FOR column. Other shapes are left for
/// PostgreSQL to reject (documented limitation).
fn rewrite_pivot(sql: &str) -> String {
    let Some(kw_at) = find_top_level_kw(sql, "PIVOT") else {
        return sql.to_string();
    };
    let head = &sql[..kw_at];
    let rest = sql[kw_at + "PIVOT".len()..].trim_start();
    if !rest.starts_with('(') {
        return sql.to_string();
    }
    let Some(close) = matching_paren(rest) else {
        return sql.to_string();
    };
    let inner = &rest[1..close];
    let tail = &rest[close + 1..];

    let Some(for_at) = find_top_level_kw(inner, "FOR") else {
        return sql.to_string();
    };
    let agg_part = inner[..for_at].trim(); // `SUM(amt)` or `SUM(amt) AS s`
    let after_for = inner[for_at + "FOR".len()..].trim_start();
    let Some(in_at) = find_top_level_kw(after_for, "IN") else {
        return sql.to_string();
    };
    let for_col = after_for[..in_at].trim();
    let in_part = after_for[in_at + "IN".len()..].trim_start();
    if !in_part.starts_with('(') {
        return sql.to_string();
    }
    let Some(in_close) = matching_paren(in_part) else {
        return sql.to_string();
    };
    let items = split_top_level_commas(&in_part[1..in_close]);

    // Split the aggregate into function name + measure expression.
    let (agg_expr, _agg_alias) = match find_top_level_kw(agg_part, "AS") {
        Some(a) => (agg_part[..a].trim(), Some(agg_part[a + 2..].trim())),
        None => (agg_part, None),
    };
    let (fname, margs) = {
        let p = agg_expr.find('(');
        let q = agg_expr.rfind(')');
        match (p, q) {
            (Some(p), Some(q)) if q > p => (agg_expr[..p].trim(), agg_expr[p + 1..q].trim()),
            _ => return sql.to_string(),
        }
    };

    // Need `FROM ( SELECT <cols> ... ) [alias]` to know the GROUP BY set.
    let from_at = find_top_level_kw_last(head, "FROM").map(|a| a + "FROM".len());
    let Some(fstart) = from_at else {
        return sql.to_string();
    };
    let src = head[fstart..].trim_start();
    if !src.starts_with('(') {
        return sql.to_string();
    }
    let Some(src_close) = matching_paren(src) else {
        return sql.to_string();
    };
    let subquery = &src[..=src_close];
    let after_src = src[src_close + 1..].trim();
    let after_src = after_src
        .strip_prefix("AS ")
        .or_else(|| after_src.strip_prefix("as "))
        .unwrap_or(after_src);
    let first_word = after_src.split_whitespace().next().unwrap_or("");
    // `PIVOT` right after `)` means there was no correlation name.
    let src_alias = if first_word.eq_ignore_ascii_case("pivot") {
        ""
    } else {
        first_word
    };

    let inner_sel = {
        let s = format!(
            " {}",
            subquery.trim_start_matches('(').trim_end_matches(')')
        );
        let Some(sel_at) = find_top_level_kw(&s, "SELECT") else {
            return sql.to_string();
        };
        let after = s[sel_at + "SELECT".len()..].to_string();
        let Some(from_kw) = find_top_level_kw(&after, "FROM") else {
            return sql.to_string();
        };
        after[..from_kw].to_string()
    };
    let group_cols: Vec<String> = split_top_level_commas(&inner_sel)
        .into_iter()
        .map(|c| {
            c.trim()
                .rsplit([' ', '.'])
                .next()
                .unwrap_or(c)
                .trim()
                .to_string()
        })
        .filter(|c| {
            !c.eq_ignore_ascii_case(margs) && !c.eq_ignore_ascii_case(for_col) && !c.is_empty()
        })
        .collect();

    let mut proj = group_cols.clone();
    for it in items {
        let Some((val, out_alias)) = pivot_in_item(it) else {
            return sql.to_string();
        };
        // Oracle names the output column after the pivot-value alias as an
        // identifier; render it double-quoted so any literal survives.
        let bare = out_alias.trim_matches('\'');
        let is_ident = !bare.is_empty()
            && !bare.as_bytes()[0].is_ascii_digit()
            && bare.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        let out_alias = if is_ident && !out_alias.starts_with('\'') {
            out_alias
        } else {
            format!("\"{}\"", bare.replace('"', "\"\""))
        };
        proj.push(format!(
            "{fname}(CASE WHEN {for_col} = {val} THEN {margs} END) AS {out_alias}"
        ));
    }

    // Replace the whole `SELECT ... FROM (subquery) [alias] PIVOT(...)` with the
    // conditional-aggregation form. The original outer projection is discarded
    // (Oracle's is fixed: grouping columns then one column per pivot value).
    let alias_ref = if src_alias.is_empty() {
        String::new()
    } else {
        format!(" {src_alias}")
    };
    let group_by = if group_cols.is_empty() {
        String::new()
    } else {
        format!(" GROUP BY {}", group_cols.join(", "))
    };
    format!(
        "SELECT {} FROM {subquery}{alias_ref}{group_by}{tail}",
        proj.join(", ")
    )
}

/// Replace `PRIOR <col>` with `<parent>.<col>` and bare `<col>` with
/// `<child>.<col>` inside a CONNECT BY condition (identifiers only).
fn rewrite_prior(cond: &str, child: &str, parent: &str) -> String {
    let mut out = String::new();
    let bytes = cond.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &cond[start..i];
            if word.eq_ignore_ascii_case("prior") {
                while i < bytes.len() && bytes[i] == b' ' {
                    i += 1;
                }
                let col_start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                out.push_str(&format!("{parent}.{}", &cond[col_start..i]));
            } else if word.eq_ignore_ascii_case("and")
                || word.eq_ignore_ascii_case("or")
                || word.eq_ignore_ascii_case("is")
                || word.eq_ignore_ascii_case("null")
                || word.eq_ignore_ascii_case("not")
            {
                out.push_str(word);
            } else if cond[i..].trim_start().starts_with('.') {
                out.push_str(word); // already qualified
            } else {
                out.push_str(&format!("{child}.{word}"));
            }
        } else {
            out.push(cond[i..].chars().next().unwrap());
            i += 1;
        }
    }
    out
}

fn qualify_bare_columns(cond: &str, alias: &str) -> String {
    rewrite_prior(cond, alias, alias)
}

/// Case-insensitive whole-identifier replacement.
fn replace_ident_ci(s: &str, ident: &str, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        if let Some(q) = quote {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            if ch.len_utf8() == 1 && bytes[i] == q {
                if bytes.get(i + 1) == Some(&q) {
                    out.push(q as char);
                    i += 1;
                } else {
                    quote = None;
                }
            }
            i += ch.len_utf8();
            continue;
        }
        if matches!(bytes[i], b'\'' | b'"' | b'`') {
            quote = Some(bytes[i]);
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let w = &s[start..i];
            if w.eq_ignore_ascii_case(ident) && bytes.get(start.wrapping_sub(1)) != Some(&b'.') {
                out.push_str(with);
            } else {
                out.push_str(w);
            }
        } else {
            let c = s[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// `<agg>(<e>) KEEP (DENSE_RANK FIRST|LAST ORDER BY <cols>)` -> pick `<e>` from
/// the first/last row by `<cols>`: `(array_agg(<e> ORDER BY <cols> [DESC]))[1]`.
fn rewrite_keep_aggregates(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let end = skip_quoted(sql, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = &sql[start..i];
            let after_name = &sql[i..];
            if after_name.starts_with('(')
                && let Some(rewrite) = try_rewrite_keep(name, after_name)
            {
                out.push_str(&rewrite.0);
                i += rewrite.1;
                continue;
            }
            out.push_str(name);
            continue;
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `after` starts at the `(` following the aggregate name. Returns
/// `(replacement, bytes_consumed_from_after)` when a `KEEP` clause follows.
fn try_rewrite_keep(_name: &str, after: &str) -> Option<(String, usize)> {
    let args_end = matching_paren(after)?;
    let args = &after[1..args_end];
    let tail = after[args_end + 1..].trim_start();
    let tail_off = args_end + 1 + (after[args_end + 1..].len() - tail.len());
    if !tail.to_ascii_uppercase().starts_with("KEEP") {
        return None;
    }
    let after_keep = tail[4..].trim_start();
    if !after_keep.starts_with('(') {
        return None;
    }
    let keep_open = tail_off + (tail.len() - after_keep.len());
    let keep_end = matching_paren(&after[keep_open..])? + keep_open;
    let inside = after[keep_open + 1..keep_end].trim();
    let upper = inside.to_ascii_uppercase();
    let last = upper.contains("LAST");
    let order_at = upper.find("ORDER BY")?;
    let cols = inside[order_at + "ORDER BY".len()..].trim();
    let direction = if last { " DESC" } else { "" };
    Some((
        format!("(array_agg({args} ORDER BY {cols}{direction}))[1]"),
        keep_end + 1,
    ))
}

/// Oracle DDL spellings PostgreSQL does not accept:
///   ALTER TABLE t ADD (c type, ...)       -> ALTER TABLE t ADD COLUMN c type, ...
///   ALTER TABLE t MODIFY (c type)         -> ALTER TABLE t ALTER COLUMN c TYPE type
///   ALTER TABLE t DROP (c, ...)            -> ALTER TABLE t DROP COLUMN c, ...
///   ALTER TABLE t SET UNUSED (c, ...)      -> ALTER TABLE t DROP COLUMN c, ...
///   ... DEFAULT ON NULL <expr> ...         -> ... DEFAULT <expr> ...
fn rewrite_oracle_ddl(sql: &str) -> String {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_ascii_uppercase();
    let mut out = sql.to_string();

    // Oracle's `GENERATED ALWAYS AS (expr) VIRTUAL` (computed on read) has a
    // native PostgreSQL equivalent only in 18+. `STORED` (12+) materialises the
    // value instead, which is transparent to every read the proxy serves and
    // keeps the translation portable across the supported PostgreSQL majors.
    if upper.starts_with("CREATE ") && upper.contains(" GENERATED ") {
        out = replace_ident_ci(&out, "VIRTUAL", "STORED");
    }

    // Oracle SYNONYM has no PostgreSQL primitive; a view over the target object
    // gives the same name-indirection for reads and writable single-table views
    // for DML. `PUBLIC` is dropped (the object lands in the caller's schema /
    // search_path, which is the closest analogue without a shared namespace).
    if let Some(rest) = strip_kw(trimmed, "CREATE") {
        let (or_replace, rest) = match strip_kw(rest, "OR REPLACE") {
            Some(r) => ("OR REPLACE ", r),
            None => ("", rest),
        };
        let rest = strip_kw(rest, "PUBLIC").unwrap_or(rest);
        if let Some(rest) = strip_kw(rest, "SYNONYM")
            && let Some(for_at) = find_top_level_kw(rest, "FOR")
        {
            let name = rest[..for_at].trim();
            let target = rest[for_at + "FOR".len()..].trim().trim_end_matches(';');
            if !name.is_empty() && !target.is_empty() {
                return format!("CREATE {or_replace}VIEW {name} AS SELECT * FROM {target}");
            }
        }
    }
    if let Some(rest) = strip_kw(trimmed, "DROP SYNONYM") {
        let rest = strip_kw(rest, "PUBLIC").unwrap_or(rest);
        let name = rest.trim().trim_end_matches(';');
        if !name.is_empty() {
            return format!("DROP VIEW IF EXISTS {name}");
        }
    }

    // Oracle global/private temporary tables have a permanent, shared
    // definition with session-private rows. PostgreSQL temp tables are
    // session-local in both definition and data, so re-running the DDL in a
    // second session must not error: emit `IF NOT EXISTS`. `ON COMMIT
    // {DELETE|PRESERVE} ROWS` is valid PostgreSQL and kept verbatim.
    for marker in [
        "CREATE GLOBAL TEMPORARY TABLE ",
        "CREATE PRIVATE TEMPORARY TABLE ",
    ] {
        if upper.starts_with(marker) {
            let tail = &trimmed[marker.len()..];
            let tail = tail
                .strip_prefix("IF NOT EXISTS ")
                .or_else(|| tail.strip_prefix("if not exists "))
                .unwrap_or(tail);
            return strip_oracle_physical_clauses(&format!(
                "CREATE TEMPORARY TABLE IF NOT EXISTS {tail}"
            ));
        }
    }

    // PostgreSQL's materialized-view primitive is compatible with Oracle's
    // query result, but not its BUILD/REFRESH policy clauses. The proxy does
    // not implement Oracle refresh jobs: retain the object name and defining
    // SELECT, while explicit REFRESH MATERIALIZED VIEW remains PostgreSQL's
    // normal operation.
    const MV_PREFIX: &str = "CREATE MATERIALIZED VIEW";
    if upper.starts_with(MV_PREFIX)
        && upper
            .as_bytes()
            .get(MV_PREFIX.len())
            .is_none_or(u8::is_ascii_whitespace)
    {
        let rest = trimmed[MV_PREFIX.len()..].trim_start();
        if let (Some(name), Some(as_at)) = (
            rest.split_whitespace().next(),
            find_top_level_kw(rest, "AS"),
        ) {
            let select = &rest[as_at + "AS".len()..];
            return format!("{MV_PREFIX} {name} AS{select}");
        }
    }

    // Function-based / expression indexes: PostgreSQL requires each non-trivial
    // index key to be parenthesised (`ON t ((a + b))`), where Oracle takes a
    // bare expression. Wrap every key that is not a plain column reference.
    if (upper.starts_with("CREATE INDEX ")
        || upper.starts_with("CREATE UNIQUE INDEX ")
        || upper.starts_with("CREATE BITMAP INDEX ")
        || upper.starts_with("CREATE UNIQUE BITMAP INDEX "))
        && let Some(on_at) = find_top_level_kw(trimmed, "ON")
    {
        let after_on = &trimmed[on_at + "ON".len()..];
        if let Some(open_rel) = after_on.find('(') {
            let open = on_at + "ON".len() + open_rel;
            if let Some(close_off) = matching_paren(&out[open..]) {
                let close = open + close_off;
                let keys = split_top_level_commas(&out[open + 1..close]);
                let wrapped = keys
                    .iter()
                    .map(|k| {
                        let k = k.trim();
                        let is_plain_ident = !k.is_empty()
                            && k.chars()
                                .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '"');
                        let already_wrapped =
                            k.starts_with('(') && matching_paren(k) == Some(k.len() - 1);
                        // `col ASC` / `col DESC` stays as-is (valid in PG too).
                        let is_sorted_ident = {
                            let mut parts = k.split_whitespace();
                            let first = parts.next().unwrap_or("");
                            let second = parts.next().unwrap_or("");
                            parts.next().is_none()
                                && first.chars().all(|c| {
                                    c.is_alphanumeric() || c == '_' || c == '.' || c == '"'
                                })
                                && matches!(second.to_ascii_uppercase().as_str(), "ASC" | "DESC")
                        };
                        if is_plain_ident || already_wrapped || is_sorted_ident {
                            k.to_string()
                        } else {
                            format!("({k})")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                out = format!("{}({wrapped}){}", &out[..open], &out[close + 1..]);
            }
        }
        return strip_oracle_physical_clauses(&out);
    }

    if upper.starts_with("ALTER TABLE ") {
        if let Some(add_at) = upper.find(" ADD (") {
            let head = &out[..add_at];
            let inner_start = add_at + " ADD (".len();
            if let Some(close) = out[inner_start..].rfind(')') {
                let cols = &out[inner_start..inner_start + close];
                let rebuilt = split_top_level_commas(cols)
                    .into_iter()
                    .map(|c| format!("ADD COLUMN {}", c.trim()))
                    .collect::<Vec<_>>()
                    .join(", ");
                return format!("{head} {rebuilt}{}", &out[inner_start + close + 1..]);
            }
        }
        if let Some(mod_at) = upper.find(" MODIFY (") {
            let head = &out[..mod_at];
            let inner_start = mod_at + " MODIFY (".len();
            if let Some(close) = out[inner_start..].rfind(')') {
                let rebuilt = split_top_level_commas(&out[inner_start..inner_start + close])
                    .into_iter()
                    .filter_map(rewrite_modify_column)
                    .collect::<Vec<_>>()
                    .join(", ");
                if !rebuilt.is_empty() {
                    return format!("{head} {rebuilt}{}", &out[inner_start + close + 1..]);
                }
            }
        }
        for (oracle, pg) in [
            (" DROP (", "DROP COLUMN "),
            (" SET UNUSED (", "DROP COLUMN "),
        ] {
            if let Some(at) = upper.find(oracle) {
                let head = &out[..at];
                let inner_start = at + oracle.len();
                if let Some(close) = out[inner_start..].rfind(')') {
                    let rebuilt = split_top_level_commas(&out[inner_start..inner_start + close])
                        .into_iter()
                        .map(|col| format!("{pg}{}", col.trim()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return format!("{head} {rebuilt}{}", &out[inner_start + close + 1..]);
                }
            }
        }
    }

    // `DEFAULT ON NULL x` -> `DEFAULT x` (the "substitute default for an
    // explicit NULL" behaviour is not emulated).
    while let Some(pos) = out.to_ascii_uppercase().find("DEFAULT ON NULL ") {
        out.replace_range(pos..pos + "DEFAULT ON NULL ".len(), "DEFAULT ");
    }
    strip_oracle_physical_clauses(&out)
}

/// Translate one entry in Oracle's parenthesised `MODIFY` list.  PostgreSQL
/// has separate operations for type, default, and nullability; keeping them as
/// comma-separated ALTER TABLE actions preserves an atomic ALTER statement.
fn rewrite_modify_column(entry: &str) -> Option<String> {
    let entry = entry.trim();
    let (column, specification) = entry.split_once(char::is_whitespace)?;
    let specification = specification.trim();
    let upper = specification.to_ascii_uppercase();
    if upper == "NULL" {
        return Some(format!("ALTER COLUMN {column} DROP NOT NULL"));
    }
    if upper == "NOT NULL" {
        return Some(format!("ALTER COLUMN {column} SET NOT NULL"));
    }
    if upper.starts_with("DEFAULT ") {
        let default = specification["DEFAULT ".len()..].trim();
        return Some(if default.eq_ignore_ascii_case("NULL") {
            format!("ALTER COLUMN {column} DROP DEFAULT")
        } else {
            format!("ALTER COLUMN {column} SET DEFAULT {default}")
        });
    }
    Some(format!("ALTER COLUMN {column} TYPE {specification}"))
}

/// Remove storage-tuning clauses which have no PostgreSQL equivalent.  This is
/// deliberately a small lexical pass: it only runs for DDL and never examines
/// quoted strings, so a column comment/default mentioning e.g. TABLESPACE is
/// left untouched.
fn strip_oracle_physical_clauses(sql: &str) -> String {
    let upper = sql.trim_start().to_ascii_uppercase();
    if !(upper.starts_with("CREATE ")
        || upper.starts_with("ALTER ")
        || upper.starts_with("CREATE INDEX"))
    {
        return sql.to_string();
    }
    // PostgreSQL has no bitmap index access method.  A normal B-tree index is
    // a useful, executable compatibility fallback; it intentionally does not
    // promise Oracle's bitmap concurrency/performance characteristics.
    let sql = replace_ddl_prefix(sql, "CREATE BITMAP INDEX", "CREATE INDEX");
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < bytes.len() {
        if matches!(bytes[i], b'\'' | b'\"') {
            let end = skip_quoted(&sql, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        let tail = &sql[i..];
        let tail_upper = tail.to_ascii_uppercase();
        let mut consumed = None;
        if depth == 0 {
            for keyword in ["TABLESPACE", "PCTFREE", "INITRANS", "MAXTRANS", "PARALLEL"] {
                if starts_keyword(&tail_upper, keyword) {
                    consumed = Some(skip_clause_value(tail, keyword.len()));
                    break;
                }
            }
            if consumed.is_none() {
                for keyword in [
                    "LOGGING",
                    "NOLOGGING",
                    "NOPARALLEL",
                    "CACHE",
                    "NOCACHE",
                    "COMPUTE STATISTICS",
                ] {
                    if starts_keyword(&tail_upper, keyword) {
                        consumed = Some(keyword.len());
                        break;
                    }
                }
            }
            if consumed.is_none() && starts_keyword(&tail_upper, "ENABLE ROW MOVEMENT") {
                consumed = Some("ENABLE ROW MOVEMENT".len());
            }
            if consumed.is_none() && starts_keyword(&tail_upper, "SEGMENT CREATION") {
                consumed = Some(skip_clause_value(tail, "SEGMENT CREATION".len()));
            }
            if consumed.is_none() && starts_keyword(&tail_upper, "STORAGE") {
                let after = tail["STORAGE".len()..].trim_start();
                if after.starts_with('(') {
                    consumed = matching_paren(after).map(|end| tail.len() - after.len() + end + 1);
                }
            }
            if consumed.is_none() && starts_keyword(&tail_upper, "LOB") {
                // `LOB (column) STORE AS (storage attributes)` is a physical
                // placement directive only.  CLOB/BLOB already became TEXT/BYTEA.
                let after_lob = tail["LOB".len()..].trim_start();
                if after_lob.starts_with('(')
                    && let Some(cols_end) = matching_paren(after_lob)
                {
                    let after_cols = &after_lob[cols_end + 1..];
                    let after_store = after_cols.trim_start();
                    if after_store.to_ascii_uppercase().starts_with("STORE AS") {
                        let after_as = after_store["STORE AS".len()..].trim_start();
                        if after_as.starts_with('(')
                            && let Some(attrs_end) = matching_paren(after_as)
                        {
                            consumed = Some(tail.len() - after_as.len() + attrs_end + 1);
                        }
                    }
                }
            }
        }
        // Constraint-state suffix — `... NOT NULL ENABLE`, `PRIMARY KEY DISABLE`,
        // `CHECK (...) ENABLE VALIDATE`, `DISABLE NOVALIDATE`. Legal anywhere in
        // the column / constraint list (any paren depth); PostgreSQL has no such
        // keyword and rejects it as a syntax error. `ENABLE/DISABLE ROW
        // MOVEMENT` is a table-level clause handled above.
        // Only at a token boundary — never when `ENABLE`/`DISABLE` is the tail
        // of an identifier like `ddl_nn_enable`.
        let at_boundary = out
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        if consumed.is_none() && at_boundary {
            for state in ["ENABLE", "DISABLE"] {
                let after_kw = tail
                    .get(state.len()..)
                    .map(|t| t.trim_start().to_ascii_uppercase())
                    .unwrap_or_default();
                // `ALTER TABLE ... ENABLE/DISABLE CONSTRAINT|TRIGGER|ALL ...`
                // and `... ROW MOVEMENT` are real statements, not a
                // constraint-state suffix — leave them alone.
                let is_table_level = ["CONSTRAINT", "TRIGGER", "ALL", "ROW MOVEMENT"]
                    .iter()
                    .any(|k| starts_keyword(&after_kw, k));
                if starts_keyword(&tail_upper, state) && !is_table_level {
                    let mut n = state.len();
                    let after = tail[n..].trim_start();
                    let after_upper = after.to_ascii_uppercase();
                    for follow in ["VALIDATE", "NOVALIDATE", "RELY", "NORELY"] {
                        if starts_keyword(&after_upper, follow) {
                            n = tail.len() - after.len() + follow.len();
                            break;
                        }
                    }
                    consumed = Some(n);
                    break;
                }
            }
        }
        if let Some(n) = consumed {
            i += n;
            continue;
        }
        let ch = tail.chars().next().expect("slice is nonempty");
        out.push(ch);
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
        }
        i += ch.len_utf8();
    }
    out
}

fn replace_ddl_prefix(sql: &str, oracle: &str, pg: &str) -> String {
    let leading = sql.len() - sql.trim_start().len();
    let tail = &sql[leading..];
    if tail.to_ascii_uppercase().starts_with(oracle)
        && tail
            .as_bytes()
            .get(oracle.len())
            .is_none_or(|b| b.is_ascii_whitespace())
    {
        format!("{}{}{}", &sql[..leading], pg, &tail[oracle.len()..])
    } else {
        sql.to_string()
    }
}

fn starts_keyword(tail_upper: &str, keyword: &str) -> bool {
    tail_upper.starts_with(keyword)
        && tail_upper
            .as_bytes()
            .get(keyword.len())
            .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
}

fn skip_clause_value(tail: &str, keyword_len: usize) -> usize {
    let rest = &tail[keyword_len..];
    let ws = rest.len() - rest.trim_start().len();
    let value = &rest[ws..];
    let end = value
        .find(|c: char| c.is_whitespace() || matches!(c, ',' | ')'))
        .unwrap_or(value.len());
    keyword_len + ws + end
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quoted = 0u8;
    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' | b'"' if quoted == 0 => quoted = b,
            b if quoted != 0 && b == quoted => quoted = 0,
            b'(' if quoted == 0 => depth += 1,
            b')' if quoted == 0 => depth -= 1,
            b',' if quoted == 0 && depth == 0 => {
                parts.push(&s[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Oracle permits a `DELETE WHERE` after the update portion of a matched MERGE
/// clause.  PostgreSQL has the same primitives, but requires the delete to be
/// a separate (and preceding) `WHEN MATCHED` clause.  The delete predicate is
/// evaluated *after* Oracle has applied the SET list, so substitute every
/// qualified target assignment with its new-value expression before lowering
/// it to PostgreSQL's predicate.
fn rewrite_merge_matched_delete(sql: &str) -> String {
    if !sql.trim_start().to_ascii_uppercase().starts_with("MERGE ") {
        return sql.to_string();
    }
    let Some(update_at) = find_top_level_kw(sql, "UPDATE") else {
        return sql.to_string();
    };
    let after_update = &sql[update_at + "UPDATE".len()..];
    let set_ws = after_update.len() - after_update.trim_start().len();
    let after_set = &after_update[set_ws..];
    if !after_set.to_ascii_uppercase().starts_with("SET")
        || !after_set.as_bytes()["SET".len()..]
            .first()
            .is_none_or(u8::is_ascii_whitespace)
    {
        return sql.to_string();
    }
    let set_start = update_at + "UPDATE".len() + set_ws + "SET".len();
    let set_tail = &sql[set_start..];
    let Some(delete_rel) = find_top_level_kw(set_tail, "DELETE") else {
        return sql.to_string();
    };
    let delete_at = set_start + delete_rel;
    let after_delete = &sql[delete_at + "DELETE".len()..];
    let where_ws = after_delete.len() - after_delete.trim_start().len();
    let after_where = &after_delete[where_ws..];
    if !after_where.to_ascii_uppercase().starts_with("WHERE")
        || !after_where.as_bytes()["WHERE".len()..]
            .first()
            .is_none_or(u8::is_ascii_whitespace)
    {
        return sql.to_string();
    }
    let predicate_start = delete_at + "DELETE".len() + where_ws + "WHERE".len();
    let predicate_tail = &sql[predicate_start..];
    let next_when = find_top_level_kw(predicate_tail, "WHEN").unwrap_or(predicate_tail.len());
    let predicate = predicate_tail[..next_when].trim();
    let remainder = &predicate_tail[next_when..];

    let assignments = sql[set_start..delete_at].trim();
    let mut new_predicate = predicate.to_string();
    for assignment in split_top_level_commas(assignments) {
        let Some(eq) = assignment.find('=') else {
            continue;
        };
        let target = assignment[..eq].trim();
        // The qualified spelling makes it unambiguous that this is a target
        // value, rather than a source column with the same name.
        if target.contains('.') {
            new_predicate = replace_identifier_outside_literals(
                &new_predicate,
                target,
                &format!("({})", assignment[eq + 1..].trim()),
            );
        }
    }

    let update_head = &sql[..update_at];
    let Some(then_at) = update_head.to_ascii_uppercase().rfind("THEN") else {
        return sql.to_string();
    };
    // `UPDATE` must belong to `WHEN MATCHED THEN UPDATE`; do not rewrite an
    // otherwise malformed/unsupported MERGE.
    if !update_head[then_at + "THEN".len()..].trim().is_empty() {
        return sql.to_string();
    }
    let Some(when_at) = update_head[..then_at].to_ascii_uppercase().rfind("WHEN") else {
        return sql.to_string();
    };
    let before_when = &update_head[..when_at];
    let matched_head = &update_head[when_at..then_at].trim_end();
    format!(
        "{}{} AND ({}) THEN DELETE {} THEN UPDATE SET {} {}",
        before_when,
        matched_head,
        new_predicate,
        matched_head,
        assignments,
        remainder.trim_start(),
    )
}

/// Replace an identifier without touching string literals, quoted identifiers,
/// or a longer identifier that merely contains it.
fn replace_identifier_outside_literals(sql: &str, needle: &str, replacement: &str) -> String {
    let bytes = sql.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'\'' | b'"') {
            let end = skip_quoted(sql, i);
            out.push_str(&sql[i..end]);
            i = end;
        } else if i + needle_bytes.len() <= bytes.len()
            && bytes[i..i + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes)
            && !bytes
                .get(i.wrapping_sub(1))
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            && !bytes
                .get(i + needle_bytes.len())
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            out.push_str(replacement);
            i += needle_bytes.len();
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// PostgreSQL's `MERGE ... WHEN MATCHED THEN UPDATE SET` does not accept a
/// table-qualified target column (`SET d.col = ...`); Oracle requires it. Strip
/// the target alias prefix inside the SET list.
fn rewrite_merge_set_aliases(sql: &str) -> String {
    let upper = sql.trim_start().to_ascii_uppercase();
    if !upper.starts_with("MERGE ") {
        return sql.to_string();
    }
    // MERGE INTO <table> [AS] <alias> USING ...
    let after_into = match upper.find(" INTO ") {
        Some(p) => &sql[p + 6..],
        None => return sql.to_string(),
    };
    let mut words = after_into.split_whitespace();
    let _table = words.next();
    let mut alias = match words.next() {
        Some(a) => a,
        None => return sql.to_string(),
    };
    if alias.eq_ignore_ascii_case("AS") {
        alias = match words.next() {
            Some(a) => a,
            None => return sql.to_string(),
        };
    }
    if alias.eq_ignore_ascii_case("USING") {
        return sql.to_string();
    }
    // Only rewrite occurrences of `<alias>.` that appear after `SET`.
    let Some(set_at) = upper.find(" SET ") else {
        return sql.to_string();
    };
    let (head, tail) = sql.split_at(set_at + 5);
    let needle = format!("{alias}.");
    let mut out = String::from(head);
    let mut rest = tail;
    let needle_upper = needle.to_ascii_uppercase();
    while let Some(pos) = rest.to_ascii_uppercase().find(&needle_upper) {
        out.push_str(&rest[..pos]);
        rest = &rest[pos + needle.len()..];
    }
    out.push_str(rest);
    out
}

/// `<sequence>.NEXTVAL` / `<sequence>.CURRVAL` -> `nextval('<sequence>')` /
/// `currval('<sequence>')`, skipping string literals, quoted identifiers and
/// comments.
fn rewrite_sequence_pseudocolumns(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let end = skip_quoted(sql, i);
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
            _ if bytes[i..].starts_with(b"--") => {
                let end = bytes[i..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |o| i + o);
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
            _ if bytes[i..].starts_with(b"/*") => {
                let end = bytes[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map_or(bytes.len(), |o| i + 4 + o);
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
            b if b.is_ascii_alphabetic() || b == b'_' || b == b'"' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let ident = &sql[start..i];
                // Look for `.NEXTVAL` / `.CURRVAL` right after the identifier.
                if bytes.get(i) == Some(&b'.') {
                    let after = &sql[i + 1..];
                    let kw_len = after
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(after.len());
                    let kw = &after[..kw_len];
                    if kw.eq_ignore_ascii_case("nextval") || kw.eq_ignore_ascii_case("currval") {
                        let func = if kw.eq_ignore_ascii_case("nextval") {
                            "nextval"
                        } else {
                            "currval"
                        };
                        out.push_str(&format!("{func}('{ident}')"));
                        i += 1 + kw_len;
                        continue;
                    }
                }
                out.push_str(ident);
                continue;
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// `LISTAGG(expr [, sep]) WITHIN GROUP (ORDER BY cols)` -> PostgreSQL
/// `string_agg(expr::text, sep ORDER BY cols)`, including the `DISTINCT` and the
/// analytic (`... OVER (...)`) forms. Done as a text pass because sqlparser 0.47
/// does not model `WITHIN GROUP` on this function.
fn normalize_oracle_aggregates(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip string / quoted-identifier contents verbatim (UTF-8 safe slice).
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let end = skip_quoted(sql, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        let rest = &sql[i..];
        let is_listagg = rest
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("listagg"))
            && !bytes
                .get(i.wrapping_sub(1))
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if is_listagg && let Some(rewritten) = rewrite_one_listagg(rest) {
            out.push_str(&rewritten.text);
            i += rewritten.consumed;
            continue;
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

struct ListaggRewrite {
    text: String,
    consumed: usize,
}

fn rewrite_one_listagg(s: &str) -> Option<ListaggRewrite> {
    let after_kw = s[7..].trim_start();
    let paren_offset = 7 + (s[7..].len() - after_kw.len());
    if !after_kw.starts_with('(') {
        return None;
    }
    let args_start = paren_offset + 1;
    let args_end = matching_paren(&s[paren_offset..])? + paren_offset;
    let args = &s[args_start..args_end];

    let rest = s[args_end + 1..].trim_start();
    let rest_offset = args_end + 1 + (s[args_end + 1..].len() - rest.len());
    let upper = rest.to_ascii_uppercase();
    if !upper.starts_with("WITHIN GROUP") {
        return None;
    }
    let wg = rest["WITHIN GROUP".len()..].trim_start();
    if !wg.starts_with('(') {
        return None;
    }
    let wg_paren_at = rest_offset + (rest.len() - wg.len());
    let wg_end = matching_paren(&s[wg_paren_at..])? + wg_paren_at;
    let order_clause = s[wg_paren_at + 1..wg_end].trim();
    let order_cols = if order_clause.to_ascii_uppercase().starts_with("ORDER BY") {
        order_clause[8..].trim()
    } else {
        order_clause
    };

    let (distinct, arg_body) = {
        let t = args.trim_start();
        if t.to_ascii_uppercase().starts_with("DISTINCT ") {
            (true, t[9..].trim())
        } else {
            (false, args.trim())
        }
    };
    let (value_expr, sep) = match split_top_level_comma(arg_body) {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (arg_body, "''"),
    };

    // Optional trailing `OVER (window)` -> analytic form. PostgreSQL rejects an
    // aggregate ORDER BY inside a window function, so the ordering has to move
    // into the window spec instead.
    let tail = s[wg_end + 1..].trim_start();
    let tail_offset = wg_end + 1 + (s[wg_end + 1..].len() - tail.len());
    let mut consumed = wg_end + 1;
    let mut over_clause = None;
    if tail.to_ascii_uppercase().starts_with("OVER") {
        let after_over = tail[4..].trim_start();
        if after_over.starts_with('(') {
            let win_open = tail_offset + (tail.len() - after_over.len());
            let win_end = matching_paren(&s[win_open..])? + win_open;
            let win_body = s[win_open + 1..win_end].trim();
            // Oracle's windowed LISTAGG aggregates the whole partition; adding
            // an ORDER BY to a PostgreSQL window would make it a running
            // aggregate, so pin an explicit full frame.
            const FRAME: &str = " ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
            let merged = if win_body.to_ascii_uppercase().contains("ORDER BY") {
                format!("{win_body}{FRAME}")
            } else if win_body.is_empty() {
                format!("ORDER BY {order_cols}{FRAME}")
            } else {
                format!("{win_body} ORDER BY {order_cols}{FRAME}")
            };
            over_clause = Some(format!(" OVER ({merged})"));
            consumed = win_end + 1;
        }
    }

    let inner = if over_clause.is_some() {
        format!("string_agg(({value_expr})::text, {sep})")
    } else if distinct {
        // PostgreSQL forbids ORDER BY together with DISTINCT in an aggregate;
        // Oracle's LISTAGG(DISTINCT ...) orders by the value anyway.
        format!("string_agg(DISTINCT ({value_expr})::text, {sep})")
    } else {
        format!("string_agg(({value_expr})::text, {sep} ORDER BY {order_cols})")
    };
    Some(ListaggRewrite {
        text: format!("{inner}{}", over_clause.unwrap_or_default()),
        consumed,
    })
}

/// Index of the `)` that closes the `(` at byte 0 of `s`.
fn matching_paren(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut quoted = 0u8;
    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' | b'"' if quoted == 0 => quoted = b,
            b if quoted != 0 && b == quoted => quoted = 0,
            b'(' if quoted == 0 => depth += 1,
            b')' if quoted == 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_comma(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut quoted = 0u8;
    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' | b'"' if quoted == 0 => quoted = b,
            b if quoted != 0 && b == quoted => quoted = 0,
            b'(' if quoted == 0 => depth += 1,
            b')' if quoted == 0 => depth -= 1,
            b',' if quoted == 0 && depth == 0 => return Some((&s[..idx], &s[idx + 1..])),
            _ => {}
        }
    }
    None
}

/// Apply only token-boundary rewrites that do not alter string literals.
fn normalize_oracle_tokens(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    // Oracle `DATE` is a second-precision date+time; PostgreSQL `DATE` drops the
    // time. In a table column definition rewrite it to `timestamp(0)` so a
    // `DATE` column keeps its time-of-day. Only for CREATE/ALTER TABLE — a
    // `DATE '…'` literal or `CAST(x AS DATE)` elsewhere must stay untouched.
    let table_ddl = {
        let u = sql.trim_start().to_ascii_uppercase();
        u.starts_with("CREATE TABLE ")
            || u.starts_with("CREATE GLOBAL TEMPORARY TABLE ")
            || u.starts_with("CREATE PRIVATE TEMPORARY TABLE ")
            || u.starts_with("ALTER TABLE ")
    } && sql.to_ascii_uppercase().find(" AS SELECT").is_none();
    let mut i = 0;
    while i < bytes.len() {
        // Preserve string literals, quoted identifiers, and comments verbatim.
        // Oracle applications frequently use identifiers such as "NUMBER" or
        // literal text containing SYSDATE (and non-ASCII data).
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let end = skip_quoted(sql, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"--") {
            let end = bytes[i..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| i + offset);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let end = bytes[i + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(bytes.len(), |offset| i + 4 + offset);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let token = &sql[start..i];
            let followed_by_paren = sql[i..].trim_start().starts_with('(');
            let preceded_by_dot = start > 0 && sql.as_bytes()[start - 1] == b'.';
            let rest_upper = sql[i..].trim_start().to_ascii_uppercase();
            let replacement = if token.eq_ignore_ascii_case("MINUS") {
                Some("EXCEPT")
            } else if token.eq_ignore_ascii_case("NVL") {
                Some("COALESCE")
            // Oracle SUBSTR supports negative offsets; orafce's `oracle.substr`
            // matches, `pg_catalog.substr` does not.
            } else if token.eq_ignore_ascii_case("SUBSTR") && followed_by_paren && !preceded_by_dot
            {
                Some("oracle.substr")
            // A NUMBER column that is an IDENTITY must be an integer type.
            } else if token.eq_ignore_ascii_case("NUMBER")
                && (rest_upper.starts_with("GENERATED")
                    || rest_upper
                        .trim_start_matches(|c: char| {
                            c.is_ascii_digit() || c == '(' || c == ')' || c == ',' || c == ' '
                        })
                        .starts_with("GENERATED"))
            {
                // Skip a trailing precision `(n)` on the NUMBER.
                if sql[i..].trim_start().starts_with('(')
                    && let Some(close) = sql[i..].find(')')
                {
                    i += close + 1;
                }
                Some("bigint")
            } else if token.eq_ignore_ascii_case("SYSDATE")
                || token.eq_ignore_ascii_case("SYSTIMESTAMP")
            {
                Some("CURRENT_TIMESTAMP")
            // Oracle niladic time-zone functions, usable without parens.
            } else if token.eq_ignore_ascii_case("SESSIONTIMEZONE") && !followed_by_paren {
                Some("public.sessiontimezone()")
            } else if token.eq_ignore_ascii_case("DBTIMEZONE") && !followed_by_paren {
                Some("public.dbtimezone()")
            // Oracle session pseudo-columns / niladic functions. `USER` is
            // upper-cased to match Oracle; only rewrite when it is not a
            // function call like `user(...)`.
            } else if token.eq_ignore_ascii_case("USER") && !followed_by_paren {
                Some("UPPER(CURRENT_USER)")
            } else if token.eq_ignore_ascii_case("UID") && !followed_by_paren {
                Some("(SELECT oid::int FROM pg_catalog.pg_roles WHERE rolname = CURRENT_USER)")
            } else if token.eq_ignore_ascii_case("ROWID") && !followed_by_paren {
                Some("ctid")
            // These are type names, so converting at token boundaries is safe
            // in every statement kind (including CREATE/ALTER TABLE).
            } else if token.eq_ignore_ascii_case("VARCHAR2")
                || token.eq_ignore_ascii_case("NVARCHAR2")
                || token.eq_ignore_ascii_case("NCHAR")
            {
                // NCHAR / NVARCHAR2 collapse to VARCHAR: UTF-8 text round-trips
                // exactly. They are reported as VARCHAR2, not an N-type — the
                // national charset id would need UTF-16 on the wire or an
                // ncharset renegotiation, disproportionate to a label change.
                Some("VARCHAR")
            } else if token.eq_ignore_ascii_case("NUMBER") {
                Some("NUMERIC")
            } else if token.eq_ignore_ascii_case("DATE")
                && table_ddl
                && !preceded_by_dot
                // Not the keyword in `... AS DATE)` inside a CTAS-style SELECT.
                && !sql[..start].trim_end().to_ascii_uppercase().ends_with(" AS")
                && !sql[..start].trim_end().eq_ignore_ascii_case("AS")
                // A column type is followed by a constraint keyword, a comma,
                // the closing paren of the column list, or the end of an
                // `ALTER TABLE … ADD col DATE`. A `DATE '…'` literal is
                // followed by a quote and is left alone.
                && {
                    let r = rest_upper.trim_start();
                    r.is_empty()
                        || r.starts_with(',')
                        || r.starts_with(')')
                        || starts_keyword(r, "DEFAULT")
                        || starts_keyword(r, "NOT")
                        || starts_keyword(r, "NULL")
                        || starts_keyword(r, "PRIMARY")
                        || starts_keyword(r, "UNIQUE")
                        || starts_keyword(r, "CHECK")
                        || starts_keyword(r, "CONSTRAINT")
                        || starts_keyword(r, "REFERENCES")
                        || starts_keyword(r, "GENERATED")
                        || starts_keyword(r, "COLLATE")
                        || starts_keyword(r, "ENABLE")
                        || starts_keyword(r, "DISABLE")
                }
            {
                Some("timestamp(0)")
            } else if token.eq_ignore_ascii_case("BINARY_FLOAT") {
                // Transparent domain over `real`; a describe-time catalog lookup
                // recovers the Oracle BINARY_FLOAT type for a declared column.
                Some("dbsaci.binary_float")
            } else if token.eq_ignore_ascii_case("BINARY_DOUBLE") {
                Some("dbsaci.binary_double")
            } else if token.eq_ignore_ascii_case("CLOB")
                || token.eq_ignore_ascii_case("NCLOB")
                || token.eq_ignore_ascii_case("LONG")
            {
                Some("TEXT")
            } else if token.eq_ignore_ascii_case("BLOB") || token.eq_ignore_ascii_case("RAW") {
                Some("BYTEA")
            } else {
                None
            };
            out.push_str(replacement.unwrap_or(token));

            // `RAW`/`BLOB` -> BYTEA takes no length: drop a trailing `(n)`.
            if matches!(replacement, Some("BYTEA")) && sql[i..].trim_start().starts_with('(') {
                let paren = i + (sql[i..].len() - sql[i..].trim_start().len());
                if let Some(close) = sql[paren..].find(')') {
                    i = paren + close + 1;
                }
            }
            // `VARCHAR2(n CHAR|BYTE)` -> `VARCHAR(n)`.
            if matches!(replacement, Some("VARCHAR")) && sql[i..].trim_start().starts_with('(') {
                let paren = i + (sql[i..].len() - sql[i..].trim_start().len());
                if let Some(close_rel) = sql[paren..].find(')') {
                    let inside = &sql[paren + 1..paren + close_rel];
                    let trimmed = inside
                        .trim()
                        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                        .trim();
                    out.push('(');
                    out.push_str(trimmed);
                    out.push(')');
                    i = paren + close_rel + 1;
                }
            }
        } else {
            let character = sql[i..]
                .chars()
                .next()
                .expect("index is within the input string");
            out.push(character);
            i += character.len_utf8();
        }
    }
    out
}

/// Rewrite Oracle legacy `(+)` outer joins to ANSI `JOIN`s. sqlparser's
/// GenericDialect cannot lex the `(+)` marker at all, so this runs on text
/// before parsing. Handles any number of comma-separated tables and `(+)`
/// predicates; bails (returns the input unchanged) on shapes it is unsure of.
fn normalize_legacy_outer_join(sql: &str) -> Result<String> {
    if !sql.contains("(+)") {
        return Ok(sql.to_string());
    }
    Ok(rewrite_legacy_outer_join_text(sql).unwrap_or_else(|| sql.to_string()))
}

fn rewrite_legacy_outer_join_text(sql: &str) -> Option<String> {
    let from_at = find_top_level_kw(sql, "FROM")?;
    let where_at = find_top_level_kw(sql, "WHERE")?;
    if where_at < from_at {
        return None;
    }
    let head = &sql[..from_at];
    let from_list = sql[from_at + 4..where_at].trim();
    let after_where = &sql[where_at + 5..];
    // Trailing clause that must stay after the WHERE.
    let suffix_at = [
        "GROUP BY",
        "ORDER BY",
        "HAVING",
        "CONNECT BY",
        "START WITH",
        "FETCH",
    ]
    .iter()
    .filter_map(|k| find_top_level_kw(after_where, k))
    .min();
    let (where_body, suffix) = match suffix_at {
        Some(p) => (after_where[..p].trim(), &after_where[p..]),
        None => (after_where.trim(), ""),
    };

    let tables = split_top_level_commas(from_list);
    if tables.len() < 2 {
        return None;
    }
    let keys: Vec<String> = tables
        .iter()
        .map(|t| last_word(t).to_ascii_lowercase())
        .collect();

    let conjuncts = split_top_level_kw(where_body, "AND");
    let mut join_preds: Vec<(Vec<String>, Vec<String>, String)> = Vec::new(); // (tables, marked, stripped)
    let mut filters: Vec<String> = Vec::new();
    for c in conjuncts {
        let c = c.trim();
        if !c.contains("(+)") {
            filters.push(c.to_string());
            continue;
        }
        let marked = marked_qualifiers(c);
        let stripped = c.replace("(+)", "").replace("  ", " ");
        let mut refs: Vec<String> = qualifiers(&stripped)
            .into_iter()
            .filter(|q| keys.contains(q))
            .collect();
        refs.dedup();
        join_preds.push((refs, marked, stripped.trim().to_string()));
    }
    if join_preds.is_empty() {
        return None;
    }
    let outer: std::collections::HashSet<String> =
        join_preds.iter().flat_map(|p| p.1.clone()).collect();

    let mut placed = vec![keys[0].clone()];
    let mut used = vec![false; join_preds.len()];
    let mut out = format!("{head} FROM {}", tables[0].trim());
    for i in 1..tables.len() {
        let key = &keys[i];
        let mut on = Vec::new();
        for (j, (refs, _m, expr)) in join_preds.iter().enumerate() {
            if used[j] || !refs.contains(key) {
                continue;
            }
            if refs.len() == 1 || refs.iter().any(|r| r != key && placed.contains(r)) {
                on.push(expr.clone());
                used[j] = true;
            }
        }
        let kind = if outer.contains(key) {
            "LEFT JOIN"
        } else if on.is_empty() {
            "CROSS JOIN"
        } else if outer.contains(&keys[0]) {
            // `a.x (+) = b.y`: the base `a` is null-padded, so keep all of `b`.
            "RIGHT JOIN"
        } else {
            "JOIN"
        };
        out.push_str(&format!(" {kind} {}", tables[i].trim()));
        if !on.is_empty() {
            out.push_str(&format!(" ON {}", on.join(" AND ")));
        }
        placed.push(key.clone());
    }
    for (j, (_r, _m, expr)) in join_preds.iter().enumerate() {
        if !used[j] {
            filters.push(expr.clone());
        }
    }
    if !filters.is_empty() {
        out.push_str(&format!(" WHERE {}", filters.join(" AND ")));
    }
    if !suffix.is_empty() {
        out.push(' ');
        out.push_str(suffix.trim());
    }
    Some(out)
}

/// Table aliases/names that appear immediately before a `(+)` marker.
fn marked_qualifiers(pred: &str) -> Vec<String> {
    let bytes = pred.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = pred[i..].find("(+)") {
        let at = i + rel;
        // step back over spaces
        let mut k = at;
        while k > 0 && bytes[k - 1] == b' ' {
            k -= 1;
        }
        // column identifier
        let col_end = k;
        while k > 0 && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_') {
            k -= 1;
        }
        if k > 0 && bytes[k - 1] == b'.' && k - 1 < col_end {
            let mut q = k - 1;
            while q > 0 && (bytes[q - 1].is_ascii_alphanumeric() || bytes[q - 1] == b'_') {
                q -= 1;
            }
            let qual = pred[q..k - 1].to_ascii_lowercase();
            if !qual.is_empty() && !out.contains(&qual) {
                out.push(qual);
            }
        }
        i = at + 3;
    }
    out
}

/// All `<qualifier>.` prefixes in a fragment, lower-cased.
fn qualifiers(frag: &str) -> Vec<String> {
    let bytes = frag.as_bytes();
    let mut out = Vec::new();
    for (idx, _) in frag.match_indices('.') {
        let mut q = idx;
        while q > 0 && (bytes[q - 1].is_ascii_alphanumeric() || bytes[q - 1] == b'_') {
            q -= 1;
        }
        if q < idx {
            let s = frag[q..idx].to_ascii_lowercase();
            if !s.chars().next().is_some_and(|c| c.is_ascii_digit()) && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

fn last_word(table_ref: &str) -> &str {
    table_ref.split_whitespace().last().unwrap_or(table_ref)
}

/// Byte offset of a top-level (depth-0, not in a string) keyword, matched on
/// word boundaries and surrounded by whitespace. Returns the offset of the
/// keyword itself.
/// If `s` (ignoring leading whitespace) begins with the space-separated
/// keyword phrase `kw` on a word boundary, return the remainder with its
/// leading whitespace trimmed. Case-insensitive; internal whitespace in `s`
/// between keyword words must be a single ASCII space run.
fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    let mut cur = s;
    for (idx, word) in kw.split_ascii_whitespace().enumerate() {
        if idx > 0 {
            let t = cur.trim_start();
            if std::ptr::eq(t, cur) {
                return None; // no whitespace separator
            }
            cur = t;
        }
        let head = cur.get(..word.len())?;
        if !head.eq_ignore_ascii_case(word) {
            return None;
        }
        cur = &cur[word.len()..];
    }
    // boundary: next byte must be whitespace or end
    if cur
        .as_bytes()
        .first()
        .is_none_or(|c| c.is_ascii_whitespace())
    {
        Some(cur.trim_start())
    } else {
        None
    }
}

fn find_top_level_kw(s: &str, kw: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let up = s.to_ascii_uppercase();
    let ub = up.as_bytes();
    let mut depth = 0i32;
    let mut quoted = 0u8;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' if quoted == 0 => quoted = b,
            _ if quoted != 0 && b == quoted => quoted = 0,
            b'(' if quoted == 0 => depth += 1,
            b')' if quoted == 0 => depth -= 1,
            _ if quoted == 0
                && depth == 0
                && i > 0
                && bytes[i - 1].is_ascii_whitespace()
                && ub[i..].starts_with(kw.as_bytes())
                && s.as_bytes()[i + kw.len()..]
                    .first()
                    .is_none_or(|c| c.is_ascii_whitespace()) =>
            {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split on a top-level keyword (` <KW> `), returning the pieces.
fn split_top_level_kw<'a>(s: &'a str, kw: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut base = 0usize;
    while let Some(rel) = find_top_level_kw(&s[base..], kw) {
        let at = base + rel;
        parts.push(&s[base..at]);
        base = at + kw.len();
    }
    parts.push(&s[base..]);
    parts
}

fn translate_query(query: &mut Query) -> Result<()> {
    // Oracle allows a self-referencing CTE without the RECURSIVE keyword;
    // PostgreSQL requires it.
    if let Some(with) = &mut query.with
        && !with.recursive
    {
        let self_ref = with.cte_tables.iter().any(|cte| {
            let name = cte.alias.name.value.to_ascii_lowercase();
            cte.query.to_string().to_ascii_lowercase().contains(&name)
        });
        if self_ref {
            with.recursive = true;
        }
    }

    // Oracle resolves SELECT-list aliases inside ORDER BY *expressions*
    // (`ORDER BY decode(constraint_type, ...)` where `constraint_type` is an
    // alias); PostgreSQL only resolves an alias when it is the whole ORDER BY
    // term. Substitute the alias with its expression where it appears nested.
    if let SetExpr::Select(select) = query.body.as_ref() {
        let aliases: std::collections::HashMap<String, Expr> = select
            .projection
            .iter()
            .filter_map(|item| match item {
                SelectItem::ExprWithAlias { expr, alias } => {
                    Some((alias.value.to_ascii_lowercase(), expr.clone()))
                }
                _ => None,
            })
            .collect();
        if !aliases.is_empty() {
            for order_by in order_by_exprs_mut(query) {
                if !matches!(order_by.expr, Expr::Identifier(_)) {
                    substitute_select_aliases(&mut order_by.expr, &aliases);
                }
            }
        }
    }

    let row_limit = translate_set_expr(&mut query.body)?;
    for order_by in order_by_exprs_mut(query) {
        rewrite_expr(&mut order_by.expr)?;
    }
    if let Some(limit) = row_limit {
        if query_limit(query).is_some() {
            return Err(Error::SqlParse(
                "ROWNUM together with an explicit LIMIT needs a nested query".to_string(),
            ));
        }
        if order_by_is_empty(query) {
            set_query_limit(query, limit);
        } else {
            // Oracle applies ROWNUM *before* ORDER BY. Reproduce that by
            // limiting the unordered body inside a derived table (widened to
            // `SELECT *` so the outer ORDER BY can still see every column) and
            // sorting the outer query.
            let order_by = query.order_by.take();
            let outer_projection = match query.body.as_mut() {
                SetExpr::Select(select) => std::mem::replace(
                    &mut select.projection,
                    vec![SelectItem::Wildcard(
                        sqlparser::ast::WildcardAdditionalOptions::default(),
                    )],
                ),
                _ => vec![SelectItem::Wildcard(
                    sqlparser::ast::WildcardAdditionalOptions::default(),
                )],
            };
            let mut inner = query.clone();
            set_query_limit(&mut inner, limit);
            *query = wrap_query_with_order_by(inner, order_by, outer_projection)?;
        }
    }
    Ok(())
}

/// Replace bare `Expr::Identifier` nodes that name a SELECT-list alias with the
/// aliased expression. Does not recurse into a substituted expression (so
/// `SELECT a AS b, b AS a` cannot loop) and leaves qualified names alone.
fn substitute_select_aliases(expr: &mut Expr, aliases: &std::collections::HashMap<String, Expr>) {
    match expr {
        Expr::Identifier(id) => {
            if let Some(replacement) = aliases.get(&id.value.to_ascii_lowercase()) {
                *expr = replacement.clone();
            }
        }
        Expr::Nested(e)
        | Expr::UnaryOp { expr: e, .. }
        | Expr::Cast { expr: e, .. }
        | Expr::Collate { expr: e, .. }
        | Expr::IsNull(e)
        | Expr::IsNotNull(e)
        | Expr::IsTrue(e)
        | Expr::IsFalse(e) => substitute_select_aliases(e, aliases),
        Expr::BinaryOp { left, right, .. } => {
            substitute_select_aliases(left, aliases);
            substitute_select_aliases(right, aliases);
        }
        Expr::Between {
            expr: e, low, high, ..
        } => {
            substitute_select_aliases(e, aliases);
            substitute_select_aliases(low, aliases);
            substitute_select_aliases(high, aliases);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                substitute_select_aliases(o, aliases);
            }
            for when in conditions {
                substitute_select_aliases(&mut when.condition, aliases);
                substitute_select_aliases(&mut when.result, aliases);
            }
            if let Some(e) = else_result {
                substitute_select_aliases(e, aliases);
            }
        }
        Expr::Function(f) => {
            if let FunctionArguments::List(list) = &mut f.args {
                for arg in &mut list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    } = arg
                    {
                        substitute_select_aliases(e, aliases);
                    }
                }
            }
        }
        _ => {}
    }
}

fn translate_set_expr(set_expr: &mut SetExpr) -> Result<Option<Expr>> {
    match set_expr {
        SetExpr::Select(select) => {
            translate_legacy_outer_join(select)?;
            // DUAL is Oracle's one-row pseudo-table. PostgreSQL selects without
            // a FROM clause, so drop the unaliased bare (or schema-qualified)
            // form entirely; the aliased and multi-table forms are left for the
            // session-local `dual` view to satisfy.
            if select.from.len() == 1
                && select.from[0].joins.is_empty()
                && let TableFactor::Table { name, alias, .. } = &select.from[0].relation
                && alias.is_none()
                && name_last(name).is_some_and(|part| part.eq_ignore_ascii_case("dual"))
            {
                select.from.clear();
            } else {
                // `SELECT ... FROM t1, sys.dual` / `FROM dual d` etc.: rewrite a
                // schema-qualified `x.dual` down to bare `dual` so it resolves
                // to the session view.
                for table in &mut select.from {
                    strip_dual_schema(&mut table.relation);
                    for join in &mut table.joins {
                        strip_dual_schema(&mut join.relation);
                    }
                }
            }

            for projection in &mut select.projection {
                match projection {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        rewrite_expr(expr)?;
                    }
                    _ => {}
                }
            }

            if let sqlparser::ast::GroupByExpr::Expressions(expressions, _) = &mut select.group_by {
                for expression in expressions {
                    rewrite_expr(expression)?;
                }
            }
            if let Some(having) = &mut select.having {
                rewrite_expr(having)?;
            }
            let mut derived_seq = 0usize;
            for table in &mut select.from {
                rewrite_table_factor(&mut table.relation, &mut derived_seq)?;
                for join in &mut table.joins {
                    rewrite_table_factor(&mut join.relation, &mut derived_seq)?;
                    rewrite_join_operator(&mut join.join_operator)?;
                }
            }

            if let Some(selection) = select.selection.take() {
                // Lift ROWNUM out of the WHERE clause *before* the generic
                // expression rewrite, which would turn a bare `rownum` into a
                // window function and hide it from the limit logic.
                let (remaining, row_limit) = strip_rownum_predicate(selection);
                if let Some(mut remaining) = remaining {
                    rewrite_expr(&mut remaining)?;
                    select.selection = Some(remaining);
                }
                return Ok(row_limit);
            }
        }
        SetExpr::Query(query) => translate_query(query)?,
        SetExpr::Values(values) => {
            for row in &mut values.rows {
                for expr in &mut row.content {
                    rewrite_expr(expr)?;
                }
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            if translate_set_expr(left)?.is_some() || translate_set_expr(right)?.is_some() {
                return Err(Error::SqlParse(
                    "ROWNUM in a set operation requires an explicit nested query".to_string(),
                ));
            }
        }
        _ => {}
    }
    Ok(None)
}

/// `<schema>.dual` -> `dual` so it resolves to the session-local view.
fn strip_dual_schema(table: &mut TableFactor) {
    if let TableFactor::Table { name, .. } = table
        && name.0.len() > 1
        && name_last(name).is_some_and(|p| p.eq_ignore_ascii_case("dual"))
    {
        let last = name.0.pop().unwrap();
        name.0.clear();
        name.0.push(last);
    }
}

fn rewrite_table_factor(table: &mut TableFactor, derived_seq: &mut usize) -> Result<()> {
    if let TableFactor::Derived {
        subquery, alias, ..
    } = table
    {
        translate_query(subquery)?;
        // Oracle allows an unaliased subquery in FROM; PostgreSQL requires an
        // alias (and did so on every supported major). Synthesise a unique one.
        if alias.is_none() {
            *alias = Some(sqlparser::ast::TableAlias {
                name: Ident::new(format!("__dbsaci_sub{derived_seq}")),
                columns: vec![],
                at: None,
                explicit: false,
            });
            *derived_seq += 1;
        }
    }
    Ok(())
}

fn rewrite_join_operator(join: &mut JoinOperator) -> Result<()> {
    use JoinOperator::{
        Anti, ArrayJoin, AsOf, CrossApply, CrossJoin, FullOuter, Inner, InnerArrayJoin, Join, Left,
        LeftAnti, LeftArrayJoin, LeftOuter, LeftSemi, OuterApply, Right, RightAnti, RightOuter,
        RightSemi, Semi, StraightJoin,
    };
    let constraint = match join {
        Join(c) | Inner(c) | Left(c) | LeftOuter(c) | Right(c) | RightOuter(c) | FullOuter(c)
        | CrossJoin(c) | Semi(c) | LeftSemi(c) | RightSemi(c) | Anti(c) | LeftAnti(c)
        | RightAnti(c) | StraightJoin(c) => c,
        AsOf { constraint, .. } => constraint,
        CrossApply | OuterApply | ArrayJoin | LeftArrayJoin | InnerArrayJoin => return Ok(()),
    };
    if let JoinConstraint::On(expression) = constraint {
        rewrite_expr(expression)?;
    }
    Ok(())
}

fn rewrite_expr(expr: &mut Expr) -> Result<()> {
    // Oracle treats the empty string literal as NULL everywhere.
    if let Expr::Value(ValueWithSpan { value, .. }) = expr
        && let Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) = value
        && s.is_empty()
    {
        *expr = lit(Value::Null);
        return Ok(());
    }
    // A `ROWNUM` reference that survived the WHERE-clause lift (projections,
    // subquery output columns) becomes a monotonically increasing row number.
    if let Expr::Identifier(id) = expr
        && id.value.eq_ignore_ascii_case("rownum")
    {
        *expr = parse_expr("ROW_NUMBER() OVER ()");
        return Ok(());
    }
    // `CAST(x AS DATE)`: Oracle's DATE keeps a time-of-day, PostgreSQL's `date`
    // does not — map to `timestamp`.
    if let Expr::Cast {
        data_type: dt @ sqlparser::ast::DataType::Date,
        ..
    } = expr
    {
        *dt = sqlparser::ast::DataType::Timestamp(None, sqlparser::ast::TimezoneInfo::None);
    }
    match expr {
        Expr::BinaryOp { left, op, right } => {
            rewrite_expr(left)?;
            rewrite_expr(right)?;
            // Oracle date arithmetic is in days; PostgreSQL needs intervals.
            if matches!(op, BinaryOperator::Plus | BinaryOperator::Minus) {
                let (l_date, r_date) = (is_date_expr(left), is_date_expr(right));
                if *op == BinaryOperator::Minus
                    && l_date
                    && r_date
                    && !(is_plain_date(left) && is_plain_date(right))
                {
                    // `<timestamp> - <timestamp>` yields an interval in
                    // PostgreSQL; Oracle yields a number of days. (`date - date`
                    // is already an integer in PostgreSQL, so it is left alone.)
                    let whole = std::mem::replace(expr, lit(Value::Null));
                    *expr = try_parse_expr(&format!("EXTRACT(EPOCH FROM ({whole})) / 86400"))?;
                    return Ok(());
                } else if l_date && is_numberish(right) {
                    mul_by_one_day(right);
                } else if r_date && is_numberish(left) {
                    mul_by_one_day(left);
                }
            }
            // Oracle `||` treats NULL operands as the empty string.
            if *op == BinaryOperator::StringConcat {
                wrap_coalesce_empty(left);
                wrap_coalesce_empty(right);
            }
            // Oracle `/` is always real division. Force it by casting the
            // numerator to NUMERIC (only when neither side already looks
            // fractional, to avoid piling casts on top of casts).
            if *op == BinaryOperator::Divide && !contains_cast_to_numeric(left) {
                let numerator = std::mem::replace(left, Box::new(lit(Value::Null)));
                **left = Expr::Cast {
                    kind: sqlparser::ast::CastKind::Cast,
                    expr: numerator,
                    data_type: sqlparser::ast::DataType::Numeric(
                        sqlparser::ast::ExactNumberInfo::None,
                    ),
                    array: false,
                    format: None,
                };
            }
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => rewrite_expr(expr)?,
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_expr(expr)?;
            rewrite_expr(low)?;
            rewrite_expr(high)?;
        }
        Expr::InList { expr, list, .. } => {
            rewrite_expr(expr)?;
            for item in list {
                rewrite_expr(item)?;
            }
        }
        Expr::InSubquery { subquery, .. }
        | Expr::Exists { subquery, .. }
        | Expr::Subquery(subquery) => {
            translate_query(subquery)?;
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                rewrite_expr(operand)?;
            }
            for when in conditions {
                rewrite_expr(&mut when.condition)?;
                rewrite_expr(&mut when.result)?;
            }
            if let Some(result) = else_result {
                rewrite_expr(result)?;
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(arguments) = &mut function.args {
                for argument in &mut arguments.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = argument {
                        rewrite_expr(expr)?;
                    }
                }
            }
            let name = name_last(&function.name).unwrap_or_default().to_string();
            let name = name.as_str();
            let args = function_args(function);
            if name.eq_ignore_ascii_case("DECODE") {
                // PostgreSQL's two-argument decode(text, format) is used for
                // RAW/BLOB bind literals; only Oracle's 3+ argument DECODE
                // conditional form should be rewritten to CASE.
                let args = args?;
                if args.len() >= 3 {
                    *expr = rewrite_decode(args)?;
                }
            } else if name.eq_ignore_ascii_case("NVL2") {
                *expr = rewrite_nvl2(args?)?;
            } else if name.eq_ignore_ascii_case("LNNVL") {
                *expr = rewrite_lnnvl(args?)?;
            } else if name.eq_ignore_ascii_case("TO_DATE") {
                // Oracle DATE carries a time-of-day; PostgreSQL `to_date` drops
                // it. Use `to_timestamp(...)::timestamp` so `HH24:MI` survives.
                let args = args?;
                if args.len() == 2 {
                    *expr = try_parse_expr(&format!(
                        "CAST(to_timestamp({}, {}) AS timestamp)",
                        args[0], args[1]
                    ))?;
                }
            } else if name.eq_ignore_ascii_case("REGEXP_REPLACE")
                || name.eq_ignore_ascii_case("REPLACE")
            {
                if let FunctionArguments::List(list) = &mut function.args {
                    // Oracle treats a NULL/absent replacement as ''.
                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(third))) =
                        list.args.get_mut(2)
                        && as_value(third) == Some(&Value::Null)
                    {
                        *third = lit(Value::SingleQuotedString(String::new()));
                    }
                    // REGEXP_REPLACE: Oracle replaces every match; PostgreSQL
                    // replaces only the first without the 'g' flag.
                    if name.eq_ignore_ascii_case("REGEXP_REPLACE") && list.args.len() == 3 {
                        list.args
                            .push(FunctionArg::Unnamed(FunctionArgExpr::Expr(lit(
                                Value::SingleQuotedString("g".into()),
                            ))));
                    }
                }
            } else if name.eq_ignore_ascii_case("RATIO_TO_REPORT") {
                // RATIO_TO_REPORT(x) OVER (w)  ->  x / SUM(x) OVER (w)
                let args = args?;
                if args.len() == 1
                    && let Some(over) = &function.over
                {
                    *expr = try_parse_expr(&format!(
                        "({0})::numeric / NULLIF(SUM({0}) OVER {1}, 0)",
                        args[0], over
                    ))?;
                }
            } else if name.eq_ignore_ascii_case("REGEXP_SUBSTR") {
                let args = args?;
                // `REGEXP_SUBSTR(s, p, 1, 1, NULL, g)` -> the g-th capture group.
                if args.len() == 6
                    && const_u64(&args[2]) == Some(1)
                    && const_u64(&args[3]) == Some(1)
                {
                    let group = args[5].clone();
                    *expr = try_parse_expr(&format!(
                        "(regexp_match({}, {}))[{}]",
                        args[0], args[1], group
                    ))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Wrap `e` as `COALESCE(CAST(e AS text), '')` (Oracle `||` stringifies operands
/// and treats NULL as ''), unless `e` is already a plain string literal.
fn wrap_coalesce_empty(e: &mut Box<Expr>) {
    if matches!(as_value(e.as_ref()), Some(Value::SingleQuotedString(_))) {
        return;
    }
    let inner = std::mem::replace(e.as_mut(), lit(Value::Null));
    let cast = Expr::Cast {
        kind: sqlparser::ast::CastKind::Cast,
        expr: Box::new(inner),
        data_type: sqlparser::ast::DataType::Text,
        array: false,
        format: None,
    };
    **e = Expr::Function(sqlparser::ast::Function {
        name: obj_name("COALESCE"),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(cast)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(lit(Value::SingleQuotedString(
                    String::new(),
                )))),
            ],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    });
}

/// Multiply `e` in place by `INTERVAL '1 day'`.
fn mul_by_one_day(e: &mut Box<Expr>) {
    let n = std::mem::replace(e.as_mut(), lit(Value::Null));
    **e = Expr::BinaryOp {
        left: Box::new(Expr::Nested(Box::new(n))),
        op: BinaryOperator::Multiply,
        right: Box::new(parse_expr("INTERVAL '1 day'")),
    };
}

/// A bare `DATE '...'` literal (PostgreSQL type `date`, where `date - date` is
/// already an integer day count).
fn is_plain_date(e: &Expr) -> bool {
    match e {
        Expr::TypedString(ts) => ts.data_type == sqlparser::ast::DataType::Date,
        Expr::Nested(inner) => is_plain_date(inner),
        _ => false,
    }
}

/// A plainly numeric operand (literal, bind, or arithmetic over such) — as
/// opposed to an interval, column or function call.
fn is_numberish(e: &Expr) -> bool {
    match e {
        _ if matches!(as_value(e), Some(Value::Number(..) | Value::Placeholder(_))) => true,
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => is_numberish(expr),
        Expr::BinaryOp { left, op, right } => {
            matches!(
                op,
                BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo
            ) && is_numberish(left)
                && is_numberish(right)
        }
        Expr::Cast { data_type, .. } => matches!(
            data_type,
            sqlparser::ast::DataType::Numeric(_)
                | sqlparser::ast::DataType::Decimal(_)
                | sqlparser::ast::DataType::Int(_)
                | sqlparser::ast::DataType::Integer(_)
                | sqlparser::ast::DataType::BigInt(_)
                | sqlparser::ast::DataType::Double(_)
                | sqlparser::ast::DataType::Float(_)
        ),
        _ => false,
    }
}

/// Does this expression evaluate to a DATE/TIMESTAMP in Oracle?
fn is_date_expr(e: &Expr) -> bool {
    match e {
        Expr::Cast { data_type, .. } => matches!(
            data_type,
            sqlparser::ast::DataType::Date
                | sqlparser::ast::DataType::Datetime(_)
                | sqlparser::ast::DataType::Timestamp(..)
        ),
        Expr::TypedString(ts) => matches!(
            ts.data_type,
            sqlparser::ast::DataType::Date
                | sqlparser::ast::DataType::Datetime(_)
                | sqlparser::ast::DataType::Timestamp(..)
        ),
        Expr::Nested(inner) => is_date_expr(inner),
        // `<date> +/- <interval>` is still a date.
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Plus | BinaryOperator::Minus,
            right,
        } => is_date_expr(left) || is_date_expr(right),
        Expr::Function(f) => {
            let name = name_last(&f.name).unwrap_or_default();
            const DATE_FNS: &[&str] = &[
                "CURRENT_TIMESTAMP",
                "CURRENT_DATE",
                "LOCALTIMESTAMP",
                "LOCALTIME",
                "NOW",
                "SYSDATE",
                "SYSTIMESTAMP",
                "TO_DATE",
                "TO_TIMESTAMP",
                "ADD_MONTHS",
                "LAST_DAY",
                "NEXT_DAY",
            ];
            if DATE_FNS.iter().any(|f| name.eq_ignore_ascii_case(f)) {
                return true;
            }
            // TRUNC/ROUND are date-valued only when their first argument is.
            if (name.eq_ignore_ascii_case("TRUNC") || name.eq_ignore_ascii_case("ROUND"))
                && let FunctionArguments::List(list) = &f.args
                && let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(first))) = list.args.first()
            {
                return is_date_expr(first);
            }
            false
        }
        Expr::Identifier(i) => {
            matches!(
                i.value.to_ascii_uppercase().as_str(),
                "SYSDATE" | "SYSTIMESTAMP"
            )
        }
        _ => false,
    }
}

fn contains_cast_to_numeric(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Cast {
            data_type: sqlparser::ast::DataType::Numeric(_)
                | sqlparser::ast::DataType::Decimal(_)
                | sqlparser::ast::DataType::Double(_)
                | sqlparser::ast::DataType::Float(_),
            ..
        }
    )
}

fn function_args(function: &sqlparser::ast::Function) -> Result<Vec<Expr>> {
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(Error::SqlParse(
            "Oracle function requires ordinary arguments".into(),
        ));
    };
    arguments
        .args
        .iter()
        .map(|argument| match argument {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr.clone()),
            _ => Err(Error::SqlParse(
                "named or wildcard Oracle function arguments are unsupported".into(),
            )),
        })
        .collect()
}

fn rewrite_decode(args: Vec<Expr>) -> Result<Expr> {
    if args.len() < 3 {
        return Err(Error::SqlParse(
            "DECODE requires at least three arguments".into(),
        ));
    }
    let input = args[0].clone();
    let pair_end = if args.len().is_multiple_of(2) {
        args.len() - 1
    } else {
        args.len()
    };
    // Oracle DECODE compares for equality with implicit type coercion (it
    // converts the selector and every search value to the type of the *first*
    // search value). PostgreSQL's `=` has no such coercion, so
    // `DECODE(sys_context(...), 0, ...)` — text selector, integer search value —
    // is `operator does not exist: text = integer`. Compare both operands as
    // text: equality is preserved for the string / enum / small-integer cases
    // DECODE is actually used for.
    let as_text = |e: &Expr| Expr::Cast {
        kind: sqlparser::ast::CastKind::Cast,
        expr: Box::new(e.clone()),
        data_type: sqlparser::ast::DataType::Text,
        array: false,
        format: None,
    };
    let mut conditions = Vec::new();
    for [cond, result] in args[1..pair_end].as_chunks::<2>().0 {
        conditions.push(CaseWhen {
            condition: Expr::IsNotDistinctFrom(Box::new(as_text(&input)), Box::new(as_text(cond))),
            result: result.clone(),
        });
    }
    Ok(Expr::Case {
        case_token: no_token(),
        end_token: no_token(),
        operand: None,
        conditions,
        else_result: if pair_end < args.len() {
            Some(Box::new(args[pair_end].clone()))
        } else {
            None
        },
    })
}

fn rewrite_nvl2(args: Vec<Expr>) -> Result<Expr> {
    if args.len() != 3 {
        return Err(Error::SqlParse(
            "NVL2 requires exactly three arguments".into(),
        ));
    }
    Ok(Expr::Case {
        case_token: no_token(),
        end_token: no_token(),
        operand: None,
        conditions: vec![CaseWhen {
            condition: Expr::IsNotNull(Box::new(args[0].clone())),
            result: args[1].clone(),
        }],
        else_result: Some(Box::new(args[2].clone())),
    })
}

fn rewrite_lnnvl(args: Vec<Expr>) -> Result<Expr> {
    if args.len() != 1 {
        return Err(Error::SqlParse(
            "LNNVL requires exactly one argument".into(),
        ));
    }
    Ok(Expr::IsNotTrue(Box::new(args[0].clone())))
}

/// Rewrite Oracle legacy `(+)` outer joins (any number of comma-separated
/// tables, any number of `(+)` predicates) into ANSI `JOIN`s.
fn translate_legacy_outer_join(select: &mut sqlparser::ast::Select) -> Result<()> {
    let Some(selection) = select.selection.take() else {
        return Ok(());
    };
    if !expr_has_outer_join(&selection) || select.from.len() < 2 {
        select.selection = Some(selection);
        return Ok(());
    }
    // Only the simple comma-join shape (no pre-existing ANSI joins).
    if select.from.iter().any(|t| !t.joins.is_empty()) {
        select.selection = Some(selection);
        return Ok(());
    }

    let table_keys: Vec<String> = select
        .from
        .iter()
        .map(|t| table_factor_key(&t.relation))
        .collect();

    let mut conjuncts = Vec::new();
    flatten_and(selection, &mut conjuncts);

    struct JoinPred {
        tables: Vec<String>,
        marked: Vec<String>, // tables that appeared inside a `(+)`
        expr: Expr,
    }
    let mut join_preds: Vec<JoinPred> = Vec::new();
    let mut filters: Vec<Expr> = Vec::new();
    for mut c in conjuncts {
        if !expr_has_outer_join(&c) {
            filters.push(c);
            continue;
        }
        let mut marked = Vec::new();
        collect_outer_marked_keys(&c, &mut marked);
        strip_outer_join(&mut c);
        let mut tables = Vec::new();
        collect_table_keys(&c, &mut tables);
        tables.retain(|t| table_keys.contains(t));
        tables.dedup();
        join_preds.push(JoinPred {
            tables,
            marked,
            expr: c,
        });
    }

    let outer_tables: std::collections::HashSet<&String> =
        join_preds.iter().flat_map(|p| p.marked.iter()).collect();

    // Assemble: first FROM entry is the base; the rest become JOINs in order.
    let mut relations: Vec<TableFactor> = select.from.drain(..).map(|t| t.relation).collect();
    let base = relations.remove(0);
    let mut placed: Vec<String> = vec![table_keys[0].clone()];
    let mut joins: Vec<Join> = Vec::new();
    let mut used = vec![false; join_preds.len()];

    for (offset, relation) in relations.into_iter().enumerate() {
        let key = &table_keys[offset + 1];
        // Predicates that connect this table to something already placed, or
        // that only mention this table (an outer-join filter like `d.c(+) = 1`).
        let mut on_parts: Vec<Expr> = Vec::new();
        for (i, p) in join_preds.iter().enumerate() {
            if used[i] || !p.tables.contains(key) {
                continue;
            }
            let connects =
                p.tables.len() == 1 || p.tables.iter().any(|t| t != key && placed.contains(t));
            if connects {
                on_parts.push(p.expr.clone());
                used[i] = true;
            }
        }
        let constraint = match rebuild_and(on_parts) {
            Some(on) => JoinConstraint::On(on),
            None => JoinConstraint::None,
        };
        let op = if outer_tables.contains(key) {
            JoinOperator::LeftOuter(constraint)
        } else if matches!(constraint, JoinConstraint::None) {
            JoinOperator::CrossJoin(JoinConstraint::None)
        } else {
            JoinOperator::Inner(constraint)
        };
        joins.push(Join {
            relation,
            global: false,
            join_operator: op,
        });
        placed.push(key.clone());
    }

    // Any join predicate we could not place stays as a filter.
    for (i, p) in join_preds.into_iter().enumerate() {
        if !used[i] {
            filters.push(p.expr);
        }
    }

    select.from = vec![sqlparser::ast::TableWithJoins {
        relation: base,
        joins,
    }];
    select.selection = rebuild_and(filters);
    Ok(())
}

fn expr_has_outer_join(e: &Expr) -> bool {
    let mut found = false;
    visit_expr(e, &mut |x| {
        if matches!(x, Expr::OuterJoin(_)) {
            found = true;
        }
    });
    found
}

fn collect_outer_marked_keys(e: &Expr, out: &mut Vec<String>) {
    visit_expr(e, &mut |x| {
        if let Expr::OuterJoin(inner) = x {
            collect_table_keys(inner, out);
        }
    });
}

fn collect_table_keys(e: &Expr, out: &mut Vec<String>) {
    visit_expr(e, &mut |x| {
        if let Expr::CompoundIdentifier(parts) = x
            && parts.len() >= 2
        {
            let k = parts[0].value.to_ascii_lowercase();
            if !out.contains(&k) {
                out.push(k);
            }
        }
    });
}

fn strip_outer_join(e: &mut Expr) {
    map_expr(e, &mut |x| {
        if let Expr::OuterJoin(inner) = x {
            *x = (**inner).clone();
        }
    });
}

fn table_factor_key(t: &TableFactor) -> String {
    match t {
        TableFactor::Table { name, alias, .. } => alias
            .as_ref()
            .map(|a| a.name.value.clone())
            .or_else(|| name_last(name).map(str::to_string))
            .unwrap_or_default()
            .to_ascii_lowercase(),
        TableFactor::Derived { alias, .. } | TableFactor::TableFunction { alias, .. } => alias
            .as_ref()
            .map(|a| a.name.value.to_ascii_lowercase())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Pre-order walk of every sub-expression (best-effort over the common variants).
fn visit_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::BinaryOp { left, right, .. } => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::OuterJoin(expr) => visit_expr(expr, f),
        Expr::Between {
            expr, low, high, ..
        } => {
            visit_expr(expr, f);
            visit_expr(low, f);
            visit_expr(high, f);
        }
        Expr::InList { expr, list, .. } => {
            visit_expr(expr, f);
            list.iter().for_each(|x| visit_expr(x, f));
        }
        Expr::Function(func) => {
            if let FunctionArguments::List(l) = &func.args {
                for a in &l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) = a {
                        visit_expr(x, f);
                    }
                }
            }
        }
        _ => {}
    }
}

fn map_expr(e: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    match e {
        Expr::BinaryOp { left, right, .. } => {
            map_expr(left, f);
            map_expr(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => map_expr(expr, f),
        Expr::Between {
            expr, low, high, ..
        } => {
            map_expr(expr, f);
            map_expr(low, f);
            map_expr(high, f);
        }
        Expr::InList { expr, list, .. } => {
            map_expr(expr, f);
            list.iter_mut().for_each(|x| map_expr(x, f));
        }
        Expr::Function(func) => {
            if let FunctionArguments::List(l) = &mut func.args {
                for a in &mut l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) = a {
                        map_expr(x, f);
                    }
                }
            }
        }
        _ => {}
    }
    f(e);
}

/// What a single `ROWNUM ...` comparison means as a row limit.
#[allow(clippy::large_enum_variant)]
enum RownumEffect {
    /// `ROWNUM <= n` etc. -> `LIMIT <expr>`.
    Limit(Expr),
    /// `ROWNUM = 2`, `ROWNUM > 1` etc. -> the query returns nothing.
    Empty,
    /// `ROWNUM >= 1`, `ROWNUM > 0` -> tautology, drop the predicate.
    AllRows,
}

/// Split a WHERE clause on top-level `AND`, lift any `ROWNUM` comparison out to a
/// row limit, and return `(remaining predicate, limit expr)`.
fn strip_rownum_predicate(selection: Expr) -> (Option<Expr>, Option<Expr>) {
    let mut conjuncts = Vec::new();
    flatten_and(selection, &mut conjuncts);

    let mut kept: Vec<Expr> = Vec::new();
    let mut limit: Option<Expr> = None;
    let mut forced_empty = false;

    for conjunct in conjuncts {
        match rownum_effect(&conjunct) {
            Some(RownumEffect::Limit(expr)) => {
                // Multiple ROWNUM bounds: keep the tighter one.
                limit = Some(match limit.take() {
                    Some(existing) => least_expr(existing, expr),
                    None => expr,
                });
            }
            Some(RownumEffect::Empty) => forced_empty = true,
            Some(RownumEffect::AllRows) => {}
            None => kept.push(conjunct),
        }
    }

    if forced_empty {
        return (
            rebuild_and(kept),
            Some(lit(Value::Number("0".into(), false))),
        );
    }
    (rebuild_and(kept), limit)
}

fn flatten_and(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            flatten_and(*left, out);
            flatten_and(*right, out);
        }
        Expr::Nested(inner) => flatten_and(*inner, out),
        other => out.push(other),
    }
}

fn rebuild_and(mut conjuncts: Vec<Expr>) -> Option<Expr> {
    if conjuncts.is_empty() {
        return None;
    }
    let mut acc = conjuncts.remove(0);
    for next in conjuncts {
        acc = Expr::BinaryOp {
            left: Box::new(acc),
            op: BinaryOperator::And,
            right: Box::new(next),
        };
    }
    Some(acc)
}

fn least_expr(a: Expr, b: Expr) -> Expr {
    match (const_u64(&a), const_u64(&b)) {
        (Some(x), Some(y)) if y < x => b,
        (Some(_), Some(_)) => a,
        _ => Expr::Function(sqlparser::ast::Function {
            name: obj_name("LEAST"),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(a)),
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(b)),
                ],
                clauses: vec![],
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: vec![],
        }),
    }
}

fn const_u64(expr: &Expr) -> Option<u64> {
    if let Some(Value::Number(n, _)) = as_value(expr) {
        return n.parse().ok();
    }
    match expr {
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            ..
        } => Some(0), // a negative bound is <= 0
        _ => None,
    }
}

/// Interpret one `ROWNUM <op> n` (or `n <op> ROWNUM`, or `ROWNUM BETWEEN a AND b`).
fn rownum_effect(expr: &Expr) -> Option<RownumEffect> {
    let is_rownum =
        |e: &Expr| matches!(e, Expr::Identifier(i) if i.value.eq_ignore_ascii_case("rownum"));

    if let Expr::Between {
        expr: subject,
        negated: false,
        low,
        high,
    } = expr
        && is_rownum(subject)
    {
        let lo = const_u64(low)?;
        return Some(if lo > 1 {
            RownumEffect::Empty
        } else {
            match const_u64(high) {
                Some(h) if h < 1 => RownumEffect::Empty,
                _ => RownumEffect::Limit((**high).clone()),
            }
        });
    }

    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    // Normalise `n <op> ROWNUM` to `ROWNUM <flipped> n`.
    let (op, rownum_side, bound) = if !is_rownum(left) && is_rownum(right) {
        (flip_comparison(op)?, right.as_ref(), left.as_ref())
    } else {
        (op.clone(), left.as_ref(), right.as_ref())
    };

    // `ROWNUM +/- k <op> m`  ->  `ROWNUM <op> (m -/+ k)`.
    let bound_owned;
    let bound = if let Expr::BinaryOp {
        left: inner,
        op: arith @ (BinaryOperator::Plus | BinaryOperator::Minus),
        right: k,
    } = rownum_side
        && is_rownum(inner)
        && let (Some(m), Some(k)) = (const_u64(bound), const_u64(k))
    {
        let adjusted = if *arith == BinaryOperator::Plus {
            m.saturating_sub(k)
        } else {
            m + k
        };
        bound_owned = num(adjusted);
        &bound_owned
    } else if is_rownum(rownum_side) {
        bound
    } else {
        return None;
    };

    // A bound we cannot fold to a constant — a bind placeholder (possibly
    // wrapped in `::text::numeric` casts, as Hibernate's Oracle10g pagination
    // emits it), a column reference, an arithmetic expression — still yields a
    // valid `LIMIT <expr>` in PostgreSQL for the `<=` / `<` upper-bound forms.
    let dynamic = const_u64(bound).is_none();
    match op {
        BinaryOperator::LtEq if dynamic => Some(RownumEffect::Limit(bound.clone())),
        BinaryOperator::Lt if dynamic => Some(RownumEffect::Limit(Expr::BinaryOp {
            left: Box::new(bound.clone()),
            op: BinaryOperator::Minus,
            right: Box::new(lit(Value::Number("1".into(), false))),
        })),
        _ => {
            let n = const_u64(bound)?;
            Some(match op {
                BinaryOperator::LtEq if n < 1 => RownumEffect::Empty,
                BinaryOperator::LtEq => RownumEffect::Limit(num(n)),
                BinaryOperator::Lt if n <= 1 => RownumEffect::Empty,
                BinaryOperator::Lt => RownumEffect::Limit(num(n - 1)),
                BinaryOperator::Eq if n == 1 => RownumEffect::Limit(num(1)),
                BinaryOperator::Eq => RownumEffect::Empty,
                BinaryOperator::NotEq if n == 1 => RownumEffect::Empty,
                BinaryOperator::NotEq => RownumEffect::AllRows,
                BinaryOperator::GtEq if n <= 1 => RownumEffect::AllRows,
                BinaryOperator::GtEq => RownumEffect::Empty,
                BinaryOperator::Gt if n < 1 => RownumEffect::AllRows,
                BinaryOperator::Gt => RownumEffect::Empty,
                _ => return None,
            })
        }
    }
}

fn num(value: u64) -> Expr {
    lit(Value::Number(value.to_string(), false))
}

fn flip_comparison(op: &BinaryOperator) -> Option<BinaryOperator> {
    Some(match op {
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        BinaryOperator::Eq => BinaryOperator::Eq,
        BinaryOperator::NotEq => BinaryOperator::NotEq,
        _ => return None,
    })
}

/// Parse a small **static** SQL expression snippet into an AST node. Callers
/// pass compile-time-constant SQL only; a failure here is a programmer error.
fn parse_expr(sql: &str) -> Expr {
    Parser::new(&GenericDialect {})
        .try_with_sql(sql)
        .and_then(|mut p| p.parse_expr())
        .expect("static expression snippet parses")
}

/// Parse an expression snippet built from re-serialized AST fragments. Unlike
/// [`parse_expr`], the input is not statically known, so a parse failure is
/// reported rather than panicked — the connection stays up and the backend
/// gets the untranslated form.
fn try_parse_expr(sql: &str) -> Result<Expr> {
    Parser::new(&GenericDialect {})
        .try_with_sql(sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| {
            Error::SqlParse(format!(
                "internal expression snippet failed to parse ({e}): {sql}"
            ))
        })
}

/// `SELECT <projection> FROM (<inner>) AS __rownum_sub ORDER BY <order_by>`.
///
/// Re-assembled through the parser rather than by hand: sqlparser 0.62's
/// `Select`/`Query` carry ~20 dialect/token fields that are irrelevant here.
fn wrap_query_with_order_by(
    inner: Query,
    order_by: Option<OrderBy>,
    projection: Vec<sqlparser::ast::SelectItem>,
) -> Result<Query> {
    let projection = projection
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let order_by = order_by.map(|ob| ob.to_string()).unwrap_or_default();
    parse_query(&format!(
        "SELECT {projection} FROM ({inner}) AS __rownum_sub {order_by}"
    ))
}

#[cfg(test)]
mod tests {
    use super::oracle_to_postgres;

    #[test]
    fn translates_dual_through_the_oracle_ast() {
        assert_eq!(
            oracle_to_postgres("SELECT 1 FROM DUAL").unwrap(),
            "SELECT 1"
        );
        assert_eq!(
            oracle_to_postgres("SELECT 1 FROM dual WHERE 1 = 1").unwrap(),
            "SELECT 1 WHERE 1 = 1"
        );
    }

    #[test]
    fn translates_simple_rownum_predicates_to_limit() {
        assert_eq!(
            oracle_to_postgres("SELECT name FROM people WHERE team_id = 1 AND ROWNUM <= 2")
                .unwrap(),
            "SELECT name FROM people WHERE team_id = 1 LIMIT 2"
        );
        assert_eq!(
            oracle_to_postgres("SELECT name FROM people WHERE ROWNUM < 3").unwrap(),
            "SELECT name FROM people LIMIT 2"
        );
    }

    #[test]
    fn translates_simple_legacy_outer_join_before_ast_parse() {
        assert_eq!(
            oracle_to_postgres("SELECT a.id, b.name FROM a, b WHERE a.id = b.a_id (+)").unwrap(),
            "SELECT a.id, b.name FROM a LEFT JOIN b ON a.id = b.a_id"
        );
        assert_eq!(
            oracle_to_postgres(
                "SELECT p.name FROM people p, teams t WHERE p.team_id = t.id (+) AND p.id > 1 ORDER BY p.id"
            )
            .unwrap(),
            "SELECT p.name FROM people p LEFT JOIN teams t ON p.team_id = t.id WHERE p.id > 1 ORDER BY p.id"
        );
    }

    #[test]
    fn preserves_set_operations_and_rejects_multi_statement_input() {
        assert_eq!(
            oracle_to_postgres("SELECT 1 FROM DUAL EXCEPT SELECT 1 FROM DUAL").unwrap(),
            "SELECT 1 EXCEPT SELECT 1"
        );
        assert!(oracle_to_postgres("SELECT 1 FROM DUAL; SELECT 2 FROM DUAL").is_err());
    }

    #[test]
    fn translates_common_oracle_tokens_without_touching_literals() {
        assert_eq!(
            oracle_to_postgres(
                "SELECT NVL(NULL, 'SYSDATE'), SYSDATE FROM DUAL MINUS SELECT 'x', SYSDATE FROM DUAL"
            )
            .unwrap(),
            "SELECT COALESCE(NULL, 'SYSDATE'), CURRENT_TIMESTAMP EXCEPT SELECT 'x', CURRENT_TIMESTAMP"
        );
    }

    #[test]
    fn preserves_quoted_identifiers_escaped_literals_and_comments() {
        assert_eq!(
            oracle_to_postgres(
                "SELECT 'O''Reilly SYSDATE', \"NUMBER\", NVL(NULL, 'x') /* MINUS SYSDATE */ FROM DUAL -- NVL\n"
            )
            .unwrap(),
            // `"NUMBER"` is an identifier, not the type name, so it is never
            // rewritten to NUMERIC. It is an all-upper quoted identifier, which
            // in Oracle is the same object as the bare form, so it folds to the
            // lower-case quoted form PostgreSQL's bare-identifier folding lands
            // on. The string literal, comment and `--` line are untouched.
            "SELECT 'O''Reilly SYSDATE', \"number\", COALESCE(NULL, 'x')"
        );
    }

    #[test]
    fn translates_common_oracle_ddl_types() {
        assert_eq!(
            oracle_to_postgres(
                "CREATE TABLE oracle_ddl_people (id NUMBER(10) PRIMARY KEY, label VARCHAR2(30), note CLOB, created_at DATE DEFAULT SYSDATE)"
            )
            .unwrap(),
            // A `DATE` column becomes `timestamp(0)` so it keeps its
            // time-of-day (Oracle DATE is second-precision date+time).
            "CREATE TABLE oracle_ddl_people (id NUMERIC(10) PRIMARY KEY, label VARCHAR(30), note TEXT, created_at TIMESTAMP(0) DEFAULT CURRENT_TIMESTAMP)"
        );
    }

    #[test]
    fn ports_orafce_decode_nvl2_and_lnnvl_cases() {
        assert_eq!(
            oracle_to_postgres("SELECT DECODE(2, 1, 'one', 2, 'two', 'other') FROM DUAL").unwrap(),
            "SELECT CASE WHEN CAST(2 AS TEXT) IS NOT DISTINCT FROM CAST(1 AS TEXT) THEN 'one' WHEN CAST(2 AS TEXT) IS NOT DISTINCT FROM CAST(2 AS TEXT) THEN 'two' ELSE 'other' END"
        );
        assert_eq!(
            oracle_to_postgres("SELECT NVL2(NULL, 'yes', 'no') FROM DUAL").unwrap(),
            "SELECT CASE WHEN NULL IS NOT NULL THEN 'yes' ELSE 'no' END"
        );
        assert_eq!(
            oracle_to_postgres("SELECT LNNVL(1 = 1) FROM DUAL").unwrap(),
            "SELECT 1 = 1 IS NOT TRUE"
        );
    }

    #[test]
    fn rewrites_oracle_expressions_across_query_clauses() {
        assert_eq!(
            oracle_to_postgres(
                "SELECT p.team_id, COUNT(*) FROM people p JOIN teams t ON NVL(p.team_id, 0) = t.id GROUP BY p.team_id HAVING LNNVL(COUNT(*) = 0) ORDER BY DECODE(p.team_id, 1, 0, 1)"
            )
            .unwrap(),
            "SELECT p.team_id, COUNT(*) FROM people p JOIN teams t ON COALESCE(p.team_id, 0) = t.id GROUP BY p.team_id HAVING COUNT(*) = 0 IS NOT TRUE ORDER BY CASE WHEN CAST(p.team_id AS TEXT) IS NOT DISTINCT FROM CAST(1 AS TEXT) THEN 0 ELSE 1 END"
        );
    }

    #[test]
    fn lowers_merge_update_delete_where_using_post_update_values() {
        assert_eq!(
            oracle_to_postgres(
                "MERGE INTO mtgt d USING (SELECT 2 AS id FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.val = 'updated' DELETE WHERE d.val = 'updated'"
            )
            .unwrap(),
            "MERGE INTO mtgt d USING (SELECT 2 AS id FROM DUAL) s ON (d.id = s.id) WHEN MATCHED AND (('updated') = 'updated') THEN DELETE WHEN MATCHED THEN UPDATE SET val = 'updated'"
        );
    }

    #[test]
    fn preserves_offset_when_formatting_timestamptz_literal() {
        assert_eq!(
            oracle_to_postgres("SELECT TO_CHAR(TIMESTAMP '2024-06-01 12:00:00 +05:00', 'HH24:MI TZH:TZM') FROM DUAL").unwrap(),
            "SELECT COALESCE(CAST(TO_CHAR(TIMESTAMPTZ '2024-06-01 12:00:00 +05:00' AT TIME ZONE 'UTC-05:00', 'HH24:MI') AS TEXT), '') || ' +05:00'"
        );
    }
}
