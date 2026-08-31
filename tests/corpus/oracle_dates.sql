# Oracle date/time functions. ADD_MONTHS / LAST_DAY / NEXT_DAY / MONTHS_BETWEEN
# and TRUNC/ROUND-on-date come from orafce; EXTRACT and date arithmetic are
# shared with PostgreSQL. Results cross the wire as Oracle DATE (7 bytes) and are
# rendered here as YYYY-MM-DD.

-- case: add_months_forward
SELECT ADD_MONTHS(DATE '2003-08-01', 3) FROM DUAL
-- expect:
2003-11-01
-- end

-- case: add_months_backward
SELECT ADD_MONTHS(DATE '2003-08-01', -3) FROM DUAL
-- expect:
2003-05-01
-- end

-- case: add_months_end_of_month_clamps
SELECT ADD_MONTHS(DATE '2003-01-31', 1) FROM DUAL
-- expect:
2003-02-28
-- end

-- case: add_months_leap_year
SELECT ADD_MONTHS(DATE '2008-02-29', 12) FROM DUAL
-- expect:
2009-02-28
-- end

-- case: last_day_february_leap
SELECT LAST_DAY(DATE '2004-02-03') FROM DUAL
-- expect:
2004-02-29
-- end

-- case: last_day_february_common
SELECT LAST_DAY(DATE '2007-02-01') FROM DUAL
-- expect:
2007-02-28
-- end

-- case: next_day_named
SELECT NEXT_DAY(DATE '2003-08-01', 'TUESDAY') FROM DUAL
-- expect:
2003-08-05
-- end

-- case: months_between_shape
SELECT MONTHS_BETWEEN(DATE '2003-07-01', DATE '2003-03-14') FROM DUAL
-- expect-regex: ^3\.58
-- end

-- case: months_between_whole
SELECT MONTHS_BETWEEN(DATE '2003-08-02', DATE '2003-06-02') FROM DUAL
-- expect:
2
-- end

-- case: extract_year
SELECT EXTRACT(YEAR FROM DATE '2024-03-15') FROM DUAL
-- expect:
2024
-- end

-- case: extract_month
SELECT EXTRACT(MONTH FROM DATE '2024-03-15') FROM DUAL
-- expect:
3
-- end

-- case: date_plus_integer
SELECT DATE '2024-01-02' + 1 FROM DUAL
-- expect:
2024-01-03
-- end

-- case: date_difference_in_days
SELECT DATE '2024-03-01' - DATE '2024-02-01' FROM DUAL
-- expect:
29
-- end

-- case: trunc_date_to_month
SELECT TRUNC(DATE '2024-05-17', 'MM') FROM DUAL
-- expect:
2024-05-01
-- end

-- case: trunc_date_to_year
SELECT TRUNC(DATE '2024-05-17', 'YYYY') FROM DUAL
-- expect:
2024-01-01
-- end

-- case: sysdate_returns_a_value
SELECT SYSDATE FROM DUAL
-- ok
-- end

-- case: current_date_returns_a_value
SELECT CURRENT_DATE FROM DUAL
-- ok
-- end

-- case: date_literal_roundtrip
SELECT DATE '2024-02-29' FROM DUAL
-- expect:
2024-02-29
-- end

-- case: timestamp_literal_has_time
SELECT TIMESTAMP '2024-02-29 13:14:15' FROM DUAL
-- expect:
2024-02-29 13:14:15
-- end

-- case: sysdate_minus_number_is_days
SELECT TRUNC(SYSDATE) - TRUNC(SYSDATE - 3) FROM DUAL
-- expect:
3
-- end

-- case: to_char_full_timestamp
SELECT TO_CHAR(TIMESTAMP '2024-03-05 14:07:09', 'YYYY-MM-DD HH24:MI:SS') FROM DUAL
-- expect:
2024-03-05 14:07:09
-- end

-- case: to_char_day_of_week_number
SELECT TO_CHAR(DATE '2024-03-04', 'D') FROM DUAL
-- expect:
2
-- end

-- case: to_char_iso_week
SELECT TO_CHAR(DATE '2024-01-04', 'IW') FROM DUAL
-- expect:
01
-- end

-- case: to_char_quarter
SELECT TO_CHAR(DATE '2024-08-15', 'Q') FROM DUAL
-- expect:
3
-- end

-- case: to_char_julian
SELECT TO_CHAR(DATE '2024-01-01', 'J') FROM DUAL
-- expect:
2460311
-- end

-- case: to_char_fm_number_mask
SELECT TO_CHAR(1234.5, 'FM9,999.00') FROM DUAL
-- expect:
1,234.50
-- end

-- case: to_char_currency_mask
SELECT TO_CHAR(1234.5, 'FM$9,999.00') FROM DUAL
-- expect:
$1,234.50
-- end

-- case: to_date_with_time
SELECT TO_CHAR(TO_DATE('2024-03-05 14:07', 'YYYY-MM-DD HH24:MI'), 'HH24:MI') FROM DUAL
-- expect:
14:07
-- end

-- case: to_date_month_abbrev
SELECT TO_CHAR(TO_DATE('15-MAR-2024', 'DD-MON-YYYY'), 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-03-15
-- end

-- case: trunc_date_to_day_strips_time
SELECT TO_CHAR(TRUNC(TIMESTAMP '2024-03-05 14:07:09'), 'YYYY-MM-DD HH24:MI:SS') FROM DUAL
-- expect:
2024-03-05 00:00:00
-- end

-- case: trunc_date_to_iso_year
SELECT TO_CHAR(TRUNC(DATE '2024-08-15', 'IY'), 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-01-01
-- end

-- case: round_date_to_month_up
SELECT TO_CHAR(ROUND(DATE '2024-03-20', 'MM'), 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-04-01
-- end

-- case: last_day_of_month_plus_days
SELECT TO_CHAR(LAST_DAY(DATE '2024-02-10') + 1, 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-03-01
-- end

-- case: months_between_same_day
SELECT MONTHS_BETWEEN(DATE '2024-06-15', DATE '2024-03-15') FROM DUAL
-- expect:
3
-- end

-- case: greatest_of_dates
SELECT TO_CHAR(GREATEST(DATE '2024-01-01', DATE '2024-06-01', DATE '2024-03-01'), 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-06-01
-- end

-- case: extract_from_timestamp
SELECT EXTRACT(HOUR FROM TIMESTAMP '2024-03-05 14:07:09') FROM DUAL
-- expect:
14
-- end
