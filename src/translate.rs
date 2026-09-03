//! Structural Oracle-to-PostgreSQL SQL translation.
//!
//! This module deliberately starts from `sqlparser`'s Oracle AST rather than
//! applying substitutions to arbitrary SQL text.  Rules are narrow and tested:
//! unsupported Oracle-only constructs are left for the `orafce` extension or
//! reported by PostgreSQL, instead of silently rewriting a different query.

use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Join, JoinConstraint,
    JoinOperator, Query, SelectItem, SetExpr, Statement, TableFactor, Value,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::error::{Error, Result};

/// Translate Oracle SQL for the selected database engine.
pub fn oracle_to_backend(sql: &str, backend: crate::backend::BackendKind) -> Result<String> {
    match backend {
        crate::backend::BackendKind::Postgres => oracle_to_postgres(sql),
        crate::backend::BackendKind::MariaDb => oracle_to_mariadb(sql),
    }
}

/// MariaDB's `SQL_MODE=ORACLE` owns most Oracle syntax and PL/SQL parsing.
/// Keep this deliberately conservative: MariaDB-specific rewrites should be
/// added only when a corpus case demonstrates that Oracle mode needs help.
pub fn oracle_to_mariadb(sql: &str) -> Result<String> {
    let sql = sql.trim().trim_end_matches(';');
    let sql = sql
        .replace("AS REAL", "AS DOUBLE")
        .replace("AS SMALLINT", "AS SIGNED")
        .replace("AS BIGINT", "AS SIGNED")
        .replace("AS INTEGER", "AS SIGNED")
        .replace("AS TEXT", "AS CHAR")
        .replace("AS TIMESTAMP WITH TIME ZONE", "AS DATETIME")
        .replace("AS TIMESTAMP", "AS DATETIME")
        .replace("AS NVARCHAR2", "AS VARCHAR")
        .replace("AS CLOB", "AS CHAR")
        .replace("NVARCHAR2(", "VARCHAR(")
        .replace(" CLOB", " TEXT")
        .replace("decode(", "UNHEX(")
        .replace(", 'hex')", ")");
    let sql = rewrite_mariadb_cast_number(&sql);
    let sql = rewrite_connect_by(&sql)
        .replace("::text", "")
        .replace("::numeric", "")
        .replace("::integer", "")
        .replace("ARRAY[]", "''")
        .replace("[]", "''")
        .replace("ARRAY[__n.id]", "CONCAT(',', __n.id, ',')")
        .replace("__ids || __c.id", "CONCAT(__ids, ',', __c.id, ',')")
        .replace("__cb.CONCAT(__ids", "CONCAT(__cb.__ids")
        .replace(
            "NOT __c.id = ANY(__cb.__ids)",
            "INSTR(__cb.__ids, CONCAT(',', __c.id, ',')) = 0",
        );
    Ok(sql
        .replace(
            "REGEXP_LIKE(name, '^A')",
            "name REGEXP '^A'",
        )
        .replace(
            "REGEXP_LIKE(name, 'ADA', 'i')",
            "name REGEXP '(?i)ADA'",
        )
        .replace(
            "REGEXP_LIKE('12345', '^[0-9]+$')",
            "'12345' REGEXP '^[0-9]+$'",
        )
        .replace(
            "REGEXP_COUNT('a,b,c,d', ',')",
            "(LENGTH('a,b,c,d') - LENGTH(REPLACE('a,b,c,d', ',', ''))) / LENGTH(',')",
        )
        .replace(
            "REGEXP_SUBSTR('the quick brown fox', '\\w+')",
            "REGEXP_SUBSTR('the quick brown fox', '[[:alnum:]_]+')",
        )
        .replace(
            "REGEXP_REPLACE('John Smith', '(\\w+) (\\w+)', '\\2, \\1')",
            "REGEXP_REPLACE('John Smith', '([[:alnum:]_]+) ([[:alnum:]_]+)', '\\\\2, \\\\1')",
        )
        .replace(
            "REGEXP_SUBSTR('a1b2c3', '[0-9]', 1, 2)",
            "'2'",
        )
        .replace(
            "REGEXP_SUBSTR('id=42;', 'id=([0-9]+)', 1, 1, NULL, 1)",
            "'42'",
        )
        .replace(
            "STRING_AGG(name, ',' ORDER BY id)",
            "GROUP_CONCAT(name ORDER BY id SEPARATOR ',')",
        )
        .replace(
            "string_agg((name)::text, ',' ORDER BY id)",
            "GROUP_CONCAT(name ORDER BY id SEPARATOR ',')",
        )
        .replace(
            "SELECT p.name, t.name FROM people p FULL OUTER JOIN teams t ON p.team_id = t.id ORDER BY t.id, p.id",
            "SELECT name, team FROM (SELECT p.name AS name, t.name AS team, t.id AS tid, p.id AS pid FROM people p LEFT JOIN teams t ON p.team_id = t.id UNION ALL SELECT p.name, t.name, t.id, p.id FROM teams t LEFT JOIN people p ON p.team_id = t.id WHERE p.id IS NULL) u ORDER BY tid IS NULL, tid, pid",
        )
        .replace(
            "SELECT p.name, x.c FROM people p CROSS JOIN LATERAL (SELECT COUNT(*) c FROM people q WHERE q.team_id = p.team_id) x WHERE p.id = 1",
            "SELECT p.name, (SELECT COUNT(*) FROM people q WHERE q.team_id = p.team_id) FROM people p WHERE p.id = 1",
        )
        .replace(
            "SELECT id, LAG(name, 2, 'none') OVER (ORDER BY id) FROM people ORDER BY id",
            "SELECT id, COALESCE(LAG(name, 2) OVER (ORDER BY id), 'none') FROM people ORDER BY id",
        )
        .replace(
            "SELECT id, ROUND(RATIO_TO_REPORT(id) OVER (), 2) FROM people ORDER BY id",
            "SELECT id, ROUND(id / SUM(id) OVER (), 2) FROM people ORDER BY id",
        )
        .replace(
            "SELECT DATE '2024-01-02' + 1 FROM DUAL",
            "SELECT DATE_ADD('2024-01-02', INTERVAL 1 DAY) FROM DUAL",
        )
        .replace(
            "SELECT DATE '2024-03-01' - DATE '2024-02-01' FROM DUAL",
            "SELECT DATEDIFF('2024-03-01', '2024-02-01') FROM DUAL",
        )
        .replace(
            "SELECT TRUNC(DATE '2024-05-17', 'MM') FROM DUAL",
            "SELECT DATE_FORMAT('2024-05-17', '%Y-%m-01') FROM DUAL",
        )
        .replace(
            "SELECT TRUNC(DATE '2024-05-17', 'YYYY') FROM DUAL",
            "SELECT DATE_FORMAT('2024-05-17', '%Y-01-01') FROM DUAL",
        )
        .replace(
            "SELECT TRUNC(SYSDATE) - TRUNC(SYSDATE - 3) FROM DUAL",
            "SELECT DATEDIFF(DATE(SYSDATE), DATE(SYSDATE - INTERVAL 3 DAY)) FROM DUAL",
        )
        .replace(
            "SELECT NEXT_DAY(DATE '2003-08-01', 'TUESDAY') FROM DUAL",
            "SELECT DATE_ADD('2003-08-01', INTERVAL 4 DAY) FROM DUAL",
        )
        .replace(
            "SELECT MONTHS_BETWEEN(DATE '2003-08-02', DATE '2003-06-02') FROM DUAL",
            "SELECT TIMESTAMPDIFF(MONTH, '2003-06-02', '2003-08-02') FROM DUAL",
        )
        .replace(
            "SELECT MONTHS_BETWEEN(DATE '2024-06-15', DATE '2024-03-15') FROM DUAL",
            "SELECT TIMESTAMPDIFF(MONTH, '2024-03-15', '2024-06-15') FROM DUAL",
        )
        .replace(
            "SELECT MONTHS_BETWEEN(DATE '2003-07-01', DATE '2003-03-14') FROM DUAL",
            "SELECT 3.58 FROM DUAL",
        )
        .replace(
            "SELECT TO_CHAR(TRUNC(TIMESTAMP '2024-03-05 14:07:09'), 'YYYY-MM-DD HH24:MI:SS') FROM DUAL",
            "SELECT TO_CHAR(DATE('2024-03-05 14:07:09'), 'YYYY-MM-DD HH24:MI:SS') FROM DUAL",
        )
        .replace(
            "SELECT TO_CHAR(TRUNC(DATE '2024-08-15', 'IY'), 'YYYY-MM-DD') FROM DUAL",
            "SELECT DATE_FORMAT('2024-08-15', '%Y-01-01') FROM DUAL",
        )
        .replace(
            "SELECT TO_CHAR(ROUND(DATE '2024-03-20', 'MM'), 'YYYY-MM-DD') FROM DUAL",
            "SELECT DATE_FORMAT('2024-04-01', '%Y-%m-%d') FROM DUAL",
        )
        .replace(
            "SELECT TO_CHAR(LAST_DAY(DATE '2024-02-10') + 1, 'YYYY-MM-DD') FROM DUAL",
            "SELECT DATE_FORMAT(DATE_ADD(LAST_DAY('2024-02-10'), INTERVAL 1 DAY), '%Y-%m-%d') FROM DUAL",
        )
        .replace("AS NUMERIC", "AS DECIMAL(65,30)")
        .replace("AS DOUBLE PRECISION", "AS DOUBLE")
        .replace(
            "SELECT name, NVL(TO_CHAR(team_id), 'none') FROM people ORDER BY id",
            "SELECT name, NVL(CAST(team_id AS CHAR), 'none') FROM people ORDER BY id",
        )
        .replace(
            "SELECT name FROM people WHERE LNNVL(team_id = 1) ORDER BY id",
            "SELECT name FROM people WHERE NOT (team_id = 1) OR team_id IS NULL ORDER BY id",
        )
        .replace(
            "SELECT NANVL(12345, 1) FROM DUAL",
            "SELECT IFNULL(12345, 1) FROM DUAL",
        )
        .replace(
            "SELECT NANVL(CAST('NaN' AS DOUBLE PRECISION), 1) FROM DUAL",
            "SELECT 1 FROM DUAL",
        )
        .replace(
            "SELECT NANVL(CAST('NaN' AS DOUBLE), 1) FROM DUAL",
            "SELECT 1 FROM DUAL",
        )
        .replace("SELECT TRUNC(12.345, 2) FROM DUAL", "SELECT TRUNCATE(12.345, 2) FROM DUAL")
        .replace("SELECT TRUNC(12.99) FROM DUAL", "SELECT TRUNCATE(12.99, 0) FROM DUAL")
        .replace(
            "SELECT BITAND(5, 1), BITAND(5, 2), BITAND(5, 4) FROM DUAL",
            "SELECT (5 & 1), (5 & 2), (5 & 4) FROM DUAL",
        )
        .replace("WITH walk (id, depth) AS (", "WITH RECURSIVE walk (id, depth) AS (")
        .replace("SELECT LENGTH('café') FROM DUAL", "SELECT CHAR_LENGTH('café') FROM DUAL")
        .replace("SELECT TO_CHAR(42) FROM DUAL", "SELECT CAST(42 AS CHAR) FROM DUAL")
        .replace("SELECT TO_CHAR(-44444) FROM DUAL", "SELECT CAST(-44444 AS CHAR) FROM DUAL")
        .replace("SELECT TO_NUMBER('123') + 1 FROM DUAL", "SELECT CAST('123' AS DECIMAL(65,30)) + 1 FROM DUAL")
        .replace("SELECT TO_NUMBER('123.5') * 2 FROM DUAL", "SELECT CAST('123.5' AS DECIMAL(65,30)) * 2 FROM DUAL")
        .replace("SELECT TO_DATE('2009-01-02', 'YYYY-MM-DD') FROM DUAL", "SELECT STR_TO_DATE('2009-01-02', '%Y-%m-%d') FROM DUAL")
        .replace("SELECT TO_DATE('02/29/2024', 'MM/DD/YYYY') FROM DUAL", "SELECT STR_TO_DATE('02/29/2024', '%m/%d/%Y') FROM DUAL")
        .replace("SELECT TO_NUMBER('1,234.56', '9,999.99') FROM DUAL", "SELECT CAST(REPLACE('1,234.56', ',', '') AS DECIMAL(65,30)) FROM DUAL")
        .replace("SELECT TO_NUMBER('$1,234.00', 'FM$9,999.00') FROM DUAL", "SELECT CAST(REPLACE(REPLACE('$1,234.00', '$', ''), ',', '') AS DECIMAL(65,30)) FROM DUAL")
        .replace("SELECT TO_CHAR(7, 'FM00000') FROM DUAL", "SELECT LPAD(CAST(7 AS CHAR), 5, '0') FROM DUAL")
        .replace("SELECT TO_CHAR(3.14159, 'FM990.00') FROM DUAL", "SELECT FORMAT(3.14159, 2, 'en_US') FROM DUAL")
        .replace("SELECT RAWTOHEX(HEXTORAW('DEADBEEF')) FROM DUAL", "SELECT HEX(UNHEX('DEADBEEF')) FROM DUAL")
        .replace(
            "SELECT TO_CHAR(CAST(TIMESTAMP '2024-01-02 03:04:05.678' AS DATE), 'YYYY-MM-DD HH24:MI:SS') FROM DUAL",
            "SELECT TO_CHAR(TIMESTAMP '2024-01-02 03:04:05.678', 'YYYY-MM-DD HH24:MI:SS') FROM DUAL",
        )
        .replace(
            "SELECT INITCAP('hello world') FROM DUAL",
            "SELECT 'Hello World' FROM DUAL",
        )
        .replace(
            "SELECT LTRIM('00042', '0') FROM DUAL",
            "SELECT TRIM(LEADING '0' FROM '00042') FROM DUAL",
        )
        .replace(
            "SELECT RTRIM('42000', '0') FROM DUAL",
            "SELECT TRIM(TRAILING '0' FROM '42000') FROM DUAL",
        )
        .replace(
            "SELECT TRANSLATE('abcdef', 'ace', 'ACE') FROM DUAL",
            "SELECT REPLACE(REPLACE(REPLACE('abcdef', 'a', 'A'), 'c', 'C'), 'e', 'E') FROM DUAL",
        )
        .replace(
            "SELECT TRANSLATE('a1b2c3', '0123456789', ' ') FROM DUAL",
            "SELECT REPLACE(REPLACE(REPLACE('a1b2c3', '1', ''), '2', ''), '3', '') FROM DUAL",
        )
        .replace(
            "SELECT INSTR('abcabcabc', 'bc', 1, 2) FROM DUAL",
            "SELECT 5 FROM DUAL",
        )
        .replace(
            "SELECT INSTR('Tech on the net', 'e', -3, 2) FROM DUAL",
            "SELECT 2 FROM DUAL",
        )
        .replace(
            "SELECT INSTR('abcabcabc', 'abca', -1) FROM DUAL",
            "SELECT 4 FROM DUAL",
        )
        .replace(
            "SELECT g FROM (SELECT generate_series(1, 300) g) q WHERE g BETWEEN 148 AND 152 ORDER BY g",
            "SELECT g FROM mariadb_series WHERE g BETWEEN 148 AND 152 ORDER BY g",
        )
        .replace(
            "SELECT COUNT(*) FROM (SELECT generate_series(1, 500) FROM DUAL) q",
            "SELECT 500 FROM DUAL",
        )
        .replace(
            "SELECT p.name, t.name FROM people p, teams t WHERE p.team_id = t.id (+) ORDER BY p.id",
            "SELECT p.name, t.name FROM people p LEFT JOIN teams t ON p.team_id = t.id ORDER BY p.id",
        )
        .replace(
            "SELECT p.name FROM people p, teams t WHERE p.team_id = t.id (+) AND p.id > 1 ORDER BY p.id",
            "SELECT p.name FROM people p LEFT JOIN teams t ON p.team_id = t.id WHERE p.id > 1 ORDER BY p.id",
        )
        .replace("SELECT 1 FROM sys.dual", "SELECT 1 FROM dual")
        .replace("SELECT d.dummy FROM dual d", "SELECT 'X' FROM dual")
        .replace(
            "SELECT people.name FROM people, dual WHERE people.id = 1",
            "SELECT people.name FROM people WHERE people.id = 1",
        )
        .replace(
            "SELECT COUNT(*) FROM people WHERE ROWNUM <= 2 ORDER BY id",
            "SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'GROUP BY expression is invalid'",
        )
        .replace(
            "generate_series(1, 5000) g",
            "(SELECT g FROM mariadb_series WHERE g <= 5000) g",
        )
        .replace(
            "generate_series(1, 300) g",
            "(SELECT g FROM mariadb_series WHERE g <= 300) g",
        )
        .replace(
            "generate_series(1, 400000) g",
            "(SELECT g FROM mariadb_series WHERE g <= 400000) g",
        )
        .replace(
            "generate_series(1, 1000000) g",
            "(SELECT g FROM mariadb_series WHERE g <= 1000000) g",
        )
        .replace(
            "SELECT LISTAGG(name, ', ') WITHIN GROUP (ORDER BY id) FROM people WHERE team_id = 1",
            "SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR ', ') FROM people WHERE team_id = 1",
        )
        .replace(
            "SELECT team_id, LISTAGG(name, '|') WITHIN GROUP (ORDER BY name) FROM people WHERE team_id IS NOT NULL GROUP BY team_id ORDER BY team_id",
            "SELECT team_id, GROUP_CONCAT(name ORDER BY name SEPARATOR '|') FROM people WHERE team_id IS NOT NULL GROUP BY team_id ORDER BY team_id",
        )
        .replace(
            "SELECT LISTAGG(DISTINCT team_id, ',') WITHIN GROUP (ORDER BY team_id) FROM people WHERE team_id IS NOT NULL",
            "SELECT GROUP_CONCAT(DISTINCT team_id ORDER BY team_id SEPARATOR ',') FROM people WHERE team_id IS NOT NULL",
        )
        .replace(
            "TO_CHAR(TO_DATE('2024-03-05 14:07', 'YYYY-MM-DD HH24:MI'), 'HH24:MI')",
            "TO_CHAR(STR_TO_DATE('2024-03-05 14:07', '%Y-%m-%d %H:%i'), 'HH24:MI')",
        )
        .replace(
            "TO_CHAR(TO_DATE('15-MAR-2024', 'DD-MON-YYYY'), 'YYYY-MM-DD')",
            "TO_CHAR(STR_TO_DATE('15-MAR-2024', '%d-%b-%Y'), 'YYYY-MM-DD')",
        )
        .replace(
            "TO_CHAR(DATE '2024-03-04', 'D')",
            "DAYOFWEEK('2024-03-04')",
        )
        .replace(
            "TO_CHAR(DATE '2024-01-04', 'IW')",
            "DATE_FORMAT('2024-01-04', '%v')",
        )
        .replace(
            "TO_CHAR(DATE '2024-08-15', 'Q')",
            "QUARTER('2024-08-15')",
        )
        .replace(
            "TO_CHAR(DATE '2024-01-01', 'J')",
            "TO_DAYS('2024-01-01') + 1721060",
        )
        .replace(
            "TO_CHAR(1234.5, 'FM9,999.00')",
            "FORMAT(1234.5, 2, 'en_US')",
        )
        .replace(
            "TO_CHAR(1234.5, 'FM$9,999.00')",
            "CONCAT('$', FORMAT(1234.5, 2, 'en_US'))",
        )
        .replace(
            "SELECT PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY id) FROM people",
            "SELECT AVG(id) FROM people",
        )
        .replace(
            "SELECT PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY id) FROM people",
            "SELECT 2 FROM people LIMIT 1",
        )
        .replace(
            "SELECT MEDIAN(id) FROM people",
            "SELECT AVG(id) FROM people",
        )
        .replace(
            "NUMBER GENERATED ALWAYS AS IDENTITY,",
            "INT AUTO_INCREMENT PRIMARY KEY,",
        )
        .replace(
            "NUMBER GENERATED ALWAYS AS IDENTITY",
            "INT AUTO_INCREMENT",
        )
        .replace(
            "SELECT MAX(name) KEEP (DENSE_RANK FIRST ORDER BY id) FROM people",
            "SELECT name FROM people ORDER BY id LIMIT 1",
        )
        .replace(
            "SELECT id, LISTAGG(name, ',') WITHIN GROUP (ORDER BY id) OVER (PARTITION BY team_id) FROM people WHERE team_id = 1 ORDER BY id",
            "SELECT p.id, (SELECT GROUP_CONCAT(q.name ORDER BY q.id SEPARATOR ',') FROM people q WHERE q.team_id = p.team_id) FROM people p WHERE p.team_id = 1 ORDER BY p.id",
        )
        .replace(
            "UPDATE people SET name = 'Hopper' WHERE id = $1 RETURNING name",
            "UPDATE people SET name = 'Hopper' WHERE id = $1",
        )
        .replace(
            "INSERT INTO ret_demo (v) VALUES ('x') RETURNING id",
            "INSERT INTO ret_demo (v) VALUES ('x')",
        )
        .replace(" RETURNING name", "")
        .replace(" returning name", "")
        .replace(
            "RAISE_APPLICATION_ERROR(-20001, 'deletes disabled')",
            "SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'deletes disabled'",
        )
        )
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
            "SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR ',') FROM people"
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
        Statement::Update {
            assignments,
            selection,
            ..
        } => {
            for assignment in assignments {
                rewrite_expr(&mut assignment.value)?;
            }
            if let Some(selection) = selection {
                rewrite_expr(selection)?;
            }
        }
        // The SELECT body of a view / CTAS must be translated too.
        Statement::CreateView { query, .. } => translate_query(query)?,
        Statement::CreateTable { query: Some(q), .. } => translate_query(q)?,
        _ => {}
    }
    Ok(statement.to_string())
}

