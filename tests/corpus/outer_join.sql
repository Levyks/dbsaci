# Legacy Oracle (+) outer-join syntax. Still pervasive in pre-2010 application
# SQL and in hand-written reports. DbSaci currently handles only a single
# two-table equality (+) predicate, so the multi-predicate / multi-table shapes
# that real code uses are the interesting cases here.

-- fixture: CREATE TABLE IF NOT EXISTS oj_dept (id INT PRIMARY KEY, name TEXT, region TEXT)
-- fixture: CREATE TABLE IF NOT EXISTS oj_emp (id INT PRIMARY KEY, name TEXT, dept_id INT, region TEXT)
-- fixture: TRUNCATE oj_emp
-- fixture: TRUNCATE oj_dept
-- fixture: INSERT INTO oj_dept VALUES (1,'Eng','US'),(2,'Sales','EU'),(3,'Ops','US')
-- fixture: INSERT INTO oj_emp VALUES (1,'Ada',1,'US'),(2,'Grace',1,'US'),(3,'Linus',2,'EU'),(4,'Nobody',NULL,'US')

-- case: single_predicate_left
SELECT e.name, d.name FROM oj_emp e, oj_dept d WHERE e.dept_id = d.id (+) ORDER BY e.id
-- expect:
Ada | Eng
Grace | Eng
Linus | Sales
Nobody | NULL
-- end

-- case: single_predicate_with_filter
SELECT e.name FROM oj_emp e, oj_dept d WHERE e.dept_id = d.id (+) AND e.id > 1 ORDER BY e.id
-- expect:
Grace
Linus
Nobody
-- end

-- case: marker_on_left_is_right_join
SELECT d.name, e.name FROM oj_emp e, oj_dept d WHERE e.dept_id (+) = d.id ORDER BY d.id
-- expect:
Eng | Ada
Eng | Grace
Sales | Linus
Ops | NULL
-- end

-- case: two_predicate_outer_join
SELECT e.name, d.name FROM oj_emp e, oj_dept d
WHERE e.dept_id = d.id (+) AND e.region = d.region (+)
ORDER BY e.id
-- expect:
Ada | Eng
Grace | Eng
Linus | Sales
Nobody | NULL
-- end

-- case: outer_join_with_constant_predicate
SELECT e.name, d.name FROM oj_emp e, oj_dept d
WHERE e.dept_id = d.id (+) AND d.region (+) = 'US'
ORDER BY e.id
-- expect:
Ada | Eng
Grace | Eng
Linus | NULL
Nobody | NULL
-- end

-- case: three_table_mixed_joins
SELECT e.name, d.name, m.name
FROM oj_emp e, oj_dept d, oj_emp m
WHERE e.dept_id = d.id (+) AND d.id = m.dept_id (+) AND e.id = 1
-- expect:
Ada | Eng | Ada
Ada | Eng | Grace
-- end

# Both (+) predicates are join conditions, so unmatched left rows (Nobody) are
# still kept.
-- case: outer_join_in_predicate
SELECT e.name, d.name FROM oj_emp e, oj_dept d
WHERE e.dept_id = d.id (+) AND d.name (+) IN ('Eng', 'Sales')
ORDER BY e.id
-- expect:
Ada | Eng
Grace | Eng
Linus | Sales
Nobody | NULL
-- end

-- case: count_with_outer_join
SELECT COUNT(*) FROM oj_emp e, oj_dept d WHERE e.dept_id = d.id (+)
-- expect:
4
-- end

# --- Probes of the text-based legacy-(+) rewriter (translate.rs
# normalize_legacy_outer_join): it splits on the literal strings " FROM " /
# " WHERE " and a single top-level comma, so formatting variations that a
# passing case doesn't have should trip it.

-- case: single_predicate_but_lowercase_keywords
select e.name, d.name from oj_emp e, oj_dept d where e.dept_id = d.id (+) order by e.id
-- expect:
Ada | Eng
Grace | Eng
Linus | Sales
Nobody | NULL
-- end

-- case: single_predicate_with_newlines
SELECT e.name, d.name
FROM oj_emp e, oj_dept d
WHERE e.dept_id = d.id (+)
ORDER BY e.id
-- expect:
Ada | Eng
Grace | Eng
Linus | Sales
Nobody | NULL
-- end

-- case: single_predicate_extra_whitespace
SELECT e.name FROM oj_emp e ,  oj_dept d WHERE e.dept_id  =  d.id  (+)  ORDER BY e.id
-- expect:
Ada
Grace
Linus
Nobody
-- end

-- case: outer_join_with_third_non_joined_table
SELECT e.name, d.name FROM oj_emp e, oj_dept d, oj_dept x
WHERE e.dept_id = d.id (+) AND x.id = 1 AND e.id = 1
-- expect:
Ada | Eng
-- end

-- case: subquery_in_from_beside_outer_join
SELECT e.name, d.name FROM (SELECT * FROM oj_emp WHERE id <= 2) e, oj_dept d
WHERE e.dept_id = d.id (+) ORDER BY e.id
-- expect:
Ada | Eng
Grace | Eng
-- end

-- case: string_literal_containing_from_keyword
SELECT e.name, 'hired from agency' AS note FROM oj_emp e, oj_dept d
WHERE e.dept_id = d.id (+) AND e.id = 1
-- expect:
Ada | hired from agency
-- end
