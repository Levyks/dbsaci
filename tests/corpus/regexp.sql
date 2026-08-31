# Oracle regular-expression functions. REGEXP_LIKE is a predicate in Oracle;
# REGEXP_SUBSTR / REGEXP_INSTR / REGEXP_COUNT / REGEXP_REPLACE are functions.
# PostgreSQL has close equivalents but the names and some semantics differ.

-- case: regexp_like_in_where
SELECT name FROM people WHERE REGEXP_LIKE(name, '^A') ORDER BY id
-- expect:
Ada
-- end

-- case: regexp_like_case_insensitive
SELECT name FROM people WHERE REGEXP_LIKE(name, 'ADA', 'i') ORDER BY id
-- expect:
Ada
-- end

-- case: regexp_replace_basic
SELECT REGEXP_REPLACE('2024-01-02', '-', '/') FROM DUAL
-- expect:
2024/01/02
-- end

-- case: regexp_replace_with_backref
SELECT REGEXP_REPLACE('John Smith', '(\w+) (\w+)', '\2, \1') FROM DUAL
-- expect:
Smith, John
-- end

-- case: regexp_substr_basic
SELECT REGEXP_SUBSTR('the quick brown fox', '\w+') FROM DUAL
-- expect:
the
-- end

-- case: regexp_substr_nth
SELECT REGEXP_SUBSTR('a1b2c3', '[0-9]', 1, 2) FROM DUAL
-- expect:
2
-- end

-- case: regexp_substr_group
SELECT REGEXP_SUBSTR('id=42;', 'id=([0-9]+)', 1, 1, NULL, 1) FROM DUAL
-- expect:
42
-- end

-- case: regexp_instr_position
SELECT REGEXP_INSTR('abc123def', '[0-9]+') FROM DUAL
-- expect:
4
-- end

-- case: regexp_count
SELECT REGEXP_COUNT('a,b,c,d', ',') FROM DUAL
-- expect:
3
-- end

-- case: regexp_replace_strip_non_digits
SELECT REGEXP_REPLACE('(555) 123-4567', '[^0-9]', '') FROM DUAL
-- expect:
5551234567
-- end

-- case: regexp_like_anchored_digits
SELECT CASE WHEN REGEXP_LIKE('12345', '^[0-9]+$') THEN 'num' ELSE 'not' END FROM DUAL
-- expect:
num
-- end
