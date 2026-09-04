//! MariaDB backend primitives.
//!
//! MariaDB's `SQL_MODE=ORACLE` performs the Oracle-language work in the
//! database. The adapter keeps one backend connection per Oracle session so
//! transactions, temporary objects, and session settings retain their state.

use std::{collections::HashSet, sync::Arc};

use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::{Conn, Opts, Params, Value, prelude::Queryable};

use crate::backend::{DescribeCaps, OracleBackend, OracleCursor};
use crate::error::{Error, Result};
use crate::wire::{BindValue, ColumnMeta, encode_oracle_number_decimal};

/// A MariaDB connection configured for Oracle compatibility mode.
pub struct MariaDbBackend {
    /// The `mysql://` URL, kept so a dropped connection can be rebuilt.
    url: String,
    /// Schema this session resolves unqualified names in: the Oracle login name
    /// when a database of that name exists (Oracle's schema == user), otherwise
    /// the configured default database. Re-applied on every reconnect.
    schema: String,
    conn: tokio::sync::Mutex<Conn>,
    /// Oracle defines CURRVAL per session and rejects it before that session
    /// has requested NEXTVAL. MariaDB's Oracle mode instead yields a value.
    sequences_with_currval: tokio::sync::Mutex<HashSet<String>>,
}

/// Session `sql_mode` applied on connect and re-asserted before each statement
/// (a `ROLLBACK`-driven reconnect or a stray `SET` must not lose it). `ORACLE`
/// is itself a compound alias; `SET sql_mode='ORACLE'` *replaces* MariaDB's
/// default and so drops strict/division-by-zero checking — the extra flags
/// restore Oracle-equivalent behaviour that would otherwise need translation:
///   * `STRICT_ALL_TABLES`        raise on write-time overflow / truncation
///   * `ERROR_FOR_DIVISION_BY_ZERO` raise on `x/0` in INSERT/UPDATE
///   * `ONLY_FULL_GROUP_BY`       reject a non-aggregated column outside GROUP BY
///   * `EMPTY_STRING_IS_NULL`     treat `''` as `NULL` (Oracle's core NULL model)
///   * `NO_BACKSLASH_ESCAPES`     no C-style escapes in string literals, so
///     `\w` / `\d` / `\1` pass straight to the regex engine — matching Oracle
const SESSION_SQL_MODE: &str = "SET sql_mode = CONCAT('ORACLE', \
     ',STRICT_ALL_TABLES,ERROR_FOR_DIVISION_BY_ZERO,ONLY_FULL_GROUP_BY\
     ,EMPTY_STRING_IS_NULL,NO_BACKSLASH_ESCAPES')";

