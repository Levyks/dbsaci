# CONNECT BY hierarchical queries: org charts, bills of material, category
# trees. Extremely common in Oracle ERP/HR schemas.

-- fixture: CREATE TABLE IF NOT EXISTS emp_tree (id INT PRIMARY KEY, name TEXT, mgr INT)
-- fixture: TRUNCATE emp_tree
-- fixture: INSERT INTO emp_tree VALUES (1,'King',NULL),(2,'Jones',1),(3,'Scott',2),(4,'Adams',3),(5,'Blake',1),(6,'Allen',5)

-- case: level_from_dual
SELECT LEVEL FROM DUAL CONNECT BY LEVEL <= 4
-- expect:
1
2
3
4
-- end

-- case: walk_down_from_root
SELECT name FROM emp_tree START WITH mgr IS NULL CONNECT BY PRIOR id = mgr ORDER BY id
-- expect:
King
Jones
Scott
Adams
Blake
Allen
-- end

-- case: level_column
SELECT name, LEVEL FROM emp_tree START WITH id = 1 CONNECT BY PRIOR id = mgr ORDER BY id
-- expect:
King | 1
Jones | 2
Scott | 3
Adams | 4
Blake | 2
Allen | 3
-- end

-- case: sys_connect_by_path
SELECT SYS_CONNECT_BY_PATH(name, '/') AS path
FROM emp_tree START WITH id = 1 CONNECT BY PRIOR id = mgr
ORDER BY id
-- expect:
/King
/King/Jones
/King/Jones/Scott
/King/Jones/Scott/Adams
/King/Blake
/King/Blake/Allen
-- end

-- case: connect_by_root
SELECT name, CONNECT_BY_ROOT name AS root
FROM emp_tree START WITH id = 2 CONNECT BY PRIOR id = mgr ORDER BY id
-- expect:
Jones | Jones
Scott | Jones
Adams | Jones
-- end

-- case: connect_by_isleaf
SELECT name FROM emp_tree
START WITH id = 1 CONNECT BY PRIOR id = mgr
WHERE CONNECT_BY_ISLEAF = 1 ORDER BY id
-- expect:
Adams
Allen
-- end

-- case: order_siblings_by
SELECT name FROM emp_tree
START WITH mgr IS NULL CONNECT BY PRIOR id = mgr
ORDER SIBLINGS BY name
-- expect:
King
Blake
Allen
Jones
Scott
Adams
-- end

-- case: walk_up_to_root
SELECT name FROM emp_tree START WITH id = 4 CONNECT BY id = PRIOR mgr
-- expect:
Adams
Scott
Jones
King
-- end

-- case: level_times_two_from_dual
SELECT 2 * LEVEL - 1 AS odd FROM DUAL CONNECT BY LEVEL <= 3
-- expect:
1
3
5
-- end

-- case: connect_by_isleaf_in_select_list
SELECT name, CONNECT_BY_ISLEAF AS leaf FROM emp_tree
START WITH id = 1 CONNECT BY PRIOR id = mgr ORDER BY id
-- expect:
King | 0
Jones | 0
Scott | 0
Adams | 1
Blake | 0
Allen | 1
-- end

-- case: connect_by_iscycle_with_nocycle
-- setup: INSERT INTO emp_tree VALUES (91,'Loop A',92),(92,'Loop B',91)
SELECT name, CONNECT_BY_ISCYCLE AS cyc FROM emp_tree
START WITH id = 91 CONNECT BY NOCYCLE PRIOR id = mgr ORDER BY id
-- teardown: DELETE FROM emp_tree WHERE id IN (91, 92)
-- expect:
Loop A | 0
Loop B | 1
-- end
