# Data-dictionary / catalog views. ORMs, migration tools, admin scripts and
# "does this object exist?" guards hit these on almost every connection.
#
# Names come back UPPERCASE, as on a real Oracle database: Oracle folds unquoted
# identifiers to upper case, so its whole catalog is upper case and Oracle
# tooling expects that. (The objects themselves are lower case in PostgreSQL;
# the query path still resolves either case.)

# Fixtures run on a direct PostgreSQL connection (no translation), so they use
# PostgreSQL DDL; the cases below still query through PgSaci with Oracle names.
-- fixture: CREATE TABLE IF NOT EXISTS dd_demo (id integer PRIMARY KEY, label varchar(40) NOT NULL, amount numeric(10,2))
-- fixture: COMMENT ON TABLE dd_demo IS 'demo table'
-- fixture: CREATE INDEX IF NOT EXISTS dd_demo_label_ix ON dd_demo (label)
-- fixture: CREATE SEQUENCE IF NOT EXISTS dd_demo_seq

-- case: dual_is_one_row
SELECT COUNT(*) FROM DUAL
-- expect:
1
-- end

-- case: user_tables_lists_table
SELECT table_name FROM user_tables WHERE table_name = 'DD_DEMO'
-- expect:
DD_DEMO
-- end

-- case: all_tables_has_owner
SELECT owner FROM all_tables WHERE table_name = 'DD_DEMO'
-- expect:
PUBLIC
-- end

-- case: user_tab_columns_names
SELECT column_name FROM user_tab_columns WHERE table_name = 'DD_DEMO' ORDER BY column_id
-- expect:
ID
LABEL
AMOUNT
-- end

-- case: all_tab_columns_alias
SELECT column_name FROM all_tab_columns WHERE table_name = 'DD_DEMO' AND column_name = 'AMOUNT'
-- expect:
AMOUNT
-- end

-- case: user_objects_lists_table
SELECT object_type FROM user_objects WHERE object_name = 'DD_DEMO' AND object_type = 'TABLE'
-- expect:
TABLE
-- end

-- case: user_constraints_has_primary_key
SELECT constraint_type FROM user_constraints WHERE table_name = 'DD_DEMO' AND constraint_type = 'P'
-- expect:
P
-- end

-- case: user_indexes_lists_index
SELECT index_name FROM user_indexes WHERE table_name = 'DD_DEMO' AND index_name = 'DD_DEMO_LABEL_IX'
-- expect:
DD_DEMO_LABEL_IX
-- end

-- case: user_sequences_lists_sequence
SELECT sequence_name FROM user_sequences WHERE sequence_name = 'DD_DEMO_SEQ'
-- expect:
DD_DEMO_SEQ
-- end

# Hibernate's Oracle dialect probes all_sequences on startup; it must exist.
-- case: all_sequences_lists_sequence
SELECT sequence_name FROM all_sequences WHERE sequence_name = 'DD_DEMO_SEQ'
-- expect:
DD_DEMO_SEQ
-- end

-- case: user_tab_comments
SELECT comments FROM user_tab_comments WHERE table_name = 'DD_DEMO'
-- expect:
demo table
-- end

-- case: v_version_banner
SELECT banner FROM v$version WHERE ROWNUM = 1
-- ok
-- end

-- case: nls_session_parameters_has_date_format
SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_DATE_FORMAT'
-- ok
-- end

-- case: nls_session_parameters_reflects_date_format_setting
-- setup: ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'
SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_DATE_FORMAT'
-- expect:
YYYY-MM-DD
-- end

-- case: nls_session_parameters_reflects_numeric_and_comparison_settings
-- setup: ALTER SESSION SET NLS_NUMERIC_CHARACTERS = ',.'
-- setup: ALTER SESSION SET NLS_COMP = LINGUISTIC
SELECT parameter || '=' || value FROM nls_session_parameters WHERE parameter IN ('NLS_COMP', 'NLS_NUMERIC_CHARACTERS') ORDER BY parameter
-- expect:
NLS_COMP=LINGUISTIC
NLS_NUMERIC_CHARACTERS=,.
-- end

-- case: user_tables_absent_object
SELECT COUNT(*) FROM user_tables WHERE table_name = 'DOES_NOT_EXIST_XYZ'
-- expect:
0
-- end

-- case: user_tables_sees_new_table_created_in_session
-- setup: CREATE TABLE catalog_probe (id NUMBER)
SELECT table_name FROM user_tables WHERE table_name = 'CATALOG_PROBE'
-- expect:
CATALOG_PROBE
-- end

-- case: user_tab_columns_counts_people_columns
SELECT COUNT(*) FROM user_tab_columns WHERE table_name = 'PEOPLE'
-- expect:
3
-- end
