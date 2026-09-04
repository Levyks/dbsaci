# Session time zone. Oracle presents TIMESTAMP WITH TIME ZONE / LOCAL TIME ZONE
# values in SESSIONTIMEZONE, which follows `ALTER SESSION SET TIME_ZONE`. DbSaci
# mirrors that onto the backend `TimeZone` GUC and converts TIMESTAMPTZ values on
# the wire (named IANA zones get per-instant, DST-correct offsets).

# Fixtures run on a direct PostgreSQL connection: one fixed UTC instant.
-- fixture: CREATE TABLE IF NOT EXISTS tz_corpus (id int PRIMARY KEY, ts timestamptz, plain timestamp)
-- fixture: TRUNCATE tz_corpus
-- fixture: INSERT INTO tz_corpus VALUES (1, TIMESTAMPTZ '2026-06-15 12:00:00+00', TIMESTAMP '2026-06-15 12:00:00')

-- case: sessiontimezone_defaults_to_utc
SELECT SESSIONTIMEZONE FROM dual
-- expect:
+00:00
-- end

-- case: dbtimezone_is_utc
SELECT DBTIMEZONE FROM dual
-- expect:
+00:00
-- end

-- case: sys_context_sessiontimezone
SELECT SYS_CONTEXT('USERENV', 'SESSIONTIMEZONE') FROM dual
-- expect:
+00:00
-- end

-- case: alter_session_set_time_zone_region
-- setup: ALTER SESSION SET TIME_ZONE = 'America/Sao_Paulo'
SELECT SESSIONTIMEZONE FROM dual
-- expect:
America/Sao_Paulo
-- end

-- case: alter_session_set_time_zone_offset
-- setup: ALTER SESSION SET TIME_ZONE = '-05:00'
SELECT SESSIONTIMEZONE FROM dual
-- expect:
-05:00
-- end

-- case: timestamptz_rendered_in_session_zone
-- skip: mariadb (MariaDB has no TIMESTAMP WITH TIME ZONE type)
-- setup: ALTER SESSION SET TIME_ZONE = 'America/Sao_Paulo'
SELECT TO_CHAR(ts, 'YYYY-MM-DD HH24:MI TZH:TZM') FROM tz_corpus WHERE id = 1
-- expect:
2026-06-15 09:00 -03:00
-- end

-- case: timestamptz_binary_value_follows_session_zone
-- skip: mariadb (MariaDB has no TIMESTAMP WITH TIME ZONE type)
-- setup: ALTER SESSION SET TIME_ZONE = 'America/Sao_Paulo'
SELECT TO_CHAR(ts, 'HH24:MI') FROM tz_corpus WHERE id = 1
-- expect:
09:00
-- end

-- case: timestamptz_offset_zone
-- skip: mariadb (MariaDB has no TIMESTAMP WITH TIME ZONE type)
-- setup: ALTER SESSION SET TIME_ZONE = '-05:00'
SELECT TO_CHAR(ts, 'HH24:MI') FROM tz_corpus WHERE id = 1
-- expect:
07:00
-- end

-- case: plain_timestamp_is_not_shifted
-- setup: ALTER SESSION SET TIME_ZONE = 'America/Sao_Paulo'
SELECT TO_CHAR(plain, 'YYYY-MM-DD HH24:MI') FROM tz_corpus WHERE id = 1
-- expect:
2026-06-15 12:00
-- end

# The raw column value (binary TIMESTAMP WITH TIME ZONE decode), not a
# server-side TO_CHAR — exercises the wire-encoding path directly. The UTC
# instant carries the session zone's offset (client renders `utc + offset`).
-- case: raw_timestamptz_carries_session_offset_region
-- skip: mariadb (MariaDB has no TIMESTAMP WITH TIME ZONE type)
-- setup: ALTER SESSION SET TIME_ZONE = 'America/Sao_Paulo'
SELECT ts FROM tz_corpus WHERE id = 1
-- expect:
2026-06-15 12:00:00 -03:00
-- end

-- case: raw_timestamptz_carries_session_offset_fixed
-- skip: mariadb (MariaDB has no TIMESTAMP WITH TIME ZONE type)
-- setup: ALTER SESSION SET TIME_ZONE = '-05:00'
SELECT ts FROM tz_corpus WHERE id = 1
-- expect:
2026-06-15 12:00:00 -05:00
-- end
