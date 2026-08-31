# Oracle's NULL rules differ from PostgreSQL's in ways that silently change
# results in ported queries. These are the ones that actually bite.

-- case: empty_string_is_null
SELECT NVL('', 'was null') FROM DUAL
-- expect:
was null
-- end

-- case: empty_string_literal_is_null_in_predicate
SELECT CASE WHEN '' IS NULL THEN 'null' ELSE 'not' END FROM DUAL
-- expect:
null
-- end

-- case: concat_ignores_null_operand
SELECT 'a' || NULL || 'b' FROM DUAL
-- expect:
ab
-- end

-- case: concat_function_ignores_null
SELECT CONCAT('x', NULL) FROM DUAL
-- expect:
x
-- end

-- case: length_of_empty_string_is_null
SELECT NVL(TO_CHAR(LENGTH('')), 'null') FROM DUAL
-- expect:
null
-- end

-- case: nvl_type_unification_number_to_string
SELECT NVL(TO_CHAR(team_id), 'n/a') FROM people WHERE id = 4
-- expect:
n/a
-- end

-- case: not_in_with_null_yields_no_rows
SELECT COUNT(*) FROM people WHERE team_id NOT IN (1, NULL)
-- expect:
0
-- end

-- case: null_compared_with_equals_is_unknown
SELECT COUNT(*) FROM people WHERE team_id = NULL
-- expect:
0
-- end

-- case: coalesce_still_distinguishes_null_from_empty
SELECT COALESCE(NULL, 'x') FROM DUAL
-- expect:
x
-- end

-- case: insert_empty_string_stores_null
-- setup: CREATE TABLE nn_demo (id NUMBER, v VARCHAR2(10))
-- setup: INSERT INTO nn_demo (id, v) VALUES (1, '')
SELECT COUNT(*) FROM nn_demo WHERE v IS NULL
-- expect:
1
-- end

-- case: order_by_nulls_last_default_desc
SELECT team_id FROM people ORDER BY team_id DESC
-- expect:
NULL
2
1
1
-- end

-- case: aggregate_ignores_nulls
SELECT COUNT(team_id), COUNT(*) FROM people
-- expect:
3 | 4
-- end
