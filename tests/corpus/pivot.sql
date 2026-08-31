# PIVOT / UNPIVOT. Oracle reporting queries lean on these heavily; PostgreSQL
# has no native syntax, so PgSaci lowers them to conditional aggregation and a
# LATERAL VALUES unnest respectively.

-- fixture: CREATE TABLE IF NOT EXISTS sales_q (region VARCHAR(10), quarter VARCHAR(2), amt INT)
-- fixture: TRUNCATE sales_q
-- fixture: INSERT INTO sales_q VALUES ('East','Q1',10),('East','Q2',20),('West','Q1',5),('West','Q2',7)
-- fixture: CREATE TABLE IF NOT EXISTS wide_scores (student VARCHAR(10), math INT, sci INT)
-- fixture: TRUNCATE wide_scores
-- fixture: INSERT INTO wide_scores VALUES ('Ann',90,85),('Bo',70,95)

-- case: pivot_sum_by_quarter
SELECT * FROM (SELECT region, quarter, amt FROM sales_q) src
PIVOT (SUM(amt) FOR quarter IN ('Q1' AS q1, 'Q2' AS q2))
ORDER BY region
-- expect:
East | 10 | 20
West | 5 | 7
-- end

-- case: pivot_without_correlation_name
SELECT * FROM (SELECT quarter, amt FROM sales_q)
PIVOT (SUM(amt) FOR quarter IN ('Q1' AS q1, 'Q2' AS q2))
-- expect:
15 | 27
-- end

-- case: unpivot_columns_to_rows
SELECT student, subject, score
FROM wide_scores
UNPIVOT (score FOR subject IN (math AS 'MATH', sci AS 'SCI'))
ORDER BY student, subject
-- expect:
Ann | MATH | 90
Ann | SCI | 85
Bo | MATH | 70
Bo | SCI | 95
-- end

-- case: unpivot_excludes_nulls_by_default
-- setup: INSERT INTO wide_scores VALUES ('Zoe', NULL, 42)
SELECT student, subject, score
FROM wide_scores
UNPIVOT (score FOR subject IN (math AS 'MATH', sci AS 'SCI'))
WHERE student = 'Zoe'
ORDER BY subject
-- expect:
Zoe | SCI | 42
-- end

-- case: unpivot_include_nulls_keeps_them
-- setup: INSERT INTO wide_scores VALUES ('Zoe', NULL, 42)
SELECT student, subject, score
FROM wide_scores
UNPIVOT INCLUDE NULLS (score FOR subject IN (math AS 'MATH', sci AS 'SCI'))
WHERE student = 'Zoe'
ORDER BY subject
-- expect:
Zoe | MATH | NULL
Zoe | SCI | 42
-- end
