# Aggregation and grouping. Baseline: people ids 1..4, team_ids 1,1,2,NULL.

-- case: count_star
SELECT COUNT(*) FROM people
-- expect:
4
-- end

-- case: count_column_skips_null
SELECT COUNT(team_id) FROM people
-- expect:
3
-- end

-- case: sum
SELECT SUM(id) FROM people
-- expect:
10
-- end

-- case: avg_is_fractional
SELECT AVG(id) FROM people
-- expect:
2.5
-- end

-- case: min_max
SELECT MIN(id), MAX(id) FROM people
-- expect:
1 | 4
-- end

-- case: group_by_counts
SELECT team_id, COUNT(*) FROM people WHERE team_id IS NOT NULL GROUP BY team_id ORDER BY team_id
-- expect:
1 | 2
2 | 1
-- end

-- case: group_by_having
SELECT team_id, COUNT(*) FROM people GROUP BY team_id HAVING COUNT(*) > 1 ORDER BY team_id
-- expect:
1 | 2
-- end

-- case: count_distinct
SELECT COUNT(DISTINCT team_id) FROM people
-- expect:
2
-- end

-- case: aggregate_over_empty_set_is_null
SELECT MAX(id) FROM people WHERE id > 100
-- expect:
NULL
-- end

-- case: count_over_empty_set_is_zero
SELECT COUNT(*) FROM people WHERE id > 100
-- expect:
0
-- end

-- case: group_by_expression
SELECT id > 2, COUNT(*) FROM people GROUP BY id > 2 ORDER BY 1
-- expect:
0 | 2
1 | 2
-- end

-- case: sum_filtered
SELECT SUM(id) FROM people WHERE team_id = 1
-- expect:
3
-- end

-- case: string_agg_ordered
SELECT STRING_AGG(name, ',' ORDER BY id) FROM people WHERE team_id = 1
-- expect:
Ada,Grace
-- end
