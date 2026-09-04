# Set operations. Oracle spells EXCEPT as MINUS; DbSaci rewrites it.

-- case: union_deduplicates
SELECT team_id FROM people WHERE team_id IS NOT NULL UNION SELECT id FROM teams ORDER BY 1
-- expect:
1
2
3
-- end

-- case: union_all_keeps_duplicates
SELECT COUNT(*) FROM (SELECT team_id FROM people WHERE team_id IS NOT NULL UNION ALL SELECT id FROM teams) q
-- expect:
6
-- end

-- case: intersect
SELECT id FROM teams INTERSECT SELECT team_id FROM people ORDER BY 1
-- expect:
1
2
-- end

-- case: except
SELECT id FROM teams EXCEPT SELECT team_id FROM people WHERE team_id IS NOT NULL ORDER BY 1
-- expect:
3
-- end

-- case: minus_is_rewritten_to_except
SELECT id FROM teams MINUS SELECT team_id FROM people WHERE team_id IS NOT NULL ORDER BY 1
-- expect:
3
-- end

-- case: union_of_literals
SELECT 1 FROM DUAL UNION SELECT 2 FROM DUAL ORDER BY 1
-- expect:
1
2
-- end
