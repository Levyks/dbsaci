# MERGE: the standard Oracle upsert, ubiquitous in ETL / staging-to-target
# loads. PostgreSQL 15+ has MERGE with compatible syntax, so the gap here is
# mostly routing (DbSaci treats MERGE as neither a query nor known DML).
# `MERGE` is a hard PostgreSQL 15 floor with no portable lowering (the
# rowcount-reporting case in particular cannot be a DO block), so this group is
# skipped on older backends.
# requires-pg: 15
# skip: mariadb (MariaDB has no MERGE statement and no portable lowering)
#
# Cases that need to inspect the result run the MERGE in `-- setup:` and assert
# the resulting table state in the body.

-- fixture: CREATE TABLE IF NOT EXISTS mtgt (id INT PRIMARY KEY, val TEXT)
-- fixture: CREATE TABLE IF NOT EXISTS msrc (id INT PRIMARY KEY, val TEXT)

-- case: merge_update_reports_rowcount
-- setup: TRUNCATE mtgt
-- setup: INSERT INTO mtgt (id, val) VALUES (1, 'old')
MERGE INTO mtgt d USING (SELECT 1 AS id, 'new' AS val FROM DUAL) s ON (d.id = s.id)
WHEN MATCHED THEN UPDATE SET d.val = s.val
-- rowcount: 1
-- end

-- case: merge_updates_the_row
-- setup: TRUNCATE mtgt
-- setup: INSERT INTO mtgt (id, val) VALUES (1, 'old')
-- setup: MERGE INTO mtgt d USING (SELECT 1 AS id, 'new' AS val FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.val = s.val
SELECT val FROM mtgt WHERE id = 1
-- expect:
new
-- end

-- case: merge_inserts_when_not_matched
-- setup: TRUNCATE mtgt
-- setup: MERGE INTO mtgt d USING (SELECT 2 AS id, 'fresh' AS val FROM DUAL) s ON (d.id = s.id) WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)
SELECT val FROM mtgt WHERE id = 2
-- expect:
fresh
-- end

-- case: merge_both_branches
-- setup: TRUNCATE mtgt
-- setup: INSERT INTO mtgt (id, val) VALUES (1, 'old')
-- setup: CREATE TABLE IF NOT EXISTS msrc2 (id INT, val TEXT)
-- setup: TRUNCATE msrc2
-- setup: INSERT INTO msrc2 VALUES (1, 'upd'), (3, 'ins')
-- setup: MERGE INTO mtgt d USING msrc2 s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.val = s.val WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)
SELECT id, val FROM mtgt ORDER BY id
-- expect:
1 | upd
3 | ins
-- end

-- case: merge_matched_delete_clause
-- setup: TRUNCATE mtgt
-- setup: INSERT INTO mtgt (id, val) VALUES (1, 'keep'), (2, 'drop')
-- setup: MERGE INTO mtgt d USING (SELECT 2 AS id FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.val = 'updated' DELETE WHERE d.val = 'updated'
SELECT id FROM mtgt ORDER BY id
-- expect:
1
-- end