/// A portable Oracle data-dictionary / session facade over MariaDB's
/// `information_schema`. Applied best-effort on connect (see `connect`). Owner
/// columns are `UPPER(schema)`; identifiers come back upper-cased, as Oracle
/// tooling expects. Nothing here is corpus-specific.
const MARIADB_FACADE: &[&str] = &[
    // ---- USER_* / ALL_* over information_schema -----------------------------
    "CREATE OR REPLACE VIEW all_tables AS SELECT UPPER(table_schema) AS owner, \
       UPPER(table_name) AS table_name, 'VALID' AS status, table_rows AS num_rows, \
       CASE WHEN table_type='SYSTEM VIEW' THEN 'Y' ELSE 'N' END AS temporary \
     FROM information_schema.tables WHERE table_type IN ('BASE TABLE','SEQUENCE')",
    "CREATE OR REPLACE VIEW user_tables AS SELECT table_name, status, num_rows, temporary \
       FROM all_tables WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_tab_columns AS SELECT UPPER(c.table_schema) AS owner, \
       UPPER(c.table_name) AS table_name, UPPER(c.column_name) AS column_name, \
       UPPER(c.data_type) AS data_type, c.character_maximum_length AS data_length, \
       c.numeric_precision AS data_precision, c.numeric_scale AS data_scale, \
       CASE WHEN c.is_nullable='YES' THEN 'Y' ELSE 'N' END AS nullable, \
       c.ordinal_position AS column_id, c.column_default AS data_default \
     FROM information_schema.columns c",
    "CREATE OR REPLACE VIEW user_tab_columns AS SELECT table_name, column_name, data_type, \
       data_length, data_precision, data_scale, nullable, column_id, data_default \
       FROM all_tab_columns WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_tab_cols AS SELECT c.*, 'NO' AS hidden_column, \
       'NO' AS virtual_column, 'YES' AS user_generated FROM all_tab_columns c",
    "CREATE OR REPLACE VIEW user_tab_cols AS SELECT * FROM all_tab_cols WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_objects AS \
       SELECT UPPER(table_schema) AS owner, UPPER(table_name) AS object_name, \
         CASE table_type WHEN 'SEQUENCE' THEN 'SEQUENCE' WHEN 'VIEW' THEN 'VIEW' ELSE 'TABLE' END AS object_type, \
         'VALID' AS status FROM information_schema.tables \
       UNION ALL SELECT UPPER(routine_schema), UPPER(routine_name), \
         CASE routine_type WHEN 'PROCEDURE' THEN 'PROCEDURE' ELSE 'FUNCTION' END, 'VALID' \
       FROM information_schema.routines \
       UNION ALL SELECT UPPER(trigger_schema), UPPER(trigger_name), 'TRIGGER', 'VALID' \
       FROM information_schema.triggers",
    "CREATE OR REPLACE VIEW user_objects AS SELECT object_name, object_type, status \
       FROM all_objects WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_constraints AS SELECT UPPER(tc.constraint_schema) AS owner, \
       UPPER(tc.constraint_name) AS constraint_name, \
       CASE tc.constraint_type WHEN 'PRIMARY KEY' THEN 'P' WHEN 'UNIQUE' THEN 'U' \
         WHEN 'FOREIGN KEY' THEN 'R' WHEN 'CHECK' THEN 'C' ELSE tc.constraint_type END AS constraint_type, \
       UPPER(tc.table_name) AS table_name, NULL AS search_condition, 'VALID' AS status \
     FROM information_schema.table_constraints tc",
    "CREATE OR REPLACE VIEW user_constraints AS SELECT constraint_name, constraint_type, \
       table_name, search_condition, status FROM all_constraints WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_cons_columns AS SELECT UPPER(constraint_schema) AS owner, \
       UPPER(constraint_name) AS constraint_name, UPPER(table_name) AS table_name, \
       UPPER(column_name) AS column_name, ordinal_position AS position \
     FROM information_schema.key_column_usage",
    "CREATE OR REPLACE VIEW user_cons_columns AS SELECT constraint_name, table_name, column_name, position \
       FROM all_cons_columns WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_indexes AS SELECT DISTINCT UPPER(index_schema) AS owner, \
       UPPER(index_name) AS index_name, 'NORMAL' AS index_type, UPPER(table_name) AS table_name, \
       UPPER(index_schema) AS table_owner, \
       CASE WHEN non_unique=0 THEN 'UNIQUE' ELSE 'NONUNIQUE' END AS uniqueness, 'VALID' AS status, \
       NULL AS ityp_owner, NULL AS ityp_name, NULL AS parameters, NULL AS funcidx_status, \
       'VISIBLE' AS visibility, 'NO' AS constraint_index, '1' AS degree \
     FROM information_schema.statistics",
    "CREATE OR REPLACE VIEW user_indexes AS SELECT index_name, index_type, table_name, uniqueness, status \
       FROM all_indexes WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_ind_columns AS SELECT UPPER(index_schema) AS index_owner, \
       UPPER(index_name) AS index_name, UPPER(table_name) AS table_name, UPPER(column_name) AS column_name, \
       seq_in_index AS column_position FROM information_schema.statistics",
    "CREATE OR REPLACE VIEW user_ind_columns AS SELECT index_name, table_name, column_name, column_position \
       FROM all_ind_columns WHERE index_owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_sequences AS SELECT UPPER(table_schema) AS sequence_owner, \
       UPPER(table_name) AS sequence_name FROM information_schema.tables WHERE table_type='SEQUENCE'",
    "CREATE OR REPLACE VIEW user_sequences AS SELECT sequence_name FROM all_sequences \
       WHERE sequence_owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_tab_comments AS SELECT UPPER(table_schema) AS owner, \
       UPPER(table_name) AS table_name, \
       CASE table_type WHEN 'VIEW' THEN 'VIEW' ELSE 'TABLE' END AS table_type, \
       NULLIF(table_comment,'') AS comments FROM information_schema.tables",
    "CREATE OR REPLACE VIEW user_tab_comments AS SELECT table_name, table_type, comments \
       FROM all_tab_comments WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_col_comments AS SELECT UPPER(table_schema) AS owner, \
       UPPER(table_name) AS table_name, UPPER(column_name) AS column_name, \
       NULLIF(column_comment,'') AS comments FROM information_schema.columns",
    "CREATE OR REPLACE VIEW user_col_comments AS SELECT table_name, column_name, comments \
       FROM all_col_comments WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_triggers AS SELECT UPPER(trigger_schema) AS owner, \
       UPPER(trigger_name) AS trigger_name, \
       CONCAT(action_timing,' EACH ROW') AS trigger_type, event_manipulation AS triggering_event, \
       UPPER(event_object_schema) AS table_owner, UPPER(event_object_table) AS table_name, \
       'ENABLED' AS status, action_statement AS trigger_body, 'PL/SQL' AS action_type \
     FROM information_schema.triggers",
    "CREATE OR REPLACE VIEW user_triggers AS SELECT trigger_name, trigger_type, triggering_event, \
       table_owner, table_name, status, trigger_body, action_type FROM all_triggers \
       WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_views AS SELECT UPPER(table_schema) AS owner, \
       UPPER(table_name) AS view_name, view_definition AS text FROM information_schema.views",
    "CREATE OR REPLACE VIEW user_views AS SELECT view_name, text FROM all_views WHERE owner = UPPER(DATABASE())",
    "CREATE OR REPLACE VIEW all_users AS SELECT DISTINCT UPPER(schema_name) AS username, \
       'OPEN' AS account_status, 'USERS' AS default_tablespace FROM information_schema.schemata",
    "CREATE OR REPLACE VIEW all_synonyms AS SELECT NULL AS owner, NULL AS synonym_name, \
       NULL AS table_owner, NULL AS table_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW user_synonyms AS SELECT * FROM all_synonyms WHERE 1=0",
    // schema-qualified sys.* copies (IDE / migration tools use the sys prefix)
    "CREATE OR REPLACE VIEW sys.all_users AS SELECT * FROM all_users",
    "CREATE OR REPLACE VIEW sys.all_tables AS SELECT * FROM all_tables",
    "CREATE OR REPLACE VIEW sys.all_tab_columns AS SELECT * FROM all_tab_columns",
    "CREATE OR REPLACE VIEW sys.all_objects AS SELECT * FROM all_objects",
    "CREATE OR REPLACE VIEW sys.all_constraints AS SELECT * FROM all_constraints",
    "CREATE OR REPLACE VIEW sys.all_cons_columns AS SELECT * FROM all_cons_columns",
    "CREATE OR REPLACE VIEW sys.all_indexes AS SELECT * FROM all_indexes",
    "CREATE OR REPLACE VIEW sys.all_ind_columns AS SELECT * FROM all_ind_columns",
    "CREATE OR REPLACE VIEW sys.all_sequences AS SELECT * FROM all_sequences",
    "CREATE OR REPLACE VIEW sys.all_triggers AS SELECT * FROM all_triggers",
    "CREATE OR REPLACE VIEW sys.all_views AS SELECT * FROM all_views",
    "CREATE OR REPLACE VIEW sys.all_tab_comments AS SELECT * FROM all_tab_comments",
    "CREATE OR REPLACE VIEW sys.all_col_comments AS SELECT * FROM all_col_comments",
    "CREATE OR REPLACE VIEW sys.all_synonyms AS SELECT * FROM all_synonyms",
    // Empty stubs an IDE introspector selects while walking a schema.
    "CREATE OR REPLACE VIEW sys.all_mview_logs AS SELECT NULL AS log_owner, NULL AS master FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_mview_comments AS SELECT NULL AS owner, NULL AS mview_name, NULL AS comments FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_object_tables AS SELECT * FROM all_tables WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_ind_expressions AS SELECT NULL AS index_owner, NULL AS index_name, NULL AS column_expression, NULL AS column_position FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_tab_partitions AS SELECT NULL AS table_owner, NULL AS table_name, NULL AS partition_name FROM DUAL WHERE 1=0",
    // Partitioning / LOB / type / privilege dictionary views an IDE schema
    // browser probes. DbSaci exposes no partitioning or object types, so every
    // one is an empty relation carrying just an `owner` column for filters.
    "CREATE OR REPLACE VIEW sys.all_part_tables AS SELECT NULL AS owner, NULL AS table_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_part_key_columns AS SELECT NULL AS owner, NULL AS name, NULL AS column_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_tab_subpartitions AS SELECT NULL AS table_owner, NULL AS table_name, NULL AS subpartition_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_ind_partitions AS SELECT NULL AS index_owner, NULL AS index_name, NULL AS partition_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_lobs AS SELECT NULL AS owner, NULL AS table_name, NULL AS column_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_nested_tables AS SELECT NULL AS owner, NULL AS table_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_trigger_cols AS SELECT NULL AS trigger_owner, NULL AS trigger_name, NULL AS column_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_type_attrs AS SELECT NULL AS owner, NULL AS type_name, NULL AS attr_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_coll_types AS SELECT NULL AS owner, NULL AS type_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_tab_privs AS SELECT NULL AS grantee, NULL AS owner, NULL AS table_name, NULL AS privilege FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_col_privs AS SELECT NULL AS grantee, NULL AS owner, NULL AS table_name, NULL AS column_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_role_privs AS SELECT NULL AS grantee, NULL AS granted_role FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_directories AS SELECT NULL AS owner, NULL AS directory_name, NULL AS directory_path FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_java_classes AS SELECT NULL AS owner, NULL AS name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_clusters AS SELECT NULL AS owner, NULL AS cluster_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_editioning_views AS SELECT NULL AS owner, NULL AS view_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_xml_schemas AS SELECT NULL AS owner, NULL AS schema_url FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_scheduler_programs AS SELECT NULL AS owner, NULL AS program_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_queue_tables AS SELECT NULL AS owner, NULL AS queue_table FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_tab_col_statistics AS SELECT NULL AS owner, NULL AS table_name, NULL AS column_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_registered_mviews AS SELECT NULL AS owner, NULL AS mview_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_identifiers AS SELECT NULL AS owner, NULL AS object_name, NULL AS name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_all_tables AS SELECT NULL AS owner, NULL AS table_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_external_tables AS SELECT NULL AS owner, NULL AS table_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_tab_identity_cols AS SELECT NULL AS owner, NULL AS table_name, NULL AS column_name FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.all_mviews AS SELECT UPPER(table_schema) AS owner, UPPER(table_name) AS mview_name FROM information_schema.tables WHERE 1=0",
    "CREATE OR REPLACE VIEW all_mviews AS SELECT * FROM sys.all_mviews",
    "CREATE OR REPLACE VIEW user_mviews AS SELECT * FROM sys.all_mviews WHERE 1=0",
    // ---- version / NLS / identity ----------------------------------------
    "CREATE OR REPLACE VIEW v_version AS SELECT CONCAT('DbSaci Oracle-compatibility proxy on MariaDB ', VERSION()) AS banner",
    "CREATE OR REPLACE VIEW `v$version` AS SELECT * FROM v_version",
    "CREATE OR REPLACE VIEW sys.`v$version` AS SELECT * FROM v_version",
    "CREATE OR REPLACE VIEW sys.product_component_version AS SELECT 'DbSaci' AS product, '19.0.0.0.0' AS version, '19.0.0.0.0' AS version_full, 'Production' AS status",
    "CREATE OR REPLACE VIEW product_component_version AS SELECT * FROM sys.product_component_version",
    "CREATE OR REPLACE VIEW global_name AS SELECT UPPER(DATABASE()) AS global_name",
    "CREATE OR REPLACE VIEW sys.global_name AS SELECT * FROM global_name",
    // Session NLS state: a real (temporary, per-connection) table so
    // `ALTER SESSION SET NLS_*` can `UPDATE` it — a MariaDB view may not read a
    // user variable, and a view over a temp table is disallowed too.
    "DROP TEMPORARY TABLE IF EXISTS nls_session_parameters",
    "CREATE TEMPORARY TABLE nls_session_parameters (parameter VARCHAR(40) PRIMARY KEY, value VARCHAR(64))",
    "INSERT INTO nls_session_parameters (parameter, value) VALUES \
       ('NLS_DATE_FORMAT','DD-MON-RR'), ('NLS_TIMESTAMP_FORMAT','DD-MON-RR HH24.MI.SSXFF'), \
       ('NLS_TIMESTAMP_TZ_FORMAT','DD-MON-RR HH24.MI.SSXFF TZR'), ('NLS_NUMERIC_CHARACTERS','.,'), \
       ('NLS_LANGUAGE','AMERICAN'), ('NLS_TERRITORY','AMERICA'), ('NLS_SORT','BINARY'), \
       ('NLS_COMP','BINARY'), ('NLS_CALENDAR','GREGORIAN'), ('NLS_DATE_LANGUAGE','AMERICAN')",
    "DROP TEMPORARY TABLE IF EXISTS nls_database_parameters",
    "CREATE TEMPORARY TABLE nls_database_parameters AS SELECT * FROM nls_session_parameters",
    "DROP TEMPORARY TABLE IF EXISTS sys.nls_database_parameters",
    "CREATE TEMPORARY TABLE sys.nls_database_parameters AS SELECT * FROM nls_session_parameters",
    "CREATE OR REPLACE VIEW user_tablespaces AS SELECT 'USERS' AS tablespace_name FROM DUAL",
    "CREATE OR REPLACE VIEW sys.session_roles AS SELECT NULL AS role FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW sys.session_privs AS SELECT NULL AS privilege FROM DUAL WHERE 1=0",
    "CREATE OR REPLACE VIEW user_dependencies AS SELECT NULL AS name, NULL AS type, NULL AS referenced_owner, NULL AS referenced_name FROM DUAL WHERE 1=0",
    // ---- session functions ---------------------------------------------
    "CREATE OR REPLACE FUNCTION sys_context(ns VARCHAR(64), p VARCHAR(64)) RETURN VARCHAR(255) AS BEGIN \
       RETURN CASE UPPER(p) \
         WHEN 'CURRENT_SCHEMA' THEN UPPER(DATABASE()) \
         WHEN 'SESSION_SCHEMA' THEN UPPER(DATABASE()) \
         WHEN 'CURRENT_USER' THEN UPPER(SUBSTRING_INDEX(CURRENT_USER(),'@',1)) \
         WHEN 'SESSION_USER' THEN UPPER(SUBSTRING_INDEX(CURRENT_USER(),'@',1)) \
         WHEN 'DB_NAME' THEN DATABASE() \
         WHEN 'DB_UNIQUE_NAME' THEN DATABASE() \
         WHEN 'SID' THEN CAST(CONNECTION_ID() AS CHAR) \
         WHEN 'SESSIONTIMEZONE' THEN CASE WHEN @@session.time_zone='SYSTEM' THEN '+00:00' ELSE @@session.time_zone END \
         WHEN 'DB_TIMEZONE' THEN '+00:00' \
         ELSE NULL END; END",
    "CREATE OR REPLACE FUNCTION sessiontimezone() RETURN VARCHAR(64) AS BEGIN \
       RETURN CASE WHEN @@session.time_zone='SYSTEM' THEN '+00:00' ELSE @@session.time_zone END; END",
    "CREATE OR REPLACE FUNCTION dbtimezone() RETURN VARCHAR(16) AS BEGIN RETURN '+00:00'; END",
    "CREATE OR REPLACE PROCEDURE dbms_output.put_line(msg TEXT) AS BEGIN RETURN; END",
    "CREATE OR REPLACE PROCEDURE `dbms_output.put_line`(msg TEXT) AS BEGIN RETURN; END",
];

