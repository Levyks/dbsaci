# CREATE TRIGGER. Oracle row triggers with :NEW / :OLD lower to a PostgreSQL
# trigger function plus CREATE OR REPLACE TRIGGER. Simple bodies (column
# assignment, INSERT into an audit table, RAISE) are covered; full PL/SQL
# trigger bodies are not.

-- fixture: CREATE TABLE IF NOT EXISTS trg_people (id INT, name TEXT, team_id INT)
-- fixture: TRUNCATE trg_people
-- fixture: CREATE TABLE IF NOT EXISTS trg_audit (op TEXT)
-- fixture: TRUNCATE trg_audit

-- case: before_insert_row_trigger_modifies_new
-- setup: CREATE OR REPLACE TRIGGER trg_upper BEFORE INSERT ON trg_people FOR EACH ROW BEGIN :NEW.name := UPPER(:NEW.name); END;
-- setup: INSERT INTO trg_people (id, name) VALUES (1, 'alice')
-- teardown: DROP TRIGGER IF EXISTS trg_upper ON trg_people
-- teardown: DELETE FROM trg_people
SELECT name FROM trg_people WHERE id = 1
-- expect:
ALICE
-- end

-- case: after_insert_trigger_writes_audit_row
-- setup: CREATE OR REPLACE TRIGGER trg_aud AFTER INSERT ON trg_people FOR EACH ROW BEGIN INSERT INTO trg_audit (op) VALUES ('inserted'); END;
-- setup: INSERT INTO trg_people (id, name) VALUES (2, 'bob')
-- teardown: DROP TRIGGER IF EXISTS trg_aud ON trg_people
-- teardown: DELETE FROM trg_people
-- teardown: DELETE FROM trg_audit
SELECT op FROM trg_audit
-- expect:
inserted
-- end

-- case: before_update_trigger_with_when_clause
-- setup: INSERT INTO trg_people (id, name, team_id) VALUES (3, 'carl', 7)
-- setup: CREATE OR REPLACE TRIGGER trg_guard BEFORE UPDATE ON trg_people FOR EACH ROW WHEN (OLD.team_id IS NOT NULL) BEGIN :NEW.team_id := OLD.team_id; END;
-- setup: UPDATE trg_people SET team_id = 99 WHERE id = 3
-- teardown: DROP TRIGGER IF EXISTS trg_guard ON trg_people
-- teardown: DELETE FROM trg_people
SELECT team_id FROM trg_people WHERE id = 3
-- expect:
7
-- end

-- case: raise_application_error_in_trigger_blocks_dml
-- setup: CREATE OR REPLACE TRIGGER trg_no_del BEFORE DELETE ON trg_people FOR EACH ROW BEGIN RAISE_APPLICATION_ERROR(-20001, 'deletes disabled'); END;
-- setup: INSERT INTO trg_people (id, name) VALUES (4, 'dora')
-- teardown: DROP TRIGGER IF EXISTS trg_no_del ON trg_people
-- teardown: DELETE FROM trg_people
DELETE FROM trg_people WHERE id = 4
-- error: deletes disabled
-- end

-- case: trigger_referencing_clause_custom_alias
-- setup: CREATE OR REPLACE TRIGGER trg_ref BEFORE INSERT ON trg_people REFERENCING NEW AS n FOR EACH ROW BEGIN :n.name := UPPER(:n.name); END;
-- setup: INSERT INTO trg_people (id, name) VALUES (5, 'eve')
-- teardown: DROP TRIGGER IF EXISTS trg_ref ON trg_people
-- teardown: DELETE FROM trg_people
SELECT name FROM trg_people WHERE id = 5
-- expect:
EVE
-- end

-- case: trigger_body_with_if_statement
-- setup: CREATE OR REPLACE TRIGGER trg_if BEFORE INSERT ON trg_people FOR EACH ROW BEGIN IF :NEW.team_id IS NULL THEN :NEW.team_id := 0; END IF; END;
-- setup: INSERT INTO trg_people (id, name) VALUES (6, 'fin')
-- teardown: DROP TRIGGER IF EXISTS trg_if ON trg_people
-- teardown: DELETE FROM trg_people
SELECT team_id FROM trg_people WHERE id = 6
-- expect:
0
-- end

-- case: instead_of_trigger_on_view_redirects_insert
-- setup: CREATE TABLE trg_base (id INT, name TEXT)
-- setup: CREATE VIEW trg_v AS SELECT id, name FROM trg_base
-- setup: CREATE OR REPLACE TRIGGER trg_io INSTEAD OF INSERT ON trg_v FOR EACH ROW BEGIN INSERT INTO trg_base (id, name) VALUES (:NEW.id, UPPER(:NEW.name)); END;
-- setup: INSERT INTO trg_v (id, name) VALUES (1, 'ada')
-- teardown: DROP VIEW IF EXISTS trg_v
-- teardown: DROP TABLE IF EXISTS trg_base
SELECT name FROM trg_base WHERE id = 1
-- expect:
ADA
-- end

-- case: before_insert_trigger_multi_event
-- setup: CREATE OR REPLACE TRIGGER trg_multi BEFORE INSERT OR UPDATE ON trg_people FOR EACH ROW BEGIN :NEW.name := TRIM(:NEW.name); END;
-- setup: INSERT INTO trg_people (id, name) VALUES (10, '  spaced  ')
-- teardown: DROP TRIGGER IF EXISTS trg_multi ON trg_people
-- teardown: DELETE FROM trg_people
SELECT name FROM trg_people WHERE id = 10
-- expect:
spaced
-- end
