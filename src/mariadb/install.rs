//! The objects DbSaci installs in [`SCHEMA`] and how they are installed.
//!
//! Installed once per server and version (not per connect): a connect checks
//! `dbsaci_sys.install_info`; when it is missing or stale and the login may
//! create it, the install runs under a server-wide `GET_LOCK`. Otherwise the
//! administrator runs `dbsaci install` once.

use super::dictionary::{RELATIONS, SCHEMA};

/// Bump on any change to the objects below.
pub const VERSION: &str = "0.3.4-2026-11-16.1";

/// Schemas that are MariaDB's own, not Oracle objects; hidden from ALL_*.
const SYSTEM_SCHEMAS: &str =
    "('mysql','information_schema','performance_schema','sys','dbsaci_sys')";

/// Routines the views and the translator rely on. `oname` maps a physical
/// name to its Oracle name (see `translate::names::oracle_name`), reading the
/// session's `@dbsaci_lower_names`; `login_user` is the session's own schema
/// as an Oracle name (USER_* views).
const SESSION_ROUTINES: &[&str] = &[
    "CREATE OR REPLACE FUNCTION dbsaci_sys.oname(n VARCHAR(255)) RETURN VARCHAR(255) \
       SQL SECURITY INVOKER AS BEGIN \
       IF n IS NULL THEN RETURN NULL; END IF; \
       IF @dbsaci_lower_names = 1 AND CAST(n AS BINARY) = CAST(LOWER(n) AS BINARY) THEN \
         RETURN UPPER(n); END IF; \
       RETURN n; END",
    // `DATABASE()` inside a stored routine is the routine's own database, so
    // the session's current schema comes from the session variable DbSaci
    // keeps (set on connect and on ALTER SESSION SET CURRENT_SCHEMA).
    "CREATE OR REPLACE FUNCTION dbsaci_sys.current_schema() RETURN VARCHAR(255) \
       SQL SECURITY INVOKER AS BEGIN \
       RETURN COALESCE(@dbsaci_current_schema, DATABASE()); END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.login_user() RETURN VARCHAR(255) \
       SQL SECURITY INVOKER AS BEGIN \
       RETURN COALESCE(@dbsaci_login_user, dbsaci_sys.oname(dbsaci_sys.current_schema())); END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.nls(p VARCHAR(64)) RETURN VARCHAR(255) \
       SQL SECURITY INVOKER AS BEGIN \
       RETURN CASE UPPER(p) \
         WHEN 'NLS_DATE_FORMAT' THEN COALESCE(@dbsaci_nls_date_format, 'DD-MON-RR') \
         WHEN 'NLS_TIMESTAMP_FORMAT' THEN COALESCE(@dbsaci_nls_timestamp_format, 'DD-MON-RR HH.MI.SSXFF AM') \
         WHEN 'NLS_TIMESTAMP_TZ_FORMAT' THEN COALESCE(@dbsaci_nls_timestamp_tz_format, 'DD-MON-RR HH.MI.SSXFF AM TZR') \
         WHEN 'NLS_NUMERIC_CHARACTERS' THEN COALESCE(@dbsaci_nls_numeric_characters, '.,') \
         WHEN 'NLS_LANGUAGE' THEN COALESCE(@dbsaci_nls_language, 'AMERICAN') \
         WHEN 'NLS_TERRITORY' THEN COALESCE(@dbsaci_nls_territory, 'AMERICA') \
         WHEN 'NLS_SORT' THEN COALESCE(@dbsaci_nls_sort, 'BINARY') \
         WHEN 'NLS_COMP' THEN COALESCE(@dbsaci_nls_comp, 'BINARY') \
         WHEN 'NLS_CALENDAR' THEN COALESCE(@dbsaci_nls_calendar, 'GREGORIAN') \
         WHEN 'NLS_DATE_LANGUAGE' THEN COALESCE(@dbsaci_nls_date_language, 'AMERICAN') \
         WHEN 'NLS_CHARACTERSET' THEN 'AL32UTF8' \
         WHEN 'NLS_NCHAR_CHARACTERSET' THEN 'AL16UTF16' \
         ELSE NULL END; END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.sessiontimezone() RETURN VARCHAR(64) \
       SQL SECURITY INVOKER AS BEGIN \
       RETURN CASE WHEN @@session.time_zone = 'SYSTEM' THEN '+00:00' ELSE @@session.time_zone END; END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.dbtimezone() RETURN VARCHAR(16) SQL SECURITY INVOKER \
       AS BEGIN RETURN '+00:00'; END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.sys_context(ns VARCHAR(64), p VARCHAR(64)) \
       RETURN VARCHAR(255) SQL SECURITY INVOKER AS BEGIN \
       RETURN CASE UPPER(p) \
         WHEN 'CURRENT_SCHEMA' THEN dbsaci_sys.oname(dbsaci_sys.current_schema()) \
         WHEN 'SESSION_SCHEMA' THEN dbsaci_sys.oname(dbsaci_sys.current_schema()) \
         WHEN 'CURRENT_USER' THEN dbsaci_sys.login_user() \
         WHEN 'SESSION_USER' THEN dbsaci_sys.login_user() \
         WHEN 'DB_NAME' THEN dbsaci_sys.oname(dbsaci_sys.current_schema()) \
         WHEN 'DB_UNIQUE_NAME' THEN dbsaci_sys.oname(dbsaci_sys.current_schema()) \
         WHEN 'INSTANCE_NAME' THEN dbsaci_sys.oname(dbsaci_sys.current_schema()) \
         WHEN 'SERVER_HOST' THEN @@hostname \
         WHEN 'SID' THEN CAST(CONNECTION_ID() AS CHAR) \
         WHEN 'SESSIONID' THEN CAST(CONNECTION_ID() AS CHAR) \
         WHEN 'LANG' THEN 'US' \
         WHEN 'LANGUAGE' THEN 'AMERICAN_AMERICA.AL32UTF8' \
         WHEN 'NLS_DATE_FORMAT' THEN dbsaci_sys.nls('NLS_DATE_FORMAT') \
         WHEN 'SESSIONTIMEZONE' THEN dbsaci_sys.sessiontimezone() \
         WHEN 'DB_TIMEZONE' THEN '+00:00' \
         ELSE NULL END; END",
    "CREATE OR REPLACE PROCEDURE dbsaci_sys.dbms_output__put_line(msg TEXT) SQL SECURITY INVOKER \
       AS BEGIN NULL; END",
    "CREATE OR REPLACE PROCEDURE dbsaci_sys.dbms_output__put(msg TEXT) SQL SECURITY INVOKER \
       AS BEGIN NULL; END",
    "CREATE OR REPLACE PROCEDURE dbsaci_sys.dbms_output__new_line() SQL SECURITY INVOKER \
       AS BEGIN NULL; END",
    "CREATE OR REPLACE PROCEDURE dbsaci_sys.dbms_output__enable(buffer_size INT DEFAULT NULL) \
       SQL SECURITY INVOKER AS BEGIN NULL; END",
    "CREATE OR REPLACE PROCEDURE dbsaci_sys.dbms_output__disable() SQL SECURITY INVOKER \
       AS BEGIN NULL; END",
    // DBMS_METADATA.GET_DDL for tables: the column list of the physical table.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.dbms_metadata__get_ddl(object_type VARCHAR(64), \
       name VARCHAR(128), schema VARCHAR(128) DEFAULT NULL) RETURN TEXT SQL SECURITY INVOKER AS \
       nsp VARCHAR(128); cols TEXT; BEGIN \
       nsp := COALESCE(NULLIF(schema, ''), dbsaci_sys.oname(dbsaci_sys.current_schema())); \
       IF UPPER(object_type) <> 'TABLE' THEN \
         RETURN CONCAT('-- DBMS_METADATA.GET_DDL is not implemented for object type ', object_type); \
       END IF; \
       SELECT GROUP_CONCAT(CONCAT(dbsaci_sys.oname(COLUMN_NAME), ' ', COLUMN_TYPE) \
                           ORDER BY ORDINAL_POSITION SEPARATOR ', ') INTO cols \
         FROM information_schema.COLUMNS \
         WHERE dbsaci_sys.oname(TABLE_SCHEMA) = nsp AND dbsaci_sys.oname(TABLE_NAME) = name; \
       IF cols IS NULL THEN RETURN CONCAT('-- table ', nsp, '.', name, ' not found'); END IF; \
       RETURN CONCAT('CREATE TABLE ', nsp, '.', name, ' (', cols, ')'); END",
];