/// Oracle scalar functions with no MariaDB `SQL_MODE=ORACLE` equivalent,
/// installed once per session as real stored functions. These are genuine
/// semantic implementations (any argument works), not corpus-specific shims —
/// the alternative would be per-call SQL rewriting in `translate.rs`.
const MARIADB_COMPAT_FUNCTIONS: &[&str] = &[
    // MONTHS_BETWEEN: whole months from the month/year fields, plus a
    // day-of-month fraction over 31; exact 0 when both days match or both are
    // end-of-month (Oracle's rule).
    "CREATE OR REPLACE FUNCTION months_between(a DATETIME, b DATETIME) \
       RETURN DECIMAL(20,10) AS whole INT; frac DECIMAL(20,10); BEGIN \
       whole := (YEAR(a)-YEAR(b))*12 + (MONTH(a)-MONTH(b)); \
       IF DAY(a) = DAY(b) OR (DAY(a) = DAY(LAST_DAY(a)) AND DAY(b) = DAY(LAST_DAY(b))) THEN frac := 0; \
       ELSE frac := (DAY(a) - DAY(b) + (TIME_TO_SEC(TIME(a)) - TIME_TO_SEC(TIME(b)))/86400) / 31; END IF; \
       RETURN whole + frac; END",
    // NEXT_DAY: the first date strictly after `d` falling on the named weekday.
    "CREATE OR REPLACE FUNCTION next_day(d DATETIME, dow VARCHAR(20)) RETURN DATETIME AS \
       target INT; cur INT; add_days INT; BEGIN \
       target := FIELD(UPPER(LEFT(dow,3)),'SUN','MON','TUE','WED','THU','FRI','SAT'); \
       IF target = 0 THEN RETURN NULL; END IF; \
       cur := DAYOFWEEK(d); add_days := MOD(target - cur + 7, 7); \
       IF add_days = 0 THEN add_days := 7; END IF; \
       RETURN DATE_ADD(d, INTERVAL add_days DAY); END",
    // INITCAP: upper-case the first alphanumeric of each run, lower-case the rest.
    "CREATE OR REPLACE FUNCTION initcap(s TEXT) RETURN TEXT AS \
       out_s TEXT DEFAULT ''; i INT DEFAULT 1; ch VARCHAR(4); prev VARCHAR(4) DEFAULT ' '; BEGIN \
       IF s IS NULL THEN RETURN NULL; END IF; \
       WHILE i <= CHAR_LENGTH(s) LOOP ch := SUBSTRING(s,i,1); \
         IF prev REGEXP '[[:alnum:]]' THEN out_s := CONCAT(out_s, LOWER(ch)); \
         ELSE out_s := CONCAT(out_s, UPPER(ch)); END IF; \
         prev := ch; i := i + 1; END LOOP; RETURN out_s; END",
    // TRANSLATE: map each char in `from_set` to the same position in `to_set`;
    // drop it when `to_set` is shorter. (`translate` is reserved, hence prefix.)
    "CREATE OR REPLACE FUNCTION oracle_translate(s TEXT, from_set TEXT, to_set TEXT) RETURN TEXT AS \
       out_s TEXT DEFAULT ''; i INT DEFAULT 1; ch VARCHAR(4); pos INT; BEGIN \
       IF s IS NULL THEN RETURN NULL; END IF; \
       WHILE i <= CHAR_LENGTH(s) LOOP ch := SUBSTRING(s,i,1); pos := INSTR(from_set, ch); \
         IF pos = 0 THEN out_s := CONCAT(out_s, ch); \
         ELSIF pos <= CHAR_LENGTH(to_set) THEN out_s := CONCAT(out_s, SUBSTRING(to_set,pos,1)); END IF; \
         i := i + 1; END LOOP; RETURN out_s; END",
    // REGEXP_COUNT: MariaDB has REGEXP_INSTR/SUBSTR but no COUNT; walk matches.
    "CREATE OR REPLACE FUNCTION regexp_count(subj TEXT, pat TEXT) RETURN INT AS \
       n INT DEFAULT 0; rest TEXT; hit INT; mlen INT; BEGIN \
       IF subj IS NULL OR pat IS NULL THEN RETURN NULL; END IF; rest := subj; \
       LOOP hit := REGEXP_INSTR(rest, pat); EXIT WHEN hit = 0; n := n + 1; \
         mlen := GREATEST(CHAR_LENGTH(REGEXP_SUBSTR(rest, pat)), 1); \
         rest := SUBSTRING(rest, hit + mlen); EXIT WHEN rest IS NULL OR rest = ''; END LOOP; \
       RETURN n; END",
    // REGEXP_SUBSTR with position / occurrence / capture group — MariaDB's is
    // 2-argument. `REGEXP_INSTR` accepts position+occurrence, so use it to
    // locate the match, then extract; a non-zero group index pulls the first
    // capture via `REGEXP_REPLACE`.
    "CREATE OR REPLACE FUNCTION oracle_regexp_substr(subj TEXT, pat TEXT, p INT, occ INT, grp INT) \
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
    "CREATE OR REPLACE FUNCTION oracle_instr(s TEXT, sub TEXT, pos INT, nth INT) RETURN INT AS \
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
    "CREATE OR REPLACE FUNCTION numtodsinterval(n DOUBLE, unit VARCHAR(16)) RETURN VARCHAR(64) AS \
       secs DOUBLE; sgn VARCHAR(1) DEFAULT ''; d INT; h INT; m INT; s DOUBLE; BEGIN \
       secs := n * CASE UPPER(unit) WHEN 'DAY' THEN 86400 WHEN 'HOUR' THEN 3600 \
         WHEN 'MINUTE' THEN 60 WHEN 'SECOND' THEN 1 ELSE 0 END; \
       IF secs < 0 THEN sgn := '-'; secs := -secs; END IF; \
       d := FLOOR(secs/86400); secs := secs - d*86400; \
       h := FLOOR(secs/3600); secs := secs - h*3600; \
       m := FLOOR(secs/60); s := secs - m*60; \
       RETURN CONCAT(sgn, LPAD(d,2,'0'), ' ', LPAD(h,2,'0'), ':', LPAD(m,2,'0'), ':', LPAD(FORMAT(s,6),9,'0')); END",
    "CREATE OR REPLACE FUNCTION numtoyminterval(n INT, unit VARCHAR(16)) RETURN VARCHAR(32) AS \
       months INT; sgn VARCHAR(1) DEFAULT ''; BEGIN \
       months := n * CASE UPPER(unit) WHEN 'YEAR' THEN 12 WHEN 'MONTH' THEN 1 ELSE 0 END; \
       IF months < 0 THEN sgn := '-'; months := -months; END IF; \
       RETURN CONCAT(sgn, LPAD(FLOOR(months/12),2,'0'), '-', LPAD(MOD(months,12),2,'0')); END",
    // LISTAGG as a true aggregate (also covers the plain GROUP_CONCAT path when
    // translate.rs cannot see a WITHIN GROUP clause to convert).
    "CREATE OR REPLACE AGGREGATE FUNCTION listagg(x TEXT, sep TEXT) RETURN TEXT AS \
       acc TEXT DEFAULT NULL; BEGIN LOOP FETCH GROUP NEXT ROW; \
       IF x IS NOT NULL THEN IF acc IS NULL THEN acc := x; ELSE acc := CONCAT(acc, sep, x); END IF; END IF; \
       END LOOP; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN acc; END",
];

