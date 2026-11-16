# Regression cases for the 0.3.0 translator review: rules must not fire
# inside literals or comments, typed date arithmetic, (+) and FULL JOIN in
# subqueries, precedence, NULL ordering, format models, regexp case,
# multi-table INSERT / MERGE semantics. Values are the Oracle results.

-- fixture: CREATE TABLE IF NOT EXISTS rv_tree (id INT PRIMARY KEY, name TEXT, mgr INT)
-- fixture: TRUNCATE rv_tree
-- fixture: INSERT INTO rv_tree VALUES (1,'root',NULL),(2,'a',1),(9,'n9',1),(10,'n10',1)
-- fixture: CREATE TABLE IF NOT EXISTS rv_flags (id INT, dual_flag INT, uid_x INT)
-- fixture: TRUNCATE rv_flags
-- fixture: INSERT INTO rv_flags VALUES (1, 7, 8)
-- fixture: CREATE TABLE IF NOT EXISTS rv_mt (id INT, v VARCHAR(10))
-- fixture: CREATE TABLE IF NOT EXISTS rv_a (x INT)
-- fixture: CREATE TABLE IF NOT EXISTS rv_b (x INT)

-- case: literal_text_is_not_rewritten
SELECT 'use TO_NUMBER(x) here, SYSDATE - 1, dual' FROM DUAL
-- expect:
use TO_NUMBER(x) here, SYSDATE - 1, dual
-- end

-- case: comment_with_apostrophe
SELECT 1 /* it's (+) */ FROM DUAL -- isn't it
-- expect:
1
-- end

-- case: column_names_containing_dual_and_uid
SELECT id, dual_flag, uid_x FROM rv_flags
-- expect:
1 | 7 | 8
-- end

-- case: string_plus_number_keeps_precedence
SELECT '1' + 2 * 3 FROM DUAL
-- expect:
7
-- end

-- case: date_difference_keeps_fraction
SELECT ROUND((DATE '2024-01-02' + 1/24 - DATE '2024-01-02') * 24, 6) FROM DUAL
-- expect:
1
-- end

-- case: outer_join_marker_in_subquery
SELECT COUNT(*) FROM (SELECT p.name FROM people p, teams t WHERE p.team_id = t.id(+)) q
-- expect:
4
-- end

-- case: outer_join_marker_with_or_is_ora_01719
SELECT p.name FROM people p, teams t WHERE p.team_id = t.id(+) OR p.id = 1
-- error: ORA-01719
-- end

-- case: full_join_in_subquery
SELECT COUNT(*) FROM (SELECT p.name, t.name AS tn FROM people p FULL JOIN teams t ON p.team_id = t.id) q
-- expect:
5
-- end

-- case: full_join_or_condition_no_duplicates
SELECT COUNT(*) FROM people p FULL OUTER JOIN teams t ON p.team_id = t.id OR p.id = t.id
-- expect:
6
-- end

-- case: full_join_grouped
SELECT t.name, COUNT(p.id) FROM people p FULL JOIN teams t ON p.team_id = t.id GROUP BY t.name ORDER BY t.name
-- expect:
Engineering | 2
Marketing | 0
Sales | 1
NULL | 1
-- end

-- case: desc_nulls_first_with_fetch
SELECT name FROM people ORDER BY team_id DESC, id FETCH FIRST 2 ROWS ONLY
-- expect:
Margaret
Linus
-- end

-- case: positional_order_desc_nulls_first
SELECT team_id, name FROM people ORDER BY 1 DESC, 2
-- expect:
NULL | Margaret
2 | Linus
1 | Ada
1 | Grace
-- end

-- case: to_char_month_abbrev_upper
SELECT TO_CHAR(DATE '2024-03-05', 'DD-MON-YY') FROM DUAL
-- expect:
05-MAR-24
-- end

-- case: to_char_day_is_blank_padded
SELECT LENGTH(TO_CHAR(DATE '2024-03-05', 'Day')) FROM DUAL
-- expect:
9
-- end

-- case: to_char_ff3
SELECT TO_CHAR(TIMESTAMP '2024-03-05 14:07:09.123456', 'HH24:MI:SS.FF3') FROM DUAL
-- expect:
14:07:09.123
-- end

-- case: to_char_quarter_and_literal
SELECT TO_CHAR(DATE '2024-08-15', 'YYYY"Q"Q') FROM DUAL
-- expect:
2024Q3
-- end

