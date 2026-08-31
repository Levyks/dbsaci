# Oracle-specific query syntax exercised end to end (translation + execution).

-- case: dual_scalar
SELECT 7 * 6 FROM DUAL
-- expect:
42
-- end

-- case: rownum_limits_result
SELECT name FROM people WHERE ROWNUM <= 2
-- expect:
Ada
Grace
-- end

-- case: rownum_strict_less_than
SELECT name FROM people WHERE ROWNUM < 3
-- expect:
Ada
Grace
-- end

-- case: rownum_one_row
SELECT name FROM people WHERE team_id = 1 AND ROWNUM <= 1
-- expect:
Ada
-- end

# Oracle applies ROWNUM before ORDER BY: the first two rows scanned (id 1,2),
# then sorted.
-- case: rownum_then_order_by_applies_rownum_first
SELECT name FROM people WHERE ROWNUM <= 2 ORDER BY id
-- expect:
Ada
Grace
-- end

-- case: legacy_outer_join_left
SELECT p.name, t.name FROM people p, teams t WHERE p.team_id = t.id (+) ORDER BY p.id
-- expect:
Ada | Engineering
Grace | Engineering
Linus | Sales
Margaret | NULL
-- end

-- case: legacy_outer_join_with_filter
SELECT p.name FROM people p, teams t WHERE p.team_id = t.id (+) AND p.id > 1 ORDER BY p.id
-- expect:
Grace
Linus
Margaret
-- end

-- case: minus_operator
SELECT id FROM teams MINUS SELECT team_id FROM people WHERE team_id IS NOT NULL
-- expect:
3
-- end

# COUNT(*) with a bare ORDER BY column is invalid in Oracle too; the session
# must survive the error.
-- case: session_survives_a_statement_error
SELECT COUNT(*) FROM people WHERE ROWNUM <= 2 ORDER BY id
-- error: GROUP BY
-- end

-- case: recovers_after_prior_error
SELECT COUNT(*) FROM people
-- expect:
4
-- end

-- case: fetch_first_rows_only
SELECT name FROM people ORDER BY id FETCH FIRST 2 ROWS ONLY
-- expect:
Ada
Grace
-- end

-- case: large_result_set_streams_back
SELECT COUNT(*) FROM (SELECT generate_series(1, 500) FROM DUAL) q
-- expect:
500
-- end

# --- Probes of the DUAL rewrite (only bare unaliased single `dual` in from[0]
# is dropped) and the first-keyword query/DML routing in server.rs.

-- case: dual_schema_qualified
SELECT 1 FROM sys.dual
-- expect:
1
-- end

-- case: dual_with_alias
SELECT d.dummy FROM dual d
-- expect:
X
-- end

-- case: dual_in_multi_table_from
SELECT people.name FROM people, dual WHERE people.id = 1
-- expect:
Ada
-- end

-- case: parenthesised_select_is_still_a_query
(SELECT 1 FROM DUAL)
-- expect:
1
-- end

-- case: parenthesised_union
(SELECT 1 FROM DUAL) UNION (SELECT 2 FROM DUAL) ORDER BY 1
-- expect:
1
2
-- end

-- case: column_named_like_a_keyword
-- setup: CREATE TABLE kw_demo ("date" NUMBER, "level" NUMBER)
-- setup: INSERT INTO kw_demo ("date", "level") VALUES (20240101, 3)
SELECT "date", "level" FROM kw_demo
-- expect:
20240101 | 3
-- end

-- case: unquoted_column_named_date
-- setup: CREATE TABLE kw_demo2 (date_col NUMBER, level_col NUMBER)
-- setup: INSERT INTO kw_demo2 (date_col, level_col) VALUES (1, 2)
SELECT date_col, level_col FROM kw_demo2
-- expect:
1 | 2
-- end
