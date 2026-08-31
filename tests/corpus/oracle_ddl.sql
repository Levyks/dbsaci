# Oracle DDL: type names are structurally translated (NUMBER -> NUMERIC,
# VARCHAR2 -> VARCHAR, CLOB -> TEXT, DATE DEFAULT SYSDATE -> ... CURRENT_TIMESTAMP)
# and the resulting table must accept DML. Each case is rolled back afterwards.

-- case: create_table_with_oracle_types
CREATE TABLE ddl_types (id NUMBER(10) PRIMARY KEY, label VARCHAR2(30) NOT NULL, note CLOB, created_at DATE DEFAULT SYSDATE)
-- rowcount: 0
-- end

-- case: oracle_typed_table_accepts_insert
-- setup: CREATE TABLE ddl_ins (id NUMBER(10) PRIMARY KEY, label VARCHAR2(30) NOT NULL, note CLOB)
INSERT INTO ddl_ins (id, label, note) VALUES (1, 'schema row', 'long text body')
-- rowcount: 1
-- end

-- case: oracle_typed_table_roundtrips_values
-- setup: CREATE TABLE ddl_rt (id NUMBER(10) PRIMARY KEY, label VARCHAR2(30) NOT NULL, note CLOB, created_at DATE DEFAULT SYSDATE)
-- setup: INSERT INTO ddl_rt (id, label, note) VALUES (1, 'schema row', 'long text body')
SELECT id, label, note FROM ddl_rt
-- expect:
1 | schema row | long text body
-- end

-- case: number_column_reports_as_number
-- setup: CREATE TABLE ddl_num (n NUMBER(10))
-- setup: INSERT INTO ddl_num (n) VALUES (42)
SELECT n + 1 FROM ddl_num
-- expect:
43
-- end

-- case: date_default_populates
-- setup: CREATE TABLE ddl_def (id NUMBER PRIMARY KEY, created_at DATE DEFAULT SYSDATE)
-- setup: INSERT INTO ddl_def (id) VALUES (1)
SELECT COUNT(*) FROM ddl_def WHERE created_at IS NOT NULL
-- expect:
1
-- end

# Oracle DATE is second-precision date+time; a DATE column must not truncate
# to midnight the way PostgreSQL date would.
-- case: date_column_keeps_time_of_day
-- setup: CREATE TABLE ddl_date_time (id NUMBER, tv DATE)
-- setup: INSERT INTO ddl_date_time (id, tv) VALUES (1, TIMESTAMP '2022-08-11 03:10:20')
-- setup: INSERT INTO ddl_date_time (id, tv) VALUES (2, TIMESTAMP '2022-08-11 06:10:20')
SELECT id, TO_CHAR(tv, 'YYYY-MM-DD HH24:MI:SS') FROM ddl_date_time ORDER BY id
-- expect:
1 | 2022-08-11 03:10:20
2 | 2022-08-11 06:10:20
-- end

# NOT NULL ENABLE / NOT NULL DISABLE — Oracle's constraint-state suffix on an
# inline column constraint. PostgreSQL has no such keyword; it must be stripped.
-- case: not_null_enable_inline_constraint
-- setup: CREATE TABLE ddl_nn_enable (id NUMBER, code VARCHAR2(32) NOT NULL ENABLE, note VARCHAR2(16) NOT NULL DISABLE)
-- setup: INSERT INTO ddl_nn_enable (id, code, note) VALUES (1, 'abc', 'n')
SELECT code, note FROM ddl_nn_enable WHERE id = 1
-- expect:
abc | n
-- end

-- case: create_then_drop
-- setup: CREATE TABLE ddl_drop (id NUMBER)
DROP TABLE ddl_drop
-- rowcount: 0
-- end

-- case: varchar2_length_enforced
-- setup: CREATE TABLE ddl_len (label VARCHAR2(4))
INSERT INTO ddl_len (label) VALUES ('toolong')
-- error: too long
-- end

-- case: varchar2_char_semantics_length
-- setup: CREATE TABLE cs_demo (a VARCHAR2(5 CHAR), b VARCHAR2(5 BYTE))
-- setup: INSERT INTO cs_demo (a, b) VALUES ('hi', 'yo')
SELECT a || b FROM cs_demo
-- expect:
hiyo
-- end

-- case: timestamp_with_precision
-- setup: CREATE TABLE ts_demo (t TIMESTAMP(6))
-- setup: INSERT INTO ts_demo (t) VALUES (TIMESTAMP '2024-01-02 03:04:05')
SELECT TO_CHAR(t, 'YYYY-MM-DD HH24:MI:SS') FROM ts_demo
-- expect:
2024-01-02 03:04:05
-- end

