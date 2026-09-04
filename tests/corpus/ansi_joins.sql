# Join shapes. `people` has (Ada,1) (Grace,1) (Linus,2) (Margaret,NULL);
# `teams` has 1=Engineering 2=Sales 3=Marketing.

-- case: inner_join
SELECT p.name, t.name FROM people p JOIN teams t ON p.team_id = t.id ORDER BY p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
-- end

-- case: inner_join_explicit_keyword
SELECT p.name, t.name FROM people p INNER JOIN teams t ON p.team_id = t.id ORDER BY p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
-- end

-- case: left_join_keeps_unmatched_left
SELECT p.name, t.name FROM people p LEFT JOIN teams t ON p.team_id = t.id ORDER BY p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
Margaret | NULL
-- end

-- case: right_join_keeps_unmatched_right
SELECT p.name, t.name FROM people p RIGHT JOIN teams t ON p.team_id = t.id ORDER BY t.id, p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
NULL | Marketing
-- end

-- case: full_outer_join
SELECT p.name, t.name FROM people p FULL OUTER JOIN teams t ON p.team_id = t.id ORDER BY t.id, p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
NULL | Marketing
Margaret | NULL
-- end

-- case: cross_join_cardinality
SELECT COUNT(*) FROM people p CROSS JOIN teams t
-- expect:
12
-- end

-- case: three_way_join
SELECT a.name FROM people a JOIN people b ON a.team_id = b.team_id JOIN teams t ON t.id = a.team_id WHERE b.name = 'Ada' ORDER BY a.id
-- expect:
Ada
Grace
-- end

-- case: join_with_aggregate
SELECT t.name, COUNT(p.id) FROM teams t LEFT JOIN people p ON p.team_id = t.id GROUP BY t.name ORDER BY t.name
-- expect:
Engineering | 2
Marketing | 0
Sales | 1
-- end

-- case: self_join_pairs
SELECT a.name, b.name FROM people a JOIN people b ON a.team_id = b.team_id AND a.id < b.id ORDER BY a.id
-- expect:
Ada | Grace
-- end

-- case: natural_join
SELECT p.name FROM people p NATURAL JOIN teams t
-- ok
-- end

-- case: join_using
SELECT COUNT(*) FROM people JOIN teams USING (id)
-- expect:
3
-- end

-- case: anti_join_via_not_exists
SELECT name FROM people p WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = p.team_id) ORDER BY p.id
-- expect:
Margaret
-- end

-- case: semi_join_via_in
SELECT name FROM people WHERE team_id IN (SELECT id FROM teams WHERE name IN ('Engineering','Sales')) ORDER BY id
-- expect:
Ada
Grace
Linus
-- end

-- case: lateral_join
SELECT p.name, x.c FROM people p CROSS JOIN LATERAL (SELECT COUNT(*) c FROM people q WHERE q.team_id = p.team_id) x WHERE p.id = 1
-- expect:
Ada | 2
-- end

-- case: three_table_join_with_filter
SELECT a.name FROM people a JOIN teams t ON a.team_id = t.id JOIN people b ON b.team_id = a.team_id WHERE b.name = 'Linus' ORDER BY a.id
-- expect:
Linus
-- end