struct MariaDbCursor {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
    offset: usize,
}

#[async_trait::async_trait]
impl OracleCursor for MariaDbCursor {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
    fn is_exhausted(&self) -> bool {
        self.offset >= self.rows.len()
    }
    async fn next_batch(&mut self, n: usize) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
        let end = (self.offset + n).min(self.rows.len());
        let batch = self.rows[self.offset..end].to_vec();
        self.offset = end;
        Ok(batch)
    }
    async fn finish(&mut self) {
        self.offset = self.rows.len();
    }
}

/// True for an error that means the TCP link to MariaDB is gone (server
/// restarted, killed the thread, network blip) rather than a SQL-level failure.
/// Such an error is retried once on a fresh connection.
fn is_connection_lost(e: &Error) -> bool {
    let Error::Postgres(m) = e else { return false };
    // `mariadb_error` renders a `Server` error as `<sqlstate>: <message>` — a
    // SQLSTATE prefix means the server answered, so it is *not* a lost link.
    let sqlstate_prefixed = m
        .split_once(": ")
        .is_some_and(|(s, _)| s.len() == 5 && s.bytes().all(|b| b.is_ascii_alphanumeric()));
    if sqlstate_prefixed {
        return false;
    }
    let m = m.to_ascii_lowercase();
    m.contains("early eof")
        || m.contains("connection reset")
        || m.contains("connection closed")
        || m.contains("connection aborted")
        || m.contains("broken pipe")
        || m.contains("unexpected end of file")
        || m.contains("io error")
        || m.contains("os error")
        || m.contains("timed out")
}

impl MariaDbBackend {
    /// Connect and enable MariaDB's Oracle compatibility mode for this session.
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        let user = user.to_lowercase();
        // Connect without a default database so `establish` can resolve the
        // effective schema (Oracle's schema == user) against `information_schema`.
        let url = format!(
            "mysql://{}:{}@{}:{}",
            // Oracle usernames are case-insensitive and the server passes the
            // authenticated name in uppercase; MariaDB account names are not.
            urlencoding(&user),
            urlencoding(password),
            host,
            port,
        );
        // Prefer a database named after the Oracle login; fall back to the
        // configured one. `USE` is exclusive in MariaDB (no `search_path`), so
        // this is a choice of exactly one, not a lookup chain.
        let schema = {
            let probe_opts = Opts::from_url(&format!("{url}/{}", urlencoding(database)))
                .map_err(|e| Error::Postgres(format!("invalid MariaDB connection URL: {e}")))?;
            let mut probe = Conn::new(probe_opts)
                .await
                .map_err(|e| Error::Postgres(format!("MariaDB connection failed: {e}")))?;
            let found: Option<String> = probe
                .exec_first(
                    "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
                    (&user,),
                )
                .await
                .ok()
                .flatten();
            let _ = probe.disconnect().await;
            found.unwrap_or_else(|| database.to_string())
        };

