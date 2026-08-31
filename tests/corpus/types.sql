# Wire-level type fidelity: how PostgreSQL result types come back across TTC.
# NUMBER values arrive base-100 encoded and are rendered here as decimal text.

-- case: small_integer
SELECT CAST(7 AS SMALLINT) FROM DUAL
-- expect:
7
-- end

-- case: regular_integer
SELECT CAST(70000 AS INTEGER) FROM DUAL
-- expect:
70000
-- end

-- case: big_integer
SELECT CAST(9223372036854775807 AS BIGINT) FROM DUAL
-- expect:
9223372036854775807
-- end

-- case: negative_integer
SELECT CAST(-12345 AS INTEGER) FROM DUAL
-- expect:
-12345
-- end

-- case: zero
SELECT 0 FROM DUAL
-- expect:
0
-- end

-- case: numeric_decimal
SELECT CAST(123.45 AS NUMERIC(10, 2)) FROM DUAL
-- expect:
123.45
-- end

-- case: numeric_negative_decimal
SELECT CAST(-0.5 AS NUMERIC(10, 2)) FROM DUAL
-- expect:
-0.5
-- end

-- case: double_precision
SELECT CAST(1.25 AS DOUBLE PRECISION) FROM DUAL
-- expect:
1.25
-- end

-- case: real
SELECT CAST(2.5 AS REAL) FROM DUAL
-- expect:
2.5
-- end

-- case: text
SELECT CAST('hello world' AS TEXT) FROM DUAL
-- expect:
hello world
-- end

-- case: ascii_text_roundtrip
SELECT 'plain ascii text 123' FROM DUAL
-- expect:
plain ascii text 123
-- end

# PgSaci sends a zero-length VARCHAR field, which oracle-rs surfaces as NULL.
# This happens to match Oracle's own '' IS NULL rule.
-- case: empty_string_reads_back_as_null
SELECT CAST('' AS TEXT) FROM DUAL
-- expect:
NULL
-- end

-- case: null_number
SELECT CAST(NULL AS NUMERIC) FROM DUAL
-- expect:
NULL
-- end

-- case: null_text
SELECT CAST(NULL AS TEXT) FROM DUAL
-- expect:
NULL
-- end

-- case: date_type
SELECT CAST('2024-02-29' AS DATE) FROM DUAL
-- expect:
2024-02-29
-- end

-- case: timestamp_type
SELECT CAST('2024-02-29 13:14:15' AS TIMESTAMP) FROM DUAL
-- expect:
2024-02-29 13:14:15
-- end

-- case: bytea_type
SELECT decode('00ff10', 'hex') FROM DUAL
-- expect:
0x00ff10
-- end

-- case: multi_column_mixed_types
SELECT id, name, team_id FROM people WHERE id = 1
-- expect:
1 | Ada | 1
-- end

-- case: non_ascii_latin_roundtrip
SELECT 'café résumé naïve' FROM DUAL
-- expect:
café résumé naïve
-- end

-- case: non_ascii_cyrillic_roundtrip
SELECT 'Привет мир' FROM DUAL
-- expect:
Привет мир
-- end

# NCHAR / NVARCHAR2 collapse to VARCHAR2; UTF-8 values round-trip exactly.
-- case: nvarchar2_declared_column_roundtrip
-- setup: CREATE TABLE nvc_demo (id NUMBER, v NVARCHAR2(30))
-- setup: INSERT INTO nvc_demo (id, v) VALUES (1, 'Cyrillic Привет')
SELECT v FROM nvc_demo WHERE id = 1
-- expect:
Cyrillic Привет
-- end

-- case: emoji_roundtrip
SELECT 'ok 🚀 done' FROM DUAL
-- expect:
ok 🚀 done
-- end

-- case: length_counts_characters_not_bytes
SELECT LENGTH('café') FROM DUAL
-- expect:
4
-- end

-- case: clob_column_roundtrip
-- setup: CREATE TABLE clob_demo (id NUMBER, body CLOB)
-- setup: INSERT INTO clob_demo (id, body) VALUES (1, 'a fairly long clob body that exceeds a few words')
SELECT body FROM clob_demo
-- expect:
a fairly long clob body that exceeds a few words
-- end

-- case: number_negative_scale_roundtrip
SELECT CAST(-9999.99 AS NUMBER(10,2)) FROM DUAL
-- expect:
-9999.99
-- end

# --- Probes: 7-byte Oracle DATE encoding drops sub-second precision and time
# zone; a zero-length VARCHAR field is indistinguishable from NULL anywhere in
# the row, not just as a lone column.

-- case: timestamp_fractional_seconds_preserved
SELECT TO_CHAR(TIMESTAMP '2024-02-29 13:14:15.123456', 'HH24:MI:SS.FF6') FROM DUAL
-- expect:
13:14:15.123456
-- end

