# INSERT ALL / INSERT FIRST: Oracle's multi-table and conditional inserts,
# common in data-warehouse load routines.

-- fixture: CREATE TABLE IF NOT EXISTS mti_a (id INT, v TEXT)
-- fixture: CREATE TABLE IF NOT EXISTS mti_b (id INT, v TEXT)

-- case: insert_all_unconditional
-- setup: TRUNCATE mti_a
-- setup: INSERT ALL INTO mti_a (id, v) VALUES (1, 'x') INTO mti_a (id, v) VALUES (2, 'y') SELECT 1 FROM DUAL
SELECT id, v FROM mti_a ORDER BY id
-- expect:
1 | x
2 | y
-- end

-- case: insert_all_two_rows_land
-- setup: TRUNCATE mti_a
-- setup: INSERT ALL INTO mti_a (id, v) VALUES (1, 'x') INTO mti_a (id, v) VALUES (2, 'y') SELECT 1 FROM DUAL
SELECT count(*) FROM mti_a
-- expect:
2
-- end

-- case: insert_all_into_two_tables
-- setup: TRUNCATE mti_a
-- setup: TRUNCATE mti_b
-- setup: INSERT ALL INTO mti_a (id, v) VALUES (id, v) INTO mti_b (id, v) VALUES (id, v) SELECT 7 AS id, 'z' AS v FROM DUAL
SELECT (SELECT count(*) FROM mti_a) + (SELECT count(*) FROM mti_b) FROM DUAL
-- expect:
2
-- end

-- case: insert_first_conditional
-- setup: TRUNCATE mti_a
-- setup: TRUNCATE mti_b
-- setup: INSERT FIRST WHEN id <= 2 THEN INTO mti_a (id, v) VALUES (id, v) ELSE INTO mti_b (id, v) VALUES (id, v) SELECT g AS id, 'r' AS v FROM generate_series(1,4) g
SELECT count(*) FROM mti_a
-- expect:
2
-- end

-- case: insert_all_from_select
-- setup: TRUNCATE mti_a
-- setup: INSERT ALL INTO mti_a (id, v) VALUES (id, name) SELECT id, name FROM people WHERE team_id = 1
SELECT count(*) FROM mti_a
-- expect:
2
-- end