        let conn = Self::establish(&url, &schema).await?;
        Ok(Self {
            url,
            schema,
            conn: tokio::sync::Mutex::new(conn),
            sequences_with_currval: tokio::sync::Mutex::new(HashSet::new()),
        })
    }

    /// Open a fresh MariaDB connection with Oracle mode, the `information_schema`
    /// facade, the compat functions, and an open transaction. `schema` is the
    /// database unqualified names resolve in; re-applied here so a reconnect
    /// keeps it.
    async fn establish(url: &str, schema: &str) -> Result<Conn> {
        let opts = Opts::from_url(url)
            .map_err(|e| Error::Postgres(format!("invalid MariaDB connection URL: {e}")))?;
        let mut conn = Conn::new(opts)
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB connection failed: {e}")))?;
        // `ORACLE` supplies the dialect; the extra flags bring MariaDB's error
        // behaviour closer to Oracle's (raise on overflow/truncation, reject a
        // non-aggregated column outside GROUP BY).
        conn.query_drop(SESSION_SQL_MODE)
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB Oracle mode failed: {e}")))?;
        // Oracle's schema == user: resolve unqualified names in the login's own
        // database when it exists. Best-effort — a session with no such schema
        // and no default simply qualifies its names.
        if !schema.is_empty() {
            let use_stmt = format!("USE `{}`", schema.replace('`', "``"));
            if let Err(e) = conn.query_drop(&use_stmt).await {
                tracing::warn!("could not `USE {schema}` ({e}); names resolve unqualified");
            }
        }
        conn.query_drop("SET NAMES utf8mb4")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB charset setup failed: {e}")))?;
        // Pin the connection collation to the current schema's default so string
        // literals in client SQL aggregate cleanly with the schema's columns.
        // Without this a `utf8mb4_general_ci` connection default (some drivers)
        // trips ER_CANT_AGGREGATE_2COLLATIONS against `utf8mb4_uca1400_ai_ci`
        // columns (the MariaDB 11.5+ table default).
        conn.query_drop("SET collation_connection = @@collation_database")
            .await
            .ok();
        // `SQL_MODE=ORACLE` supplies syntax and built-ins but not Oracle's data
        // dictionary or a few session functions. Install a portable facade over
        // `information_schema`. Best-effort: a role without the rights to create
        // it still gets ordinary queries.
        conn.query_drop("CREATE SCHEMA IF NOT EXISTS sys")
            .await
            .ok();
        conn.query_drop("CREATE SCHEMA IF NOT EXISTS dbms_output")
            .await
            .ok();
        for ddl in MARIADB_FACADE.iter().chain(MARIADB_COMPAT_FUNCTIONS) {
            if let Err(e) = conn.query_drop(*ddl).await {
                tracing::debug!("mariadb facade statement skipped ({e}): {ddl}");
            }
        }
        conn.query_drop("START TRANSACTION")
            .await
            .map_err(|e| Error::Postgres(format!("MariaDB transaction setup failed: {e}")))?;
        Ok(conn)
    }

    /// Lightweight connectivity probe.
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.lock().await;
        match conn.query_drop("SELECT 1").await {
            Ok(()) => Ok(()),
            Err(e) => {
                let e = Error::Postgres(format!("MariaDB query failed: {e}"));
                if !is_connection_lost(&e) {
                    return Err(e);
                }
                tracing::warn!("MariaDB connection lost ({e}); reconnecting");
                *conn = Self::establish(&self.url, &self.schema).await?;
                conn.query_drop("SELECT 1")
                    .await
                    .map_err(|e| Error::Postgres(format!("MariaDB query failed: {e}")))
            }
        }
    }

    async fn check_and_record_sequence_pseudocolumns(&self, sql: &str) -> Result<()> {
        let mut seen = self.sequences_with_currval.lock().await;
        for word in sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')) {
            let upper = word.to_ascii_uppercase();
            if let Some(sequence) = upper.strip_suffix(".CURRVAL")
                && !seen.contains(sequence)
            {
                return Err(Error::Postgres(format!(
                    "currval of sequence {sequence} is not yet defined in this session"
                )));
            }
            if let Some(sequence) = upper.strip_suffix(".NEXTVAL") {
                seen.insert(sequence.to_string());
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl OracleBackend for Arc<MariaDbBackend> {
    async fn open_cursor(
        &self,
        sql: &str,
        binds: &[BindValue],
        caps: DescribeCaps,
    ) -> Result<Box<dyn OracleCursor>> {
        self.check_and_record_sequence_pseudocolumns(sql).await?;
        // Keep the result metadata separately from the rows.  A query with
        // zero rows still has a useful Oracle describe response; deriving
        // columns from `rows.first()` silently produced an empty response
        // for those queries, which made JDBC see an empty HTTP body rather
        // than `[]`/a valid zero-row result.
        let (result_columns, rows): (std::sync::Arc<[mysql_async::Column]>, Vec<mysql_async::Row>) = {
            let mut conn = self.conn.lock().await;
            match fetch_all(&mut conn, sql, binds).await {
                Err(ref e) if is_connection_lost(e) => {
                    tracing::warn!("MariaDB connection lost ({e}); reconnecting and retrying once");
                    *conn = MariaDbBackend::establish(&self.url, &self.schema).await?;
                    fetch_all(&mut conn, sql, binds).await?
                }
                other => other?,
            }
        };
        // Per-column temporal wire form, decided once so the describe metadata
        // and the row encoding below agree.
        let temporal: Vec<TemporalWire> = result_columns
            .iter()
            .map(|col| temporal_wire(col, &caps))
            .collect();
        let columns = result_columns
            .iter()
            .zip(&temporal)
            .map(|(col, &tw)| {
                let name = col.name_str().into_owned();
                match tw {
                    TemporalWire::Timestamp(scale) => return ColumnMeta::timestamp(name, scale),
                    TemporalWire::Date => return ColumnMeta::date(name),
                    TemporalWire::None => {}
                }
                // charset 63 == binary. Only a genuine binary-string
                // column (BLOB / `BINARY` / `VARBINARY` / `UNHEX(...)`)
                // is RAW for an Oracle client. MariaDB also reports DATE
                // / DATETIME / TIME columns with charset 63, so a
                // `STR_TO_DATE` / `ADD_MONTHS` / `DATE()` result must be
                // excluded or it would go over the wire as `0x…` hex.
                if col.character_set() == 63
                    && is_binary_string_column(col.column_type())
                    && !is_numeric_column(col.column_type())
                {
                    return ColumnMeta::raw(name, 4000);
                }
                match col.column_type() {
                    mysql_async::consts::ColumnType::MYSQL_TYPE_FLOAT
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_DOUBLE
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_DECIMAL
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_LONGLONG
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_LONG
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_SHORT
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_TINY => {
                        ColumnMeta::number(name, 38, 0)
                    }
                    _ => ColumnMeta::varchar(name, 4000),
                }
            })
            .collect::<Vec<_>>();
        let encoded = rows
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|i| {
                        let kind = row.columns_ref()[i].column_type();
                        let tw = temporal.get(i).copied().unwrap_or(TemporalWire::None);
                        row.as_ref(i)
                            .map(|value| {
                                encode_value_for_column(value, is_numeric_column(kind), tw)
                            })
                            .transpose()
                    })
                    .collect()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Box::new(MariaDbCursor {
            columns,
            rows: encoded,
            offset: 0,
        }))
    }

    async fn execute_simple(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        self.check_and_record_sequence_pseudocolumns(sql).await?;
        let command = sql.trim().trim_end_matches(';');
        if command.eq_ignore_ascii_case("COMMIT") {
            conn.query_drop("COMMIT").await.map_err(mariadb_error)?;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        if command.eq_ignore_ascii_case("ROLLBACK") {
            conn.query_drop("ROLLBACK").await.map_err(mariadb_error)?;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        if command.eq_ignore_ascii_case("BEGIN")
            || command.eq_ignore_ascii_case("START TRANSACTION")
        {
            return Ok(0);
        }
        let upper = command.to_ascii_uppercase();
        // `SET TRANSACTION` in MariaDB configures the *next* transaction and is
        // rejected mid-transaction; DbSaci always has one open. End it, apply,
        // start a fresh one.
        if upper.starts_with("SET TRANSACTION ") {
            conn.query_drop("COMMIT").await.map_err(mariadb_error)?;
            conn.query_drop(command).await.map_err(mariadb_error)?;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        if upper.starts_with("SAVEPOINT ")
            || upper.starts_with("RELEASE SAVEPOINT ")
            || upper.starts_with("ROLLBACK TO ")
        {
            conn.query_drop(mariadb_sql(command))
                .await
                .map_err(mariadb_error)?;
            return Ok(0);
        }
        conn.query_drop(SESSION_SQL_MODE)
            .await
            .map_err(mariadb_error)?;
        let (msql, params) = mariadb_prepare(command, binds)?;
        if params.is_empty() && msql.contains(crate::translate::MARIADB_BATCH_SEP.trim()) {
            let mut affected = 0;
            conn.query_drop("SAVEPOINT dbsaci_statement")
                .await
                .map_err(mariadb_error)?;
            for statement in msql.split(crate::translate::MARIADB_BATCH_SEP.trim()) {
                let statement = statement.trim();
                if !statement.is_empty() {
                    affected += run_drained(&mut conn, statement).await?;
                }
            }
            conn.query_drop("RELEASE SAVEPOINT dbsaci_statement")
                .await
                .map_err(mariadb_error)?;
            return Ok(affected);
        }
        // A stored program can execute DDL through `EXECUTE IMMEDIATE`. MariaDB
        // commits around that DDL, which destroys an enclosing savepoint. Run
        // the whole program as its own atomic unit instead; normal statements
        // retain the savepoint-based error recovery below.
        let program = params.is_empty() && is_plsql_definition(command);
        if program {
            let affected = run_drained(&mut conn, command).await?;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(mariadb_error)?;
            return Ok(affected);
        }
        conn.query_drop("SAVEPOINT dbsaci_statement")
            .await
            .map_err(mariadb_error)?;
        // `INSERT/DELETE … RETURNING` yields a result set, and MariaDB then
        // reports `affected_rows` as -1; count the returned rows instead.
        let returning = upper.contains(" RETURNING ")
            && (upper.trim_start().starts_with("INSERT ")
                || upper.trim_start().starts_with("DELETE "));
        let result = if returning {
            match conn.exec_iter(msql, Params::Positional(params)).await {
                Ok(mut iter) => {
                    let rows: mysql_async::Result<Vec<mysql_async::Row>> = iter.collect().await;
                    // Drain any trailing result set before releasing the lock.
                    let _ = iter.drop_result().await;
                    rows.map(|rows| rows.len() as u64)
                }
                Err(e) => Err(e),
            }
        } else {
            match conn.exec_iter(msql, Params::Positional(params)).await {
                Ok(iter) => {
                    let affected = iter.affected_rows();
                    let _ = iter.drop_result().await;
                    Ok(affected)
                }
                Err(e) => Err(e),
            }
        };
        match result {
            Ok(affected) => {
                conn.query_drop("RELEASE SAVEPOINT dbsaci_statement")
                    .await
                    .map_err(mariadb_error)?;
                Ok(affected)
            }
            Err(error) => {
                let _ = conn
                    .query_drop("ROLLBACK TO SAVEPOINT dbsaci_statement")
                    .await;
                let _ = conn.query_drop("RELEASE SAVEPOINT dbsaci_statement").await;
                Err(mariadb_error(error))
            }
        }
    }

    async fn execute_ddl(&self, sql: &str, binds: &[BindValue]) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        conn.query_drop("COMMIT").await.map_err(mariadb_error)?;
        let (msql, params) = mariadb_prepare(sql, binds)?;
        // A `CREATE TRIGGER`/`FUNCTION`/`PROCEDURE` body uses `:NEW`/`:OLD`/
        // `:alias`, which `mysql_async`'s prepared-statement path parses as
        // named placeholders and rewrites to `?`. Send those verbatim.
        let result: Result<u64> =
            if params.is_empty() && msql.contains(crate::translate::MARIADB_BATCH_SEP.trim()) {
                let mut total = Ok(0u64);
                for statement in msql.split(crate::translate::MARIADB_BATCH_SEP.trim()) {
                    let statement = statement.trim();
                    if !statement.is_empty() {
                        total = match total {
                            Ok(n) => run_drained(&mut conn, statement).await.map(|d| n + d),
                            Err(e) => Err(e),
                        };
                    }
                }
                total
            } else if params.is_empty() && is_plsql_definition(&msql) {
                run_drained(&mut conn, &msql).await
            } else {
                match conn.exec_iter(msql, Params::Positional(params)).await {
                    Ok(iter) => {
                        let affected = iter.affected_rows();
                        let _ = iter.drop_result().await;
                        Ok(affected)
                    }
                    Err(e) => Err(mariadb_error(e)),
                }
            };
        // DDL commits in MariaDB. A failed DDL must still restart DbSaci's
        // persistent session transaction, otherwise later DML has no
        // savepoint to roll back to.
        let _ = conn.query_drop("COMMIT").await;
        conn.query_drop("START TRANSACTION")
            .await
            .map_err(mariadb_error)?;
        result
    }

    async fn execute_returning(
        &self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<(u64, Vec<Vec<Option<Vec<u8>>>>)> {
        let mut conn = self.conn.lock().await;
        conn.query_drop(SESSION_SQL_MODE)
            .await
            .map_err(mariadb_error)?;
        let (msql, params) = mariadb_prepare(sql, binds)?;
        // MariaDB `INSERT … RETURNING <cols>` yields the projected rows directly.
        let rows: Vec<mysql_async::Row> = if params.is_empty() {
            conn.query(&msql).await.map_err(mariadb_error)?
        } else {
            conn.exec(&msql, Params::Positional(params))
                .await
                .map_err(mariadb_error)?
        };
        let ncols = rows.first().map(mysql_async::Row::len).unwrap_or(0);
        let mut per_col: Vec<Vec<Option<Vec<u8>>>> = vec![Vec::with_capacity(rows.len()); ncols];
        for row in &rows {
            for (i, col) in per_col.iter_mut().enumerate() {
                let kind = row.columns_ref()[i].column_type();
                // `RETURNING` has no describe round trip to negotiate caps on;
                // a returned temporal column takes the plain Oracle DATE form.
                let tw = if is_temporal_column(kind) {
                    TemporalWire::Date
                } else {
                    TemporalWire::None
                };
                let encoded = row
                    .as_ref(i)
                    .map(|value| encode_value_for_column(value, is_numeric_column(kind), tw))
                    .transpose()?;
                col.push(encoded);
            }
        }
        Ok((rows.len() as u64, per_col))
    }

    async fn cancel(&self) {}
}

fn mariadb_error(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(server) => {
            Error::Postgres(format!("{}: {}", server.state, server.message))
        }
        other => Error::Postgres(format!("MariaDB error: {other}")),
    }
}

/// Run a statement that is not expected to hand back rows (DDL, a stored-program
/// definition, an anonymous PL/SQL block) and **fully drain** every result set
/// it does produce.
///
/// `mysql_async`'s `QueryResult` has no `Drop` cleanup: dropping one while the
/// connection still holds a pending result set leaves stray bytes in the socket
/// and the *next* command desyncs — surfacing later as `I/O error: early eof`.
/// An anonymous `BEGIN … END` block or `CREATE PROCEDURE` whose body ends in a
/// bare `SELECT` is exactly such a statement, so every non-row execution path
/// goes through here.
async fn run_drained(conn: &mut Conn, sql: &str) -> Result<u64> {
    let result = conn.query_iter(sql).await.map_err(mariadb_error)?;
    let affected = result.affected_rows();
    result.drop_result().await.map_err(mariadb_error)?;
    Ok(affected)
}

/// Run a row-returning statement and hand back its columns and fully-buffered
/// rows, draining every trailing result set.
///
/// A free function (not a closure) so the `&mut Conn` future stays `Send` under
/// `#[async_trait]`; `open_cursor` calls it, and calls it a second time on a
/// fresh connection if the first attempt died with a lost link.
async fn fetch_all(
    conn: &mut Conn,
    sql: &str,
    binds: &[BindValue],
) -> Result<(std::sync::Arc<[mysql_async::Column]>, Vec<mysql_async::Row>)> {
    conn.query_drop(SESSION_SQL_MODE)
        .await
        .map_err(mariadb_error)?;
    let (msql, params) = mariadb_prepare(sql, binds)?;
    // `query_iter` and `exec_iter` yield different protocol-typed `QueryResult`s,
    // so each branch drains its own.
    if params.is_empty() {
        let mut result = conn.query_iter(&msql).await.map_err(mariadb_error)?;
        let columns = result.columns().unwrap_or_default();
        let rows: Vec<mysql_async::Row> = result.collect().await.map_err(mariadb_error)?;
        // A statement can return more than one result set (`CALL`); drain the
        // rest so the next command does not read stale bytes.
        result.drop_result().await.map_err(mariadb_error)?;
        Ok((columns, rows))
    } else {
        let mut result = conn
            .exec_iter(&msql, Params::Positional(params))
            .await
            .map_err(mariadb_error)?;
        let columns = result.columns().unwrap_or_default();
        let rows: Vec<mysql_async::Row> = result.collect().await.map_err(mariadb_error)?;
        result.drop_result().await.map_err(mariadb_error)?;
        Ok((columns, rows))
    }
}

fn bind_value(bind: &BindValue) -> Result<Value> {
    match bind {
        BindValue::Null => Ok(Value::NULL),
        // Oracle sends a TIMESTAMP/DATE bind as an Oracle literal like
        // `TIMESTAMP '2024-02-29 13:14:15'`; MariaDB wants the plain datetime.
        BindValue::Temporal(s) => {
            let t = s.trim();
            let inner = t
                .strip_prefix("TIMESTAMPTZ ")
                .or_else(|| t.strip_prefix("TIMESTAMP "))
                .or_else(|| t.strip_prefix("DATE "))
                .or_else(|| t.strip_prefix("TIMESTAMP'"))
                .map(str::trim)
                .unwrap_or(t)
                .trim_matches('\'')
                .trim();
            // MariaDB has no zone-aware datetime; drop a trailing numeric
            // offset (`... +05:00`, `... -0300`) so the naive wall-clock value
            // still binds.
            let inner = match inner.rsplit_once(' ') {
                Some((head, tail))
                    if tail.starts_with(['+', '-'])
                        && tail[1..].bytes().all(|b| b.is_ascii_digit() || b == b':') =>
                {
                    head.trim_end()
                }
                _ => inner,
            };
            Ok(Value::Bytes(inner.as_bytes().to_vec()))
        }
        BindValue::String(s) | BindValue::Number(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
        BindValue::Bytes(b) => Ok(Value::Bytes(b.clone())),
        BindValue::Boolean(b) => Ok(Value::Int(i64::from(*b))),
        BindValue::BinaryDouble(v) if v.is_finite() => Ok(Value::Double(*v)),
        BindValue::BinaryDouble(_) => Err(Error::DataConversionError(
            "non-finite floating bind is unsupported".into(),
        )),
    }
}

/// Convert DbSaci's PostgreSQL-shaped SQL (`$1`, `$1::text`, …) to MariaDB's
/// `?` placeholders **and** build the positional parameter list. A `$N` that
/// repeats gets its bind value supplied once per occurrence (MariaDB `?` is
/// strictly positional and cannot reference a parameter twice).
/// A stored-program definition or anonymous PL/SQL block, whose body legitimately
/// contains `:NEW` / `:OLD` / `:alias` tokens that must not be treated as bind
/// placeholders by the driver's prepared-statement path.
fn is_plsql_definition(sql: &str) -> bool {
    let head = sql.trim_start();
    let up = head
        .get(..head.len().min(64))
        .unwrap_or(head)
        .to_ascii_uppercase();
    up.starts_with("BEGIN")
        || up.starts_with("DECLARE")
        || (up.starts_with("CREATE")
            && ["TRIGGER", "FUNCTION", "PROCEDURE", "PACKAGE"]
                .iter()
                .any(|k| up.contains(k)))
}

fn mariadb_prepare(sql: &str, binds: &[BindValue]) -> Result<(String, Vec<Value>)> {
    let bytes = sql.as_bytes();
    // Byte buffer, not a `String`: the loop only special-cases ASCII bytes, and
    // every UTF-8 lead/continuation byte is >= 0x80, so copying non-special
    // bytes verbatim preserves multibyte characters. (`push(b as char)` would
    // Latin-1-expand them and double-encode every accented literal.)
    let mut out: Vec<u8> = Vec::with_capacity(sql.len());
    let mut params: Vec<Value> = Vec::new();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            out.push(b);
            if b == q {
                if bytes.get(i + 1) == Some(&q) {
                    out.push(q);
                    i += 1;
                } else {
                    quote = None;
                }
            }
            i += 1;
        } else if matches!(b, b'\'' | b'"' | b'`') {
            quote = Some(b);
            out.push(b);
            i += 1;
        } else if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let mut j = i + 1;
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            let n: usize = sql[i + 1..j].parse().unwrap_or(0);
            let bind = binds.get(n.wrapping_sub(1)).ok_or_else(|| {
                Error::DataConversionError(format!("bind ${n} referenced but not supplied"))
            })?;
            params.push(bind_value(bind)?);
            // The shared bind rewriter annotates each PG parameter with a cast
            // chain (`$1::text::numeric`). MariaDB has no `::` cast operator and
            // its client parser reads `:numeric` as a *named* placeholder, so
            // the chain must be consumed here, not left in the text.
            let (repl, next) = consume_pg_cast_chain(sql, j);
            out.extend_from_slice(repl.as_bytes());
            i = next;
        } else {
            out.push(b);
            i += 1;
        }
    }
    let out = String::from_utf8(out).map_err(|e| {
        Error::DataConversionError(format!("non-UTF-8 SQL after bind rewrite: {e}"))
    })?;
    Ok((out, params))
}

