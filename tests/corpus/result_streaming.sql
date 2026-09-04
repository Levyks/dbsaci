# Result-set delivery. DbSaci pulls rows out of PostgreSQL incrementally (the
# backend result is never buffered whole) but still packs them into one TTC
# Data packet for the client. These cases push the row count up to make sure
# nothing breaks between "streamed from PG" and "delivered in one packet".

-- fixture: CREATE TABLE IF NOT EXISTS streaming_rows AS SELECT g AS n FROM generate_series(1, 20000) g

-- case: fifty_rows
SELECT n FROM streaming_rows WHERE n <= 50 ORDER BY n
-- rows: 50
-- end

-- case: five_hundred_rows
SELECT n FROM streaming_rows WHERE n <= 500 ORDER BY n
-- rows: 500
-- end

-- case: two_thousand_rows
SELECT n FROM streaming_rows WHERE n <= 2000 ORDER BY n
-- rows: 2000
-- end

-- case: ten_thousand_rows
SELECT n FROM streaming_rows WHERE n <= 10000 ORDER BY n
-- rows: 10000
-- end

-- case: twenty_thousand_rows
SELECT n FROM streaming_rows ORDER BY n
-- rows: 20000
-- end

-- case: wide_rows_many_columns
SELECT n, n * 2, 'padding text value for width ' || n, n / 3.0 FROM streaming_rows WHERE n <= 1000 ORDER BY n
-- rows: 1000
-- end

# 5000 rows streamed out of PG one RowStream item at a time, then delivered in
# a single packet. Its job is to stay green now that reads go through the
# streamed RowCursor rather than a buffered client.query().
-- case: large_result_via_fetch_loop
SELECT g FROM generate_series(1, 5000) g ORDER BY g
-- rows: 5000
-- end

-- case: large_result_first_and_last_correct
SELECT min(g), max(g), count(*) FROM (SELECT g FROM generate_series(1, 5000) g ORDER BY g) q
-- expect:
1 | 5000 | 5000
-- end

-- case: streamed_rows_keep_order
SELECT g FROM (SELECT generate_series(1, 300) g) q WHERE g BETWEEN 148 AND 152 ORDER BY g
-- expect:
148
149
150
151
152
-- end

# A result whose single TTC packet would exceed MAX_TNS_PACKET_SIZE (64 MiB).
# This exercises true Execute + repeated Fetch streaming through the corpus
# client: the first batch is bounded by the prefetch size, and continuation
# fetches pull the remaining rows without buffering the whole result in DbSaci.
-- case: result_larger_than_one_packet
SELECT g, repeat('x', 200) FROM generate_series(1, 400000) g
-- rows: 400000
-- end

# Definition-of-done check: a >= 1,000,000-row result streams through Execute +
# repeated Fetch without DbSaci buffering the whole result and without a single
# oversized packet.
-- case: one_million_row_stream
SELECT g FROM generate_series(1, 1000000) g
-- rows: 1000000
-- end
