# Analytic / window functions (identical syntax in Oracle and PostgreSQL).

-- case: row_number
SELECT name, ROW_NUMBER() OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
Ada | 1
Grace | 2
Linus | 3
Margaret | 4
-- end

-- case: rank_with_ties
SELECT name, RANK() OVER (ORDER BY team_id) FROM people WHERE team_id IS NOT NULL ORDER BY id
-- expect:
Ada | 1
Grace | 1
Linus | 3
-- end

-- case: dense_rank_with_ties
SELECT name, DENSE_RANK() OVER (ORDER BY team_id) FROM people WHERE team_id IS NOT NULL ORDER BY id
-- expect:
Ada | 1
Grace | 1
Linus | 2
-- end

-- case: count_over_partition
SELECT name, COUNT(*) OVER (PARTITION BY team_id) FROM people WHERE team_id IS NOT NULL ORDER BY id
-- expect:
Ada | 2
Grace | 2
Linus | 1
-- end

-- case: running_sum
SELECT id, SUM(id) OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
1 | 1
2 | 3
3 | 6
4 | 10
-- end

-- case: lag
SELECT name, LAG(name) OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
Ada | NULL
Grace | Ada
Linus | Grace
Margaret | Linus
-- end

-- case: lead
SELECT name, LEAD(name) OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
Ada | Grace
Grace | Linus
Linus | Margaret
Margaret | NULL
-- end

-- case: partitioned_row_number
SELECT name, ROW_NUMBER() OVER (PARTITION BY team_id ORDER BY id) FROM people WHERE team_id IS NOT NULL ORDER BY id
-- expect:
Ada | 1
Grace | 2
Linus | 1
-- end

-- case: sum_over_partition
SELECT name, SUM(id) OVER (PARTITION BY team_id) FROM people WHERE team_id IS NOT NULL ORDER BY id
-- expect:
Ada | 3
Grace | 3
Linus | 3
-- end

-- case: lag_with_offset_and_default
SELECT id, LAG(name, 2, 'none') OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
1 | none
2 | none
3 | Ada
4 | Grace
-- end

-- case: first_value
SELECT id, FIRST_VALUE(name) OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
1 | Ada
2 | Ada
3 | Ada
4 | Ada
-- end

-- case: last_value_with_frame
SELECT id, LAST_VALUE(name) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM people ORDER BY id
-- expect:
1 | Margaret
2 | Margaret
3 | Margaret
4 | Margaret
-- end

-- case: ntile
SELECT id, NTILE(2) OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
1 | 1
2 | 1
3 | 2
4 | 2
-- end

-- case: running_sum_rows_frame
SELECT id, SUM(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM people ORDER BY id
-- expect:
1 | 1
2 | 3
3 | 5
4 | 7
-- end

-- case: rank_keep_dense_rank_first
SELECT MAX(name) KEEP (DENSE_RANK FIRST ORDER BY id) FROM people
-- expect:
Ada
-- end

-- case: ratio_to_report
SELECT id, ROUND(RATIO_TO_REPORT(id) OVER (), 2) FROM people ORDER BY id
-- expect:
1 | 0.1
2 | 0.2
3 | 0.3
4 | 0.4
-- end

-- case: count_distinct_over_is_rejected_without_window
SELECT COUNT(*) OVER (PARTITION BY team_id ORDER BY id) FROM people ORDER BY id
-- expect:
1
2
1
1
-- end

-- case: listagg_as_analytic
SELECT id, LISTAGG(name, ',') WITHIN GROUP (ORDER BY id) OVER (PARTITION BY team_id) FROM people WHERE team_id = 1 ORDER BY id
-- expect:
1 | Ada,Grace
2 | Ada,Grace
-- end

-- case: percentile_cont_median
SELECT PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY id) FROM people
-- expect:
2.5
-- end

-- case: percentile_disc
SELECT PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY id) FROM people
-- expect:
2
-- end

-- case: median_function
SELECT MEDIAN(id) FROM people
-- expect:
2.5
-- end

-- case: cume_dist_over_order
SELECT id, CUME_DIST() OVER (ORDER BY id) FROM people ORDER BY id
-- expect:
1 | 0.25
2 | 0.5
3 | 0.75
4 | 1
-- end

-- case: nth_value_window
SELECT id, NTH_VALUE(name, 2) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM people ORDER BY id
-- expect:
1 | Grace
2 | Grace
3 | Grace
4 | Grace
-- end