/// Starting just past a `?`-placeholder in `sql` at byte `at`, consume a
/// PostgreSQL cast chain (`::text`, `::text::numeric`, `::double precision`, …)
/// and return the MariaDB replacement for the placeholder plus the byte offset
/// to resume from. A leading `::text` with no numeric target keeps an explicit
/// `CAST(? AS CHAR(4000))` so an untyped parameter inside `COALESCE`/`IF`/`CASE`
/// still fixes a column type; every other chain collapses to a bare `?`, which
/// MariaDB infers from the bound value.
fn consume_pg_cast_chain(sql: &str, at: usize) -> (&'static str, usize) {
    const TYPES: &[&str] = &[
        "text",
        "numeric",
        "bytea",
        "timestamptz",
        "timestamp",
        "boolean",
        "double precision",
        "integer",
        "bigint",
        "decimal",
    ];
    let mut cur = at;
    let mut casts: Vec<&str> = Vec::new();
    loop {
        let rest = &sql[cur..];
        let Some(after) = rest.strip_prefix("::") else {
            break;
        };
        let after = after.trim_start();
        let Some(ty) = TYPES.iter().find(|t| {
            after
                .get(..t.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(t))
        }) else {
            break;
        };
        casts.push(ty);
        cur += (rest.len() - after.len()) + ty.len();
    }
    if casts.is_empty() {
        return ("?", cur);
    }
    let numeric = casts.iter().any(|t| {
        matches!(
            *t,
            "numeric" | "double precision" | "integer" | "bigint" | "decimal"
        )
    });
    if casts.iter().any(|t| t.eq_ignore_ascii_case("bytea")) {
        // Keep the parameter binary so the result column is surfaced as RAW,
        // not decoded as (invalid) UTF-8 text.
        ("CAST(? AS BINARY)", cur)
    } else if casts[0].eq_ignore_ascii_case("text") && !numeric {
        ("CAST(? AS CHAR(4000))", cur)
    } else {
        ("?", cur)
    }
}

