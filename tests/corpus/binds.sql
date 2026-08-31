# Bind-variable handling. oracle-rs sends typed bind values over TTC; PgSaci
# currently substitutes them as SQL literals before executing. These cases pin
# the observable behaviour for each scalar type and for placeholder edge cases.

-- case: positional_number_bind
-- bind: int 3
SELECT name FROM people WHERE id = :1
-- expect:
Linus
-- end

-- case: two_positional_binds
-- bind: int 2
-- bind: str Grace
SELECT name FROM people WHERE id = :1 AND name = :2
-- expect:
Grace
-- end

-- case: same_value_two_placeholders
-- bind: int 2
-- bind: int 2
SELECT name FROM people WHERE id = :1 OR team_id = :2 ORDER BY id
-- expect:
Grace
Linus
-- end

-- case: string_bind_with_apostrophe_is_escaped
-- bind: str O'Reilly
SELECT :1 FROM DUAL
-- expect:
O'Reilly
-- end

-- case: number_bind_arithmetic
-- bind: int 41
SELECT :1 + 1 FROM DUAL
-- expect:
42
-- end

-- case: float_bind
-- bind: float 1.25
SELECT :1 * 2 FROM DUAL
-- expect:
2.5
-- end

-- case: null_bind_becomes_sql_null
-- bind: null
SELECT :1 FROM DUAL
-- expect:
NULL
-- end


-- case: date_bind_roundtrip
-- bind: date 2024-02-29 13:14:15
SELECT :1 FROM DUAL
-- expect:
2024-02-29 13:14:15
-- end

-- case: bytes_bind_roundtrip
-- bind: bytes 000102ff
SELECT :1 FROM DUAL
-- expect:
0x000102ff
-- end

-- case: placeholder_inside_string_literal_is_not_a_bind
-- bind: str ignored
SELECT ':1' FROM DUAL
-- expect:
:1
-- end

-- case: bind_in_where_and_projection
-- bind: int 1
-- bind: int 1
SELECT id + :1 FROM people WHERE team_id = :2 ORDER BY id
-- expect:
2
3
-- end

-- case: bind_used_in_insert
-- bind: int 30
-- bind: str BoundInsert
INSERT INTO people (id, name, team_id) VALUES (:1, :2, 2)
-- rowcount: 1
-- end

-- case: bind_insert_lands_in_table
-- bind: int 31
-- setup: INSERT INTO people (id, name, team_id) VALUES (31, 'PreBound', 2)
SELECT name FROM people WHERE id = :1
-- expect:
PreBound
-- end

-- case: null_bind_is_real_null_through_coalesce
-- bind: null
-- bind: str fallback
SELECT COALESCE(:1, :2) FROM DUAL
-- expect:
fallback
-- end

# oracle-rs 0.1.7 mis-frames the Execute descriptor area for this repeated
# bind before PgSaci can recover it.
-- case: single_placeholder_referenced_twice
-- bind: int 2
SELECT name FROM people WHERE id = :1 OR team_id = :1 ORDER BY id
-- expect:
Grace
Linus
-- end

-- case: named_bind
-- bind: int 3
SELECT name FROM people WHERE id = :pid
-- expect:
Linus
-- end

-- case: bind_in_like_pattern
-- bind: str A%
SELECT name FROM people WHERE name LIKE :1 ORDER BY id
-- expect:
Ada
-- end

-- case: bind_in_in_list
-- bind: int 1
-- bind: int 3
SELECT name FROM people WHERE id IN (:1, :2) ORDER BY id
-- expect:
Ada
Linus
-- end

# --- Probes of substitute_bind_values: the inserted literal must not be
# re-scanned for further placeholders, and Oracle rejects a bind-count
# mismatch that PgSaci silently tolerates.

-- case: bind_value_that_looks_like_a_placeholder
-- bind: str :2 and :name
SELECT :1 FROM DUAL
-- expect:
:2 and :name
-- end

-- case: string_bind_with_single_quote_and_placeholder
-- bind: str it's :1 again
SELECT :1 FROM DUAL
-- expect:
it's :1 again
-- end

# Same client-side framing problem as the numbered form above.
-- case: named_bind_reused_twice
-- bind: int 3
SELECT :p + :p FROM DUAL
-- expect:
6
-- end

# oracle-rs mis-frames the execute packet when more values are supplied than the
# SQL has placeholders (client-side).
-- case: surplus_bind_values_are_ignored
-- bind: int 7
-- bind: int 99
SELECT :1 FROM DUAL
-- expect:
7
-- end
