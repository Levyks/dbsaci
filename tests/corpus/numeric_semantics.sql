# Oracle NUMBER arithmetic semantics. Differences here change financial results
# silently, so they matter more than most.

-- case: division_is_always_float
SELECT 10 / 4 FROM DUAL
-- expect:
2.5
-- end

-- case: integer_operands_still_divide_as_float
SELECT 7 / 2 FROM DUAL
-- expect:
3.5
-- end

-- case: division_result_of_evenly_divisible
SELECT 9 / 3 FROM DUAL
-- expect:
3
-- end

-- case: modulo_of_negative
SELECT MOD(-7, 3) FROM DUAL
-- expect:
-1
-- end

-- case: remainder_function
SELECT REMAINDER(7, 3) FROM DUAL
-- expect:
1
-- end

-- case: round_half_away_from_zero
SELECT ROUND(2.5), ROUND(-2.5), ROUND(3.5) FROM DUAL
-- expect:
3 | -3 | 4
-- end

-- case: round_to_negative_scale
SELECT ROUND(12345, -2) FROM DUAL
-- expect:
12300
-- end

-- case: trunc_to_negative_scale
SELECT TRUNC(12345, -2) FROM DUAL
-- expect:
12300
-- end

# NOTE: PgSaci trims trailing zeros from NUMBER results (declared scale is not
# carried on the wire), so 1.500 comes back as 1.5.
-- case: number_trailing_zeros_are_trimmed
SELECT CAST(1.5 AS NUMBER(10,3)) FROM DUAL
-- expect:
1.5
-- end

-- case: number_rounds_to_declared_scale
SELECT CAST(1.23456 AS NUMBER(10,2)) FROM DUAL
-- expect:
1.23
-- end

-- case: large_number_precision
SELECT 9999999999999999 + 1 FROM DUAL
-- expect:
10000000000000000
-- end

-- case: division_by_zero_raises
SELECT 1 / 0 FROM DUAL
-- error: ORA-01476
-- end

-- case: string_to_number_implicit
SELECT '10' + 5 FROM DUAL
-- expect:
15
-- end

-- case: non_numeric_string_arithmetic_errors
SELECT 'abc' + 1 FROM DUAL
-- error: ORA-01722
-- end

-- case: power_returns_number
SELECT POWER(10, 3) FROM DUAL
-- expect:
1000
-- end

-- case: exact_decimal_sum
SELECT 0.1 + 0.2 FROM DUAL
-- expect:
0.3
-- end

# --- Probes of the hand-rolled base-10000 NUMERIC decoder (backend.rs
# PgNumericText). The passing decimal cases all happen to have a first
# fractional digit-group >= 1000; values whose leading fractional group is
# small ("cents", sub-thousandths) exercise the group-zero-padding path.

-- case: small_fraction_five_hundredths
SELECT CAST(0.05 AS NUMBER(10,2)) FROM DUAL
-- expect:
0.05
-- end

-- case: small_fraction_nine_cents
SELECT 12.09 FROM DUAL
-- expect:
12.09
-- end

-- case: small_fraction_one_thousandth
SELECT CAST(0.001 AS NUMBER(10,3)) FROM DUAL
-- expect:
0.001
-- end

-- case: fraction_with_leading_zero_group
SELECT 1.0009 FROM DUAL
-- expect:
1.0009
-- end

-- case: fraction_tiny
SELECT CAST(0.0001 AS NUMBER(12,4)) FROM DUAL
-- expect:
0.0001
-- end

-- case: money_like_value
SELECT CAST(1234.05 AS NUMBER(10,2)) FROM DUAL
-- expect:
1234.05
-- end

-- case: negative_small_fraction
SELECT CAST(-0.07 AS NUMBER(10,2)) FROM DUAL
-- expect:
-0.07
-- end

-- case: very_large_number_across_many_groups
SELECT CAST(123456789012345678 AS NUMBER(30)) FROM DUAL
-- expect:
123456789012345678
-- end

-- case: large_number_with_fraction
SELECT CAST(98765432109876.54 AS NUMBER(30,2)) FROM DUAL
-- expect:
98765432109876.54
-- end

-- case: sum_of_cents_stays_exact
-- setup: CREATE TABLE cents (amt NUMBER(10,2))
-- setup: INSERT INTO cents (amt) VALUES (0.01), (0.02), (0.03), (0.09)
SELECT SUM(amt) FROM cents
-- expect:
0.15
-- end