/// Oracle scalar functions with no MariaDB `SQL_MODE=ORACLE` equivalent,
/// installed once per session as real stored functions. These are genuine
/// semantic implementations (any argument works), not corpus-specific shims —
/// the alternative would be per-call SQL rewriting in `translate.rs`.
const COMPAT_FUNCTIONS: &[&str] = &[
    // MONTHS_BETWEEN: whole months from the month/year fields, plus a
    // day-of-month fraction over 31; exact 0 when both days match or both are
    // end-of-month (Oracle's rule).
    "CREATE OR REPLACE FUNCTION dbsaci_sys.months_between(a DATETIME, b DATETIME) \
       RETURN DECIMAL(20,10) AS whole INT; frac DECIMAL(20,10); BEGIN \
       whole := (YEAR(a)-YEAR(b))*12 + (MONTH(a)-MONTH(b)); \
       IF DAY(a) = DAY(b) OR (DAY(a) = DAY(LAST_DAY(a)) AND DAY(b) = DAY(LAST_DAY(b))) THEN frac := 0; \
       ELSE frac := (DAY(a) - DAY(b) + (TIME_TO_SEC(TIME(a)) - TIME_TO_SEC(TIME(b)))/86400) / 31; END IF; \
       RETURN whole + frac; END",
    // NEXT_DAY: the first date strictly after `d` falling on the named weekday.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.next_day(d DATETIME, dow VARCHAR(20)) RETURN DATETIME AS \
       target INT; cur INT; add_days INT; BEGIN \
       target := FIELD(UPPER(LEFT(dow,3)),'SUN','MON','TUE','WED','THU','FRI','SAT'); \
       IF target = 0 THEN RETURN NULL; END IF; \
       cur := DAYOFWEEK(d); add_days := MOD(target - cur + 7, 7); \
       IF add_days = 0 THEN add_days := 7; END IF; \
       RETURN DATE_ADD(d, INTERVAL add_days DAY); END",
    // INITCAP: upper-case the first alphanumeric of each run, lower-case the rest.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.initcap(s TEXT) RETURN TEXT AS \
       out_s TEXT DEFAULT ''; i INT DEFAULT 1; ch VARCHAR(4); prev VARCHAR(4) DEFAULT ' '; BEGIN \
       IF s IS NULL THEN RETURN NULL; END IF; \
       WHILE i <= CHAR_LENGTH(s) LOOP ch := SUBSTRING(s,i,1); \
         IF prev REGEXP '[[:alnum:]]' THEN out_s := CONCAT(out_s, LOWER(ch)); \
         ELSE out_s := CONCAT(out_s, UPPER(ch)); END IF; \
         prev := ch; i := i + 1; END LOOP; RETURN out_s; END",
    // TRANSLATE: map each char in `from_set` to the same position in `to_set`;
    // drop it when `to_set` is shorter. (`translate` is reserved, hence prefix.)
    "CREATE OR REPLACE FUNCTION dbsaci_sys.oracle_translate(s TEXT, from_set TEXT, to_set TEXT) RETURN TEXT AS \
       out_s TEXT DEFAULT ''; i INT DEFAULT 1; ch VARCHAR(4); pos INT; BEGIN \
       IF s IS NULL THEN RETURN NULL; END IF; \
       WHILE i <= CHAR_LENGTH(s) LOOP ch := SUBSTRING(s,i,1); pos := INSTR(from_set, ch); \
         IF pos = 0 THEN out_s := CONCAT(out_s, ch); \
         ELSIF pos <= CHAR_LENGTH(to_set) THEN out_s := CONCAT(out_s, SUBSTRING(to_set,pos,1)); END IF; \
         i := i + 1; END LOOP; RETURN out_s; END",
    // REGEXP_COUNT: replace every match with a marker and count markers, so
    // anchors (`^a`) apply to the whole subject, case-sensitively (`(?-i)`)
    // as in Oracle regardless of the column collation.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.regexp_count(subj TEXT, pat TEXT) RETURN INT AS        marked TEXT; before_n INT; BEGIN        IF subj IS NULL OR pat IS NULL THEN RETURN NULL; END IF;        before_n := CHAR_LENGTH(subj) - CHAR_LENGTH(REPLACE(subj, CHAR(1), ''));        marked := REGEXP_REPLACE(subj, CONCAT('(?-i)(?:', pat, ')'), CHAR(1));        RETURN CHAR_LENGTH(marked) - CHAR_LENGTH(REPLACE(marked, CHAR(1), '')) - before_n; END",
    // REGEXP_SUBSTR with position / occurrence / capture group — MariaDB's is
    // 2-argument. `REGEXP_INSTR` accepts position+occurrence, so use it to
    // locate the match, then extract; a non-zero group index pulls the first
    // capture via `REGEXP_REPLACE`.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.oracle_regexp_substr(subj TEXT, pat TEXT, p INT, occ INT, grp INT) \
       RETURN TEXT AS rest TEXT; m TEXT; n INT DEFAULT 0; hit INT; mlen INT; BEGIN \
       IF subj IS NULL OR pat IS NULL THEN RETURN NULL; END IF; \
       rest := SUBSTRING(subj, GREATEST(p, 1)); \
       LOOP hit := REGEXP_INSTR(rest, pat); EXIT WHEN hit = 0; \
         m := REGEXP_SUBSTR(rest, pat); n := n + 1; \
         IF n >= GREATEST(occ, 1) THEN \
           IF grp IS NULL OR grp = 0 THEN RETURN m; END IF; \
           RETURN REGEXP_REPLACE(m, CONCAT('^', pat, '$'), CONCAT(CHAR(92), grp)); \
         END IF; \
         mlen := GREATEST(CHAR_LENGTH(m), 1); rest := SUBSTRING(rest, hit + mlen); \
         EXIT WHEN rest IS NULL OR rest = ''; END LOOP; \
       RETURN NULL; END",
    // INSTR with position + occurrence (native INSTR is 2-arg). Negative `pos`
    // searches backwards from |pos| chars before the end.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.oracle_instr(s TEXT, sub TEXT, pos INT, nth INT) RETURN INT AS \
       cur INT; c INT DEFAULT 0; BEGIN \
       IF pos >= 0 THEN cur := IF(pos = 0, 1, pos); \
         WHILE c < nth LOOP cur := LOCATE(sub, s, cur); IF cur = 0 THEN RETURN 0; END IF; \
           c := c + 1; IF c < nth THEN cur := cur + 1; END IF; END LOOP; RETURN cur; \
       ELSE cur := CHAR_LENGTH(s) + pos + 1; \
         WHILE cur >= 1 LOOP IF SUBSTRING(s, cur, CHAR_LENGTH(sub)) = sub THEN c := c + 1; \
           IF c = nth THEN RETURN cur; END IF; END IF; cur := cur - 1; END LOOP; RETURN 0; END IF; END",
    // NUMTODSINTERVAL / NUMTOYMINTERVAL as bare values: Oracle's canonical text
    // form. In `date + NUMTOxINTERVAL(...)` arithmetic translate.rs rewrites to
    // DATE_ADD instead.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.numtodsinterval(n DOUBLE, unit VARCHAR(16)) RETURN VARCHAR(64) AS \
       secs DOUBLE; sgn VARCHAR(1) DEFAULT '+'; d INT; h INT; m INT; s DOUBLE; BEGIN \
       secs := n * CASE UPPER(unit) WHEN 'DAY' THEN 86400 WHEN 'HOUR' THEN 3600 \
         WHEN 'MINUTE' THEN 60 WHEN 'SECOND' THEN 1 ELSE 0 END; \
       IF secs < 0 THEN sgn := '-'; secs := -secs; END IF; \
       d := FLOOR(secs/86400); secs := secs - d*86400; \
       h := FLOOR(secs/3600); secs := secs - h*3600; \
       m := FLOOR(secs/60); s := secs - m*60; \
       RETURN CONCAT(sgn, LPAD(d,2,'0'), ' ', LPAD(h,2,'0'), ':', LPAD(m,2,'0'), ':', LPAD(FORMAT(s,6),9,'0')); END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.numtoyminterval(n INT, unit VARCHAR(16)) RETURN VARCHAR(32) AS \
       months INT; sgn VARCHAR(1) DEFAULT '+'; BEGIN \
       months := n * CASE UPPER(unit) WHEN 'YEAR' THEN 12 WHEN 'MONTH' THEN 1 ELSE 0 END; \
       IF months < 0 THEN sgn := '-'; months := -months; END IF; \
       RETURN CONCAT(sgn, LPAD(FLOOR(months/12),2,'0'), '-', LPAD(MOD(months,12),2,'0')); END",
    // LISTAGG as a true aggregate (also covers the plain GROUP_CONCAT path when
    // translate.rs cannot see a WITHIN GROUP clause to convert).
    "CREATE OR REPLACE AGGREGATE FUNCTION dbsaci_sys.listagg(x TEXT, sep TEXT) RETURN TEXT AS \
       acc TEXT DEFAULT NULL; BEGIN LOOP FETCH GROUP NEXT ROW; \
       IF x IS NOT NULL THEN IF acc IS NULL THEN acc := x; ELSE acc := CONCAT(acc, sep, x); END IF; END IF; \
       END LOOP; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN acc; END",
    // Oracle raises on SELECT-time faults that MariaDB softens to NULL/0. These
    // helpers SIGNAL the SQLSTATEs `oracle_error_for_pos` already maps.
    "CREATE OR REPLACE FUNCTION dbsaci_sys.dbsaci_div(a DECIMAL(65,30), b DECIMAL(65,30)) RETURN DECIMAL(65,30) AS \
       BEGIN IF b IS NULL THEN RETURN NULL; END IF; \
       IF b = 0 THEN SIGNAL SQLSTATE '22012' SET MESSAGE_TEXT = 'ORA-01476: divisor is equal to zero'; END IF; \
       RETURN a / b; END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.dbsaci_to_number(s TEXT) RETURN DECIMAL(65,30) AS \
       BEGIN IF s IS NULL THEN RETURN NULL; END IF; \
       IF TRIM(s) = '' OR s NOT REGEXP '^[+-]?[0-9]+(\\.[0-9]*)?([eE][+-]?[0-9]+)?$' THEN \
         SIGNAL SQLSTATE '22018' SET MESSAGE_TEXT = 'ORA-01722: invalid number'; END IF; \
       RETURN CAST(s AS DECIMAL(65,30)); END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.dbsaci_to_date(s TEXT) RETURN DATETIME AS \
       d DATETIME; BEGIN IF s IS NULL THEN RETURN NULL; END IF; \
       d := STR_TO_DATE(s, '%Y-%m-%d %H:%i:%s'); \
       IF d IS NULL THEN d := STR_TO_DATE(s, '%Y-%m-%d'); END IF; \
       IF d IS NULL THEN d := STR_TO_DATE(s, '%d-%b-%Y'); END IF; \
       IF d IS NULL THEN \
         SIGNAL SQLSTATE '22007' SET MESSAGE_TEXT = 'ORA-01858: a non-numeric character was found where a numeric was expected'; END IF; \
       RETURN d; END",
    "CREATE OR REPLACE FUNCTION dbsaci_sys.dbsaci_num_add(a TEXT, b DECIMAL(65,30)) RETURN DECIMAL(65,30) AS \
       BEGIN RETURN dbsaci_to_number(a) + b; END",
];

