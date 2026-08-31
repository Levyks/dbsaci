# Oracle numeric functions. All shared with PostgreSQL except BITAND (orafce).

-- case: mod
SELECT MOD(17, 5) FROM DUAL
-- expect:
2
-- end

-- case: abs
SELECT ABS(-9) FROM DUAL
-- expect:
9
-- end

-- case: ceil
SELECT CEIL(2.1) FROM DUAL
-- expect:
3
-- end

-- case: floor
SELECT FLOOR(2.9) FROM DUAL
-- expect:
2
-- end

-- case: power
SELECT POWER(2, 8) FROM DUAL
-- expect:
256
-- end

-- case: sqrt
SELECT SQRT(81) FROM DUAL
-- expect:
9
-- end

-- case: sign_negative
SELECT SIGN(-3) FROM DUAL
-- expect:
-1
-- end

-- case: sign_zero
SELECT SIGN(0) FROM DUAL
-- expect:
0
-- end

-- case: round_half
SELECT ROUND(12.345, 2) FROM DUAL
-- expect:
12.35
-- end

-- case: round_no_scale
SELECT ROUND(12.5) FROM DUAL
-- expect:
13
-- end

-- case: trunc_scale
SELECT TRUNC(12.345, 2) FROM DUAL
-- expect:
12.34
-- end

-- case: trunc_no_scale
SELECT TRUNC(12.99) FROM DUAL
-- expect:
12
-- end

-- case: bitand
SELECT BITAND(5, 1), BITAND(5, 2), BITAND(5, 4) FROM DUAL
-- expect:
1 | 0 | 4
-- end

-- case: negative_literal
SELECT -42 + 2 FROM DUAL
-- expect:
-40
-- end

-- case: large_integer
SELECT 1234567890123456 FROM DUAL
-- expect:
1234567890123456
-- end

-- case: negative_decimal
SELECT -123.75 FROM DUAL
-- expect:
-123.75
-- end

