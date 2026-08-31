# Subquery forms: IN, NOT IN, EXISTS, scalar, correlated, derived table.

-- case: in_subquery
SELECT name FROM people WHERE team_id IN (SELECT id FROM teams WHERE name = 'Engineering') ORDER BY id
-- expect:
Ada
Grace
-- end

-- case: not_in_subquery
SELECT name FROM people WHERE team_id NOT IN (SELECT id FROM teams WHERE name = 'Engineering') ORDER BY id
-- expect:
Linus
-- end

-- case: exists_correlated
SELECT p.name FROM people p WHERE EXISTS (SELECT 1 FROM teams t WHERE t.id = p.team_id) ORDER BY p.id
-- expect:
Ada
Grace
Linus
-- end

-- case: not_exists
SELECT p.name FROM people p WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = p.team_id) ORDER BY p.id
-- expect:
Margaret
-- end

-- case: scalar_subquery_in_where
SELECT name FROM people WHERE id = (SELECT MIN(id) FROM people)
-- expect:
Ada
-- end

-- case: scalar_subquery_in_projection
SELECT (SELECT COUNT(*) FROM people) FROM DUAL
-- expect:
4
-- end

-- case: correlated_scalar_projection
SELECT p.name, (SELECT t.name FROM teams t WHERE t.id = p.team_id) FROM people p ORDER BY p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
Margaret | NULL
-- end

-- case: derived_table
SELECT COUNT(*) FROM (SELECT DISTINCT team_id FROM people) q
-- expect:
3
-- end

-- case: derived_table_with_filter
SELECT q.name FROM (SELECT name, id FROM people WHERE team_id = 1) q WHERE q.id > 1
-- expect:
Grace
-- end

-- case: in_subquery_max
SELECT name FROM people WHERE id = (SELECT MAX(id) FROM people)
-- expect:
Margaret
-- end
