# PL/SQL: anonymous blocks, DBMS_OUTPUT, EXECUTE IMMEDIATE, and calling stored
# routines. Migration tools and app frameworks issue these constantly (session
# setup blocks, "SELECT ... INTO", packaged APIs).

-- case: anonymous_block_noop
BEGIN NULL; END;
-- ok
-- end

-- case: anonymous_block_with_dbms_output
BEGIN DBMS_OUTPUT.PUT_LINE('hello'); END;
-- ok
-- end

-- case: block_declares_and_assigns
DECLARE v NUMBER; BEGIN v := 1 + 1; END;
-- ok
-- end

-- case: execute_immediate_ddl
-- setup: BEGIN EXECUTE IMMEDIATE 'CREATE TABLE ei_tbl (id INT)'; END;
SELECT table_name FROM user_tables WHERE table_name = 'ei_tbl'
-- expect:
ei_tbl
-- end

-- case: create_and_call_function
-- setup: CREATE OR REPLACE FUNCTION add_one(p NUMBER) RETURN NUMBER IS BEGIN RETURN p + 1; END;
SELECT add_one(41) FROM DUAL
-- expect:
42
-- end

-- case: create_and_call_procedure
-- setup: CREATE TABLE proc_log (msg TEXT)
-- setup: CREATE OR REPLACE PROCEDURE log_msg(p VARCHAR2) IS BEGIN INSERT INTO proc_log (msg) VALUES (p); END;
-- setup: BEGIN log_msg('called'); END;
SELECT msg FROM proc_log
-- expect:
called
-- end

-- case: block_select_into
DECLARE n NUMBER; BEGIN SELECT COUNT(*) INTO n FROM people; END;
-- ok
-- end

-- case: pragma_autonomous_transaction_block
DECLARE PRAGMA AUTONOMOUS_TRANSACTION; BEGIN NULL; END;
-- ok
-- end

-- case: block_with_exception_handler
BEGIN RAISE_APPLICATION_ERROR(-20001, 'boom'); EXCEPTION WHEN OTHERS THEN NULL; END;
-- ok
-- end

-- case: block_exception_dup_val_on_index
-- setup: CREATE TABLE plq_u (id INT PRIMARY KEY)
-- setup: INSERT INTO plq_u VALUES (1)
-- setup: BEGIN INSERT INTO plq_u VALUES (1); EXCEPTION WHEN DUP_VAL_ON_INDEX THEN INSERT INTO plq_u VALUES (2); END;
-- teardown: DROP TABLE plq_u
SELECT COUNT(*) FROM plq_u
-- expect:
2
-- end

-- case: block_exception_no_data_found
-- setup: CREATE TABLE plq_n (v INT)
-- setup: DECLARE x INT; BEGIN SELECT v INTO x FROM plq_n WHERE v = 999; EXCEPTION WHEN NO_DATA_FOUND THEN INSERT INTO plq_n VALUES (7); END;
-- teardown: DROP TABLE plq_n
SELECT v FROM plq_n
-- expect:
7
-- end

-- case: block_numeric_for_loop
-- setup: CREATE TABLE plq_loop (n INT)
-- setup: BEGIN FOR i IN 1..3 LOOP INSERT INTO plq_loop VALUES (i); END LOOP; END;
-- teardown: DROP TABLE plq_loop
SELECT SUM(n) FROM plq_loop
-- expect:
6
-- end

-- case: function_param_anchored_type
-- setup: CREATE OR REPLACE FUNCTION plq_name(p_id people.id%TYPE) RETURN VARCHAR2 IS r VARCHAR2(100); BEGIN SELECT name INTO r FROM people WHERE id = p_id; RETURN r; END;
SELECT plq_name(1) FROM DUAL
-- expect:
Ada
-- end

-- case: block_rowtype_variable
-- setup: CREATE TABLE plq_rt (id INT, name TEXT)
-- setup: DECLARE r people%ROWTYPE; BEGIN SELECT * INTO r FROM people WHERE id = 1; INSERT INTO plq_rt (id, name) VALUES (r.id, r.name); END;
-- teardown: DROP TABLE plq_rt
SELECT name FROM plq_rt WHERE id = 1
-- expect:
Ada
-- end

