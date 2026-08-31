# Oracle date/time arithmetic with INTERVAL literals and the NUMTO*INTERVAL
# constructors. Common in scheduling, SLA and retention logic.

-- case: interval_day_to_second_add
SELECT TO_CHAR(TIMESTAMP '2024-01-01 00:00:00' + INTERVAL '2' DAY, 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-01-03
-- end

-- case: interval_hour_add
SELECT TO_CHAR(TIMESTAMP '2024-01-01 22:00:00' + INTERVAL '3' HOUR, 'YYYY-MM-DD HH24:MI') FROM DUAL
-- expect:
2024-01-02 01:00
-- end

-- case: interval_year_to_month
SELECT TO_CHAR(DATE '2024-01-15' + INTERVAL '1-6' YEAR TO MONTH, 'YYYY-MM-DD') FROM DUAL
-- expect:
2025-07-15
-- end

-- case: interval_minute_literal
SELECT TO_CHAR(TIMESTAMP '2024-01-01 00:00:00' + INTERVAL '90' MINUTE, 'HH24:MI') FROM DUAL
-- expect:
01:30
-- end

-- case: numtodsinterval_hours
SELECT TO_CHAR(TIMESTAMP '2024-01-01 00:00:00' + NUMTODSINTERVAL(36, 'HOUR'), 'YYYY-MM-DD HH24:MI') FROM DUAL
-- expect:
2024-01-02 12:00
-- end

-- case: numtoyminterval_months
SELECT TO_CHAR(DATE '2024-01-31' + NUMTOYMINTERVAL(1, 'MONTH'), 'YYYY-MM-DD') FROM DUAL
-- expect:
2024-02-29
-- end

-- case: sysdate_minus_days_is_a_date
SELECT CASE WHEN SYSDATE - 7 < SYSDATE THEN 'ok' ELSE 'bad' END FROM DUAL
-- expect:
ok
-- end

-- case: fraction_of_day_arithmetic
SELECT TO_CHAR(TIMESTAMP '2024-01-01 00:00:00' + 1/24, 'HH24:MI') FROM DUAL
-- expect:
01:00
-- end

-- case: interval_difference_of_timestamps
SELECT EXTRACT(DAY FROM (TIMESTAMP '2024-01-10 00:00:00' - TIMESTAMP '2024-01-01 00:00:00')) FROM DUAL
-- expect:
9
-- end

# A raw INTERVAL result column (a genuine PostgreSQL `interval`, not Oracle
# DATE-minus-DATE which is a NUMBER of days). python-oracledb thin gets the
# native TTC form (types 182 / 183); oracle-rs and the JDBC/ODP.NET path get an
# Oracle-style text rendering (previously such a column decoded as NULL).
-- case: interval_day_to_second_result_column
SELECT CAST('9 06:30:00' AS INTERVAL DAY TO SECOND) FROM DUAL
-- expect:
+09 06:30:00.000000
-- end

-- case: interval_year_to_month_result_column
SELECT CAST('1-6' AS INTERVAL YEAR TO MONTH) FROM DUAL
-- expect:
+01-06
-- end

-- case: negative_interval_result_column
SELECT NUMTODSINTERVAL(-2.5, 'DAY') FROM DUAL
-- expect:
-02 12:00:00.000000
-- end
