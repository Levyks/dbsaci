# CASE and the ANSI null/choice functions.

-- case: searched_case
SELECT CASE WHEN 1 = 1 THEN 'Y' ELSE 'N' END FROM DUAL
-- expect:
Y
-- end

-- case: simple_case
SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END FROM DUAL
-- expect:
two
-- end

-- case: case_no_else_is_null
SELECT CASE WHEN 1 = 0 THEN 'x' END FROM DUAL
-- expect:
NULL
-- end

-- case: case_over_rows
SELECT name, CASE WHEN team_id IS NULL THEN 'unassigned' ELSE 'assigned' END FROM people ORDER BY id
-- expect:
Ada | assigned
Grace | assigned
Linus | assigned
Margaret | unassigned
-- end

-- case: coalesce_first_non_null
SELECT COALESCE(NULL, NULL, 'third', 'fourth') FROM DUAL
-- expect:
third
-- end

-- case: nullif_equal_gives_null
SELECT NULLIF(5, 5) FROM DUAL
-- expect:
NULL
-- end

-- case: nullif_unequal_gives_first
SELECT NULLIF(5, 6) FROM DUAL
-- expect:
5
-- end

-- case: greatest
SELECT GREATEST(3, 9, 1, 7) FROM DUAL
-- expect:
9
-- end

-- case: least
SELECT LEAST(3, 9, 1, 7) FROM DUAL
-- expect:
1
-- end

-- case: greatest_of_strings
SELECT GREATEST('apple', 'pear', 'kiwi') FROM DUAL
-- expect:
pear
-- end
