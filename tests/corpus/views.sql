# Views and CREATE TABLE AS SELECT: the SELECT body must be run through the SQL
# translator, not passed to PostgreSQL verbatim.

-- fixture: CREATE TABLE IF NOT EXISTS v_dept (id INT PRIMARY KEY, name TEXT)
-- fixture: CREATE TABLE IF NOT EXISTS v_emp (id INT PRIMARY KEY, name TEXT, dept_id INT)
-- fixture: TRUNCATE v_emp
-- fixture: TRUNCATE v_dept
-- fixture: INSERT INTO v_dept VALUES (1,'Eng'),(2,'Sales')
-- fixture: INSERT INTO v_emp VALUES (1,'Ada',1),(2,'Grace',1),(3,'Nobody',NULL)

-- case: plain_view
-- setup: CREATE VIEW v_plain AS SELECT name FROM v_emp WHERE dept_id = 1
SELECT name FROM v_plain ORDER BY name
-- expect:
Ada
Grace
-- end

-- case: view_over_dual
-- setup: CREATE VIEW v_dual AS SELECT 42 AS answer FROM DUAL
SELECT answer FROM v_dual
-- expect:
42
-- end

-- case: view_over_legacy_outer_join
-- setup: CREATE VIEW v_oj AS SELECT e.name AS ename, d.name AS dname FROM v_emp e, v_dept d WHERE e.dept_id = d.id (+)
SELECT ename, dname FROM v_oj ORDER BY ename
-- expect:
Ada | Eng
Grace | Eng
Nobody | NULL
-- end

-- case: view_over_rownum
-- setup: CREATE VIEW v_top AS SELECT name FROM v_emp WHERE ROWNUM <= 2
SELECT count(*) FROM v_top
-- expect:
2
-- end

-- case: view_over_decode
-- setup: CREATE VIEW v_decode AS SELECT name, DECODE(dept_id, 1, 'Eng', 'Other') AS d FROM v_emp
SELECT name, d FROM v_decode ORDER BY name
-- expect:
Ada | Eng
Grace | Eng
Nobody | Other
-- end

-- case: create_or_replace_view
-- setup: CREATE OR REPLACE VIEW v_repl AS SELECT 1 AS n FROM DUAL
-- setup: CREATE OR REPLACE VIEW v_repl AS SELECT 2 AS n FROM DUAL
SELECT n FROM v_repl
-- expect:
2
-- end

-- case: ctas_over_oracle_sql
-- setup: CREATE TABLE v_ctas AS SELECT e.name AS ename, NVL(TO_CHAR(e.dept_id), 'none') AS dep FROM v_emp e
SELECT ename, dep FROM v_ctas ORDER BY ename
-- expect:
Ada | 1
Grace | 1
Nobody | none
-- end

-- case: view_with_column_list
-- setup: CREATE VIEW v_cols (who, dept) AS SELECT name, dept_id FROM v_emp WHERE dept_id IS NOT NULL
SELECT who, dept FROM v_cols ORDER BY who
-- expect:
Ada | 1
Grace | 1
-- end