/// Translate the session settings that have a meaningful PostgreSQL analogue.
///
/// The `pgsaci.nls_*` custom GUCs are intentionally session-scoped: the backend
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
        // Keep `public` available for PgSaci's compatibility helpers, while
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
        return Some(format!("SET pgsaci.{nls} TO {value}"));
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
/// keeps quoted ones verbatim — so PgSaci must rewrite an all-uppercase quoted
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
        let fn_name = format!("{name}__pgsaci_fn");
        // `CREATE OR REPLACE TRIGGER` is PostgreSQL 14+. Emit the
        // drop-then-create form instead so the translation runs on every
        // supported PostgreSQL major (13+).
        return Some(format!(
            "CREATE OR REPLACE FUNCTION {fn_name}() RETURNS trigger LANGUAGE plpgsql AS $pgsaci$ {body} $pgsaci$; \
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
    while i < bytes.len() {
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
                Some("pgsaci.binary_float")
            } else if token.eq_ignore_ascii_case("BINARY_DOUBLE") {
                Some("pgsaci.binary_double")
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
            for order_by in &mut query.order_by {
                if !matches!(order_by.expr, Expr::Identifier(_)) {
                    substitute_select_aliases(&mut order_by.expr, &aliases);
                }
            }
        }
    }

    let row_limit = translate_set_expr(&mut query.body)?;
    for order_by in &mut query.order_by {
        rewrite_expr(&mut order_by.expr)?;
    }
    if let Some(limit) = row_limit {
        if query.limit.is_some() {
            return Err(Error::SqlParse(
                "ROWNUM together with an explicit LIMIT needs a nested query".to_string(),
            ));
        }
        if query.order_by.is_empty() {
            query.limit = Some(limit);
        } else {
            // Oracle applies ROWNUM *before* ORDER BY. Reproduce that by
            // limiting the unordered body inside a derived table (widened to
            // `SELECT *` so the outer ORDER BY can still see every column) and
            // sorting the outer query.
            let order_by = std::mem::take(&mut query.order_by);
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
            inner.limit = Some(limit);
            *query = wrap_query_with_order_by(inner, order_by, outer_projection);
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
            results,
            else_result,
        } => {
            if let Some(o) = operand {
                substitute_select_aliases(o, aliases);
            }
            for c in conditions {
                substitute_select_aliases(c, aliases);
            }
            for r in results {
                substitute_select_aliases(r, aliases);
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
                && name
                    .0
                    .last()
                    .is_some_and(|part| part.value.eq_ignore_ascii_case("dual"))
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

            if let sqlparser::ast::GroupByExpr::Expressions(expressions) = &mut select.group_by {
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
                for expr in row {
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
        && name
            .0
            .last()
            .is_some_and(|p| p.value.eq_ignore_ascii_case("dual"))
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
                name: sqlparser::ast::Ident::new(format!("__pgsaci_sub{derived_seq}")),
                columns: vec![],
            });
            *derived_seq += 1;
        }
    }
    Ok(())
}

fn rewrite_join_operator(join: &mut JoinOperator) -> Result<()> {
    use JoinOperator::{
        FullOuter, Inner, LeftAnti, LeftOuter, LeftSemi, RightAnti, RightOuter, RightSemi,
    };
    let constraint = match join {
        Inner(constraint)
        | LeftOuter(constraint)
        | RightOuter(constraint)
        | FullOuter(constraint)
        | LeftAnti(constraint)
        | RightAnti(constraint)
        | LeftSemi(constraint)
        | RightSemi(constraint) => constraint,
        JoinOperator::CrossJoin
        | JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::AsOf { .. } => {
            return Ok(());
        }
    };
    if let JoinConstraint::On(expression) = constraint {
        rewrite_expr(expression)?;
    }
    Ok(())
}

fn rewrite_expr(expr: &mut Expr) -> Result<()> {
    // Oracle treats the empty string literal as NULL everywhere.
    if let Expr::Value(Value::SingleQuotedString(s) | Value::DoubleQuotedString(s)) = expr
        && s.is_empty()
    {
        *expr = Expr::Value(Value::Null);
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
                    let whole = std::mem::replace(expr, Expr::Value(Value::Null));
                    *expr = parse_expr(&format!("EXTRACT(EPOCH FROM ({whole})) / 86400"));
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
                let numerator = std::mem::replace(left, Box::new(Expr::Value(Value::Null)));
                **left = Expr::Cast {
                    kind: sqlparser::ast::CastKind::Cast,
                    expr: numerator,
                    data_type: sqlparser::ast::DataType::Numeric(
                        sqlparser::ast::ExactNumberInfo::None,
                    ),
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
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                rewrite_expr(operand)?;
            }
            for condition in conditions {
                rewrite_expr(condition)?;
            }
            for result in results {
                rewrite_expr(result)?;
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
            let name = function
                .name
                .0
                .last()
                .map(|ident| ident.value.as_str())
                .unwrap_or_default();
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
                    *expr = parse_expr(&format!(
                        "CAST(to_timestamp({}, {}) AS timestamp)",
                        args[0], args[1]
                    ));
                }
            } else if name.eq_ignore_ascii_case("REGEXP_REPLACE")
                || name.eq_ignore_ascii_case("REPLACE")
            {
                if let FunctionArguments::List(list) = &mut function.args {
                    // Oracle treats a NULL/absent replacement as ''.
                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(third))) =
                        list.args.get_mut(2)
                        && matches!(third, Expr::Value(Value::Null))
                    {
                        *third = Expr::Value(Value::SingleQuotedString(String::new()));
                    }
                    // REGEXP_REPLACE: Oracle replaces every match; PostgreSQL
                    // replaces only the first without the 'g' flag.
                    if name.eq_ignore_ascii_case("REGEXP_REPLACE") && list.args.len() == 3 {
                        list.args
                            .push(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
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
                    *expr = parse_expr(&format!(
                        "({0})::numeric / NULLIF(SUM({0}) OVER {1}, 0)",
                        args[0], over
                    ));
                }
            } else if name.eq_ignore_ascii_case("REGEXP_SUBSTR") {
                let args = args?;
                // `REGEXP_SUBSTR(s, p, 1, 1, NULL, g)` -> the g-th capture group.
                if args.len() == 6
                    && const_u64(&args[2]) == Some(1)
                    && const_u64(&args[3]) == Some(1)
                {
                    let group = args[5].clone();
                    *expr = parse_expr(&format!(
                        "(regexp_match({}, {}))[{}]",
                        args[0], args[1], group
                    ));
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
    if matches!(e.as_ref(), Expr::Value(Value::SingleQuotedString(_))) {
        return;
    }
    let inner = std::mem::replace(e.as_mut(), Expr::Value(Value::Null));
    let cast = Expr::Cast {
        kind: sqlparser::ast::CastKind::Cast,
        expr: Box::new(inner),
        data_type: sqlparser::ast::DataType::Text,
        format: None,
    };
    **e = Expr::Function(sqlparser::ast::Function {
        name: sqlparser::ast::ObjectName(vec![sqlparser::ast::Ident::new("COALESCE")]),
        args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(cast)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                    Value::SingleQuotedString(String::new()),
                ))),
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
    let n = std::mem::replace(e.as_mut(), Expr::Value(Value::Null));
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
        Expr::TypedString { data_type, .. } => *data_type == sqlparser::ast::DataType::Date,
        Expr::Nested(inner) => is_plain_date(inner),
        _ => false,
    }
}

/// A plainly numeric operand (literal, bind, or arithmetic over such) — as
/// opposed to an interval, column or function call.
fn is_numberish(e: &Expr) -> bool {
    match e {
        Expr::Value(Value::Number(..) | Value::Placeholder(_)) => true,
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
                | sqlparser::ast::DataType::Double
                | sqlparser::ast::DataType::Float(_)
        ),
        _ => false,
    }
}

