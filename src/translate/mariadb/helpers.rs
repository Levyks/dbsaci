//! Stored functions the MariaDB translator's output may call.
//!
//! These are the runtime half of rules that cannot be expressed inline without
//! evaluating an operand more than once (which would break positional binds)
//! or that must raise an Oracle error. The adapter installs them (in
//! `dbsaci_sys` once dictionary routing is on) under `SQL_MODE=ORACLE`; every
//! runtime raise uses `SIGNAL SQLSTATE '45000'` with a `MESSAGE_TEXT` of
//! `ORA-nnnnn: <text>`, which the adapter maps to that Oracle code.
//!
//! Each entry is `(name, CREATE statement)`. The translator emits calls by
//! `name`; with routing on, names listed here are qualified like
//! [`crate::mariadb::dictionary::FUNCTIONS`].

/// Helper routines, in install order (later ones may call earlier ones).
pub const HELPERS: &[(&str, &str)] = &[
    (
        "dbsaci_raise",
        "CREATE OR REPLACE FUNCTION dbsaci_raise(code INT, msg TEXT) RETURN INT NO SQL AS \
           m VARCHAR(512); BEGIN \
           m := CONCAT('ORA-', LPAD(ABS(code), 5, '0'), ': ', COALESCE(msg, '')); \
           SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = m; RETURN NULL; END",
    ),
    (
        "dbsaci_str_to_date",
        "CREATE OR REPLACE FUNCTION dbsaci_str_to_date(s TEXT, f VARCHAR(255), rr INT) \
           RETURN DATETIME(6) NOT DETERMINISTIC NO SQL AS d DATETIME(6); y INT; m INT; dd INT; BEGIN \
           IF s IS NULL THEN RETURN NULL; END IF; \
           BEGIN d := STR_TO_DATE(s COLLATE utf8mb4_general_ci, f COLLATE utf8mb4_general_ci); EXCEPTION WHEN OTHERS THEN d := NULL; END; \
           IF d IS NULL THEN SIGNAL SQLSTATE '45000' \
             SET MESSAGE_TEXT = 'ORA-01861: literal does not match format string'; END IF; \
           y := YEAR(d); m := MONTH(d); dd := DAY(d); \
           IF INSTR(f, '%Y') = 0 AND INSTR(f, '%y') = 0 THEN y := YEAR(NOW()); END IF; \
           IF m = 0 AND INSTR(f, '%m') = 0 AND INSTR(f, '%c') = 0 AND INSTR(f, '%b') = 0 \
              AND INSTR(f, '%M') = 0 THEN m := MONTH(NOW()); END IF; \
           IF dd = 0 AND INSTR(f, '%d') = 0 AND INSTR(f, '%e') = 0 THEN dd := 1; END IF; \
           IF rr = 1 AND y BETWEEN 2050 AND 2069 THEN y := y - 100; END IF; \
           IF m < 1 OR m > 12 THEN SIGNAL SQLSTATE '45000' \
             SET MESSAGE_TEXT = 'ORA-01843: not a valid month'; END IF; \
           IF dd < 1 OR dd > DAY(LAST_DAY(MAKEDATE(y, 1) + INTERVAL (m - 1) MONTH)) THEN \
             SIGNAL SQLSTATE '45000' \
             SET MESSAGE_TEXT = 'ORA-01839: date not valid for month specified'; END IF; \
           RETURN MAKEDATE(y, 1) + INTERVAL (m - 1) MONTH + INTERVAL (dd - 1) DAY \
             + INTERVAL TIME_TO_SEC(TIME(d)) SECOND + INTERVAL MICROSECOND(d) MICROSECOND; END",
    ),
    (
        "dbsaci_trunc_date",
        "CREATE OR REPLACE FUNCTION dbsaci_trunc_date(d DATETIME(6), fmt VARCHAR(16)) \
           RETURN DATETIME DETERMINISTIC NO SQL AS u VARCHAR(16); x DATE; y INT; BEGIN \
           IF d IS NULL THEN RETURN NULL; END IF; u := UPPER(TRIM(fmt)); x := DATE(d); \
           IF u IN ('DD', 'DDD', 'J') THEN RETURN x; \
           ELSIF u IN ('MM', 'MON', 'MONTH', 'RM') THEN RETURN x - INTERVAL (DAY(x) - 1) DAY; \
           ELSIF u IN ('YYYY', 'SYYYY', 'YEAR', 'SYEAR', 'YYY', 'YY', 'Y') THEN RETURN MAKEDATE(YEAR(x), 1); \
           ELSIF u IN ('IYYY', 'IYY', 'IY', 'I') THEN \
             y := FLOOR(YEARWEEK(x, 3) / 100); \
             RETURN MAKEDATE(y, 4) - INTERVAL WEEKDAY(MAKEDATE(y, 4)) DAY; \
           ELSIF u = 'Q' THEN RETURN MAKEDATE(YEAR(x), 1) + INTERVAL (QUARTER(x) - 1) QUARTER; \
           ELSIF u = 'IW' THEN RETURN x - INTERVAL WEEKDAY(x) DAY; \
           ELSIF u = 'WW' THEN RETURN MAKEDATE(YEAR(x), 1) + INTERVAL (FLOOR((DAYOFYEAR(x) - 1) / 7) * 7) DAY; \
           ELSIF u = 'W' THEN RETURN x - INTERVAL (DAY(x) - 1) DAY + INTERVAL (FLOOR((DAY(x) - 1) / 7) * 7) DAY; \
           ELSIF u IN ('DAY', 'DY', 'D') THEN RETURN x - INTERVAL (DAYOFWEEK(x) - 1) DAY; \
           ELSIF u IN ('CC', 'SCC') THEN RETURN MAKEDATE(FLOOR((YEAR(x) - 1) / 100) * 100 + 1, 1); \
           ELSIF u IN ('HH', 'HH12', 'HH24') THEN RETURN x + INTERVAL HOUR(d) HOUR; \
           ELSIF u = 'MI' THEN RETURN x + INTERVAL HOUR(d) HOUR + INTERVAL MINUTE(d) MINUTE; \
           END IF; \
           SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'ORA-01821: date format not recognized'; \
           RETURN NULL; END",
    ),
    (
        "dbsaci_round_date",
        "CREATE OR REPLACE FUNCTION dbsaci_round_date(d DATETIME(6), fmt VARCHAR(16)) \
           RETURN DATETIME DETERMINISTIC NO SQL AS u VARCHAR(16); x DATE; BEGIN \
           IF d IS NULL THEN RETURN NULL; END IF; u := UPPER(TRIM(fmt)); x := DATE(d); \
           IF u IN ('DD', 'DDD', 'J') THEN RETURN DATE(d + INTERVAL 12 HOUR); \
           ELSIF u IN ('MM', 'MON', 'MONTH', 'RM') THEN \
             RETURN dbsaci_trunc_date(IF(DAY(x) >= 16, x + INTERVAL 1 MONTH, x), 'MM'); \
           ELSIF u IN ('YYYY', 'SYYYY', 'YEAR', 'SYEAR', 'YYY', 'YY', 'Y') THEN \
             RETURN MAKEDATE(YEAR(x) + IF(MONTH(x) >= 7, 1, 0), 1); \
           ELSIF u = 'Q' THEN \
             RETURN dbsaci_trunc_date(IF(MOD(MONTH(x) - 1, 3) = 2 OR (MOD(MONTH(x) - 1, 3) = 1 AND DAY(x) >= 16), \
               x + INTERVAL 1 QUARTER, x), 'Q'); \
           ELSIF u IN ('IW', 'WW', 'W', 'DAY', 'DY', 'D', 'IYYY', 'IY', 'I') THEN \
             RETURN dbsaci_trunc_date(d + INTERVAL 84 HOUR, u); \
           ELSIF u IN ('CC', 'SCC') THEN \
             RETURN MAKEDATE(FLOOR((YEAR(x) - 1) / 100) * 100 + 1 + IF(MOD(YEAR(x) - 1, 100) >= 50, 100, 0), 1); \
           ELSIF u IN ('HH', 'HH12', 'HH24') THEN RETURN dbsaci_trunc_date(d + INTERVAL 30 MINUTE, 'HH'); \
           ELSIF u = 'MI' THEN RETURN dbsaci_trunc_date(d + INTERVAL 30 SECOND, 'MI'); \
           END IF; \
           SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'ORA-01821: date format not recognized'; \
           RETURN NULL; END",
    ),
    (
        "dbsaci_to_char_num",
        // Oracle number format models: 9 0 , G . D $ FM S (lead/trail) MI PR.
        "CREATE OR REPLACE FUNCTION dbsaci_to_char_num(n DECIMAL(65,30), fmt VARCHAR(64)) \
           RETURN VARCHAR(128) DETERMINISTIC NO SQL AS \
           f VARCHAR(64); fm INT DEFAULT 0; dollar INT DEFAULT 0; slead INT DEFAULT 0; \
           strail INT DEFAULT 0; mi INT DEFAULT 0; pr INT DEFAULT 0; ip VARCHAR(64); fp VARCHAR(64); \
           dpos INT; nd INT; r DECIMAL(65,30); istr VARCHAR(80); fstr VARCHAR(80); \
           slots INT; zfrom INT; outs VARCHAR(128) DEFAULT ''; i INT; c VARCHAR(1); k INT; \
           need INT; neg INT; res VARCHAR(128); BEGIN \
           IF n IS NULL THEN RETURN NULL; END IF; \
           f := UPPER(fmt); \
           IF f IN ('TM', 'TM9') THEN \
             res := CAST(n AS CHAR); \
             IF INSTR(res, '.') > 0 THEN res := TRIM(TRAILING '.' FROM TRIM(TRAILING '0' FROM res)); END IF; \
             IF LEFT(res, 2) = '0.' THEN res := SUBSTRING(res, 2); END IF; \
             IF LEFT(res, 3) = '-0.' THEN res := CONCAT('-', SUBSTRING(res, 3)); END IF; \
             RETURN res; END IF; \
           IF LEFT(f, 2) = 'FM' THEN fm := 1; f := SUBSTRING(f, 3); END IF; \
           IF INSTR(f, '$') > 0 THEN dollar := 1; f := REPLACE(f, '$', ''); END IF; \
           IF LEFT(f, 1) = 'S' THEN slead := 1; f := SUBSTRING(f, 2); \
           ELSIF RIGHT(f, 1) = 'S' THEN strail := 1; f := LEFT(f, CHAR_LENGTH(f) - 1); \
           ELSIF RIGHT(f, 2) = 'MI' THEN mi := 1; f := LEFT(f, CHAR_LENGTH(f) - 2); \
           ELSIF RIGHT(f, 2) = 'PR' THEN pr := 1; f := LEFT(f, CHAR_LENGTH(f) - 2); END IF; \
           f := REPLACE(REPLACE(f, 'G', ','), 'D', '.'); \
           dpos := INSTR(f, '.'); \
           IF dpos > 0 THEN ip := LEFT(f, dpos - 1); fp := SUBSTRING(f, dpos + 1); \
           ELSE ip := f; fp := ''; END IF; \
           nd := COALESCE(CHAR_LENGTH(REPLACE(fp, ',', '')), 0); \
           neg := IF(n < 0, 1, 0); r := ROUND(ABS(n), nd); \
           istr := CAST(TRUNCATE(r, 0) AS CHAR); \
           IF nd > 0 THEN fstr := SUBSTRING_INDEX(CAST(CAST(r AS DECIMAL(65,30)) AS CHAR), '.', -1); \
             fstr := RPAD(LEFT(fstr, nd), nd, '0'); ELSE fstr := ''; END IF; \
           slots := CHAR_LENGTH(REPLACE(ip, ',', '')); \
           zfrom := INSTR(REPLACE(ip, ',', ''), '0'); \
           IF istr = '0' THEN \
             IF zfrom > 0 OR nd = 0 THEN istr := '0'; ELSE istr := ''; END IF; END IF; \
           IF CHAR_LENGTH(istr) > slots THEN \
             RETURN RPAD('#', CHAR_LENGTH(fmt) + IF(fm = 1, -1, 1), '#'); END IF; \
           need := IF(zfrom > 0, slots - zfrom + 1, 0); \
           IF CHAR_LENGTH(istr) < need THEN istr := LPAD(istr, need, '0'); END IF; \
           i := CHAR_LENGTH(ip); k := COALESCE(CHAR_LENGTH(istr), 0); \
           WHILE i >= 1 LOOP c := SUBSTRING(ip, i, 1); \
             IF c = ',' THEN outs := CONCAT(IF(k >= 1, ',', ' '), outs); \
             ELSE outs := CONCAT(IF(k >= 1, SUBSTRING(istr, k, 1), ' '), outs); k := k - 1; END IF; \
             i := i - 1; END LOOP; \
           res := outs; \
           IF dpos > 0 THEN res := CONCAT(res, '.', fstr); END IF; \
           IF dollar = 1 THEN res := CONCAT(LEFT(res, CHAR_LENGTH(res) - CHAR_LENGTH(LTRIM(res))), '$', LTRIM(res)); END IF; \
           IF slead = 1 THEN res := CONCAT(LEFT(res, CHAR_LENGTH(res) - CHAR_LENGTH(LTRIM(res))), IF(neg = 1, '-', '+'), LTRIM(res)); \
           ELSIF strail = 1 THEN res := CONCAT(res, IF(neg = 1, '-', '+')); \
           ELSIF mi = 1 THEN res := CONCAT(res, IF(neg = 1, '-', ' ')); \
           ELSIF pr = 1 THEN res := IF(neg = 1, CONCAT(LEFT(res, CHAR_LENGTH(res) - CHAR_LENGTH(LTRIM(res))), '<', LTRIM(res), '>'), CONCAT(' ', res, ' ')); \
           ELSE res := CONCAT(LEFT(res, CHAR_LENGTH(res) - CHAR_LENGTH(LTRIM(res))), IF(neg = 1, '-', ' '), LTRIM(res)); END IF; \
           IF fm = 1 THEN RETURN TRIM(res); END IF; \
           RETURN res; END",
    ),
];

/// Is `name` (any case) one of the helper routines?
pub fn is_helper(name: &str) -> bool {
    HELPERS.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}