# The result path now emits the native 11-byte Oracle TIMESTAMP (7-byte DATE +
# 4-byte big-endian nanoseconds), so sub-second precision survives without a
# TO_CHAR round trip.
-- case: timestamp_native_fractional_seconds
SELECT CAST(TIMESTAMP '2024-02-29 13:14:15.123456' AS TIMESTAMP) FROM DUAL
-- expect:
2024-02-29 13:14:15.123456
-- end

-- case: timestamp_native_millis
SELECT CAST(TIMESTAMP '2001-09-11 08:46:00.500' AS TIMESTAMP) FROM DUAL
-- expect:
2001-09-11 08:46:00.5
-- end

# TIMESTAMP WITH TIME ZONE result column: native 13-byte form (11-byte TIMESTAMP
# + `+00:00` tz bytes, since PostgreSQL stores TIMESTAMPTZ as UTC). The point of
# the case is that the sub-second component now survives on a TSTZ column
# instead of being truncated to the 7-byte DATE.
-- case: timestamptz_native_subsecond_survives
SELECT CAST(TIMESTAMP '2024-06-01 07:00:00.25' AS TIMESTAMP WITH TIME ZONE) FROM DUAL
-- expect:
2024-06-01 07:00:00.25
-- end

# A column *declared* BINARY_FLOAT / BINARY_DOUBLE is delivered in the native
# IEEE wire form (types 100 / 101), recovered via a describe-time catalog lookup
# on the `pgsaci.binary_*` domains — verified end-to-end by the python-oracledb
# probe (`clients/python/probe.py`: `DB_TYPE_BINARY_DOUBLE` + exact value).
# oracle-rs 0.1.7 mis-decodes a BINARY_DOUBLE result column (its own tests only
# cover the raw encode/decode functions, never a describe+row from a server), so
# there is no corpus case for the declared-column path. What the corpus *can*
# lock in is the other half: a computed double (Oracle returns NUMBER for
# POWER/SQRT/AVG/…) must NOT be promoted to BINARY_DOUBLE.
-- case: computed_double_stays_number
SELECT POWER(2, 10) FROM DUAL
-- expect:
1024
-- end

# KNOWN GAP: a TIMESTAMP literal carrying an offset is TIMESTAMP WITH TIME ZONE
# in Oracle but plain TIMESTAMP (offset ignored) in PostgreSQL; PgSaci has no
# TSTZ wire encoding to carry the original offset back.
-- case: timestamptz_keeps_offset
SELECT TO_CHAR(TIMESTAMP '2024-06-01 12:00:00 +05:00', 'HH24:MI TZH:TZM') FROM DUAL
-- expect:
12:00 +05:00
-- end

-- case: timestamptz_value_not_shifted_to_utc
SELECT TO_CHAR(CAST(TIMESTAMP '2024-06-01 12:00:00 +05:00' AS TIMESTAMP), 'HH24:MI') FROM DUAL
-- expect:
12:00
-- end

# Oracle's '' IS NULL, so the middle column comes back NULL — in every position,
# not just as a lone column.
-- case: empty_string_between_populated_columns
SELECT 'a', CAST('' AS VARCHAR2(4)), 'c' FROM DUAL
-- expect:
a | NULL | c
-- end

-- case: null_and_empty_in_same_projection_are_both_null
SELECT NVL(CAST('' AS VARCHAR2(4)), 'E'), NVL(CAST(NULL AS VARCHAR2(4)), 'N') FROM DUAL
-- expect:
E | N
-- end

-- case: date_year_below_1000
SELECT TO_CHAR(DATE '0042-01-01', 'YYYY-MM-DD') FROM DUAL
-- expect:
0042-01-01
-- end

-- case: rowid_pseudocolumn_is_stable_within_query
SELECT COUNT(DISTINCT ROWID) FROM people WHERE id <= 4
-- expect:
4
-- end

-- case: rowid_round_trips_for_refetch
SELECT name FROM people WHERE ROWID = (SELECT ROWID FROM people WHERE id = 3)
-- expect:
Linus
-- end

-- case: boolean_result_maps_to_number_one_zero
SELECT (id = 1) AS is_first FROM people WHERE id <= 4 ORDER BY id
-- expect:
1
0
0
0
-- end

-- case: dbms_lob_getlength
SELECT DBMS_LOB.GETLENGTH(CAST('hello world' AS CLOB)) FROM DUAL
-- expect:
11
-- end

-- case: dbms_lob_substr
SELECT DBMS_LOB.SUBSTR(CAST('abcdefgh' AS CLOB), 3, 2) FROM DUAL
-- expect:
bcd
-- end

-- case: dbms_lob_instr
SELECT DBMS_LOB.INSTR(CAST('abcdeabcde' AS CLOB), 'cd', 4) FROM DUAL
-- expect:
8
-- end
