# Oracle identifier semantics: an unquoted name is upper-cased when the
# statement is read, whatever its role (table, view, sequence, alias, schema);
# a quoted name is exact. On a case-sensitive backend (MariaDB with
# lower_case_table_names=0) every spelling must still reach the same object.

-- case: mixed_case_spellings_reach_one_table
-- setup: CREATE TABLE IdCase_T (Id NUMBER, Name VARCHAR2(10))
-- setup: INSERT INTO idcase_t VALUES (1, 'a')
SELECT ID, name FROM IDCASE_T
-- expect:
1 | a
-- teardown: DROP TABLE IF EXISTS idcase_t
-- end

-- case: drop_table_with_other_spelling
-- setup: CREATE TABLE idc_drop (a NUMBER)
DROP TABLE IDC_Drop
-- ok
-- teardown: DROP TABLE IF EXISTS idc_drop
-- end

-- case: view_name_resolves_in_any_case
-- setup: CREATE VIEW idc_v AS SELECT 1 AS x FROM dual
SELECT X FROM IDC_V
-- expect:
1
-- teardown: DROP VIEW IF EXISTS idc_v
-- end

-- case: foreign_key_reference_in_other_case
-- setup: CREATE TABLE idc_p (id NUMBER PRIMARY KEY)
-- setup: CREATE TABLE idc_c (pid NUMBER REFERENCES IDC_P (id))
-- setup: INSERT INTO idc_p VALUES (1)
INSERT INTO Idc_C VALUES (1)
-- rowcount: 1
-- teardown: DROP TABLE IF EXISTS idc_c
-- teardown: DROP TABLE IF EXISTS idc_p
-- end

-- case: sequence_in_other_case
-- setup: CREATE SEQUENCE idc_seq START WITH 7
SELECT IDC_SEQ.NEXTVAL FROM dual
-- expect:
7
-- teardown: DROP SEQUENCE IF EXISTS idc_seq
-- end

-- case: table_alias_in_other_case
SELECT E.NAME FROM people e WHERE e.ID = 1
-- expect:
Ada
-- end

-- case: quoted_mixed_case_name_is_exact
-- setup: CREATE TABLE "IdcMixed" (v NUMBER)
-- setup: INSERT INTO "IdcMixed" VALUES (5)
SELECT v FROM "IdcMixed"
-- expect:
5
-- teardown: DROP TABLE IF EXISTS "IdcMixed"
-- end

-- case: quoted_upper_case_is_the_unquoted_name
-- setup: CREATE TABLE idc_q (v NUMBER)
-- setup: INSERT INTO idc_q VALUES (3)
SELECT "V" FROM "IDC_Q"
-- expect:
3
-- teardown: DROP TABLE IF EXISTS idc_q
-- end

-- case: dictionary_reports_oracle_names
-- setup: CREATE TABLE idc_list (some_col NUMBER)
SELECT table_name || '.' || column_name FROM user_tab_columns WHERE table_name = 'IDC_LIST'
-- expect:
IDC_LIST.SOME_COL
-- teardown: DROP TABLE IF EXISTS idc_list
-- end

-- case: hibernate_sequence_probe
# Hibernate's Oracle dialect asks for every sequence at startup.
-- setup: CREATE SEQUENCE idc_hib_seq
SELECT sequence_name FROM all_sequences WHERE sequence_name = 'IDC_HIB_SEQ'
-- expect:
IDC_HIB_SEQ
-- teardown: DROP SEQUENCE IF EXISTS idc_hib_seq
-- end

-- case: sys_qualified_dictionary_view
-- setup: CREATE TABLE idc_sys (a NUMBER)
SELECT COUNT(*) FROM sys.all_tables WHERE table_name = 'IDC_SYS'
-- expect:
1
-- teardown: DROP TABLE IF EXISTS idc_sys
-- end
