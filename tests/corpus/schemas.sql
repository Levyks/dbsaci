# Schema == user. Every pgSaci user owns a PostgreSQL schema of its own name;
# unqualified DDL/DML lands there, other schemas are reached by qualifying
# (`hr.emp`) or `ALTER SESSION SET CURRENT_SCHEMA`, and `public` is the shared
# fallback on the search_path. The corpus connects as user `corpus`, so its
# schema is CORPUS.

# Fixtures run on a direct PostgreSQL connection (no translation): another
# user's schema, and a shared object in `public`.
-- fixture: CREATE SCHEMA IF NOT EXISTS corpus_hr
-- fixture: CREATE TABLE IF NOT EXISTS corpus_hr.regions (region_id integer PRIMARY KEY, region_name text)
-- fixture: TRUNCATE corpus_hr.regions
-- fixture: INSERT INTO corpus_hr.regions VALUES (1, 'Europe'), (2, 'Americas'), (3, 'Asia')
-- fixture: CREATE TABLE IF NOT EXISTS public.corpus_shared_ref (k text PRIMARY KEY, v text)
-- fixture: TRUNCATE public.corpus_shared_ref
-- fixture: INSERT INTO public.corpus_shared_ref VALUES ('env', 'prod'), ('region', 'eu')

-- case: session_schema_is_the_connected_user
SELECT sys_context('USERENV', 'CURRENT_SCHEMA') FROM dual
-- expect:
CORPUS
-- end

-- case: all_users_lists_the_connected_user
SELECT COUNT(*) FROM sys.all_users WHERE username = 'CORPUS'
-- expect:
1
-- end

-- case: table_created_in_session_is_owned_by_the_user
-- setup: CREATE TABLE schema_probe (id NUMBER)
SELECT owner FROM all_tables WHERE table_name = 'SCHEMA_PROBE'
-- expect:
CORPUS
-- end

-- case: table_created_in_session_is_not_in_public
-- setup: CREATE TABLE schema_probe_pub (id NUMBER)
SELECT COUNT(*) FROM all_tables WHERE table_name = 'SCHEMA_PROBE_PUB' AND owner = 'PUBLIC'
-- expect:
0
-- end

-- case: user_tables_shows_only_the_users_own
-- setup: CREATE TABLE mine_only (id NUMBER)
SELECT COUNT(*) FROM user_tables WHERE table_name = 'MINE_ONLY'
-- expect:
1
-- end

-- case: another_schema_is_visible_in_all_tables
SELECT owner, table_name FROM all_tables WHERE owner = 'CORPUS_HR'
-- expect:
CORPUS_HR | REGIONS
-- end

-- case: cross_schema_qualified_read
SELECT region_name FROM corpus_hr.regions ORDER BY region_id
-- expect:
Europe
Americas
Asia
-- end

-- case: public_is_the_search_path_fallback_unqualified
SELECT v FROM corpus_shared_ref WHERE k = 'env'
-- expect:
prod
-- end

-- case: public_reachable_when_explicitly_qualified
SELECT v FROM public.corpus_shared_ref WHERE k = 'region'
-- expect:
eu
-- end

-- case: alter_session_current_schema_redirects_unqualified_names
-- setup: ALTER SESSION SET CURRENT_SCHEMA = corpus_hr
SELECT COUNT(*) FROM regions
-- expect:
3
-- end

-- case: get_ddl_resolves_in_the_user_schema
-- setup: CREATE TABLE ddl_probe (id NUMBER PRIMARY KEY, label VARCHAR2(20))
SELECT CASE WHEN dbms_metadata.get_ddl('TABLE', 'DDL_PROBE', 'CORPUS') LIKE '%corpus.ddl_probe%'
            THEN 'ok' ELSE 'miss' END FROM dual
-- expect:
ok
-- end