-- case: to_char_number_keeps_decimals
SELECT TO_CHAR(1234.5, 'FM999G990D00') FROM DUAL
-- expect:
1,234.50
-- end

-- case: regexp_like_is_case_sensitive
SELECT CASE WHEN REGEXP_LIKE('ABC', 'abc') THEN 'y' ELSE 'n' END, CASE WHEN REGEXP_LIKE('ABC', 'abc', 'i') THEN 'y' ELSE 'n' END FROM DUAL
-- expect:
n | y
-- end

-- case: regexp_like_i_keeps_class_escapes
SELECT CASE WHEN REGEXP_LIKE('a1', '^A\D', 'i') THEN 'y' ELSE 'n' END FROM DUAL
-- expect:
n
-- end

-- case: for_update_wait
SELECT name FROM people WHERE id = 1 FOR UPDATE WAIT 3
-- expect:
Ada
-- end

-- case: listagg_default_delimiter_is_empty
SELECT LISTAGG(name) WITHIN GROUP (ORDER BY id) FROM people WHERE team_id = 1
-- expect:
AdaGrace
-- end

-- case: order_siblings_by_is_numeric
SELECT name FROM rv_tree START WITH mgr IS NULL CONNECT BY PRIOR id = mgr ORDER SIBLINGS BY id
-- expect:
root
a
n9
n10
-- end

-- case: recursive_cte_with_recursive_named_column
WITH r (n) AS (SELECT 1 FROM DUAL UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n AS recursive_flag FROM r ORDER BY 1
-- expect:
1
2
3
-- end

-- case: insert_first_null_condition_goes_to_else
-- setup: DELETE FROM rv_a
-- setup: DELETE FROM rv_b
-- setup: INSERT FIRST WHEN x > 1 THEN INTO rv_a (x) VALUES (x) ELSE INTO rv_b (x) VALUES (x) SELECT CAST(NULL AS NUMBER) x FROM DUAL UNION ALL SELECT 5 FROM DUAL
SELECT (SELECT COUNT(*) FROM rv_a) || '/' || (SELECT COUNT(*) FROM rv_b) FROM DUAL
-- expect:
1/1
-- end

-- case: insert_first_inserts_each_row_once
-- setup: DELETE FROM rv_a
-- setup: INSERT FIRST WHEN x > 1 THEN INTO rv_a (x) VALUES (x) WHEN x > 0 THEN INTO rv_a (x) VALUES (x) SELECT 5 x FROM DUAL
SELECT COUNT(*) FROM rv_a
-- expect:
1
-- end

-- case: merge_without_unique_key_honours_on
-- setup: DELETE FROM rv_mt
-- setup: INSERT INTO rv_mt (id, v) VALUES (1, 'a')
-- setup: MERGE INTO rv_mt d USING (SELECT 1 id, 'b' v FROM DUAL UNION ALL SELECT 2, 'c' FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.v = s.v WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)
SELECT id, v FROM rv_mt ORDER BY id
-- expect:
1 | b
2 | c
-- end

-- case: merge_update_where_and_insert_where
-- setup: DELETE FROM rv_mt
-- setup: INSERT INTO rv_mt (id, v) VALUES (1, 'a')
-- setup: INSERT INTO rv_mt (id, v) VALUES (3, 'z')
-- setup: MERGE INTO rv_mt d USING (SELECT 1 id, 'b' v FROM DUAL UNION ALL SELECT 2, 'c' FROM DUAL UNION ALL SELECT 3, 'y' FROM DUAL UNION ALL SELECT 4, 'q' FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.v = s.v WHERE s.id <> 3 WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v) WHERE s.id <> 4
SELECT id, v FROM rv_mt ORDER BY id
-- expect:
1 | b
2 | c
3 | z
-- end

-- case: merge_delete_only_updated_rows
-- setup: DELETE FROM rv_mt
-- setup: INSERT INTO rv_mt (id, v) VALUES (1, 'a')
-- setup: MERGE INTO rv_mt d USING (SELECT 1 id, 'gone' v FROM DUAL UNION ALL SELECT 2, 'gone' FROM DUAL) s ON (d.id = s.id) WHEN MATCHED THEN UPDATE SET d.v = s.v DELETE WHERE d.v = 'gone' WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)
SELECT id, v FROM rv_mt ORDER BY id
-- expect:
2 | gone
-- end