-- case: number_star_is_float
-- setup: CREATE TABLE num_star (n NUMBER)
-- setup: INSERT INTO num_star (n) VALUES (3.14159)
SELECT n FROM num_star
-- expect:
3.14159
-- end

-- case: default_on_null
-- setup: CREATE TABLE don_demo (id NUMBER, v VARCHAR2(10) DEFAULT ON NULL 'dflt' NOT NULL)
-- setup: INSERT INTO don_demo (id) VALUES (1)
SELECT v FROM don_demo
-- expect:
dflt
-- end

-- case: virtual_column
-- setup: CREATE TABLE vc_demo (price NUMBER, qty NUMBER, total NUMBER GENERATED ALWAYS AS (price * qty) VIRTUAL)
-- setup: INSERT INTO vc_demo (price, qty) VALUES (10, 3)
SELECT total FROM vc_demo
-- expect:
30
-- end

-- case: add_column
-- setup: CREATE TABLE alt_demo (id NUMBER)
-- setup: ALTER TABLE alt_demo ADD (label VARCHAR2(20) DEFAULT 'x')
-- setup: INSERT INTO alt_demo (id) VALUES (1)
SELECT label FROM alt_demo
-- expect:
x
-- end

-- case: modify_column_type
-- setup: CREATE TABLE mod_demo (n VARCHAR2(5))
-- setup: ALTER TABLE mod_demo MODIFY (n VARCHAR2(50))
-- setup: INSERT INTO mod_demo (n) VALUES ('a much longer value now')
SELECT LENGTH(n) FROM mod_demo
-- expect:
23
-- end

-- case: create_table_as_select
-- setup: CREATE TABLE ctas_demo AS SELECT id, name FROM people WHERE team_id = 2
SELECT name FROM ctas_demo
-- expect:
Linus
-- end

-- case: composite_primary_key
-- setup: CREATE TABLE cpk (a NUMBER, b NUMBER, CONSTRAINT cpk_pk PRIMARY KEY (a, b))
-- setup: INSERT INTO cpk (a, b) VALUES (1, 1)
INSERT INTO cpk (a, b) VALUES (1, 1)
-- error: ORA-00001
-- end

-- case: comment_on_column
-- setup: CREATE TABLE com_demo (x NUMBER)
COMMENT ON COLUMN com_demo.x IS 'the x'
-- rowcount: 0
-- end

-- case: materialized_view_build_and_refresh
-- setup: CREATE MATERIALIZED VIEW ddl_mv AS SELECT id, name FROM people WHERE team_id = 1
-- setup: REFRESH MATERIALIZED VIEW ddl_mv
SELECT name FROM ddl_mv ORDER BY id
-- expect:
Ada
Grace
-- end

-- case: rename_column
-- setup: CREATE TABLE rename_demo (old_name VARCHAR2(20))
-- setup: INSERT INTO rename_demo (old_name) VALUES ('kept')
-- setup: ALTER TABLE rename_demo RENAME COLUMN old_name TO new_name
SELECT new_name FROM rename_demo
-- expect:
kept
-- end

-- case: drop_parenthesised_columns
-- setup: CREATE TABLE drop_cols_demo (id NUMBER, obsolete_a VARCHAR2(10), obsolete_b VARCHAR2(10))
-- setup: ALTER TABLE drop_cols_demo DROP (obsolete_a, obsolete_b)
-- setup: INSERT INTO drop_cols_demo (id) VALUES (7)
SELECT id FROM drop_cols_demo
-- expect:
7
-- end

-- case: set_unused_hides_columns
-- setup: CREATE TABLE unused_cols_demo (id NUMBER, obsolete VARCHAR2(10))
-- setup: ALTER TABLE unused_cols_demo SET UNUSED (obsolete)
-- setup: INSERT INTO unused_cols_demo (id) VALUES (8)
SELECT id FROM unused_cols_demo
-- expect:
8
-- end

-- case: modify_default
-- setup: CREATE TABLE mod_default_demo (id NUMBER, label VARCHAR2(20))
-- setup: ALTER TABLE mod_default_demo MODIFY (label DEFAULT 'revised')
-- setup: INSERT INTO mod_default_demo (id) VALUES (1)
SELECT label FROM mod_default_demo
-- expect:
revised
-- end

