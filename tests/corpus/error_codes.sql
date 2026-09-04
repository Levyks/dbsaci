# Error-code mapping. Applications branch on ORA-nnnnn (dup key, FK, no rows,
# invalid number...). If DbSaci returns the wrong code or a generic one, retry
# and upsert logic in the app misbehaves.

-- case: unique_constraint_violation
INSERT INTO people (id, name, team_id) VALUES (1, 'Dup', 1)
-- error: ORA-00001
-- end

-- case: foreign_key_violation
INSERT INTO people (id, name, team_id) VALUES (99, 'NoTeam', 42)
-- error: ORA-02291
-- end

-- case: not_null_violation
INSERT INTO people (id, name, team_id) VALUES (98, NULL, 1)
-- error: ORA-01400
-- end

-- case: table_does_not_exist
SELECT * FROM no_such_table_here
-- error: ORA-00942
-- end

-- case: invalid_identifier
SELECT no_such_column FROM people
-- error: ORA-00904
-- end

-- case: invalid_number_conversion
-- skip: mariadb (MariaDB returns NULL not ORA-01722 for a bad numeric cast in SELECT)
SELECT TO_NUMBER('not a number') FROM DUAL
-- error: ORA-01722
-- end

-- case: value_too_large_for_column
-- setup: CREATE TABLE tl (c VARCHAR2(3))
INSERT INTO tl (c) VALUES ('abcd')
-- error: ORA-12899
-- end

-- case: divisor_is_zero
-- skip: mariadb (MariaDB returns NULL not ORA-01476 for division by zero in SELECT)
SELECT 5 / 0 FROM DUAL
-- error: ORA-01476
-- end

-- case: check_constraint_violation
-- setup: CREATE TABLE ck (n NUMBER CHECK (n > 0))
INSERT INTO ck (n) VALUES (-1)
-- error: ORA-02290
-- end

-- case: subquery_returns_too_many_rows
SELECT name FROM people WHERE id = (SELECT id FROM people)
-- error: ORA-01427
-- end

-- case: not_enough_values
INSERT INTO people (id, name, team_id) VALUES (50, 'Short')
-- error: ORA-00947
-- end

-- case: duplicate_object_name
-- setup: CREATE TABLE dup_obj (id NUMBER)
CREATE TABLE dup_obj (id NUMBER)
-- error: ORA-00955
-- end

-- case: drop_missing_object
DROP TABLE no_such_table_xyz
-- error: ORA-00942
-- end

-- case: value_larger_than_precision
-- setup: CREATE TABLE prec_demo (n NUMBER(2))
INSERT INTO prec_demo (n) VALUES (12345)
-- error: ORA-01438
-- end

-- case: invalid_datetime_conversion
-- skip: mariadb (MariaDB returns NULL not ORA-01858 for a bad datetime in SELECT)
SELECT CAST('not a date' AS DATE) FROM DUAL
-- error: ORA-01858
-- end

-- case: error_message_has_no_sqlstate_prefix
INSERT INTO people (id, name, team_id) VALUES (1, 'Dup', 1)
-- error: ORA-00001 ~ 23505
-- end

-- case: statement_timeout_is_user_cancel
-- skip: mariadb (no statement-timeout to ORA-01013 mapping)
SELECT pg_sleep(3) FROM DUAL
-- error: ORA-01013
-- end

-- case: statement_timeout_recovers_session
SELECT 42 FROM DUAL
-- expect:
42
-- end

-- case: ambiguous_column_reference
SELECT id FROM people, teams
-- error: ORA-00918
-- end

-- case: column_not_a_group_by_expression
SELECT team_id, name FROM people GROUP BY team_id
-- error: ORA-00979
-- end

-- case: invalid_regular_expression
SELECT REGEXP_SUBSTR('abc', '(') FROM DUAL
-- error: ORA-12726
-- end