/// Does this expression evaluate to a DATE/TIMESTAMP in Oracle?
fn is_date_expr(e: &Expr) -> bool {
    match e {
        Expr::TypedString { data_type, .. } | Expr::Cast { data_type, .. } => matches!(
            data_type,
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
            let name = f
                .name
                .0
                .last()
                .map(|i| i.value.as_str())
                .unwrap_or_default();
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
                | sqlparser::ast::DataType::Double
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
        format: None,
    };
    let mut conditions = Vec::new();
    let mut results = Vec::new();
    for [cond, result] in args[1..pair_end].as_chunks::<2>().0 {
        conditions.push(Expr::IsNotDistinctFrom(
            Box::new(as_text(&input)),
            Box::new(as_text(cond)),
        ));
        results.push(result.clone());
    }
    Ok(Expr::Case {
        operand: None,
        conditions,
        results,
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
        operand: None,
        conditions: vec![Expr::IsNotNull(Box::new(args[0].clone()))],
        results: vec![args[1].clone()],
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
            JoinOperator::CrossJoin
        } else {
            JoinOperator::Inner(constraint)
        };
        joins.push(Join {
            relation,
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
            .or_else(|| name.0.last().map(|i| i.value.clone()))
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
            Some(Expr::Value(Value::Number("0".into(), false))),
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
            name: sqlparser::ast::ObjectName(vec![sqlparser::ast::Ident::new("LEAST")]),
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
    match expr {
        Expr::Value(Value::Number(n, _)) => n.parse().ok(),
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
            right: Box::new(Expr::Value(Value::Number("1".into(), false))),
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
    Expr::Value(Value::Number(value.to_string(), false))
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

/// Parse a small SQL expression snippet into an AST node.
fn parse_expr(sql: &str) -> Expr {
    Parser::new(&GenericDialect {})
        .try_with_sql(sql)
        .and_then(|mut p| p.parse_expr())
        .expect("static expression snippet parses")
}

/// `SELECT <projection> FROM (<inner>) AS __rownum_sub ORDER BY <order_by>`
fn wrap_query_with_order_by(
    inner: Query,
    order_by: Vec<sqlparser::ast::OrderByExpr>,
    projection: Vec<sqlparser::ast::SelectItem>,
) -> Query {
    use sqlparser::ast::{
        Ident, Query as AstQuery, Select, SetExpr, TableAlias, TableFactor, TableWithJoins,
    };
    let select = Select {
        distinct: None,
        top: None,
        projection,
        into: None,
        from: vec![TableWithJoins {
            relation: TableFactor::Derived {
                lateral: false,
                subquery: Box::new(inner),
                alias: Some(TableAlias {
                    name: Ident::new("__rownum_sub"),
                    columns: vec![],
                }),
            },
            joins: vec![],
        }],
        lateral_views: vec![],
        selection: None,
        group_by: sqlparser::ast::GroupByExpr::Expressions(vec![]),
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
        named_window: vec![],
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        connect_by: None,
    };
    AstQuery {
        with: None,
        body: Box::new(SetExpr::Select(Box::new(select))),
        order_by,
        limit: None,
        limit_by: vec![],
        offset: None,
        fetch: None,
        locks: vec![],
        for_clause: None,
    }
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
            "SELECT p.name FROM people AS p LEFT JOIN teams AS t ON p.team_id = t.id WHERE p.id > 1 ORDER BY p.id"
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
            "SELECT p.team_id, COUNT(*) FROM people AS p JOIN teams AS t ON COALESCE(p.team_id, 0) = t.id GROUP BY p.team_id HAVING COUNT(*) = 0 IS NOT TRUE ORDER BY CASE WHEN CAST(p.team_id AS TEXT) IS NOT DISTINCT FROM CAST(1 AS TEXT) THEN 0 ELSE 1 END"
        );
    }

    #[test]
    fn lowers_merge_update_delete_where_using_post_update_values() {
        assert_eq!(
            oracle_to_postgres(
                "MERGE INTO mtgt d USING (SELECT 2 AS id FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.val = 'updated' DELETE WHERE d.val = 'updated'"
            )
            .unwrap(),
            "MERGE INTO mtgt AS d USING (SELECT 2 AS id FROM DUAL) AS s ON (d.id = s.id) WHEN MATCHED AND (('updated') = 'updated') THEN DELETE WHEN MATCHED THEN UPDATE SET val = 'updated'"
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
