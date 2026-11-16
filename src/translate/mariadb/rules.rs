//! Registry of the MariaDB translation rules: what each does, whether it is
//! exact, the minimum server version it relies on, and golden pairs that the
//! unit tests replay (`oracle input => MariaDB output`, `!` = Oracle error)
//! with dictionary routing on, the production configuration.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// Same result as Oracle for every input.
    Exact,
    /// Matches Oracle for the documented subset; documented divergence.
    Approximate,
    /// Raises the Oracle error (no exact lowering exists).
    Error,
}

#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub name: &'static str,
    pub kind: RuleKind,
    /// Minimum MariaDB version (major, minor).
    pub min_version: (u16, u16),
    pub notes: &'static str,
    pub goldens: &'static [(&'static str, &'static str)],
}

use RuleKind::*;

pub const RULES: &[Rule] = &[
    Rule {
        name: "identifiers",
        kind: Exact,
        min_version: (11, 8),
        notes: "Oracle identifier rule applied once (names::canonicalize); quoted names exact",
        goldens: &[
            ("select e.id from emp e", "SELECT E.ID FROM EMP E"),
            (
                r#"select "MixedCase" from t"#,
                r#"SELECT "MixedCase" FROM T"#,
            ),
            ("drop table emp", "DROP TABLE EMP"),
        ],
    },
    Rule {
        name: "opaque_literals_comments",
        kind: Exact,
        min_version: (11, 8),
        notes: "no rule fires inside literals, comments, hints or quoted names",
        goldens: &[
            (
                "INSERT INTO notes VALUES ('use TO_NUMBER(x) here')",
                "INSERT INTO NOTES VALUES ('use TO_NUMBER(x) here')",
            ),
            (
                "SELECT /*+ INDEX(t i) */ 'due SYSDATE - 1 day' FROM t -- it's a comment",
                "SELECT /*+ INDEX(t i) */ 'due SYSDATE - 1 day' FROM T -- it's a comment",
            ),
            (
                "SELECT \"UID\", t.uid FROM t",
                "SELECT \"UID\", T.UID FROM T",
            ),
        ],
    },
    Rule {
        name: "dual",
        kind: Exact,
        min_version: (11, 8),
        notes: "DUAL kept; aliased DUAL becomes a one-row derived table; `, DUAL` dropped",
        goldens: &[
            ("SELECT 1 FROM dual", "SELECT 1 FROM DUAL"),
            ("SELECT id, dual_flag FROM t", "SELECT ID, DUAL_FLAG FROM T"),
            (
                "SELECT d.dummy FROM dual d",
                "SELECT D.DUMMY FROM (SELECT 'X' AS DUMMY) D",
            ),
            (
                "SELECT p.name FROM people p, dual",
                "SELECT P.NAME FROM PEOPLE P",
            ),
            ("SELECT 1 FROM sys.dual", "SELECT 1 FROM DUAL"),
        ],
    },
    Rule {
        name: "pseudo_columns",
        kind: Approximate,
        min_version: (11, 8),
        notes: "SYSDATE→NOW(), SYSTIMESTAMP→CURRENT_TIMESTAMP(6), USER→session user; \
                UID→CONNECTION_ID() (no numeric user ids on MariaDB)",
        goldens: &[
            (
                "SELECT SYSTIMESTAMP FROM dual",
                "SELECT CURRENT_TIMESTAMP(6) AS \"SYSTIMESTAMP\" FROM DUAL",
            ),
            (
                "SELECT x FROM t WHERE d < sysdate",
                "SELECT X FROM T WHERE D < NOW()",
            ),
        ],
    },
    Rule {
        name: "date_arithmetic",
        kind: Exact,
        min_version: (11, 8),
        notes: "typed: DATE ± n → DATE_ADD/SUB (fractional days via seconds), \
                DATE - DATE → fractional days, TIMESTAMP - TIMESTAMP → interval; \
                unknown operand types are left to MariaDB",
        goldens: &[
            (
                "SELECT DATE '2024-01-02' + 1 FROM DUAL",
                "SELECT DATE_ADD(DATE '2024-01-02', INTERVAL 1 DAY) FROM DUAL",
            ),
            (
                "SELECT DATE '2024-03-01' - DATE '2024-02-01' FROM DUAL",
                "SELECT DATEDIFF(DATE '2024-03-01', DATE '2024-02-01') FROM DUAL",
            ),
            (
                "SELECT SYSDATE - 1/24 FROM DUAL",
                "SELECT DATE_SUB(NOW(), INTERVAL ROUND((1/24) * 86400) SECOND) FROM DUAL",
            ),
            (
                "SELECT SYSDATE - TRUNC(SYSDATE) FROM DUAL",
                "SELECT (CAST(TIMESTAMPDIFF(SECOND, DATE(NOW()), NOW()) AS DECIMAL(65,30)) / 86400) FROM DUAL",
            ),
        ],
    },
    Rule {
        name: "string_number_coercion",
        kind: Exact,
        min_version: (11, 8),
        notes: "a text operand of + - * / goes through dbsaci_to_number (ORA-01722); \
                numeric string literals are left as is; precedence preserved",
        goldens: &[
            (
                "SELECT '1' + 2 * 3 FROM DUAL",
                "SELECT '1' + 2 * 3 FROM DUAL",
            ),
            (
                "SELECT 'abc' + 1 FROM DUAL",
                "SELECT `dbsaci_sys`.`dbsaci_to_number`('abc') + 1 FROM DUAL",
            ),
        ],
    },
    Rule {
        name: "division_by_zero",
        kind: Approximate,
        min_version: (11, 8),
        notes: "literal zero divisor raises ORA-01476; a divisor that evaluates to 0 at \
                run time yields NULL on MariaDB (not detected)",
        goldens: &[(
            "SELECT 5 / 0 FROM DUAL",
            "SELECT `dbsaci_sys`.`dbsaci_div`(5, 0) FROM DUAL",
        )],
    },
    Rule {
        name: "trunc_round",
        kind: Exact,
        min_version: (11, 8),
        notes: "TRUNC/ROUND dispatch on the argument type (schema / binds / literals); \
                a date unit literal implies a date; unknown → numeric",
        goldens: &[
            (
                "SELECT TRUNC(12.99) FROM DUAL",
                "SELECT TRUNCATE(12.99, 0) FROM DUAL",
            ),
            (
                "SELECT TRUNC(DATE '2024-05-17', 'MM') FROM DUAL",
                "SELECT CAST(DATE_FORMAT(DATE '2024-05-17', '%Y-%m-01') AS DATE) FROM DUAL",
            ),
            (
                "SELECT ROUND(DATE '2024-03-20', 'MM') FROM DUAL",
                "SELECT CAST(DATE_FORMAT(DATE_ADD(DATE '2024-03-20', INTERVAL IF(DAYOFMONTH(DATE '2024-03-20') >= 16, 1, 0) MONTH), '%Y-%m-01') AS DATE) FROM DUAL",
            ),
        ],
    },
    Rule {
        name: "to_char",
        kind: Exact,
        min_version: (11, 8),
        notes: "datetime models compiled to DATE_FORMAT (case, FM, padding, FFn, RR); \
                number models via dbsaci_to_char_num; unknown element → ORA-01821",
        goldens: &[
            (
                "SELECT TO_CHAR(d, 'YYYY-MM-DD') FROM t",
                "SELECT DATE_FORMAT(D, '%Y-%m-%d') FROM T",
            ),
            (
                "SELECT TO_CHAR(d, 'DD-MON-YYYY') FROM t",
                "SELECT UPPER(DATE_FORMAT(D, '%d-%b-%Y')) FROM T",
            ),
            (
                "SELECT TO_CHAR(1234.5, 'FM999G990D00') FROM DUAL",
                "SELECT `dbsaci_sys`.`dbsaci_to_char_num`(1234.5, 'FM999G990D00') FROM DUAL",
            ),
            ("SELECT TO_CHAR(d, 'YYYY-BOGUS') FROM t", "!"),
        ],
    },
    Rule {
        name: "to_date",
        kind: Exact,
        min_version: (11, 8),
        notes: "STR_TO_DATE spec + validation and RR century in dbsaci_str_to_date \
                (ORA-01861/01843/01839)",
        goldens: &[(
            "SELECT TO_DATE('05-MAR-60', 'DD-MON-RR') FROM DUAL",
            "SELECT CAST(`dbsaci_sys`.`dbsaci_str_to_date`('05-MAR-60', '%d-%b-%y', 1) AS DATETIME) FROM DUAL",
        )],
    },
    Rule {
        name: "regexp_case",
        kind: Exact,
        min_version: (11, 8),
        notes: "Oracle regexps are case-sensitive by default: pattern prefixed (?-i); \
                match parameter i/c/n/m/x become an inline flag group",
        goldens: &[
            (
                "SELECT 1 FROM t WHERE REGEXP_LIKE(name, '^a')",
                "SELECT 1 FROM T WHERE (NAME REGEXP '(?-i)^a')",
            ),
            (
                "SELECT 1 FROM t WHERE REGEXP_LIKE(name, '\\D', 'i')",
                "SELECT 1 FROM T WHERE (NAME REGEXP '(?i)\\D')",
            ),
        ],
    },
    Rule {
        name: "null_ordering",
        kind: Exact,
        min_version: (11, 8),
        notes: "Oracle sorts NULLs last for ASC, first for DESC: each ORDER BY item gets an \
                `expr IS NULL [DESC]` key (also inside OVER/WITHIN GROUP; positional items \
                and RANGE frames are left as is)",
        goldens: &[
            (
                "SELECT a FROM t ORDER BY a",
                "SELECT A FROM T ORDER BY A IS NULL, A",
            ),
            (
                "SELECT a FROM t ORDER BY a DESC FETCH FIRST 5 ROWS ONLY",
                "SELECT A FROM T ORDER BY A IS NULL DESC, A DESC FETCH FIRST 5 ROWS ONLY",
            ),
            (
                "SELECT a FROM t ORDER BY a NULLS FIRST, b DESC NULLS LAST",
                "SELECT A FROM T ORDER BY A, B DESC",
            ),
        ],
    },
    Rule {
        name: "for_update",
        kind: Exact,
        min_version: (11, 8),
        notes: "WAIT n / NOWAIT / SKIP LOCKED are native; `OF col-list` dropped (MariaDB \
                locks rows of all tables)",
        goldens: &[(
            "SELECT a FROM t FOR UPDATE OF a WAIT 3",
            "SELECT A FROM T FOR UPDATE WAIT 3",
        )],
    },
    Rule {
        name: "outer_join_plus",
        kind: Exact,
        min_version: (11, 8),
        notes: "(+) → LEFT JOIN per query block (subqueries included); OR/IN → ORA-01719, \
                two outer tables in one predicate → ORA-01468, mutual → ORA-01416",
        goldens: &[
            (
                "SELECT a.x, b.y FROM a, b WHERE a.id = b.id(+)",
                "SELECT A.X, B.Y FROM A LEFT JOIN B ON A.ID = B.ID",
            ),
            ("SELECT 1 FROM a, b WHERE a.id = b.id(+) OR a.z = 1", "!"),
        ],
    },
    Rule {
        name: "full_outer_join",
        kind: Exact,
        min_version: (11, 8),
        notes: "two-row driver + LEFT JOINs + NOT EXISTS for unmatched right rows; works in \
                any block shape (GROUP BY, subqueries); duplicates impossible",
        goldens: &[],
    },
    Rule {
        name: "merge",
        kind: Exact,
        min_version: (11, 8),
        notes: "temp table of the source with a matched flag, ORA-30926 check, UPDATE…JOIN, \
                DELETE WHERE on updated rows, INSERT of unmatched rows; one batch",
        goldens: &[],
    },
    Rule {
        name: "insert_all_first",
        kind: Exact,
        min_version: (11, 8),
        notes: "source evaluated once into a temp table; FIRST uses IS TRUE / IS NOT TRUE \
                chains so NULL conditions fall through like Oracle",
        goldens: &[],
    },
    Rule {
        name: "connect_by",
        kind: Exact,
        min_version: (11, 8),
        notes: "recursive CTE with depth-first order path (ORDER SIBLINGS BY numeric-safe), \
                cycle path; without NOCYCLE a cycle raises ORA-01436",
        goldens: &[],
    },
    Rule {
        name: "ddl_types",
        kind: Exact,
        min_version: (11, 8),
        notes: "NUMBER → DECIMAL(65,30) (unconstrained marker), NUMBER(p[,s]) → DECIMAL, \
                INTEGER → DECIMAL(38,0), FLOAT/REAL/DOUBLE PRECISION → DECIMAL(65,30), \
                BINARY_DOUBLE/FLOAT → DOUBLE/FLOAT, TIMESTAMP(n) → DATETIME(n), identity → \
                BIGINT AUTO_INCREMENT",
        goldens: &[
            (
                "CREATE TABLE t (a NUMBER, b NUMBER(5), c NUMBER(10,2), d INTEGER, e BINARY_DOUBLE)",
                "CREATE TABLE T (A DECIMAL(65,30), B DECIMAL(5,0), C DECIMAL(10,2), D DECIMAL(38,0), E DOUBLE)",
            ),
            (
                "CREATE TABLE t (id NUMBER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v VARCHAR2(10 CHAR))",
                "CREATE TABLE T (ID BIGINT AUTO_INCREMENT PRIMARY KEY, V VARCHAR(10))",
            ),
            (
                "CREATE TABLE t (a NUMBER(*,2), b NUMBER(5,-2), c NUMBER(2,5), d TIMESTAMP)",
                "CREATE TABLE T (A DECIMAL(38,2), B DECIMAL(5,0), C DECIMAL(5,5), D DATETIME(6))",
            ),
        ],
    },
    Rule {
        name: "with_recursive",
        kind: Exact,
        min_version: (11, 8),
        notes: "RECURSIVE added only when a CTE references itself",
        goldens: &[(
            "WITH r (n) AS (SELECT 1 FROM dual UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT recursive_flag FROM r",
            "WITH RECURSIVE R (N) AS (SELECT 1 FROM DUAL UNION ALL SELECT N + 1 FROM R WHERE N < 3) SELECT RECURSIVE_FLAG FROM R",
        )],
    },
    Rule {
        name: "reserved_identifiers",
        kind: Exact,
        min_version: (11, 8),
        notes: "words MariaDB reserves but Oracle does not are back-quoted in identifier \
                positions",
        goldens: &[(
            "SELECT t.body, rank FROM doc t",
            "SELECT T.`BODY`, `RANK` FROM DOC T",
        )],
    },
    Rule {
        name: "listagg",
        kind: Approximate,
        min_version: (11, 8),
        notes: "LISTAGG … WITHIN GROUP → GROUP_CONCAT (literal delimiter only, else \
                ORA-03001); ON OVERFLOW dropped (group_concat_max_len applies)",
        goldens: &[(
            "SELECT LISTAGG(name, ', ') WITHIN GROUP (ORDER BY id) FROM people",
            "SELECT GROUP_CONCAT(NAME ORDER BY ID IS NULL, ID SEPARATOR ', ') FROM PEOPLE",
        )],
    },
    Rule {
        name: "fetch_with_ties",
        kind: Exact,
        min_version: (11, 8),
        notes: "native in MariaDB 10.6+; passed through",
        goldens: &[],
    },
];