/// The catalog views: `(name, SELECT)`. Owners and object names go through
/// `oname`, MariaDB's own schemas are hidden, and every view runs with the
/// *invoker's* rights so `information_schema` shows what the session's user
/// can see (Oracle's ALL_* semantics).
fn view_definitions() -> Vec<(&'static str, String)> {
    let sys = SYSTEM_SCHEMAS;
    let numeric = "('tinyint','smallint','mediumint','int','bigint')";
    let mut v: Vec<(&'static str, String)> = vec![
        (
            "all_tables",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS owner, dbsaci_sys.oname(table_name) AS table_name, \
                   'USERS' AS tablespace_name, 'VALID' AS status, table_rows AS num_rows, \
                   CASE WHEN table_type = 'TEMPORARY' THEN 'Y' ELSE 'N' END AS temporary, \
                   NULL AS iot_type, 'NO' AS partitioned, 'NO' AS nested, 'NO' AS dropped, \
                   NULL AS blocks, NULL AS empty_blocks, avg_row_length AS avg_row_len, \
                   update_time AS last_analyzed, 'YES' AS logging, 'DISABLED' AS compression \
                 FROM information_schema.tables \
                 WHERE table_type IN ('BASE TABLE', 'TEMPORARY') AND table_schema NOT IN {sys}"
            ),
        ),
        (
            "all_tab_columns",
            format!(
                "SELECT dbsaci_sys.oname(c.table_schema) AS owner, dbsaci_sys.oname(c.table_name) AS table_name, \
                   dbsaci_sys.oname(c.column_name) AS column_name, \
                   CASE \
                     WHEN c.data_type = 'decimal' OR c.data_type IN {numeric} THEN 'NUMBER' \
                     WHEN c.data_type = 'double' THEN 'BINARY_DOUBLE' \
                     WHEN c.data_type = 'float' THEN 'BINARY_FLOAT' \
                     WHEN c.data_type IN ('varchar','enum','set') THEN 'VARCHAR2' \
                     WHEN c.data_type = 'char' THEN 'CHAR' \
                     WHEN c.data_type IN ('text','mediumtext','longtext','tinytext','json') THEN 'CLOB' \
                     WHEN c.data_type IN ('blob','mediumblob','longblob','tinyblob') THEN 'BLOB' \
                     WHEN c.data_type IN ('varbinary','binary') THEN 'RAW' \
                     WHEN c.data_type IN ('datetime','timestamp') AND c.datetime_precision > 0 \
                       THEN CONCAT('TIMESTAMP(', c.datetime_precision, ')') \
                     WHEN c.data_type IN ('datetime','timestamp','date') THEN 'DATE' \
                     ELSE UPPER(c.data_type) END AS data_type, \
                   CASE WHEN c.data_type = 'decimal' OR c.data_type IN {numeric} THEN 22 \
                        WHEN c.data_type IN ('datetime','timestamp','date') THEN 7 \
                        ELSE c.character_maximum_length END AS data_length, \
                   CASE WHEN c.data_type = 'decimal' \
                          AND NOT (c.numeric_precision = 65 AND c.numeric_scale = 30) THEN c.numeric_precision \
                        WHEN c.data_type IN {numeric} THEN c.numeric_precision \
                        ELSE NULL END AS data_precision, \
                   CASE WHEN c.data_type = 'decimal' \
                          AND NOT (c.numeric_precision = 65 AND c.numeric_scale = 30) THEN c.numeric_scale \
                        WHEN c.data_type IN {numeric} THEN 0 \
                        ELSE NULL END AS data_scale, \
                   CASE WHEN c.is_nullable = 'YES' THEN 'Y' ELSE 'N' END AS nullable, \
                   c.ordinal_position AS column_id, c.column_default AS data_default, \
                   c.character_maximum_length AS char_length, \
                   CASE WHEN c.character_maximum_length IS NULL THEN NULL ELSE 'C' END AS char_used, \
                   NULL AS data_type_owner, NULL AS data_type_mod, 'NO' AS default_on_null \
                 FROM information_schema.columns c WHERE c.table_schema NOT IN {sys}"
            ),
        ),
        (
            "all_objects",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS owner, dbsaci_sys.oname(table_name) AS object_name, \
                   NULL AS subobject_name, NULL AS object_id, \
                   CASE table_type WHEN 'SEQUENCE' THEN 'SEQUENCE' WHEN 'VIEW' THEN 'VIEW' ELSE 'TABLE' END \
                     AS object_type, create_time AS created, update_time AS last_ddl_time, \
                   'VALID' AS status, CASE WHEN table_type = 'TEMPORARY' THEN 'Y' ELSE 'N' END AS temporary \
                 FROM information_schema.tables WHERE table_schema NOT IN {sys} \
                 UNION ALL SELECT dbsaci_sys.oname(routine_schema), dbsaci_sys.oname(routine_name), NULL, NULL, \
                   routine_type, created, last_altered, 'VALID', 'N' \
                 FROM information_schema.routines WHERE routine_schema NOT IN {sys} \
                 UNION ALL SELECT dbsaci_sys.oname(trigger_schema), dbsaci_sys.oname(trigger_name), NULL, NULL, \
                   'TRIGGER', created, created, 'VALID', 'N' \
                 FROM information_schema.triggers WHERE trigger_schema NOT IN {sys} \
                 UNION ALL SELECT dbsaci_sys.oname(index_schema), dbsaci_sys.oname(index_name), NULL, NULL, \
                   'INDEX', NULL, NULL, 'VALID', 'N' \
                 FROM (SELECT DISTINCT index_schema, index_name FROM information_schema.statistics \
                       WHERE index_schema NOT IN {sys} AND index_name <> 'PRIMARY') ix"
            ),
        ),
        (
            "all_constraints",
            format!(
                "SELECT dbsaci_sys.oname(tc.constraint_schema) AS owner, \
                   dbsaci_sys.oname(tc.constraint_name) AS constraint_name, \
                   CASE tc.constraint_type WHEN 'PRIMARY KEY' THEN 'P' WHEN 'UNIQUE' THEN 'U' \
                     WHEN 'FOREIGN KEY' THEN 'R' WHEN 'CHECK' THEN 'C' ELSE tc.constraint_type END \
                     AS constraint_type, \
                   dbsaci_sys.oname(tc.table_name) AS table_name, cc.check_clause AS search_condition, \
                   dbsaci_sys.oname(rc.unique_constraint_schema) AS r_owner, \
                   dbsaci_sys.oname(rc.unique_constraint_name) AS r_constraint_name, \
                   CASE WHEN rc.delete_rule IS NULL THEN NULL \
                        WHEN rc.delete_rule IN ('CASCADE', 'SET NULL') THEN rc.delete_rule \
                        ELSE 'NO ACTION' END AS delete_rule, \
                   'ENABLED' AS status, 'NOT DEFERRABLE' AS deferrable, 'IMMEDIATE' AS deferred, \
                   'VALIDATED' AS validated, 'USER NAME' AS generated, NULL AS index_name \
                 FROM information_schema.table_constraints tc \
                 LEFT JOIN information_schema.referential_constraints rc \
                   ON rc.constraint_schema = tc.constraint_schema \
                  AND rc.constraint_name = tc.constraint_name AND rc.table_name = tc.table_name \
                 LEFT JOIN information_schema.check_constraints cc \
                   ON cc.constraint_schema = tc.constraint_schema \
                  AND cc.constraint_name = tc.constraint_name AND cc.table_name = tc.table_name \
                 WHERE tc.constraint_schema NOT IN {sys}"
            ),
        ),
        (
            "all_cons_columns",
            format!(
                "SELECT dbsaci_sys.oname(constraint_schema) AS owner, \
                   dbsaci_sys.oname(constraint_name) AS constraint_name, \
                   dbsaci_sys.oname(table_name) AS table_name, \
                   dbsaci_sys.oname(column_name) AS column_name, ordinal_position AS position \
                 FROM information_schema.key_column_usage WHERE constraint_schema NOT IN {sys}"
            ),
        ),
        (
            "all_indexes",
            format!(
                "SELECT dbsaci_sys.oname(index_schema) AS owner, dbsaci_sys.oname(index_name) AS index_name, \
                   'NORMAL' AS index_type, dbsaci_sys.oname(index_schema) AS table_owner, \
                   dbsaci_sys.oname(table_name) AS table_name, 'TABLE' AS table_type, \
                   CASE WHEN MIN(non_unique) = 0 THEN 'UNIQUE' ELSE 'NONUNIQUE' END AS uniqueness, \
                   'VALID' AS status, 'USERS' AS tablespace_name, NULL AS ityp_owner, NULL AS ityp_name, \
                   NULL AS parameters, NULL AS funcidx_status, 'VISIBLE' AS visibility, \
                   CASE WHEN index_name = 'PRIMARY' THEN 'YES' ELSE 'NO' END AS constraint_index, \
                   '1' AS degree, 'NO' AS partitioned, 'N' AS temporary, \
                   MAX(cardinality) AS distinct_keys, NULL AS leaf_blocks, NULL AS blevel, \
                   NULL AS clustering_factor, MAX(cardinality) AS num_rows, \
                   NULL AS last_analyzed \
                 FROM information_schema.statistics WHERE index_schema NOT IN {sys} \
                 GROUP BY index_schema, index_name, table_name"
            ),
        ),
        (
            "all_ind_columns",
            format!(
                "SELECT dbsaci_sys.oname(index_schema) AS index_owner, \
                   dbsaci_sys.oname(index_name) AS index_name, \
                   dbsaci_sys.oname(index_schema) AS table_owner, dbsaci_sys.oname(table_name) AS table_name, \
                   dbsaci_sys.oname(column_name) AS column_name, seq_in_index AS column_position, \
                   sub_part AS column_length, CASE WHEN collation = 'D' THEN 'DESC' ELSE 'ASC' END AS descend \
                 FROM information_schema.statistics WHERE index_schema NOT IN {sys}"
            ),
        ),
        (
            "all_sequences",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS sequence_owner, \
                   dbsaci_sys.oname(table_name) AS sequence_name, \
                   NULL AS min_value, NULL AS max_value, 1 AS increment_by, 'N' AS cycle_flag, \
                   'N' AS order_flag, 0 AS cache_size, NULL AS last_number \
                 FROM information_schema.tables \
                 WHERE table_type = 'SEQUENCE' AND table_schema NOT IN {sys}"
            ),
        ),
        (
            "all_tab_comments",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS owner, dbsaci_sys.oname(table_name) AS table_name, \
                   CASE table_type WHEN 'VIEW' THEN 'VIEW' ELSE 'TABLE' END AS table_type, \
                   NULLIF(table_comment, '') AS comments \
                 FROM information_schema.tables \
                 WHERE table_schema NOT IN {sys} AND table_type <> 'SEQUENCE'"
            ),
        ),
        (
            "all_col_comments",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS owner, dbsaci_sys.oname(table_name) AS table_name, \
                   dbsaci_sys.oname(column_name) AS column_name, NULLIF(column_comment, '') AS comments \
                 FROM information_schema.columns WHERE table_schema NOT IN {sys}"
            ),
        ),
        // Multi-event Oracle triggers are split into `name` +
        // `name__dbsaci_update`; reassemble them under one name.
        (
            "all_triggers",
            format!(
                "SELECT owner, trigger_name, MAX(trigger_type) AS trigger_type, \
                   GROUP_CONCAT(DISTINCT event_manipulation ORDER BY \
                     FIELD(event_manipulation,'INSERT','UPDATE','DELETE') SEPARATOR ' OR ') \
                     AS triggering_event, \
                   MAX(table_owner) AS table_owner, 'TABLE' AS base_object_type, \
                   MAX(table_name) AS table_name, NULL AS column_name, \
                   'REFERENCING NEW AS NEW OLD AS OLD' AS referencing_names, NULL AS when_clause, \
                   MAX(status) AS status, NULL AS description, MAX(action_type) AS action_type, \
                   MAX(trigger_body) AS trigger_body, MAX(before_row) AS before_row \
                 FROM ( \
                   SELECT dbsaci_sys.oname(trigger_schema) AS owner, \
                     dbsaci_sys.oname(IF(RIGHT(trigger_name, 15) = '__dbsaci_update', \
                       LEFT(trigger_name, CHAR_LENGTH(trigger_name) - 15), trigger_name)) AS trigger_name, \
                     CONCAT(action_timing, ' EACH ROW') AS trigger_type, event_manipulation, \
                     dbsaci_sys.oname(event_object_schema) AS table_owner, \
                     dbsaci_sys.oname(event_object_table) AS table_name, 'ENABLED' AS status, \
                     action_statement AS trigger_body, 'PL/SQL' AS action_type, \
                     IF(action_timing = 'BEFORE', 'YES', 'NO') AS before_row \
                   FROM information_schema.triggers WHERE trigger_schema NOT IN {sys} \
                 ) t GROUP BY owner, trigger_name"
            ),
        ),
        (
            "all_views",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS owner, dbsaci_sys.oname(table_name) AS view_name, \
                   CHAR_LENGTH(view_definition) AS text_length, view_definition AS text, \
                   'N' AS read_only \
                 FROM information_schema.views WHERE table_schema NOT IN {sys}"
            ),
        ),
        (
            "all_users",
            format!(
                "SELECT dbsaci_sys.oname(schema_name) AS username, NULL AS user_id, NULL AS created, \
                   'NO' AS common, 'NO' AS oracle_maintained, 'OPEN' AS account_status, \
                   'USERS' AS default_tablespace \
                 FROM information_schema.schemata WHERE schema_name NOT IN {sys}"
            ),
        ),
        (
            "all_procedures",
            format!(
                "SELECT dbsaci_sys.oname(routine_schema) AS owner, \
                   dbsaci_sys.oname(routine_name) AS object_name, NULL AS procedure_name, \
                   NULL AS object_id, 1 AS subprogram_id, NULL AS overload, \
                   routine_type AS object_type, 'NO' AS aggregate, 'NO' AS pipelined, \
                   NULL AS impltypeowner, NULL AS impltypename, 'NO' AS parallel, \
                   'NO' AS interface, \
                   CASE WHEN is_deterministic = 'YES' THEN 'YES' ELSE 'NO' END AS `deterministic`, \
                   CASE WHEN security_type = 'INVOKER' THEN 'CURRENT_USER' ELSE 'DEFINER' END \
                     AS authid \
                 FROM information_schema.routines WHERE routine_schema NOT IN {sys}"
            ),
        ),
        (
            "all_arguments",
            format!(
                "SELECT dbsaci_sys.oname(p.specific_schema) AS owner, \
                   dbsaci_sys.oname(p.specific_name) AS object_name, NULL AS package_name, \
                   NULL AS object_id, NULL AS overload, 1 AS subprogram_id, \
                   dbsaci_sys.oname(p.parameter_name) AS argument_name, \
                   COALESCE(p.ordinal_position, 0) AS position, \
                   COALESCE(p.ordinal_position, 0) AS sequence, 0 AS data_level, \
                   UPPER(p.data_type) AS data_type, NULL AS defaulted, \
                   CASE WHEN p.parameter_mode IS NULL THEN 'OUT' \
                        WHEN p.parameter_mode = 'INOUT' THEN 'IN/OUT' \
                        ELSE p.parameter_mode END AS in_out, \
                   p.character_maximum_length AS data_length, \
                   p.numeric_precision AS data_precision, p.numeric_scale AS data_scale, \
                   10 AS radix, NULL AS char_length \
                 FROM information_schema.parameters p WHERE p.specific_schema NOT IN {sys}"
            ),
        ),
        (
            "all_tab_identity_cols",
            format!(
                "SELECT dbsaci_sys.oname(table_schema) AS owner, dbsaci_sys.oname(table_name) AS table_name, \
                   dbsaci_sys.oname(column_name) AS column_name, 'BY DEFAULT' AS generation_type, \
                   NULL AS sequence_name, NULL AS identity_options \
                 FROM information_schema.columns \
                 WHERE extra LIKE '%auto_increment%' AND table_schema NOT IN {sys}"
            ),
        ),
        (
            "all_synonyms",
            "SELECT NULL AS owner, NULL AS synonym_name, NULL AS table_owner, \
               NULL AS table_name, NULL AS db_link FROM DUAL WHERE 1=0"
                .into(),
        ),
        (
            "all_mviews",
            "SELECT NULL AS owner, NULL AS mview_name, NULL AS query FROM DUAL WHERE 1=0".into(),
        ),
        (
            "all_tab_cols",
            "SELECT c.*, 'NO' AS hidden_column, 'NO' AS virtual_column, 'YES' AS user_generated, \
               c.column_id AS internal_column_id FROM dbsaci_sys.all_tab_columns c"
                .into(),
        ),
        (
            // Object tables only in DbSaci's model (empty), as on PostgreSQL.
            "all_all_tables",
            "SELECT * FROM dbsaci_sys.all_tables WHERE 1=0".into(),
        ),
        (
            "all_object_tables",
            "SELECT * FROM dbsaci_sys.all_tables WHERE 1=0".into(),
        ),
        (
            "user_tablespaces",
            "SELECT 'USERS' AS tablespace_name, 'ONLINE' AS status FROM DUAL".into(),
        ),
        (
            "user_dependencies",
            "SELECT NULL AS name, NULL AS type, NULL AS referenced_owner, \
               NULL AS referenced_name, NULL AS referenced_type FROM DUAL WHERE 1=0"
                .into(),
        ),
        (
            "session_privs",
            "SELECT NULL AS privilege FROM DUAL WHERE 1=0".into(),
        ),
        (
            "session_roles",
            "SELECT NULL AS role FROM DUAL WHERE 1=0".into(),
        ),
        (
            "v$version",
            "SELECT CONCAT('Oracle Database 19c compatible (DbSaci on MariaDB ', VERSION(), ')') \
               AS banner FROM DUAL"
                .into(),
        ),
        (
            "product_component_version",
            "SELECT 'Oracle Database 19c' AS product, '19.0.0.0.0' AS version, \
               '19.0.0.0.0' AS version_full, 'Production' AS status FROM DUAL"
                .into(),
        ),
        (
            "global_name",
            "SELECT dbsaci_sys.oname(dbsaci_sys.current_schema()) AS global_name FROM DUAL".into(),
        ),
    ];
    let nls_params = [
        "NLS_LANGUAGE",
        "NLS_TERRITORY",
        "NLS_DATE_FORMAT",
        "NLS_DATE_LANGUAGE",
        "NLS_NUMERIC_CHARACTERS",
        "NLS_TIMESTAMP_FORMAT",
        "NLS_TIMESTAMP_TZ_FORMAT",
        "NLS_SORT",
        "NLS_COMP",
        "NLS_CALENDAR",
        "NLS_CHARACTERSET",
        "NLS_NCHAR_CHARACTERSET",
    ];
    let nls = nls_params
        .iter()
        .map(|p| format!("SELECT '{p}' AS parameter, dbsaci_sys.nls('{p}') AS value FROM DUAL"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    v.push(("nls_session_parameters", nls.clone()));
    v.push(("nls_database_parameters", nls));
    // USER_* = ALL_* restricted to the session's own schema.
    let user_views: &[(&'static str, &str, &str)] = &[
        ("user_tables", "all_tables", "owner"),
        ("user_tab_columns", "all_tab_columns", "owner"),
        ("user_tab_cols", "all_tab_cols", "owner"),
        ("user_objects", "all_objects", "owner"),
        ("user_constraints", "all_constraints", "owner"),
        ("user_cons_columns", "all_cons_columns", "owner"),
        ("user_indexes", "all_indexes", "owner"),
        ("user_ind_columns", "all_ind_columns", "index_owner"),
        ("user_sequences", "all_sequences", "sequence_owner"),
        ("user_tab_comments", "all_tab_comments", "owner"),
        ("user_col_comments", "all_col_comments", "owner"),
        ("user_triggers", "all_triggers", "owner"),
        ("user_views", "all_views", "owner"),
        ("user_synonyms", "all_synonyms", "owner"),
        ("user_mviews", "all_mviews", "owner"),
        ("user_procedures", "all_procedures", "owner"),
        ("user_arguments", "all_arguments", "owner"),
    ];
    for (user, all, owner) in user_views {
        v.push((
            user,
            // DbSaci's shared `PUBLIC` schema (fixtures, reference data) is
            // listed with the user's own objects, as on PostgreSQL.
            format!(
                "SELECT * FROM dbsaci_sys.{all}                  WHERE {owner} = dbsaci_sys.login_user() OR {owner} = 'PUBLIC'"
            ),
        ));
    }
    // Empty dictionary views IDE schema browsers walk; each carries the
    // columns those introspectors filter on.
    let stubs: &[(&'static str, &str)] = &[
        ("all_mview_logs", "NULL AS log_owner, NULL AS master"),
        (
            "all_mview_comments",
            "NULL AS owner, NULL AS mview_name, NULL AS comments",
        ),
        (
            "all_ind_expressions",
            "NULL AS index_owner, NULL AS index_name, NULL AS table_owner, NULL AS table_name, \
             NULL AS column_expression, NULL AS column_position",
        ),
        (
            "all_tab_partitions",
            "NULL AS table_owner, NULL AS table_name, NULL AS partition_name",
        ),
        ("all_part_tables", "NULL AS owner, NULL AS table_name"),
        (
            "all_part_key_columns",
            "NULL AS owner, NULL AS name, NULL AS column_name",
        ),
        (
            "all_tab_subpartitions",
            "NULL AS table_owner, NULL AS table_name, NULL AS subpartition_name",
        ),
        (
            "all_ind_partitions",
            "NULL AS index_owner, NULL AS index_name, NULL AS partition_name",
        ),
        (
            "all_lobs",
            "NULL AS owner, NULL AS table_name, NULL AS column_name",
        ),
        ("all_nested_tables", "NULL AS owner, NULL AS table_name"),
        (
            "all_trigger_cols",
            "NULL AS trigger_owner, NULL AS trigger_name, NULL AS column_name",
        ),
        (
            "all_type_attrs",
            "NULL AS owner, NULL AS type_name, NULL AS attr_name",
        ),
        ("all_coll_types", "NULL AS owner, NULL AS type_name"),
        (
            "all_tab_privs",
            "NULL AS grantee, NULL AS owner, NULL AS table_name, NULL AS privilege",
        ),
        (
            "all_col_privs",
            "NULL AS grantee, NULL AS owner, NULL AS table_name, NULL AS column_name",
        ),
        ("all_role_privs", "NULL AS grantee, NULL AS granted_role"),
        (
            "all_directories",
            "NULL AS owner, NULL AS directory_name, NULL AS directory_path",
        ),
        ("all_java_classes", "NULL AS owner, NULL AS name"),
        ("all_clusters", "NULL AS owner, NULL AS cluster_name"),
        ("all_editioning_views", "NULL AS owner, NULL AS view_name"),
        ("all_xml_schemas", "NULL AS owner, NULL AS schema_url"),
        (
            "all_scheduler_programs",
            "NULL AS owner, NULL AS program_name",
        ),
        ("all_queue_tables", "NULL AS owner, NULL AS queue_table"),
        (
            "all_tab_col_statistics",
            "NULL AS owner, NULL AS table_name, NULL AS column_name",
        ),
        ("all_registered_mviews", "NULL AS owner, NULL AS mview_name"),
        (
            "all_identifiers",
            "NULL AS owner, NULL AS object_name, NULL AS name",
        ),
        ("all_external_tables", "NULL AS owner, NULL AS table_name"),
    ];
    for (name, cols) in stubs {
        v.push((name, format!("SELECT {cols} FROM DUAL WHERE 1=0")));
    }
    v
}

/// The one collation of everything DbSaci installs. Binary, like Oracle's
/// dictionary comparisons. Pinned explicitly because MariaDB otherwise mixes
/// the server collation (`collation_server`, used for routine parameters and
/// results) with `character_set_collations` (used for literals and new
/// databases from 11.x), and comparing the two raises "Illegal mix of
/// collations".
pub const COLLATION: &str = "utf8mb4_bin";

/// Give every character type in a stored-routine definition (`VARCHAR(n)`,
/// `TEXT`, in parameters, return types and local variables) an explicit
/// charset and [`COLLATION`].
fn pin_collation(ddl: &str) -> String {
    let mut out = String::with_capacity(ddl.len() + 128);
    let bytes = ddl.as_bytes();
    let mut i = 0;
    let mut in_lit = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' {
            in_lit = !in_lit;
            out.push('\'');
            i += 1;
            continue;
        }
        let word_start =
            !in_lit && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'));
        let rest = &ddl[i..];
        let up = rest.get(..8).unwrap_or(rest).to_ascii_uppercase();
        let ty_len = if word_start && up.starts_with("VARCHAR(") {
            rest.find(')').map(|e| e + 1)
        } else if word_start
            && up.starts_with("TEXT")
            && !rest[4..].starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
        {
            Some(4)
        } else {
            None
        };
        if let Some(n) = ty_len {
            out.push_str(&rest[..n]);
            if !rest[n..]
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CHARACTER SET")
            {
                out.push_str(&format!(" CHARACTER SET utf8mb4 COLLATE {COLLATION}"));
            }
            i += n;
            continue;
        }
        let ch = rest.chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Every statement of an install, in order (idempotent: `CREATE OR REPLACE`).
pub fn install_statements() -> Vec<String> {
    let mut out = vec![
        format!("CREATE DATABASE IF NOT EXISTS {SCHEMA} CHARACTER SET utf8mb4 COLLATE {COLLATION}"),
        // An existing (older) install may have been created with the server's
        // default collation.
        format!("ALTER DATABASE {SCHEMA} CHARACTER SET utf8mb4 COLLATE {COLLATION}"),
        format!(
            "CREATE TABLE IF NOT EXISTS {SCHEMA}.install_info \
             (only_one TINYINT PRIMARY KEY DEFAULT 1, version VARCHAR(64) NOT NULL)"
        ),
    ];
    out.extend(SESSION_ROUTINES.iter().map(|s| pin_collation(s)));
    out.extend(COMPAT_FUNCTIONS.iter().map(|s| pin_collation(s)));
    // The translator's runtime helpers (ORA errors raised at run time, RR
    // dates, TO_DATE validation, date TRUNC/ROUND, number masks).
    out.extend(
        crate::translate::mariadb::HELPERS
            .iter()
            .map(|(_, ddl)| pin_collation(&ddl.replacen(" FUNCTION ", " FUNCTION dbsaci_sys.", 1))),
    );
    for (name, select) in view_definitions() {
        out.push(format!(
            "CREATE OR REPLACE SQL SECURITY INVOKER VIEW {SCHEMA}.`{name}` AS {select}"
        ));
    }
    out.push(format!(
        "INSERT INTO {SCHEMA}.install_info (only_one, version) VALUES (1, '{VERSION}') \
         ON DUPLICATE KEY UPDATE version = VALUES(version)"
    ));
    out
}

/// Lets every account use the dictionary (`PUBLIC`, MariaDB 10.11+).
pub fn grant_statements() -> Vec<String> {
    vec![format!("GRANT SELECT, EXECUTE ON {SCHEMA}.* TO PUBLIC")]
}

/// Names of the per-schema objects 0.2.x created on every connect:
/// `(views, routines)`. `dbsaci install --drop-legacy` removes them.
pub fn legacy_object_names() -> (Vec<&'static str>, Vec<&'static str>) {
    let views: Vec<&str> = RELATIONS.iter().copied().chain(["v_version"]).collect();
    let routines = vec![
        "months_between",
        "next_day",
        "initcap",
        "oracle_translate",
        "regexp_count",
        "oracle_regexp_substr",
        "oracle_instr",
        "numtodsinterval",
        "numtoyminterval",
        "listagg",
        "dbsaci_div",
        "dbsaci_to_number",
        "dbsaci_to_date",
        "dbsaci_num_add",
        "sys_context",
        "sessiontimezone",
        "dbtimezone",
        "dbms_output.put_line",
        "dbms_metadata.get_ddl",
    ];
    (views, routines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_character_types_get_one_collation() {
        let ddl = pin_collation(
            "CREATE FUNCTION f(a VARCHAR(10), t TEXT) RETURN VARCHAR(255) AS v TEXT; \
             BEGIN RETURN CONCAT('TEXT', a); END",
        );
        assert_eq!(ddl.matches("COLLATE utf8mb4_bin").count(), 4, "{ddl}");
        assert!(ddl.contains("'TEXT'"));
        for stmt in install_statements() {
            if stmt.contains(" FUNCTION ") || stmt.contains(" PROCEDURE ") {
                let bare = stmt.to_ascii_uppercase().matches("VARCHAR(").count();
                let pinned = stmt.matches("COLLATE utf8mb4_bin").count();
                assert!(
                    pinned >= bare,
                    "unpinned type in: {}",
                    &stmt[..80.min(stmt.len())]
                );
            }
        }
    }

    #[test]
    fn every_listed_relation_is_installed() {
        let defined: Vec<&str> = view_definitions().iter().map(|(n, _)| *n).collect();
        for r in RELATIONS {
            assert!(
                defined.contains(r),
                "dictionary relation {r} has no definition"
            );
        }
        for (n, _) in view_definitions() {
            assert!(
                RELATIONS.contains(&n),
                "view {n} is not routed (add it to RELATIONS)"
            );
        }
    }

    #[test]
    fn every_listed_function_is_installed() {
        let all = install_statements().join("\n");
        for f in super::super::dictionary::FUNCTIONS {
            assert!(
                all.contains(&format!("FUNCTION dbsaci_sys.{f}(")),
                "function {f} has no definition"
            );
        }
        for (_, _, r) in super::super::dictionary::PACKAGE_ROUTINES {
            assert!(
                all.contains(&format!("dbsaci_sys.{r}(")),
                "routine {r} missing"
            );
        }
    }
}
