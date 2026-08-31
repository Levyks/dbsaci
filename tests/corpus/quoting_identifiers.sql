# String quoting and identifier quoting. The q'...' alternative-quote operator
# shows up wherever SQL contains apostrophes (addresses, names, generated DDL).

-- case: doubled_apostrophe_literal
SELECT 'O''Reilly' FROM DUAL
-- expect:
O'Reilly
-- end

-- case: q_quote_brackets
SELECT q'[O'Reilly]' FROM DUAL
-- expect:
O'Reilly
-- end

-- case: q_quote_braces
SELECT q'{it's a test}' FROM DUAL
-- expect:
it's a test
-- end

-- case: q_quote_angle
SELECT q'<a'b'c>' FROM DUAL
-- expect:
a'b'c
-- end

-- case: q_quote_parens
SELECT q'(don't stop)' FROM DUAL
-- expect:
don't stop
-- end

-- case: q_quote_custom_delimiter
SELECT q'!plain!' FROM DUAL
-- expect:
plain
-- end

-- case: nq_quote
SELECT nq'[unicode ok]' FROM DUAL
-- expect:
unicode ok
-- end

-- case: quoted_identifier_case_preserved
-- setup: CREATE TABLE "MixedCase" ("Col1" NUMBER)
-- setup: INSERT INTO "MixedCase" ("Col1") VALUES (7)
SELECT "Col1" FROM "MixedCase"
-- expect:
7
-- end

-- case: unquoted_identifier_folds_to_lower_for_backend
-- setup: CREATE TABLE FoldMe (X NUMBER)
-- setup: INSERT INTO foldme (x) VALUES (3)
SELECT X FROM FOLDME
-- expect:
3
-- end

# An all-uppercase double-quoted identifier names the SAME object as the bare
# form (Oracle folds bare identifiers to upper, so "FOO" == FOO). DDL that
# quotes it must stay reachable from later unquoted DML and vice versa.
-- case: uppercase_quoted_identifier_equals_unquoted
-- setup: CREATE TABLE "UP_QUOTED" ("COL_A" NUMBER, "COL_B" VARCHAR2(10))
-- setup: INSERT INTO up_quoted (col_a, col_b) VALUES (5, 'x')
-- setup: INSERT INTO "UP_QUOTED" ("COL_A", "COL_B") VALUES (6, 'y')
SELECT col_a, "COL_B" FROM "UP_QUOTED" ORDER BY col_a
-- expect:
5 | x
6 | y
-- end

-- case: uppercase_quoted_column_join_matches_unquoted
-- setup: CREATE TABLE "UQ_ORG" ("NO" NUMBER, "NAME" VARCHAR2(10))
-- setup: CREATE TABLE uq_acct (org_no NUMBER, label VARCHAR2(10))
-- setup: INSERT INTO "UQ_ORG" ("NO", "NAME") VALUES (1, 'HQ')
-- setup: INSERT INTO uq_acct (org_no, label) VALUES (1, 'main')
SELECT a.label, o.name FROM uq_acct a LEFT JOIN uq_org o ON a.org_no = o.no
-- expect:
main | HQ
-- end

-- case: string_with_embedded_newline_has_expected_length
SELECT LENGTH('line1' || CHR(10) || 'line2') FROM DUAL
-- expect:
11
-- end

-- case: q_quote_uppercase_Q
SELECT Q'[UP]' FROM DUAL
-- expect:
UP
-- end

-- case: q_quote_nested_braces
SELECT q'{outer {inner} outer}' FROM DUAL
-- expect:
outer {inner} outer
-- end

-- case: q_quote_bracket_pairs_angle
SELECT q'<a<b>c>' FROM DUAL
-- expect:
a<b>c
-- end