-- case: add_and_drop_constraint
-- setup: CREATE TABLE constraint_demo (id NUMBER, code VARCHAR2(10))
-- setup: ALTER TABLE constraint_demo ADD CONSTRAINT constraint_demo_code_uq UNIQUE (code)
-- setup: INSERT INTO constraint_demo (id, code) VALUES (1, 'same')
INSERT INTO constraint_demo (id, code) VALUES (2, 'same')
-- error: ORA-00001
-- end

-- case: physical_storage_clauses_ignored
-- setup: CREATE TABLE physical_demo (id NUMBER, label VARCHAR2(20)) TABLESPACE users PCTFREE 10 INITRANS 2 STORAGE (INITIAL 64K NEXT 1M) LOGGING PARALLEL 4 SEGMENT CREATION IMMEDIATE
-- setup: INSERT INTO physical_demo (id, label) VALUES (1, 'stored')
SELECT label FROM physical_demo
-- expect:
stored
-- end

-- case: synonym_is_a_view_over_target
-- setup: CREATE SYNONYM people_syn FOR people
SELECT name FROM people_syn WHERE id = 1
-- teardown: DROP VIEW IF EXISTS people_syn
-- expect:
Ada
-- end

-- case: create_or_replace_synonym
-- setup: CREATE SYNONYM t_syn FOR teams
-- setup: CREATE OR REPLACE SYNONYM t_syn FOR people
SELECT name FROM t_syn WHERE id = 3
-- teardown: DROP VIEW IF EXISTS t_syn
-- expect:
Linus
-- end

-- case: drop_synonym
-- setup: CREATE SYNONYM gone_syn FOR people
-- setup: DROP SYNONYM gone_syn
SELECT COUNT(*) FROM user_tables WHERE table_name = 'gone_syn'
-- expect:
0
-- end

-- case: global_temporary_table_delete_rows
-- setup: CREATE GLOBAL TEMPORARY TABLE gtt_demo (id NUMBER) ON COMMIT PRESERVE ROWS
-- setup: INSERT INTO gtt_demo (id) VALUES (42)
SELECT id FROM gtt_demo
-- teardown: DROP TABLE IF EXISTS gtt_demo
-- expect:
42
-- end

-- case: global_temporary_table_redeclare_is_idempotent
-- setup: CREATE GLOBAL TEMPORARY TABLE gtt_idem (id NUMBER) ON COMMIT PRESERVE ROWS
CREATE GLOBAL TEMPORARY TABLE gtt_idem (id NUMBER) ON COMMIT PRESERVE ROWS
-- ok
-- teardown: DROP TABLE IF EXISTS gtt_idem
-- end

-- case: comment_on_table_and_column
-- setup: COMMENT ON TABLE people IS 'staff directory'
COMMENT ON COLUMN people.name IS 'full display name'
-- ok
-- end

-- case: function_based_index_upper
-- setup: CREATE TABLE ddl_fbi (id NUMBER PRIMARY KEY, name VARCHAR2(50))
-- setup: CREATE INDEX ddl_fbi_uname ON ddl_fbi (UPPER(name))
-- setup: INSERT INTO ddl_fbi (id, name) VALUES (1, 'Ada')
-- teardown: DROP TABLE ddl_fbi
SELECT id FROM ddl_fbi WHERE UPPER(name) = 'ADA'
-- expect:
1
-- end

-- case: function_based_index_expression
-- setup: CREATE TABLE ddl_fbe (id NUMBER PRIMARY KEY, a NUMBER, b NUMBER)
-- setup: CREATE INDEX ddl_fbe_sum ON ddl_fbe (NVL(a, 0) + NVL(b, 0))
-- setup: INSERT INTO ddl_fbe (id, a, b) VALUES (1, 10, NULL)
-- teardown: DROP TABLE ddl_fbe
SELECT id FROM ddl_fbe WHERE NVL(a, 0) + NVL(b, 0) = 10
-- expect:
1
-- end

-- case: unique_function_based_index
-- setup: CREATE TABLE ddl_ufbi (id NUMBER PRIMARY KEY, email VARCHAR2(80))
-- setup: CREATE UNIQUE INDEX ddl_ufbi_email ON ddl_ufbi (LOWER(email))
-- setup: INSERT INTO ddl_ufbi (id, email) VALUES (1, 'A@X.COM')
-- teardown: DROP TABLE ddl_ufbi
INSERT INTO ddl_ufbi (id, email) VALUES (2, 'a@x.com')
-- error: duplicate
-- end
