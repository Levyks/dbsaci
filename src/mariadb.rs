//! MariaDB backend primitives.
//!
//! MariaDB's `SQL_MODE=ORACLE` performs the Oracle-language work in the
//! database. The adapter keeps one backend connection per Oracle session so
//! transactions, temporary objects, and session settings retain their state.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::{Conn, Opts, Params, Value, prelude::Queryable};

use crate::backend::{DescribeCaps, OracleBackend, OracleCursor};
use crate::error::{Error, Result};
use crate::wire::{BindValue, ColumnMeta, encode_oracle_number_decimal};

/// An `INSTEAD OF` trigger body MariaDB cannot store natively; applied when the
/// session issues DML against the target view.
#[derive(Clone, Debug)]
struct InsteadOfTrigger {
    event: String,
    /// Trigger body with `:NEW.col` / `:OLD.col` still present for expansion.
    body: String,
}

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
    /// View-name (lower) → INSTEAD OF trigger registered for this session.
    instead_of: tokio::sync::Mutex<HashMap<String, InsteadOfTrigger>>,
    /// Per-statement wall-clock cap (`max_statement_time`); maps to ORA-01013.
    statement_timeout: Option<Duration>,
    ssl: bool,
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
    // Include PUBLIC: fixture tables land in the shared `public` schema (Oracle/
    // PostgreSQL visibility), while session DDL stays in DATABASE().
    "CREATE OR REPLACE VIEW user_tables AS SELECT table_name, status, num_rows, temporary \
       FROM all_tables WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_tab_columns AS SELECT UPPER(c.table_schema) AS owner, \
       UPPER(c.table_name) AS table_name, UPPER(c.column_name) AS column_name, \
       UPPER(c.data_type) AS data_type, c.character_maximum_length AS data_length, \
       c.numeric_precision AS data_precision, c.numeric_scale AS data_scale, \
       CASE WHEN c.is_nullable='YES' THEN 'Y' ELSE 'N' END AS nullable, \
       c.ordinal_position AS column_id, c.column_default AS data_default \
     FROM information_schema.columns c",
    "CREATE OR REPLACE VIEW user_tab_columns AS SELECT table_name, column_name, data_type, \
       data_length, data_precision, data_scale, nullable, column_id, data_default \
       FROM all_tab_columns WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_tab_cols AS SELECT c.*, 'NO' AS hidden_column, \
       'NO' AS virtual_column, 'YES' AS user_generated FROM all_tab_columns c",
    "CREATE OR REPLACE VIEW user_tab_cols AS SELECT * FROM all_tab_cols \
       WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
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
       FROM all_objects WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_constraints AS SELECT UPPER(tc.constraint_schema) AS owner, \
       UPPER(tc.constraint_name) AS constraint_name, \
       CASE tc.constraint_type WHEN 'PRIMARY KEY' THEN 'P' WHEN 'UNIQUE' THEN 'U' \
         WHEN 'FOREIGN KEY' THEN 'R' WHEN 'CHECK' THEN 'C' ELSE tc.constraint_type END AS constraint_type, \
       UPPER(tc.table_name) AS table_name, NULL AS search_condition, 'VALID' AS status \
     FROM information_schema.table_constraints tc",
    "CREATE OR REPLACE VIEW user_constraints AS SELECT constraint_name, constraint_type, \
       table_name, search_condition, status FROM all_constraints \
       WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
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
       FROM all_indexes WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_ind_columns AS SELECT UPPER(index_schema) AS index_owner, \
       UPPER(index_name) AS index_name, UPPER(table_name) AS table_name, UPPER(column_name) AS column_name, \
       seq_in_index AS column_position FROM information_schema.statistics",
    "CREATE OR REPLACE VIEW user_ind_columns AS SELECT index_name, table_name, column_name, column_position \
       FROM all_ind_columns WHERE index_owner = UPPER(DATABASE()) OR index_owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_sequences AS SELECT UPPER(table_schema) AS sequence_owner, \
       UPPER(table_name) AS sequence_name FROM information_schema.tables WHERE table_type='SEQUENCE'",
    "CREATE OR REPLACE VIEW user_sequences AS SELECT sequence_name FROM all_sequences \
       WHERE sequence_owner = UPPER(DATABASE()) OR sequence_owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_tab_comments AS SELECT UPPER(table_schema) AS owner, \
       UPPER(table_name) AS table_name, \
       CASE table_type WHEN 'VIEW' THEN 'VIEW' ELSE 'TABLE' END AS table_type, \
       NULLIF(table_comment,'') AS comments FROM information_schema.tables",
    "CREATE OR REPLACE VIEW user_tab_comments AS SELECT table_name, table_type, comments \
       FROM all_tab_comments WHERE owner = UPPER(DATABASE()) OR owner = 'PUBLIC'",
    "CREATE OR REPLACE VIEW all_col_comments AS SELECT UPPER(table_schema) AS owner, \
       UPPER(table_name) AS table_name, UPPER(column_name) AS column_name, \
       NULLIF(column_comment,'') AS comments FROM information_schema.columns",
    "CREATE OR REPLACE VIEW user_col_comments AS SELECT table_name, column_name, comments \
       FROM all_col_comments WHERE owner = UPPER(DATABASE())",
    // Multi-event Oracle triggers are split into `name` + `name__dbsaci_update`;
    // reassemble them so ALL_TRIGGERS reports `INSERT OR UPDATE` under the
    // original name, with Oracle's BEFORE/AFTER EACH ROW flags.
    "CREATE OR REPLACE VIEW all_triggers AS SELECT owner, trigger_name, \
       MAX(trigger_type) AS trigger_type, \
       GROUP_CONCAT(DISTINCT event_manipulation ORDER BY \
         FIELD(event_manipulation,'INSERT','UPDATE','DELETE') SEPARATOR ' OR ') AS triggering_event, \
       MAX(table_owner) AS table_owner, MAX(table_name) AS table_name, \
       MAX(status) AS status, MAX(trigger_body) AS trigger_body, \
       MAX(action_type) AS action_type, MAX(before_row) AS before_row \
     FROM ( \
       SELECT UPPER(trigger_schema) AS owner, \
         UPPER(IF(RIGHT(trigger_name, 15) = '__dbsaci_update', \
                  LEFT(trigger_name, CHAR_LENGTH(trigger_name) - 15), trigger_name)) AS trigger_name, \
         CONCAT(action_timing, ' EACH ROW') AS trigger_type, event_manipulation, \
         UPPER(event_object_schema) AS table_owner, UPPER(event_object_table) AS table_name, \
         'ENABLED' AS status, action_statement AS trigger_body, 'PL/SQL' AS action_type, \
         IF(action_timing = 'BEFORE', 'YES', 'NO') AS before_row \
       FROM information_schema.triggers \
     ) t GROUP BY owner, trigger_name",
    "CREATE OR REPLACE VIEW user_triggers AS SELECT trigger_name, trigger_type, triggering_event, \
       table_owner, table_name, status, trigger_body, action_type, before_row FROM all_triggers \
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
         WHEN 'CURRENT_SCHEMA' THEN COALESCE(@dbsaci_current_schema, UPPER(DATABASE())) \
         WHEN 'SESSION_SCHEMA' THEN COALESCE(@dbsaci_current_schema, UPPER(DATABASE())) \
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
    // Minimal DBMS_METADATA.GET_DDL: reconstruct CREATE TABLE text so IDEs and
    // the corpus probe see `schema.table` in the returned DDL.
    "CREATE OR REPLACE FUNCTION dbms_metadata.get_ddl(object_type VARCHAR(64), name VARCHAR(128), \
       schema VARCHAR(128)) RETURN TEXT AS \
       nsp VARCHAR(128); rel VARCHAR(128); cols TEXT; BEGIN \
       nsp := LOWER(COALESCE(NULLIF(schema, ''), DATABASE())); \
       rel := LOWER(name); \
       IF UPPER(object_type) <> 'TABLE' THEN \
         RETURN CONCAT('-- DBMS_METADATA.GET_DDL is not implemented for object type ', object_type); \
       END IF; \
       SELECT GROUP_CONCAT(CONCAT(COLUMN_NAME, ' ', COLUMN_TYPE) \
                           ORDER BY ORDINAL_POSITION SEPARATOR ', ') INTO cols \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = nsp AND TABLE_NAME = rel; \
       IF cols IS NULL THEN RETURN CONCAT('-- table ', nsp, '.', rel, ' not found'); END IF; \
       RETURN CONCAT('CREATE TABLE ', nsp, '.', rel, ' (', cols, ')'); END",
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
       secs DOUBLE; sgn VARCHAR(1) DEFAULT '+'; d INT; h INT; m INT; s DOUBLE; BEGIN \
       secs := n * CASE UPPER(unit) WHEN 'DAY' THEN 86400 WHEN 'HOUR' THEN 3600 \
         WHEN 'MINUTE' THEN 60 WHEN 'SECOND' THEN 1 ELSE 0 END; \
       IF secs < 0 THEN sgn := '-'; secs := -secs; END IF; \
       d := FLOOR(secs/86400); secs := secs - d*86400; \
       h := FLOOR(secs/3600); secs := secs - h*3600; \
       m := FLOOR(secs/60); s := secs - m*60; \
       RETURN CONCAT(sgn, LPAD(d,2,'0'), ' ', LPAD(h,2,'0'), ':', LPAD(m,2,'0'), ':', LPAD(FORMAT(s,6),9,'0')); END",
    "CREATE OR REPLACE FUNCTION numtoyminterval(n INT, unit VARCHAR(16)) RETURN VARCHAR(32) AS \
       months INT; sgn VARCHAR(1) DEFAULT '+'; BEGIN \
       months := n * CASE UPPER(unit) WHEN 'YEAR' THEN 12 WHEN 'MONTH' THEN 1 ELSE 0 END; \
       IF months < 0 THEN sgn := '-'; months := -months; END IF; \
       RETURN CONCAT(sgn, LPAD(FLOOR(months/12),2,'0'), '-', LPAD(MOD(months,12),2,'0')); END",
    // LISTAGG as a true aggregate (also covers the plain GROUP_CONCAT path when
    // translate.rs cannot see a WITHIN GROUP clause to convert).
    "CREATE OR REPLACE AGGREGATE FUNCTION listagg(x TEXT, sep TEXT) RETURN TEXT AS \
       acc TEXT DEFAULT NULL; BEGIN LOOP FETCH GROUP NEXT ROW; \
       IF x IS NOT NULL THEN IF acc IS NULL THEN acc := x; ELSE acc := CONCAT(acc, sep, x); END IF; END IF; \
       END LOOP; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN acc; END",
    // Oracle raises on SELECT-time faults that MariaDB softens to NULL/0. These
    // helpers SIGNAL the SQLSTATEs `oracle_error_for_pos` already maps.
    "CREATE OR REPLACE FUNCTION dbsaci_div(a DECIMAL(65,30), b DECIMAL(65,30)) RETURN DECIMAL(65,30) AS \
       BEGIN IF b IS NULL THEN RETURN NULL; END IF; \
       IF b = 0 THEN SIGNAL SQLSTATE '22012' SET MESSAGE_TEXT = 'divisor is equal to zero'; END IF; \
       RETURN a / b; END",
    "CREATE OR REPLACE FUNCTION dbsaci_to_number(s TEXT) RETURN DECIMAL(65,30) AS \
       BEGIN IF s IS NULL THEN RETURN NULL; END IF; \
       IF TRIM(s) = '' OR s NOT REGEXP '^[+-]?[0-9]+(\\.[0-9]*)?([eE][+-]?[0-9]+)?$' THEN \
         SIGNAL SQLSTATE '22018' SET MESSAGE_TEXT = 'invalid number'; END IF; \
       RETURN CAST(s AS DECIMAL(65,30)); END",
    "CREATE OR REPLACE FUNCTION dbsaci_to_date(s TEXT) RETURN DATETIME AS \
       d DATETIME; BEGIN IF s IS NULL THEN RETURN NULL; END IF; \
       d := STR_TO_DATE(s, '%Y-%m-%d %H:%i:%s'); \
       IF d IS NULL THEN d := STR_TO_DATE(s, '%Y-%m-%d'); END IF; \
       IF d IS NULL THEN d := STR_TO_DATE(s, '%d-%b-%Y'); END IF; \
       IF d IS NULL THEN \
         SIGNAL SQLSTATE '22007' SET MESSAGE_TEXT = 'incorrect datetime value'; END IF; \
       RETURN d; END",
    "CREATE OR REPLACE FUNCTION dbsaci_num_add(a TEXT, b DECIMAL(65,30)) RETURN DECIMAL(65,30) AS \
       BEGIN RETURN dbsaci_to_number(a) + b; END",
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
        statement_timeout: Option<Duration>,
        ssl: bool,
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
            let probe_opts =
                mariadb_connection_opts(&format!("{url}/{}", urlencoding(database)), ssl)?;
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

        let conn = Self::establish(&url, &schema, statement_timeout, ssl).await?;
        Ok(Self {
            url,
            schema,
            conn: tokio::sync::Mutex::new(conn),
            sequences_with_currval: tokio::sync::Mutex::new(HashSet::new()),
            instead_of: tokio::sync::Mutex::new(HashMap::new()),
            statement_timeout,
            ssl,
        })
    }

    /// Open a fresh MariaDB connection with Oracle mode, the `information_schema`
    /// facade, the compat functions, and an open transaction. `schema` is the
    /// database unqualified names resolve in; re-applied here so a reconnect
    /// keeps it.
    async fn establish(
        url: &str,
        schema: &str,
        statement_timeout: Option<Duration>,
        ssl: bool,
    ) -> Result<Conn> {
        let opts = mariadb_connection_opts(url, ssl)?;
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
        if let Some(timeout) = statement_timeout {
            // MariaDB's cap is fractional seconds; ceil so a 2s budget cannot
            // under-fire on a 3s SLEEP.
            let secs = (timeout.as_secs_f64()).max(0.001);
            let _ = conn
                .query_drop(format!("SET max_statement_time = {secs}"))
                .await;
        }
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
        conn.query_drop("CREATE SCHEMA IF NOT EXISTS dbms_metadata")
            .await
            .ok();
        // Shared / catalog schemas Oracle clients reach by name. `public` holds
        // fixture tables; `pg_catalog` exists so `ALTER SESSION SET
        // CURRENT_SCHEMA = pg_catalog` can `USE` it (USERENV then reports it).
        conn.query_drop("CREATE SCHEMA IF NOT EXISTS `public`")
            .await
            .ok();
        conn.query_drop("CREATE SCHEMA IF NOT EXISTS `pg_catalog`")
            .await
            .ok();
        for ddl in MARIADB_FACADE.iter().chain(MARIADB_COMPAT_FUNCTIONS) {
            if let Err(e) = conn.query_drop(*ddl).await {
                tracing::debug!("mariadb facade statement skipped ({e}): {ddl}");
            }
        }
        conn.query_drop("SET max_recursive_iterations = 1100000")
            .await
            .ok();
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
                *conn = Self::establish(&self.url, &self.schema, self.statement_timeout, self.ssl)
                    .await?;
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
        let (result_columns, rows, ieee): (
            std::sync::Arc<[mysql_async::Column]>,
            Vec<mysql_async::Row>,
            Vec<FloatWire>,
        ) = {
            let mut conn = self.conn.lock().await;
            let (cols, rows) = match fetch_all(&mut conn, sql, binds).await {
                Err(ref e) if is_connection_lost(e) => {
                    tracing::warn!("MariaDB connection lost ({e}); reconnecting and retrying once");
                    *conn = MariaDbBackend::establish(
                        &self.url,
                        &self.schema,
                        self.statement_timeout,
                        self.ssl,
                    )
                    .await?;
                    fetch_all(&mut conn, sql, binds).await?
                }
                other => other?,
            };
            let ieee = lookup_ieee_float_wire(&mut conn, &cols, caps.native_binary_floats).await;
            (cols, rows, ieee)
        };
        // Per-column temporal wire form, decided once so the describe metadata
        // and the row encoding below agree. INTERVAL YEAR/DAY result columns
        // stay VARCHAR on MariaDB (no type-182/183 promotion).
        let temporal: Vec<TemporalWire> = result_columns
            .iter()
            .map(|col| temporal_wire(col, &caps))
            .collect();
        let columns = result_columns
            .iter()
            .zip(&temporal)
            .zip(&ieee)
            .map(|((col, &tw), &fw)| {
                let name = col.name_str().into_owned();
                match tw {
                    TemporalWire::Timestamp(scale) => return ColumnMeta::timestamp(name, scale),
                    TemporalWire::Date => return ColumnMeta::date(name),
                    TemporalWire::None => {}
                }
                match fw {
                    FloatWire::BinaryDouble => {
                        return ColumnMeta {
                            name,
                            oracle_type: 101,
                            flags: 0,
                            precision: 0,
                            scale: 0,
                            buffer_size: 8,
                            max_size: 8,
                            charset_id: 0,
                            charset_form: 0,
                            nullable: true,
                            schema: None,
                            type_name: None,
                            position: 1,
                        };
                    }
                    FloatWire::BinaryFloat => {
                        return ColumnMeta {
                            name,
                            oracle_type: 100,
                            flags: 0,
                            precision: 0,
                            scale: 0,
                            buffer_size: 4,
                            max_size: 4,
                            charset_id: 0,
                            charset_form: 0,
                            nullable: true,
                            schema: None,
                            type_name: None,
                            position: 1,
                        };
                    }
                    FloatWire::Number => {}
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
                        if caps.report_number_scale
                            && matches!(
                                col.column_type(),
                                mysql_async::consts::ColumnType::MYSQL_TYPE_DECIMAL
                                    | mysql_async::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
                            )
                        {
                            let scale = col.decimals() as i8;
                            // Protocol column_length for DECIMAL is typically
                            // precision + 2 (sign + decimal point).
                            let precision =
                                col.column_length().saturating_sub(2).clamp(1, 38) as i8;
                            ColumnMeta::number(name, precision, scale)
                        } else {
                            ColumnMeta::number(name, 38, 0)
                        }
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
                        let fw = ieee.get(i).copied().unwrap_or(FloatWire::Number);
                        row.as_ref(i)
                            .map(|value| {
                                encode_value_for_column(value, is_numeric_column(kind), tw, fw)
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
        if let Some(expanded) = expand_instead_of_dml(command, &*self.instead_of.lock().await) {
            return run_drained(&mut conn, &expanded).await;
        }
        // Rowcount-only DML path: MariaDB rejects `UPDATE … RETURNING`. OUT-bind
        // RETURNING goes through `execute_returning` instead; here just strip.
        let command_owned = {
            let up = command.to_ascii_uppercase();
            if up.trim_start().starts_with("UPDATE ")
                && let Some(r) = up.rfind(" RETURNING ")
            {
                command[..r].trim().to_string()
            } else {
                command.to_string()
            }
        };
        let command = command_owned.as_str();
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
        if let Some((view, trigger)) = parse_instead_of_trigger(sql) {
            self.instead_of.lock().await.insert(view, trigger);
            let _ = conn.query_drop("START TRANSACTION").await;
            return Ok(0);
        }
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
        // MariaDB has no `UPDATE … RETURNING`. Lower to UPDATE then SELECT of
        // the returned expressions under the same WHERE (Oracle RETURNING sees
        // NEW values; SET columns that also appear in WHERE are rare and the
        // corpus/probe shapes keep the key stable).
        let sql = rewrite_update_returning_for_mariadb(sql);
        let (msql, params) = mariadb_prepare(&sql, binds)?;
        // Multi-statement form: `UPDATE …; SELECT …` — run both, keep SELECT rows.
        let rows: Vec<mysql_async::Row> = if msql.contains(';') && params.is_empty() {
            let mut parts = msql.split(';').map(str::trim).filter(|s| !s.is_empty());
            let Some(update_sql) = parts.next() else {
                return Ok((0, Vec::new()));
            };
            let Some(select_sql) = parts.next() else {
                return Err(Error::Postgres(
                    "UPDATE RETURNING lowering produced no SELECT".into(),
                ));
            };
            conn.query_drop(update_sql).await.map_err(mariadb_error)?;
            conn.query(select_sql).await.map_err(mariadb_error)?
        } else if params.is_empty() {
            // MariaDB `INSERT/DELETE … RETURNING <cols>` yields the projected rows.
            conn.query(&msql).await.map_err(mariadb_error)?
        } else if msql.contains(';') {
            // Bound UPDATE RETURNING: same split; binds apply to both statements
            // only when placeholders appear in both — our lowering puts binds in
            // the UPDATE SET/WHERE and none in the SELECT of literals/columns.
            let mut parts = msql.split(';').map(str::trim).filter(|s| !s.is_empty());
            let Some(update_sql) = parts.next() else {
                return Ok((0, Vec::new()));
            };
            let Some(select_sql) = parts.next() else {
                return Err(Error::Postgres(
                    "UPDATE RETURNING lowering produced no SELECT".into(),
                ));
            };
            let (upd_sql, upd_params) = mariadb_prepare(update_sql, binds)?;
            conn.exec_drop(upd_sql, Params::Positional(upd_params))
                .await
                .map_err(mariadb_error)?;
            conn.query(select_sql).await.map_err(mariadb_error)?
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
                    .map(|value| {
                        encode_value_for_column(
                            value,
                            is_numeric_column(kind),
                            tw,
                            FloatWire::Number,
                        )
                    })
                    .transpose()?;
                col.push(encoded);
            }
        }
        Ok((rows.len() as u64, per_col))
    }

    async fn cancel(&self) {}
}

fn expand_instead_of_dml(
    sql: &str,
    triggers: &HashMap<String, InsteadOfTrigger>,
) -> Option<String> {
    let up = sql.to_ascii_uppercase();
    let (event, rest_orig) = if up.starts_with("INSERT INTO ") {
        ("INSERT", &sql["INSERT INTO ".len()..])
    } else if up.starts_with("UPDATE ") {
        ("UPDATE", &sql["UPDATE ".len()..])
    } else if up.starts_with("DELETE FROM ") {
        ("DELETE", &sql["DELETE FROM ".len()..])
    } else {
        return None;
    };
    let table = rest_orig
        .split_whitespace()
        .next()?
        .trim_matches(|c| c == '"' || c == '`' || c == '(')
        .to_ascii_lowercase();
    let trig = triggers.get(&table)?;
    if !trig.event.eq_ignore_ascii_case(event) {
        return None;
    }
    let mut body = trig.body.clone();
    if event == "INSERT" {
        // INSERT INTO v (c1, c2) VALUES (v1, v2)
        let cols_vals = parse_insert_cols_vals(sql)?;
        for (col, val) in cols_vals {
            body = replace_new_old_col(&body, "NEW", &col, &val);
        }
    }
    // Drop a trailing END; / END from the PL/SQL wrapper if present — body is
    // already the executable statement list extracted at CREATE time.
    Some(body.trim().trim_end_matches(';').to_string())
}

fn parse_insert_cols_vals(sql: &str) -> Option<Vec<(String, String)>> {
    let up = sql.to_ascii_uppercase();
    let values_at = up.find(" VALUES ")?;
    let before = sql[..values_at].trim_end();
    let cols_open = before.rfind('(')?;
    let cols_close = before.rfind(')')?;
    let cols: Vec<String> = before[cols_open + 1..cols_close]
        .split(',')
        .map(|c| {
            c.trim()
                .trim_matches(|ch| ch == '"' || ch == '`' || ch == '\'')
                .to_ascii_lowercase()
        })
        .collect();
    let after = sql[values_at + " VALUES ".len()..].trim_start();
    if !after.starts_with('(') {
        return None;
    }
    let close = matching_paren_basic(after)?;
    let vals = split_top_level_args(&after[1..close]);
    if cols.len() != vals.len() {
        return None;
    }
    Some(
        cols.into_iter()
            .zip(vals.into_iter().map(|v| v.trim().to_string()))
            .collect(),
    )
}

fn replace_new_old_col(body: &str, which: &str, col: &str, val: &str) -> String {
    let mut out = body.to_string();
    for pat in [
        format!(":{which}.{col}"),
        format!(":{which}.{u}", u = col.to_ascii_uppercase()),
        format!("{which}.{col}"),
        format!("{which}.{u}", u = col.to_ascii_uppercase()),
    ] {
        // Case-insensitive replace of the correlation reference.
        while let Some(at) = out.to_ascii_uppercase().find(&pat.to_ascii_uppercase()) {
            out.replace_range(at..at + pat.len(), val);
        }
    }
    out
}

fn split_top_level_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if let Some(q) = quote {
            if b[i] == q {
                if b.get(i + 1) == Some(&q) {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'\'' | b'"' => quote = Some(b[i]),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].to_string());
    out
}

fn matching_paren_basic(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        if let Some(q) = quote {
            if c == q {
                if b.get(i + 1) == Some(&q) {
                    continue;
                }
                quote = None;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
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

fn parse_instead_of_trigger(sql: &str) -> Option<(String, InsteadOfTrigger)> {
    let up = sql.to_ascii_uppercase();
    if !up.contains(" INSTEAD OF ") {
        return None;
    }
    // CREATE [OR REPLACE] TRIGGER name INSTEAD OF INSERT|UPDATE|DELETE ON view …
    let on_at = up.find(" ON ")?;
    let instead = up.find(" INSTEAD OF ")?;
    let event = up[instead + " INSTEAD OF ".len()..]
        .split_whitespace()
        .next()?
        .to_string();
    let after_on = sql[on_at + 4..].trim();
    let view = after_on
        .split_whitespace()
        .next()?
        .trim_matches(|c| c == '"' || c == '`')
        .to_ascii_lowercase();
    // Body: innermost BEGIN … END block content (statements only).
    let body = extract_plsql_begin_body(sql).unwrap_or_else(|| sql.to_string());
    Some((view, InsteadOfTrigger { event, body }))
}

fn extract_plsql_begin_body(sql: &str) -> Option<String> {
    let up = sql.to_ascii_uppercase();
    let begin_at = up.rfind(" BEGIN ")?;
    let after = sql[begin_at + " BEGIN ".len()..].trim();
    let after_up = after.to_ascii_uppercase();
    let end_at = after_up.rfind("END")?;
    Some(after[..end_at].trim().trim_end_matches(';').to_string())
}

fn mariadb_error(e: mysql_async::Error) -> Error {
    mariadb_error_for_sql(e, "")
}

/// Prefer [`mariadb_error_for_sql`] when the failing statement text is known so
/// Oracle `error_pos` can point at the bad identifier.
fn mariadb_error_for_sql(e: mysql_async::Error, sql: &str) -> Error {
    match e {
        mysql_async::Error::Server(server) => {
            let detail = format!("{}: {}", server.state, server.message);
            let position = estimate_mariadb_error_position(sql, &server.message);
            Error::PgStatement { detail, position }
        }
        other => Error::Postgres(format!("MariaDB error: {other}")),
    }
}

/// Best-effort 1-based offset into `sql` for common MariaDB error shapes
/// (`Unknown column 'x'`, `Unknown table 't'`).
fn estimate_mariadb_error_position(sql: &str, message: &str) -> Option<u32> {
    if sql.is_empty() {
        return None;
    }
    for prefix in ["Unknown column '", "Unknown table '", "Table '", "Column '"] {
        if let Some(rest) = message.strip_prefix(prefix)
            && let Some(name) = rest.split('\'').next()
            && !name.is_empty()
        {
            let lower_sql = sql.to_ascii_lowercase();
            let lower_name = name.to_ascii_lowercase();
            if let Some(idx) = lower_sql.find(&lower_name) {
                return Some((idx + 1) as u32);
            }
        }
    }
    // Fallback: first non-keyword token after WHERE / SET often hosts the fault.
    if let Some(w) = sql.to_ascii_uppercase().find(" WHERE ") {
        return Some((w + " WHERE ".len() + 1) as u32);
    }
    Some(1)
}

/// `UPDATE t SET … WHERE … RETURNING exprs` → `UPDATE …; SELECT exprs FROM t WHERE …`.
fn rewrite_update_returning_for_mariadb(sql: &str) -> String {
    let up = sql.to_ascii_uppercase();
    if !up.trim_start().starts_with("UPDATE ") {
        return sql.to_string();
    }
    let Some(r_rel) = up.rfind(" RETURNING ") else {
        return sql.to_string();
    };
    let update_part = sql[..r_rel].trim().trim_end_matches(';');
    let exprs = sql[r_rel + " RETURNING ".len()..]
        .trim()
        .trim_end_matches(';');
    if exprs.is_empty() {
        return sql.to_string();
    }
    let after_update = update_part
        .trim_start()
        .strip_prefix("UPDATE ")
        .or_else(|| update_part.trim_start().strip_prefix("update "))
        .unwrap_or(update_part.trim_start());
    let after_up = after_update.to_ascii_uppercase();
    let Some(set_at) = after_up.find(" SET ") else {
        return sql.to_string();
    };
    let table = after_update[..set_at].trim();
    let where_clause = after_up
        .rfind(" WHERE ")
        .map(|w| &after_update[w..])
        .unwrap_or("");
    format!("{update_part}; SELECT {exprs} FROM {table}{where_clause}")
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
        let mut result = conn
            .query_iter(&msql)
            .await
            .map_err(|e| mariadb_error_for_sql(e, sql))?;
        let columns = result.columns().unwrap_or_default();
        let rows: Vec<mysql_async::Row> = result
            .collect()
            .await
            .map_err(|e| mariadb_error_for_sql(e, sql))?;
        // A statement can return more than one result set (`CALL`); drain the
        // rest so the next command does not read stale bytes.
        result
            .drop_result()
            .await
            .map_err(|e| mariadb_error_for_sql(e, sql))?;
        Ok((columns, rows))
    } else {
        let mut result = conn
            .exec_iter(&msql, Params::Positional(params))
            .await
            .map_err(|e| mariadb_error_for_sql(e, sql))?;
        let columns = result.columns().unwrap_or_default();
        let rows: Vec<mysql_async::Row> = result
            .collect()
            .await
            .map_err(|e| mariadb_error_for_sql(e, sql))?;
        result
            .drop_result()
            .await
            .map_err(|e| mariadb_error_for_sql(e, sql))?;
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
    // Native MariaDB bind style (`?`) from `wire::bind_parameters`. Do not look
    // for `$n` — the placeholders are already positional.
    if !sql
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'$' && w[1].is_ascii_digit())
        && sql.contains('?')
        && !binds.is_empty()
    {
        let params = binds.iter().map(bind_value).collect::<Result<Vec<_>>>()?;
        return Ok((sql.to_string(), params));
    }
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
        let scale = if caps.timestamp_scale_zero {
            0
        } else {
            decimals.min(9) as i8
        };
        TemporalWire::Timestamp(scale)
    } else {
        TemporalWire::Date
    }
}

fn encode_value_for_column(
    value: &Value,
    number: bool,
    temporal: TemporalWire,
    float: FloatWire,
) -> Result<Vec<u8>> {
    match float {
        FloatWire::BinaryDouble => {
            let v = value_as_f64(value).ok_or_else(|| {
                Error::DataConversionError("BINARY_DOUBLE value is not numeric".into())
            })?;
            return Ok(crate::backend::encode_binary_double(v).to_vec());
        }
        FloatWire::BinaryFloat => {
            let v = value_as_f64(value).ok_or_else(|| {
                Error::DataConversionError("BINARY_FLOAT value is not numeric".into())
            })?;
            return Ok(crate::backend::encode_binary_float(v as f32).to_vec());
        }
        FloatWire::Number => {}
    }
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

fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Float(v) => Some(*v as f64),
        Value::Double(v) => Some(*v),
        Value::Int(v) => Some(*v as f64),
        Value::UInt(v) => Some(*v as f64),
        Value::Bytes(bytes) => std::str::from_utf8(bytes).ok()?.parse().ok(),
        _ => None,
    }
}

/// How a MariaDB FLOAT/DOUBLE column is delivered to the Oracle client.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FloatWire {
    /// Oracle NUMBER (computed doubles, bare FLOAT/DOUBLE).
    Number,
    /// Oracle BINARY_FLOAT (type 100) — declared column tagged via COMMENT.
    BinaryFloat,
    /// Oracle BINARY_DOUBLE (type 101) — declared column tagged via COMMENT.
    BinaryDouble,
}

/// Recover declared `BINARY_FLOAT` / `BINARY_DOUBLE` columns via the COMMENT
/// marker written by [`crate::translate::oracle_to_mariadb`]. Computed floats
/// (no table oid / empty table name) stay NUMBER.
async fn lookup_ieee_float_wire(
    conn: &mut Conn,
    cols: &[mysql_async::Column],
    native: bool,
) -> Vec<FloatWire> {
    let mut out = vec![FloatWire::Number; cols.len()];
    if !native {
        return out;
    }
    let needs_lookup = cols.iter().any(|col| {
        !col.table_str().is_empty()
            && matches!(
                col.column_type(),
                mysql_async::consts::ColumnType::MYSQL_TYPE_FLOAT
                    | mysql_async::consts::ColumnType::MYSQL_TYPE_DOUBLE
            )
    });
    if !needs_lookup {
        return out;
    }
    let rows: Vec<(String, String, String)> = conn
        .query(
            "SELECT LOWER(TABLE_NAME), LOWER(COLUMN_NAME), COLUMN_COMMENT \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() \
               AND COLUMN_COMMENT IN ('dbsaci.binary_double', 'dbsaci.binary_float')",
        )
        .await
        .unwrap_or_default();
    let mut map = std::collections::HashMap::new();
    for (table, column, comment) in rows {
        map.insert((table, column), comment);
    }
    for (i, col) in cols.iter().enumerate() {
        let table = col.table_str().to_ascii_lowercase();
        if table.is_empty() {
            continue;
        }
        let name = col.name_str().to_ascii_lowercase();
        match map.get(&(table, name)).map(String::as_str) {
            Some("dbsaci.binary_double") => out[i] = FloatWire::BinaryDouble,
            Some("dbsaci.binary_float") => out[i] = FloatWire::BinaryFloat,
            _ => {}
        }
    }
    out
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

/// Build MariaDB connection options. `--db-ssl` is applied here so the schema
/// probe and the session connection use the same TLS policy.
fn mariadb_connection_opts(url: &str, ssl: bool) -> Result<Opts> {
    let opts = Opts::from_url(url)
        .map_err(|e| Error::Postgres(format!("invalid MariaDB connection URL: {e}")))?;
    // CLIENT_FOUND_ROWS: UPDATE rowcount = rows matched, matching Oracle / the
    // python-oracledb executemany assertion (not "rows actually changed").
    let builder = mysql_async::OptsBuilder::from_opts(opts).client_found_rows(true);
    Ok(if ssl {
        builder.ssl_opts(mysql_async::SslOpts::default()).into()
    } else {
        builder.into()
    })
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
    use super::{
        InsteadOfTrigger, estimate_mariadb_error_position, expand_instead_of_dml,
        mariadb_connection_opts, mariadb_sql, parse_instead_of_trigger,
        rewrite_update_returning_for_mariadb,
    };
    use std::collections::HashMap;

    #[test]
    fn converts_backend_parameters_but_not_literals_or_identifiers() {
        assert_eq!(
            mariadb_sql("SELECT $1, '$2', \"$3\", `$4` WHERE x = $12"),
            "SELECT ?, '$2', \"$3\", `$4` WHERE x = ?"
        );
    }

    #[test]
    fn schema_probe_and_session_share_ssl_opts() {
        let url = "mysql://u:p@127.0.0.1:3306/db";
        let plain = mariadb_connection_opts(url, false).expect("plain opts");
        let tls = mariadb_connection_opts(url, true).expect("tls opts");
        assert!(
            plain.ssl_opts().is_none(),
            "plaintext must not attach SslOpts"
        );
        assert!(
            tls.ssl_opts().is_some(),
            "--db-ssl must attach SslOpts on every Conn including the schema probe"
        );
    }

    #[test]
    fn update_returning_becomes_update_then_select() {
        assert_eq!(
            rewrite_update_returning_for_mariadb(
                "UPDATE people SET team_id = 3 WHERE id = 77 RETURNING team_id"
            ),
            "UPDATE people SET team_id = 3 WHERE id = 77; SELECT team_id FROM people WHERE id = 77"
        );
    }

    #[test]
    fn error_position_finds_unknown_column() {
        let sql = "SELECT 1 FROM people WHERE nonexistent_col_xyz = 1";
        let pos = estimate_mariadb_error_position(
            sql,
            "Unknown column 'nonexistent_col_xyz' in 'where clause'",
        );
        assert_eq!(
            pos,
            Some(sql.find("nonexistent_col_xyz").unwrap() as u32 + 1)
        );
    }

    #[test]
    fn instead_of_insert_expands_new_bindings() {
        let ddl = "CREATE OR REPLACE TRIGGER trg_io INSTEAD OF INSERT ON trg_v \
             FOR EACH ROW BEGIN INSERT INTO trg_base (id, name) VALUES (:NEW.id, UPPER(:NEW.name)); END;";
        let (view, trig) = parse_instead_of_trigger(ddl).expect("parse");
        assert_eq!(view, "trg_v");
        assert_eq!(trig.event, "INSERT");
        let mut map = HashMap::new();
        map.insert(view, trig);
        let expanded =
            expand_instead_of_dml("INSERT INTO trg_v (id, name) VALUES (1, 'ada')", &map)
                .expect("expand");
        assert!(
            expanded.to_ascii_uppercase().contains("TRG_BASE"),
            "{expanded}"
        );
        assert!(
            expanded.contains("UPPER('ada')") || expanded.contains("UPPER('ADA')"),
            "{expanded}"
        );
        assert!(expanded.contains('1'), "{expanded}");
        assert!(
            !expanded.to_ascii_uppercase().contains(":NEW"),
            "{expanded}"
        );
    }

    #[test]
    fn instead_of_parse_ignores_ordinary_triggers() {
        assert!(
            parse_instead_of_trigger(
                "CREATE OR REPLACE TRIGGER t BEFORE INSERT ON people FOR EACH ROW BEGIN NULL; END;"
            )
            .is_none()
        );
        let _ = InsteadOfTrigger {
            event: "INSERT".into(),
            body: "NULL".into(),
        };
    }
}
