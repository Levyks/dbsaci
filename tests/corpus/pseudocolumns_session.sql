# Session functions and pseudo-columns that legacy code sprinkles through
# audit columns, row filters and "who am I" checks.

-- case: user_function
SELECT USER FROM DUAL
-- expect:
CORPUS
-- end

-- case: user_lowercase_context
SELECT SYS_CONTEXT('USERENV', 'SESSION_USER') FROM DUAL
-- expect:
CORPUS
-- end

-- case: current_schema_context
SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM DUAL
-- expect:
CORPUS
-- end

-- case: alter_session_current_schema_changes_userenv
# known-gap (mariadb): tracked in expected-failures.mariadb — uses pg_catalog, a PostgreSQL-only schema
-- setup: ALTER SESSION SET CURRENT_SCHEMA = pg_catalog
SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM DUAL
-- expect:
PG_CATALOG
-- end

-- case: alter_session_time_zone_applies_to_current_timestamp
-- setup: ALTER SESSION SET TIME_ZONE = '-03:00'
SELECT TO_CHAR(CURRENT_TIMESTAMP, 'TZH:TZM') FROM DUAL
-- expect:
-03:00
-- end

-- case: harmless_alter_session_setting_is_accepted
-- setup: ALTER SESSION SET SQL_TRACE = TRUE
SELECT 1 FROM DUAL
-- expect:
1
-- end

-- case: uid_is_numeric
SELECT CASE WHEN UID >= 0 THEN 'ok' ELSE 'bad' END FROM DUAL
-- expect:
ok
-- end

-- case: sysdate_shape
SELECT TO_CHAR(SYSDATE, 'YYYY-MM-DD') FROM DUAL
-- expect-regex: ^\d\d\d\d-\d\d-\d\d$
-- end

-- case: systimestamp_shape
SELECT TO_CHAR(SYSTIMESTAMP, 'YYYY-MM-DD"T"HH24:MI:SS') FROM DUAL
-- expect-regex: ^\d\d\d\d-\d\d-\d\dT\d\d:\d\d:\d\d$
-- end

-- case: rowid_pseudocolumn
# known-gap (mariadb): expected-failures.mariadb — no ROWID (no id-column shim)
SELECT COUNT(ROWID) FROM people
-- expect:
4
-- end

-- case: rownum_pseudocolumn
SELECT MAX(rn) FROM (SELECT ROWNUM rn FROM people)
-- expect:
4
-- end

-- case: current_date_shape
SELECT TO_CHAR(CURRENT_DATE, 'YYYY-MM-DD') FROM DUAL
-- expect-regex: ^\d\d\d\d-\d\d-\d\d$
-- end

-- case: sysdate_used_as_default_audit_column
-- setup: CREATE TABLE audit_demo (id NUMBER, created DATE DEFAULT SYSDATE, created_by VARCHAR2(30) DEFAULT USER)
-- setup: INSERT INTO audit_demo (id) VALUES (1)
SELECT created_by FROM audit_demo WHERE id = 1
-- expect:
CORPUS
-- end
