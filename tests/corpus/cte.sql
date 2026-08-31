# Common table expressions, including the recursive form Oracle apps use as the
# ANSI replacement for CONNECT BY.

-- case: simple_with
WITH t AS (SELECT id, name FROM people WHERE team_id = 1)
SELECT name FROM t ORDER BY id
-- expect:
Ada
Grace
-- end

-- case: with_multiple_ctes
WITH eng AS (SELECT id FROM teams WHERE name = 'Engineering'),
     members AS (SELECT name FROM people WHERE team_id IN (SELECT id FROM eng))
SELECT name FROM members ORDER BY name
-- expect:
Ada
Grace
-- end

-- case: with_aggregate_then_join
WITH counts AS (SELECT team_id, COUNT(*) c FROM people GROUP BY team_id)
SELECT t.name, c.c FROM teams t JOIN counts c ON c.team_id = t.id ORDER BY t.name
-- expect:
Engineering | 2
Sales | 1
-- end

-- case: recursive_with_counter
WITH nums (n) AS (
  SELECT 1 FROM DUAL
  UNION ALL
  SELECT n + 1 FROM nums WHERE n < 5
)
SELECT n FROM nums ORDER BY n
-- expect:
1
2
3
4
5
-- end

# Oracle allows a self-referencing CTE without the RECURSIVE keyword.
-- case: recursive_with_hierarchy
-- setup: CREATE TABLE cte_tree (id INT PRIMARY KEY, parent INT)
-- setup: INSERT INTO cte_tree VALUES (1, NULL), (2, 1), (3, 2), (4, 2)
WITH walk (id, depth) AS (
  SELECT id, 1 FROM cte_tree WHERE parent IS NULL
  UNION ALL
  SELECT c.id, w.depth + 1 FROM cte_tree c JOIN walk w ON c.parent = w.id
)
SELECT id, depth FROM walk ORDER BY id
-- expect:
1 | 1
2 | 2
3 | 3
4 | 3
-- end

-- case: with_referenced_twice
WITH t AS (SELECT id FROM people)
SELECT (SELECT COUNT(*) FROM t) + (SELECT MAX(id) FROM t) FROM DUAL
-- expect:
8
-- end
