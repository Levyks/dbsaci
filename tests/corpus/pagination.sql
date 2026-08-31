# Top-N and pagination. The nested-ROWNUM idiom below is how essentially every
# pre-12c Oracle application paginates; getting it right is table stakes for a
# drop-in replacement.

-- fixture: CREATE TABLE IF NOT EXISTS nums AS SELECT g AS n, 'row ' || g AS label FROM generate_series(1, 50) g

-- case: rownum_le_limits
SELECT n FROM nums WHERE ROWNUM <= 3 ORDER BY n
-- expect:
1
2
3
-- end

-- case: rownum_lt_is_one_based
SELECT count(*) FROM (SELECT n FROM nums WHERE ROWNUM < 5)
-- expect:
4
-- end

-- case: rownum_equals_one
SELECT n FROM nums WHERE ROWNUM = 1
-- expect:
1
-- end

-- case: rownum_equals_one_with_predicate
SELECT n FROM nums WHERE n > 10 AND ROWNUM = 1
-- expect:
11
-- end

-- case: rownum_greater_than_one_is_always_empty
SELECT n FROM nums WHERE ROWNUM > 1
-- expect:
-- end

-- case: rownum_in_projection
SELECT ROWNUM, n FROM nums WHERE n <= 3 ORDER BY n
-- expect:
1 | 1
2 | 2
3 | 3
-- end

-- case: classic_nested_rownum_pagination
SELECT label FROM (
  SELECT a.*, ROWNUM rn FROM (
    SELECT label FROM nums ORDER BY n
  ) a WHERE ROWNUM <= 20
) WHERE rn > 15
-- expect:
row 16
row 17
row 18
row 19
row 20
-- end

-- case: top_n_by_order
SELECT n FROM (SELECT n FROM nums ORDER BY n DESC) WHERE ROWNUM <= 3
-- expect:
50
49
48
-- end

-- case: rownum_alias_then_filter
SELECT rn FROM (SELECT ROWNUM rn, n FROM nums) WHERE rn BETWEEN 5 AND 7
-- expect:
5
6
7
-- end

-- case: fetch_first_rows_only
SELECT n FROM nums ORDER BY n FETCH FIRST 3 ROWS ONLY
-- expect:
1
2
3
-- end

-- case: offset_fetch
SELECT n FROM nums ORDER BY n OFFSET 5 ROWS FETCH NEXT 3 ROWS ONLY
-- expect:
6
7
8
-- end

-- case: fetch_first_with_ties
SELECT team_id FROM people ORDER BY team_id NULLS LAST FETCH FIRST 1 ROW WITH TIES
-- expect:
1
1
-- end

# ROWNUM applied before ORDER BY: rows 1..3 scanned, then sorted descending.
-- case: rownum_before_order_by_desc
SELECT n FROM nums WHERE ROWNUM <= 3 ORDER BY n DESC
-- expect:
3
2
1
-- end

# --- ROWNUM->LIMIT lifting: buried in a conjunction, reversed operands,
# BETWEEN, and arithmetic on ROWNUM.

-- case: rownum_between_two_other_predicates
SELECT n FROM nums WHERE n > 0 AND ROWNUM <= 3 AND n < 100 ORDER BY n
-- expect:
1
2
3
-- end

-- case: rownum_as_second_operand_of_and
SELECT n FROM nums WHERE label IS NOT NULL AND ROWNUM <= 2
-- rows: 2
-- end

-- case: rownum_then_order_by_runs_in_oracle
SELECT n FROM nums WHERE ROWNUM <= 2 AND label IS NOT NULL ORDER BY n
-- rows: 2
-- end

-- case: rownum_operands_reversed
SELECT n FROM nums WHERE 3 >= ROWNUM ORDER BY n
-- expect:
1
2
3
-- end

-- case: rownum_between
SELECT n FROM nums WHERE ROWNUM BETWEEN 1 AND 3 ORDER BY n
-- expect:
1
2
3
-- end


-- case: rownum_in_a_parenthesised_group
SELECT n FROM nums WHERE (ROWNUM <= 3 AND n > 0) ORDER BY n
-- expect:
1
2
3
-- end

-- case: rownum_plus_expression
SELECT n FROM nums WHERE ROWNUM - 1 < 3 ORDER BY n
-- expect:
1
2
3
-- end
