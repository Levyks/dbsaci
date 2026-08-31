# INSERT / UPDATE / DELETE: row counts, in-session visibility, and independent
# confirmation via `-- verify:` (a second PostgreSQL connection that only sees
# committed state). Every case is rolled back afterwards unless it COMMITs and
# cleans up after itself.

-- case: insert_reports_one_row
INSERT INTO people (id, name, team_id) VALUES (10, 'Inserted', 2)
-- rowcount: 1
-- end

-- case: inserted_row_is_visible_in_session
-- setup: INSERT INTO people (id, name, team_id) VALUES (11, 'Visible', 2)
SELECT name FROM people WHERE id = 11
-- expect:
Visible
-- end

-- case: update_reports_affected_rows
UPDATE people SET name = 'Renamed' WHERE team_id = 1
-- rowcount: 2
-- end

-- case: update_changes_value_in_session
-- setup: UPDATE people SET name = 'Marge' WHERE id = 2
SELECT name FROM people WHERE id = 2
-- expect:
Marge
-- end

-- case: delete_reports_affected_rows
DELETE FROM people WHERE team_id = 1
-- rowcount: 2
-- end

-- case: delete_removes_row_in_session
-- setup: DELETE FROM people WHERE id = 3
SELECT COUNT(*) FROM people WHERE id = 3
-- expect:
0
-- end

-- case: update_no_match_reports_zero
UPDATE people SET name = 'x' WHERE id = 999
-- rowcount: 0
-- end

-- case: unique_violation_surfaces_as_error
INSERT INTO people (id, name, team_id) VALUES (1, 'Duplicate', 1)
-- error: ORA-00001
-- end

-- case: foreign_key_violation_surfaces_as_error
INSERT INTO people (id, name, team_id) VALUES (12, 'BadTeam', 99)
-- error: ORA-02291
-- end

-- case: not_null_violation_surfaces_as_error
INSERT INTO people (id, team_id) VALUES (13, 1)
-- error: ORA-01400
-- end

-- case: session_recovers_after_statement_error
-- setup: INSERT INTO people (id, name, team_id) VALUES (14, 'Survivor', 2)
SELECT name FROM people WHERE id = 14
-- expect:
Survivor
-- end

-- case: uncommitted_insert_not_visible_to_other_connection
-- setup: INSERT INTO people (id, name, team_id) VALUES (20, 'Uncommitted', 2)
SELECT name FROM people WHERE id = 20
-- verify: SELECT count(*) FROM people WHERE id = 20 => 0
-- expect:
Uncommitted
-- end

-- case: commit_makes_insert_visible_to_other_connection
-- setup: INSERT INTO people (id, name, team_id) VALUES (21, 'Committed', 2)
-- setup: COMMIT
-- teardown: DELETE FROM people WHERE id = 21
SELECT name FROM people WHERE id = 21
-- verify: SELECT count(*) FROM people WHERE id = 21 => 1
-- expect:
Committed
-- end

-- case: rollback_discards_insert
-- setup: INSERT INTO people (id, name, team_id) VALUES (22, 'Rolled', 2)
-- setup: ROLLBACK
SELECT COUNT(*) FROM people WHERE id = 22
-- expect:
0
-- end

-- case: insert_select_copies_rows
-- setup: CREATE TABLE people_copy (id NUMBER, name VARCHAR2(50))
-- setup: INSERT INTO people_copy (id, name) SELECT id, name FROM people WHERE team_id = 1
SELECT name FROM people_copy ORDER BY id
-- expect:
Ada
Grace
-- end

-- case: insert_multi_row_values
-- setup: CREATE TABLE mrv (id NUMBER, v VARCHAR2(10))
INSERT INTO mrv (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')
-- rowcount: 3
-- end

-- case: update_from_subquery
-- setup: CREATE TABLE upd_demo (id NUMBER, team_name VARCHAR2(30))
-- setup: INSERT INTO upd_demo (id) SELECT id FROM people
-- setup: UPDATE upd_demo d SET team_name = (SELECT t.name FROM teams t JOIN people p ON p.team_id = t.id WHERE p.id = d.id)
SELECT team_name FROM upd_demo WHERE id = 1
-- expect:
Engineering
-- end

-- case: correlated_update_only_touches_matches
-- setup: UPDATE people p SET name = name WHERE EXISTS (SELECT 1 FROM teams t WHERE t.id = p.team_id AND t.name = 'Sales')
UPDATE people p SET name = 'Torvalds' WHERE EXISTS (SELECT 1 FROM teams t WHERE t.id = p.team_id AND t.name = 'Sales')
-- rowcount: 1
-- end

-- case: delete_with_subquery
-- setup: CREATE TABLE del_demo AS SELECT * FROM people
-- setup: DELETE FROM del_demo WHERE team_id IN (SELECT id FROM teams WHERE name = 'Engineering')
SELECT COUNT(*) FROM del_demo
-- expect:
2
-- end

# RETURNING is accepted (the returned rows are not surfaced as OUT binds).
-- case: update_with_returning_clause
-- bind: int 2
UPDATE people SET name = 'Hopper' WHERE id = :1 RETURNING name
-- rowcount: 1
-- end

-- case: insert_with_returning_clause
-- setup: CREATE TABLE ret_demo (id NUMBER GENERATED ALWAYS AS IDENTITY, v VARCHAR2(10))
INSERT INTO ret_demo (v) VALUES ('x') RETURNING id
-- rowcount: 1
-- end

-- case: truncate_empties_table
-- setup: CREATE TABLE trunc_demo AS SELECT * FROM people
-- setup: TRUNCATE TABLE trunc_demo
SELECT COUNT(*) FROM trunc_demo
-- expect:
0
-- end

-- case: update_all_rows
UPDATE people SET name = UPPER(name)
-- rowcount: 4
-- end

-- case: delete_all_rows_reports_count
-- setup: CREATE TABLE del_all AS SELECT * FROM people
DELETE FROM del_all
-- rowcount: 4
-- end

-- case: insert_with_default_and_explicit_mix
-- setup: CREATE TABLE mix_demo (id NUMBER, created DATE DEFAULT SYSDATE, note VARCHAR2(20) DEFAULT 'auto')
-- setup: INSERT INTO mix_demo (id, note) VALUES (1, 'explicit')
-- setup: INSERT INTO mix_demo (id) VALUES (2)
SELECT id, note FROM mix_demo ORDER BY id
-- expect:
1 | explicit
2 | auto
-- end
