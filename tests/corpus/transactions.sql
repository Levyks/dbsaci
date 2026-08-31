# Transaction control. Oracle sessions are transactional by default; apps rely
# on explicit COMMIT/ROLLBACK and on SAVEPOINT for partial rollback.

-- case: rollback_discards_insert
-- setup: INSERT INTO people (id, name, team_id) VALUES (60, 'Temp', 2)
-- setup: ROLLBACK
SELECT COUNT(*) FROM people WHERE id = 60
-- expect:
0
-- end

-- case: commit_persists_insert
-- setup: INSERT INTO people (id, name, team_id) VALUES (61, 'Kept', 2)
-- setup: COMMIT
-- teardown: DELETE FROM people WHERE id = 61
SELECT name FROM people WHERE id = 61
-- verify: SELECT count(*) FROM people WHERE id = 61 => 1
-- expect:
Kept
-- end

-- case: uncommitted_change_invisible_to_other_session
-- setup: INSERT INTO people (id, name, team_id) VALUES (62, 'Pending', 2)
SELECT name FROM people WHERE id = 62
-- verify: SELECT count(*) FROM people WHERE id = 62 => 0
-- expect:
Pending
-- end

-- case: savepoint_then_rollback_to
-- setup: INSERT INTO people (id, name, team_id) VALUES (63, 'A', 2)
-- setup: SAVEPOINT sp1
-- setup: INSERT INTO people (id, name, team_id) VALUES (64, 'B', 2)
-- setup: ROLLBACK TO sp1
SELECT id FROM people WHERE id IN (63, 64) ORDER BY id
-- expect:
63
-- end

-- case: ddl_implicitly_commits
-- setup: INSERT INTO people (id, name, team_id) VALUES (65, 'BeforeDDL', 2)
-- setup: CREATE TABLE ddl_commit_probe (x NUMBER)
-- setup: ROLLBACK
-- teardown: DROP TABLE IF EXISTS ddl_commit_probe
-- teardown: DELETE FROM people WHERE id = 65
SELECT name FROM people WHERE id = 65
-- expect:
BeforeDDL
-- end

-- case: statement_error_does_not_roll_back_prior_work
-- setup: INSERT INTO people (id, name, team_id) VALUES (66, 'Good', 2)
-- setup?: INSERT INTO people (id, name, team_id) VALUES (1, 'DupFails', 1)
SELECT name FROM people WHERE id = 66
-- expect:
Good
-- end

-- case: set_transaction_read_only
SET TRANSACTION READ ONLY
-- ok
-- end

-- case: set_transaction_isolation_read_committed
SET TRANSACTION ISOLATION LEVEL READ COMMITTED
-- ok
-- end

-- case: select_for_update_locks_row
SELECT name FROM people WHERE id = 1 FOR UPDATE
-- expect:
Ada
-- end

-- case: select_for_update_of_column_drops_column_list
SELECT name FROM people WHERE id = 2 FOR UPDATE OF name NOWAIT
-- expect:
Grace
-- end

-- case: select_for_update_wait_n_is_accepted
SELECT name FROM people WHERE id = 3 FOR UPDATE WAIT 3
-- expect:
Linus
-- end

-- case: select_for_update_skip_locked
SELECT name FROM people WHERE id = 4 FOR UPDATE SKIP LOCKED
-- expect:
Margaret
-- end
