# NVL / NVL2 / DECODE / LNNVL / NANVL. DbSaci lowers NVL2, DECODE and LNNVL to
# CASE against the AST; NVL becomes COALESCE; NANVL is orafce.

-- case: nvl_returns_fallback_on_null
SELECT NVL(NULL, 'fallback') FROM DUAL
-- expect:
fallback
-- end

-- case: nvl_returns_value_when_present
SELECT NVL('value', 'fallback') FROM DUAL
-- expect:
value
-- end

-- case: nvl_numeric
SELECT NVL(CAST(NULL AS NUMERIC), 99) FROM DUAL
-- expect:
99
-- end

-- case: nvl_over_column
SELECT name, NVL(TO_CHAR(team_id), 'none') FROM people ORDER BY id
-- expect:
Ada | 1
Grace | 1
Linus | 2
Margaret | none
-- end

-- case: nvl2_non_null_picks_second
SELECT NVL2('x', 'yes', 'no') FROM DUAL
-- expect:
yes
-- end

-- case: nvl2_null_picks_third
SELECT NVL2(NULL, 'yes', 'no') FROM DUAL
-- expect:
no
-- end

-- case: decode_match_first
SELECT DECODE(1, 1, 'one', 2, 'two', 'other') FROM DUAL
-- expect:
one
-- end

-- case: decode_match_second
SELECT DECODE(2, 1, 'one', 2, 'two', 'other') FROM DUAL
-- expect:
two
-- end

-- case: decode_default
SELECT DECODE(3, 1, 'one', 2, 'two', 'other') FROM DUAL
-- expect:
other
-- end

-- case: decode_no_default_no_match_is_null
SELECT DECODE(3, 1, 'one', 2, 'two') FROM DUAL
-- expect:
NULL
-- end

-- case: decode_matches_null_argument
SELECT DECODE(NULL, 1, 'a', NULL, 'b', 'c') FROM DUAL
-- expect:
b
-- end

-- case: decode_over_rows
SELECT name, DECODE(team_id, 1, 'Eng', 2, 'Sales', 'n/a') FROM people ORDER BY id
-- expect:
Ada | Eng
Grace | Eng
Linus | Sales
Margaret | n/a
-- end

-- case: lnnvl_filters_false_and_null_rows
SELECT name FROM people WHERE LNNVL(team_id = 1) ORDER BY id
-- expect:
Linus
Margaret
-- end

-- case: nanvl_passes_number
SELECT NANVL(12345, 1) FROM DUAL
-- expect:
12345
-- end

-- case: nanvl_replaces_nan
-- skip: mariadb (MariaDB has no NaN value in DOUBLE arithmetic)
SELECT NANVL(CAST('NaN' AS DOUBLE PRECISION), 1) FROM DUAL
-- expect:
1
-- end

-- case: coalesce_multi
SELECT COALESCE(NULL, NULL, 'x') FROM DUAL
-- expect:
x
-- end
