# TO_CHAR / TO_NUMBER / TO_DATE / CAST. orafce supplies the single-argument
# TO_CHAR(number) and TO_NUMBER forms; the format-model forms are PostgreSQL's.

-- case: to_char_integer
SELECT TO_CHAR(42) FROM DUAL
-- expect:
42
-- end

-- case: to_char_negative
SELECT TO_CHAR(-44444) FROM DUAL
-- expect:
-44444
-- end

-- case: to_char_date_format
SELECT TO_CHAR(DATE '2024-01-02', 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-01-02
-- end

-- case: to_char_date_month_name
SELECT TRIM(TO_CHAR(DATE '2024-01-02', 'Month')) FROM DUAL
-- expect:
January
-- end

-- case: to_number_text
SELECT TO_NUMBER('123') + 1 FROM DUAL
-- expect:
124
-- end

-- case: to_number_decimal_text
SELECT TO_NUMBER('123.5') * 2 FROM DUAL
-- expect:
247
-- end

-- case: to_date_iso
SELECT TO_DATE('2009-01-02', 'YYYY-MM-DD') FROM DUAL
-- expect:
2009-01-02
-- end

-- case: to_date_custom_format
SELECT TO_DATE('02/29/2024', 'MM/DD/YYYY') FROM DUAL
-- expect:
2024-02-29
-- end

-- case: cast_text_to_number
SELECT CAST('42' AS NUMERIC) + 8 FROM DUAL
-- expect:
50
-- end

-- case: cast_number_to_float
SELECT CAST(1.25 AS DOUBLE PRECISION) FROM DUAL
-- expect:
1.25
-- end

-- case: cast_string_to_date
SELECT CAST('2024-01-02' AS DATE) FROM DUAL
-- expect:
2024-01-02
-- end

-- case: implicit_number_to_string_in_concat
SELECT 'count: ' || 42 FROM DUAL
-- expect:
count: 42
-- end

-- case: implicit_string_to_number_in_where
SELECT name FROM people WHERE id = '2'
-- expect:
Grace
-- end

-- case: implicit_string_to_date_comparison
SELECT COUNT(*) FROM (SELECT DATE '2024-01-01' d FROM DUAL) WHERE d = '2024-01-01'
-- expect:
1
-- end

-- case: to_number_with_format_mask
SELECT TO_NUMBER('1,234.56', '9,999.99') FROM DUAL
-- expect:
1234.56
-- end

-- case: to_number_currency
SELECT TO_NUMBER('$1,234.00', 'FM$9,999.00') FROM DUAL
-- expect:
1234
-- end

-- case: to_char_number_leading_zero
SELECT TO_CHAR(7, 'FM00000') FROM DUAL
-- expect:
00007
-- end

-- case: to_char_number_rounds
SELECT TO_CHAR(3.14159, 'FM990.00') FROM DUAL
-- expect:
3.14
-- end

-- case: cast_number_to_varchar2
SELECT CAST(123 AS VARCHAR2(10)) || 'x' FROM DUAL
-- expect:
123x
-- end

-- case: cast_timestamp_to_date_drops_fraction
SELECT TO_CHAR(CAST(TIMESTAMP '2024-01-02 03:04:05.678' AS DATE), 'YYYY-MM-DD HH24:MI:SS') FROM DUAL
-- expect:
2024-01-02 03:04:05
-- end

-- case: hextoraw_and_rawtohex_roundtrip
SELECT RAWTOHEX(HEXTORAW('DEADBEEF')) FROM DUAL
-- expect:
DEADBEEF
-- end




