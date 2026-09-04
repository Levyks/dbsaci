# Data-dictionary / catalog views. ORMs, migration tools, admin scripts and
# "does this object exist?" guards hit these on almost every connection.
#
# Names come back UPPERCASE, as on a real Oracle database: Oracle folds unquoted
# identifiers to upper case, so its whole catalog is upper case and Oracle
# tooling expects that. (The objects themselves are lower case in PostgreSQL;
# the query path still resolves either case.)

# Fixtures run on a direct PostgreSQL connection (no translation), so they use
# PostgreSQL DDL; the cases below still query through DbSaci with Oracle names.
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
-- skip: mariadb (facade reports the connected Oracle schema, not the fixture's PostgreSQL PUBLIC alias)
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

# A table created through DbSaci lands in the shared `oracle` schema, but the
# catalog facade must present it as owned by the connected user (Oracle's
# "schema == user" model) so an IDE shows it under the user's node — not under
# a literal ORACLE schema. `all_users` must likewise list the connected user.
-- case: all_tables_owner_is_connected_user_for_session_table
-- setup: CREATE TABLE owner_probe (id NUMBER)
SELECT COUNT(*) FROM all_tables
WHERE table_name = 'OWNER_PROBE' AND owner = sys_context('USERENV','CURRENT_SCHEMA')
-- expect:
1
-- end

-- case: all_users_lists_connected_user
SELECT COUNT(*) FROM sys.all_users
WHERE username = sys_context('USERENV','CURRENT_SCHEMA')
-- expect:
1
-- end

-- case: user_tab_columns_counts_people_columns
SELECT COUNT(*) FROM user_tab_columns WHERE table_name = 'PEOPLE'
-- expect:
3
-- end

# Materialized-view-log dictionary views: no equivalent in PostgreSQL, so
# always empty — but an IDE walking the mview branch selects from them and
# errors if the relation is missing (ORA-00942).
-- case: all_mview_logs_is_queryable_and_empty
SELECT COUNT(*) FROM sys.all_mview_logs
-- expect:
0
-- end

-- case: all_mview_comments_is_queryable_and_empty
SELECT COUNT(*) FROM sys.all_mview_comments
-- expect:
0
-- end

-- case: all_object_tables_is_queryable_and_empty
SELECT COUNT(*) FROM sys.all_object_tables
-- expect:
0
-- end

-- case: all_indexes_has_domain_index_columns
SELECT ityp_owner, ityp_name, parameters, funcidx_status, visibility
FROM sys.all_indexes WHERE index_name = 'DD_DEMO_LABEL_IX'
-- expect:
NULL | NULL | NULL | NULL | VISIBLE
-- end

-- case: all_ind_expressions_is_queryable_and_empty
SELECT COUNT(*) FROM sys.all_ind_expressions
-- expect:
0
-- end

-- case: all_triggers_reports_timing_event_action_type
-- skip: mariadb (a BEFORE INSERT OR UPDATE trigger is split into two single-event MariaDB triggers; the all_triggers facade does not reassemble the combined event)
-- setup: CREATE TABLE trg_probe (id NUMBER, n NUMBER)
-- setup: CREATE OR REPLACE TRIGGER trg_probe_biu BEFORE INSERT OR UPDATE ON trg_probe FOR EACH ROW BEGIN :NEW.n := NVL(:NEW.n, 0) + 1; END;
SELECT trigger_type, triggering_event, action_type, before_row, status
FROM sys.all_triggers WHERE trigger_name = 'TRG_PROBE_BIU'
-- expect:
BEFORE EACH ROW | INSERT OR UPDATE | PL/SQL | YES | ENABLED
-- end

# The long tail of Oracle dictionary views an IDE introspector walks: no
# PostgreSQL equivalent, so empty, but each must resolve instead of ORA-00942.
-- case: ide_introspection_stub_views_all_resolve_empty
SELECT (SELECT COUNT(*) FROM sys.all_tab_partitions)
     + (SELECT COUNT(*) FROM sys.all_part_tables)
     + (SELECT COUNT(*) FROM sys.all_part_key_columns)
     + (SELECT COUNT(*) FROM sys.all_tab_subpartitions)
     + (SELECT COUNT(*) FROM sys.all_ind_partitions)
     + (SELECT COUNT(*) FROM sys.all_lobs)
     + (SELECT COUNT(*) FROM sys.all_nested_tables)
     + (SELECT COUNT(*) FROM sys.all_trigger_cols)
     + (SELECT COUNT(*) FROM sys.all_type_attrs)
     + (SELECT COUNT(*) FROM sys.all_coll_types)
     + (SELECT COUNT(*) FROM sys.all_tab_privs)
     + (SELECT COUNT(*) FROM sys.all_col_privs)
     + (SELECT COUNT(*) FROM sys.all_role_privs)
     + (SELECT COUNT(*) FROM sys.all_directories)
     + (SELECT COUNT(*) FROM sys.all_java_classes)
     + (SELECT COUNT(*) FROM sys.all_clusters)
     + (SELECT COUNT(*) FROM sys.all_editioning_views)
     + (SELECT COUNT(*) FROM sys.all_xml_schemas)
     + (SELECT COUNT(*) FROM sys.all_scheduler_programs)
     + (SELECT COUNT(*) FROM sys.all_queue_tables)
     + (SELECT COUNT(*) FROM sys.all_tab_col_statistics)
     + (SELECT COUNT(*) FROM sys.all_registered_mviews)
     + (SELECT COUNT(*) FROM sys.all_identifiers)
     + (SELECT COUNT(*) FROM sys.all_all_tables)
     + (SELECT COUNT(*) FROM sys.all_external_tables)
     + (SELECT COUNT(*) FROM sys.all_tab_identity_cols WHERE owner = 'NO_SUCH_OWNER')
     AS total FROM dual
-- expect:
0
-- end

-- case: product_component_version_has_a_row
SELECT status FROM sys.product_component_version
-- expect:
Production
-- end

-- case: global_name_view_resolves
SELECT COUNT(*) FROM global_name
-- expect:
1
-- end