-- case: block_explicit_cursor_for_loop
-- setup: CREATE TABLE plq_cur (n INT)
-- setup: DECLARE CURSOR c IS SELECT id FROM people ORDER BY id; BEGIN FOR rec IN c LOOP INSERT INTO plq_cur VALUES (rec.id); END LOOP; END;
-- teardown: DROP TABLE plq_cur
SELECT SUM(n) FROM plq_cur
-- expect:
10
-- end

-- case: block_explicit_cursor_open_fetch_close
-- setup: CREATE TABLE plq_of (v TEXT)
-- setup: DECLARE CURSOR c IS SELECT name FROM people ORDER BY id; nm people.name%TYPE; BEGIN OPEN c; FETCH c INTO nm; CLOSE c; INSERT INTO plq_of VALUES (nm); END;
-- teardown: DROP TABLE plq_of
SELECT v FROM plq_of
-- expect:
Ada
-- end

-- case: block_cursor_where_current_of
-- setup: CREATE TABLE plq_wco (id INT, name TEXT)
-- setup: INSERT INTO plq_wco VALUES (1, 'a'), (2, 'b'), (3, 'c')
-- setup: DECLARE CURSOR c IS SELECT id FROM plq_wco FOR UPDATE; BEGIN FOR rec IN c LOOP UPDATE plq_wco SET name = 'x' WHERE CURRENT OF c; END LOOP; END;
-- teardown: DROP TABLE plq_wco
SELECT DISTINCT name FROM plq_wco
-- expect:
x
-- end

-- case: pragma_exception_init_custom_error
-- setup: CREATE TABLE plq_ei (msg TEXT)
-- setup: DECLARE e_custom EXCEPTION; PRAGMA EXCEPTION_INIT(e_custom, -20055); BEGIN BEGIN RAISE_APPLICATION_ERROR(-20055, 'boom'); EXCEPTION WHEN e_custom THEN INSERT INTO plq_ei VALUES ('caught'); END; END;
-- teardown: DROP TABLE plq_ei
SELECT msg FROM plq_ei
-- expect:
caught
-- end

-- case: block_cursor_for_loop_inline_query
-- setup: CREATE TABLE plq_inl (n INT)
-- setup: BEGIN FOR rec IN (SELECT id FROM people WHERE id <= 3) LOOP INSERT INTO plq_inl VALUES (rec.id); END LOOP; END;
-- teardown: DROP TABLE plq_inl
SELECT SUM(n) FROM plq_inl
-- expect:
6
-- end

-- case: block_while_loop
-- setup: CREATE TABLE plq_wh (n INT)
-- setup: DECLARE i INT := 1; BEGIN WHILE i <= 3 LOOP INSERT INTO plq_wh VALUES (i); i := i + 1; END LOOP; END;
-- teardown: DROP TABLE plq_wh
SELECT SUM(n) FROM plq_wh
-- expect:
6
-- end

-- case: block_loop_exit_when
-- setup: CREATE TABLE plq_ew (n INT)
-- setup: DECLARE i INT := 0; BEGIN LOOP i := i + 1; EXIT WHEN i > 3; INSERT INTO plq_ew VALUES (i); END LOOP; END;
-- teardown: DROP TABLE plq_ew
SELECT SUM(n) FROM plq_ew
-- expect:
6
-- end

-- case: block_case_statement
-- setup: CREATE TABLE plq_cs (label TEXT)
-- setup: DECLARE n INT := 2; BEGIN CASE n WHEN 1 THEN INSERT INTO plq_cs VALUES ('one'); WHEN 2 THEN INSERT INTO plq_cs VALUES ('two'); ELSE INSERT INTO plq_cs VALUES ('other'); END CASE; END;
-- teardown: DROP TABLE plq_cs
SELECT label FROM plq_cs
-- expect:
two
-- end

-- case: block_nested_block_scope
-- setup: CREATE TABLE plq_nb (n INT)
-- setup: DECLARE x INT := 1; BEGIN DECLARE y INT := 10; BEGIN INSERT INTO plq_nb VALUES (x + y); END; END;
-- teardown: DROP TABLE plq_nb
SELECT n FROM plq_nb
-- expect:
11
-- end
