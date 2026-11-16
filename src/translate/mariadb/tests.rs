use super::*;
use crate::translate::schema::StaticSchemaInfo;

fn tr(sql: &str) -> String {
    oracle_to_mariadb(sql).unwrap_or_else(|e| format!("ERR {e}"))
}

#[test]
fn rule_goldens() {
    let mut failures = vec![];
    let cx = MariaDbContext {
        route_dictionary: true,
        ..MariaDbContext::default()
    };
    for rule in RULES {
        for (input, expected) in rule.goldens {
            let got = oracle_to_mariadb_ctx(input, &cx);
            match (*expected, got) {
                ("!", Err(_)) => {}
                ("!", Ok(out)) => failures.push(format!(
                    "[{}] {input}\n  expected error, got {out}",
                    rule.name
                )),
                (_, Err(e)) => failures.push(format!("[{}] {input}\n  error {e}", rule.name)),
                (exp, Ok(out)) if out != exp => failures.push(format!(
                    "[{}] {input}\n  want {exp}\n  got  {out}",
                    rule.name
                )),
                _ => {}
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Inputs used by the property tests (bug-list adversarial cases included).
const SAMPLES: &[&str] = &[
    "INSERT INTO notes VALUES ('use TO_NUMBER(x) here', 'x -- y', q'[it's (+) here]')",
    "SELECT 'due SYSDATE - 1 day' /* SYSDATE - 1 */ FROM t -- it's\nWHERE a = 1",
    "SELECT /*+ FULL(t) */ uid_x, t.uid FROM t ORDER BY 1",
    "SELECT id, dual_flag FROM t WHERE note = ', dual'",
    "SELECT a.x FROM a, b WHERE a.id = b.id(+) AND b.s(+) = 'it''s'",
    "MERGE INTO t d USING (SELECT 1 id, 'TO_DATE(x)' v FROM dual) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.v = s.v WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)",
    "SELECT TO_CHAR(d) FROM t WHERE c = 'ORDER BY x' ORDER BY d",
    "CREATE TABLE t (a NUMBER DEFAULT 0, b VARCHAR2(10) DEFAULT 'NUMBER(5)') -- NUMBER",
];

/// Every string literal and comment of the input survives byte-for-byte.
#[test]
fn literals_and_comments_survive() {
    for s in SAMPLES {
        let out = tr(s);
        for t in lex::lex(s) {
            let text = &s[t.s as usize..t.e as usize];
            if matches!(t.k, K::Str | K::Comment) {
                assert!(out.contains(text), "lost {text:?}\n in: {s}\nout: {out}");
            }
        }
    }
}

#[test]
fn idempotent_where_applicable() {
    for s in [
        "SELECT a, b FROM t WHERE c = 'x'",
        "INSERT INTO t (a) VALUES (1)",
        "UPDATE t SET a = 1 WHERE b = 2",
    ] {
        let once = tr(s);
        assert_eq!(tr(&once), once, "{s}");
    }
}

#[test]
fn bug_list_adversarial() {
    // 1. rules never fire in literals; quoted "UID" column untouched
    assert_eq!(
        tr("SELECT \"UID\" FROM t WHERE n = 'TO_NUMBER(x)'"),
        "SELECT \"UID\" FROM T WHERE N = 'TO_NUMBER(x)'"
    );
    // 5. column named NOW is a column
    assert_eq!(tr("SELECT now FROM t"), "SELECT NOW FROM T");
    // 8. precedence preserved
    assert_eq!(
        tr("SELECT 'x' + 2 * 3 FROM dual"),
        "SELECT dbsaci_to_number('x') + 2 * 3 FROM DUAL"
    );
    // 9. (+) in a subquery
    let out = tr("SELECT * FROM (SELECT a.x FROM a, b WHERE a.id = b.id(+)) q");
    assert!(out.contains("LEFT JOIN B ON A.ID = B.ID"), "{out}");
    // 15. WAIT kept
    assert_eq!(
        tr("SELECT 1 FROM t FOR UPDATE WAIT 3"),
        "SELECT 1 FROM T FOR UPDATE WAIT 3"
    );
    // 16. a column containing RECURSIVE does not suppress WITH RECURSIVE
    assert!(tr("WITH r (n) AS (SELECT 1 FROM dual UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT recursive_flag FROM r").starts_with("WITH RECURSIVE"));
    // hints survive
    assert!(tr("SELECT /*+ INDEX(t i) */ a FROM t").contains("/*+ INDEX(t i) */"));
}

#[test]
fn typed_rules_use_schema_and_binds() {
    let schema = StaticSchemaInfo(vec![
        (None, "T".into(), "CREATED".into(), ValueKind::Date),
        (None, "T".into(), "AMT".into(), ValueKind::Number),
    ]);
    let kinds = [ValueKind::Date];
    let cx = MariaDbContext {
        schema: &schema,
        bind_kinds: &kinds,
        ..MariaDbContext::default()
    };
    let t = |s: &str| oracle_to_mariadb_ctx(s, &cx).unwrap();
    assert_eq!(
        t("SELECT TRUNC(created) FROM t"),
        "SELECT DATE(CREATED) FROM T"
    );
    assert_eq!(
        t("SELECT TRUNC(x.amt) FROM t x"),
        "SELECT TRUNCATE(X.AMT, 0) FROM T X"
    );
    assert_eq!(
        t("SELECT 1 FROM t WHERE created > ? - 7"),
        "SELECT 1 FROM T WHERE CREATED > DATE_SUB(?, INTERVAL 7 DAY)"
    );
}

#[test]
fn merge_is_a_batch() {
    let out = tr(
        "MERGE INTO mtgt d USING msrc s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.val = s.val WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
    );
    let parts: Vec<&str> = out.split(crate::translate::MARIADB_BATCH_SEP).collect();
    assert!(
        parts.iter().any(|p| p
            .starts_with("UPDATE MTGT D JOIN __dbsaci_merge S ON (D.ID = S.ID) SET D.VAL = S.VAL")),
        "{out}"
    );
    assert!(parts.iter().any(|p| p.starts_with("INSERT INTO MTGT (ID, VAL) SELECT S.ID, S.VAL FROM __dbsaci_merge S WHERE NOT S.__dbsaci_matched")), "{out}");
    assert!(
        parts
            .iter()
            .all(|p| p.starts_with(NOCOUNT) || p.starts_with("UPDATE") || p.starts_with("INSERT"))
    );
}

#[test]
fn binds_are_hoisted_when_reordered() {
    let out = tr("INSERT ALL INTO t (a) VALUES (?) SELECT ? AS x FROM dual");
    assert!(
        out.starts_with("/* dbsaci:nocount */ SET @dbsaci_b0 = ?, @dbsaci_b1 = ?"),
        "{out}"
    );
}

#[test]
fn alter_session_is_case_insensitive() {
    assert_eq!(
        tr("alter   session set time_zone = '+02:00'"),
        "SET time_zone = '+02:00'"
    );
    assert!(tr("Alter Session Set Current_Schema = hr").contains("@dbsaci_current_schema = 'HR'"));
}

#[test]
fn execute_immediate_literal_is_translated() {
    assert_eq!(
        tr("BEGIN EXECUTE IMMEDIATE 'DROP TABLE reexec_demo'; END"),
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE REEXEC_DEMO'; END"
    );
    assert_eq!(
        tr("BEGIN EXECUTE IMMEDIATE 'CREATE TABLE t (a ' || 'NUMBER)'; END"),
        "BEGIN EXECUTE IMMEDIATE 'CREATE TABLE T (A DECIMAL(65,30))'; END"
    );
    // dynamic SQL from a variable is left alone
    assert_eq!(
        tr("BEGIN EXECUTE IMMEDIATE v_sql; END"),
        "BEGIN EXECUTE IMMEDIATE V_SQL; END"
    );
}

#[test]
fn order_by_across_lines_and_in_windows() {
    assert_eq!(
        tr("SELECT a FROM t\nORDER\nBY a DESC"),
        "SELECT A FROM T\nORDER\nBY A IS NULL DESC, A DESC"
    );
    assert_eq!(
        tr("SELECT RANK() OVER (PARTITION BY d ORDER BY s DESC) FROM t"),
        "SELECT RANK() OVER (PARTITION BY D ORDER BY S IS NULL DESC, S DESC) FROM T"
    );
    // ordered-set aggregates take a single key
    assert_eq!(
        tr("SELECT PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY s) OVER () FROM t"),
        "SELECT PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY S) OVER () FROM T"
    );
}

#[test]
fn cast_char_targets_pass_through() {
    assert_eq!(
        tr("SELECT CAST(? AS CHAR) FROM dual"),
        "SELECT CAST(? AS CHAR) FROM DUAL"
    );
    assert_eq!(
        tr("SELECT CAST(? AS CHAR(32767)), CAST(? AS BINARY) FROM dual"),
        "SELECT CAST(? AS CHAR(32767)), CAST(? AS BINARY) FROM DUAL"
    );
}