/// DbSaci's bind rewriter emits PostgreSQL-style `$1`, `$2`, … placeholders;
/// MariaDB prepared statements use `?`. Preserve quoted SQL while converting
/// only actual parameter tokens.
fn mariadb_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            out.push(b as char);
            if b == q {
                if bytes.get(i + 1) == Some(&q) {
                    out.push(q as char);
                    i += 1;
                } else {
                    quote = None;
                }
            }
            i += 1;
        } else if matches!(b, b'\'' | b'"' | b'`') {
            quote = Some(b);
            out.push(b as char);
            i += 1;
        } else if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            out.push('?');
            i += 2;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    // The shared bind rewriter annotates PostgreSQL parameters with casts.
    // MariaDB infers these from the bound value and treats `::name` as a
    // named-parameter marker, so remove only casts attached to `?`.
    for ty in [
        "text",
        "numeric",
        "bytea",
        "timestamp",
        "timestamptz",
        "boolean",
        "double precision",
    ] {
        out = out.replace(&format!("?::{ty}"), "?");
    }
    out
}

fn encode_value(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::NULL => Ok(Vec::new()),
        Value::Bytes(bytes) => Ok(bytes.clone()),
        Value::Int(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::UInt(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::Float(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::Double(v) => encode_oracle_number_decimal(&v.to_string()),
        Value::Date(y, m, d, h, min, s, _) => Ok(vec![
            (*y / 100 + 100) as u8,
            (*y % 100 + 100) as u8,
            *m,
            *d,
            h.saturating_add(1),
            min.saturating_add(1),
            s.saturating_add(1),
        ]),
        Value::Time(_, _, h, min, s, _) => Ok(vec![
            100,
            100,
            1,
            1,
            h.saturating_add(1),
            min.saturating_add(1),
            s.saturating_add(1),
        ]),
    }
}

/// How a MariaDB temporal column is delivered to the Oracle client.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TemporalWire {
    /// Not a temporal column.
    None,
    /// Oracle `DATE` (internal type 12) — second precision, 7 bytes.
    Date,
    /// Oracle `TIMESTAMP` (internal type 180) — 7-byte date + 4-byte
    /// nanoseconds; the value is `scale` fractional-second digits.
    Timestamp(i8),
}

/// Decide the Oracle wire form for a MariaDB result column, mirroring the
/// PostgreSQL backend's `timestamp` describe logic: a `DATETIME(n)` /
/// `TIMESTAMP(n)` with a fractional-second precision becomes native Oracle
/// `TIMESTAMP` for clients that negotiated it (`oracle-rs`, python-oracledb,
/// OCI); `DATE` columns, sub-second-less temporals, and the "describe as DATE"
/// thin clients (ojdbc thin / ODP.NET) all take the 7-byte Oracle `DATE` form.
fn temporal_wire(col: &mysql_async::Column, caps: &DescribeCaps) -> TemporalWire {
    use mysql_async::consts::ColumnType::*;
    let kind = col.column_type();
    if !is_temporal_column(kind) {
        return TemporalWire::None;
    }
    // A pure TIME column has no Oracle scalar equivalent; render it as text.
    if matches!(kind, MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2) {
        return TemporalWire::None;
    }
    let decimals = col.decimals();
    if decimals > 0 && caps.native_timestamps && !caps.datetime_as_date {
        TemporalWire::Timestamp(decimals.min(9) as i8)
    } else {
        TemporalWire::Date
    }
}

fn encode_value_for_column(value: &Value, number: bool, temporal: TemporalWire) -> Result<Vec<u8>> {
    if number {
        return match value {
            Value::Bytes(bytes) => encode_oracle_number_decimal(
                std::str::from_utf8(bytes)
                    .map_err(|e| Error::DataConversionError(e.to_string()))?,
            ),
            _ => encode_value(value),
        };
    }
    if temporal != TemporalWire::None
        && let Value::Bytes(bytes) = value
        && let Ok(text) = std::str::from_utf8(bytes)
        && let Some(dt) = parse_mariadb_datetime(text)
    {
        // MariaDB returns temporal expressions as text bytes; the Oracle
        // describe metadata above requires the driver's binary date/timestamp
        // form.
        return Ok(match temporal {
            TemporalWire::Timestamp(_) => crate::backend::encode_oracle_timestamp(dt, None),
            _ => crate::backend::encode_oracle_date(dt),
        });
    }
    encode_value(value)
}

/// Parse a MariaDB temporal text rendering (`2024-02-29 13:14:15.123456`,
/// `2024-02-29`) into a `NaiveDateTime`, keeping the fractional seconds.
fn parse_mariadb_datetime(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim();
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).expect("valid midnight"))
        })
        .ok()
}

