# Core ANSI SELECT surface. PgSaci strips FROM DUAL and otherwise passes these
# straight to PostgreSQL, so they double as a wire-format smoke test for the
# scalar result path.

-- case: integer_literal
SELECT 1 FROM DUAL
-- expect:
1
-- end

-- case: string_literal
SELECT 'Oracle' FROM DUAL
-- expect:
Oracle
-- end

-- case: quoted_apostrophe_literal
SELECT 'O''Reilly' FROM DUAL
-- expect:
O'Reilly
-- end

-- case: null_literal
SELECT NULL FROM DUAL
-- expect:
NULL
-- end

-- case: addition
SELECT 1 + 2 FROM DUAL
-- expect:
3
-- end

-- case: subtraction
SELECT 9 - 4 FROM DUAL
-- expect:
5
-- end

-- case: multiplication
SELECT 3 * 7 FROM DUAL
-- expect:
21
-- end

-- case: exact_division
SELECT 20 / 5 FROM DUAL
-- expect:
4
-- end

-- case: fractional_division
SELECT 7 / 2.0 FROM DUAL
-- expect:
3.5
-- end

-- case: parenthesised_precedence
SELECT (2 + 3) * 4 FROM DUAL
-- expect:
20
-- end

-- case: string_concat_operator
SELECT 'ora' || 'cle' FROM DUAL
-- expect:
oracle
-- end

-- case: multi_column_projection
SELECT 'x', 2 + 3, 'y' FROM DUAL
-- expect:
x | 5 | y
-- end

-- case: where_true
SELECT 1 FROM DUAL WHERE 1 = 1
-- expect:
1
-- end

-- case: where_false_returns_no_rows
SELECT 1 FROM DUAL WHERE 1 = 0
-- expect:
-- end

-- case: order_by_desc
SELECT name FROM people ORDER BY id DESC
-- expect:
Margaret
Linus
Grace
Ada
-- end

-- case: where_and
SELECT name FROM people WHERE id >= 1 AND id <= 2 ORDER BY id
-- expect:
Ada
Grace
-- end

-- case: where_or
SELECT name FROM people WHERE id = 1 OR id = 3 ORDER BY id
-- expect:
Ada
Linus
-- end

-- case: where_in_list
SELECT name FROM people WHERE id IN (2, 4) ORDER BY id
-- expect:
Grace
Margaret
-- end

-- case: where_between
SELECT name FROM people WHERE id BETWEEN 2 AND 3 ORDER BY id
-- expect:
Grace
Linus
-- end

-- case: where_like_prefix
SELECT name FROM people WHERE name LIKE 'A%' ORDER BY id
-- expect:
Ada
-- end

-- case: where_is_null
SELECT name FROM people WHERE team_id IS NULL
-- expect:
Margaret
-- end

-- case: where_is_not_null
SELECT COUNT(*) FROM people WHERE team_id IS NOT NULL
-- expect:
3
-- end

-- case: distinct_values
SELECT DISTINCT team_id FROM people WHERE team_id IS NOT NULL ORDER BY team_id
-- expect:
1
2
-- end

-- case: column_alias
SELECT name AS who FROM people WHERE id = 1
-- expect:
Ada
-- end

# Oracle (pre-23c) has no boolean in the projection list; PgSaci follows its
# PostgreSQL backend and renders bool as NUMBER 1/0.
-- case: boolean_comparison_scalar
SELECT 1 = 1 FROM DUAL
-- expect:
1
-- end

-- case: not_in_list
SELECT name FROM people WHERE id NOT IN (1, 2, 3) ORDER BY id
-- expect:
Margaret
-- end

-- case: concat_number_coerces_to_text
SELECT 'id=' || id FROM people WHERE id = 3
-- expect:
id=3
-- end

# Oracle resolves a SELECT-list alias inside an ORDER BY expression; PostgreSQL
# only resolves a bare alias term. The translator substitutes the alias.
-- case: order_by_alias_inside_expression
SELECT name nm FROM people ORDER BY LENGTH(nm) DESC, nm
-- expect:
Margaret
Grace
Linus
Ada
-- end