fn is_numeric_column(kind: mysql_async::consts::ColumnType) -> bool {
    matches!(
        kind,
        mysql_async::consts::ColumnType::MYSQL_TYPE_FLOAT
            | mysql_async::consts::ColumnType::MYSQL_TYPE_DOUBLE
            | mysql_async::consts::ColumnType::MYSQL_TYPE_DECIMAL
            | mysql_async::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONGLONG
            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONG
            | mysql_async::consts::ColumnType::MYSQL_TYPE_SHORT
            | mysql_async::consts::ColumnType::MYSQL_TYPE_TINY
            | mysql_async::consts::ColumnType::MYSQL_TYPE_INT24
            | mysql_async::consts::ColumnType::MYSQL_TYPE_YEAR
    )
}

/// A DATE / DATETIME / TIMESTAMP / TIME column. MariaDB reports these with the
/// binary character set (63), so they must be excluded from the RAW branch
/// below — an `ADD_MONTHS` / `STR_TO_DATE` / `DATE()` result would otherwise be
/// sent to the client as `0x…` hex instead of a date string.
fn is_temporal_column(kind: mysql_async::consts::ColumnType) -> bool {
    use mysql_async::consts::ColumnType::*;
    matches!(
        kind,
        MYSQL_TYPE_DATE
            | MYSQL_TYPE_NEWDATE
            | MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2
            | MYSQL_TYPE_TIME
            | MYSQL_TYPE_TIME2
    )
}

/// A genuine binary-string column (`BLOB` family, or `BINARY`/`VARBINARY` which
/// MariaDB reports as `STRING`/`VAR_STRING`). Only these, when also carrying the
/// binary character set, are surfaced to Oracle clients as RAW.
fn is_binary_string_column(kind: mysql_async::consts::ColumnType) -> bool {
    use mysql_async::consts::ColumnType::*;
    matches!(
        kind,
        MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_GEOMETRY
    )
}

/// Percent-encode the small set of URL-significant bytes accepted in
/// configured credentials/database names without adding another URL parser.
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::mariadb_sql;

    #[test]
    fn converts_backend_parameters_but_not_literals_or_identifiers() {
        assert_eq!(
            mariadb_sql("SELECT $1, '$2', \"$3\", `$4` WHERE x = $12"),
            "SELECT ?, '$2', \"$3\", `$4` WHERE x = ?"
        );
    }
}
