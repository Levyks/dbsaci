# Oracle string functions. Names with no PostgreSQL builtin (INSTR) resolve to
# orafce's `oracle` schema; the rest are shared with PostgreSQL.

-- case: upper
SELECT UPPER('oracle') FROM DUAL
-- expect:
ORACLE
-- end

-- case: lower
SELECT LOWER('ORACLE') FROM DUAL
-- expect:
oracle
-- end

-- case: initcap
SELECT INITCAP('hello world') FROM DUAL
-- expect:
Hello World
-- end

-- case: length
SELECT LENGTH('abcdef') FROM DUAL
-- expect:
6
-- end

-- case: substr_positive
SELECT SUBSTR('abcdef', 2, 3) FROM DUAL
-- expect:
bcd
-- end

-- case: substr_to_end
SELECT SUBSTR('abcdef', 4) FROM DUAL
-- expect:
def
-- end

-- case: instr_basic
SELECT INSTR('Tech on the net', 'e') FROM DUAL
-- expect:
2
-- end

-- case: instr_nth_occurrence
SELECT INSTR('abcabcabc', 'bc', 1, 2) FROM DUAL
-- expect:
5
-- end

-- case: instr_not_found
SELECT INSTR('abc', 'z') FROM DUAL
-- expect:
0
-- end

-- case: instr_from_negative_start
SELECT INSTR('Tech on the net', 'e', -3, 2) FROM DUAL
-- expect:
2
-- end

-- case: lpad
SELECT LPAD('7', 4, '0') FROM DUAL
-- expect:
0007
-- end

-- case: rpad
SELECT RPAD('x', 4, '!') FROM DUAL
-- expect:
x!!!
-- end

-- case: ltrim_set
SELECT LTRIM('00042', '0') FROM DUAL
-- expect:
42
-- end

-- case: rtrim_set
SELECT RTRIM('42000', '0') FROM DUAL
-- expect:
42
-- end

-- case: trim_both
SELECT TRIM('  x  ') FROM DUAL
-- expect:
x
-- end

-- case: replace
SELECT REPLACE('a-b-c', '-', '+') FROM DUAL
-- expect:
a+b+c
-- end

-- case: translate
SELECT TRANSLATE('abcdef', 'ace', 'ACE') FROM DUAL
-- expect:
AbCdEf
-- end

-- case: reverse
SELECT REVERSE('abc') FROM DUAL
-- expect:
cba
-- end

-- case: ascii
SELECT ASCII('A') FROM DUAL
-- expect:
65
-- end

-- case: chr
SELECT CHR(65) FROM DUAL
-- expect:
A
-- end

-- case: concat_two_args
SELECT CONCAT('ora', 'cle') FROM DUAL
-- expect:
oracle
-- end

-- case: concat_pipes_three
SELECT 'a' || 'b' || 'c' FROM DUAL
-- expect:
abc
-- end

-- case: substr_negative_start
SELECT SUBSTR('TechOnTheNet', -3, 3) FROM DUAL
-- expect:
Net
-- end

-- case: substr_negative_start_to_end
SELECT SUBSTR('TechOnTheNet', -6) FROM DUAL
-- expect:
TheNet
-- end

-- case: instr_negative_start
SELECT INSTR('abcabcabc', 'abca', -1) FROM DUAL
-- expect:
4
-- end

-- case: trim_leading_char
SELECT TRIM(LEADING '0' FROM '000123') FROM DUAL
-- expect:
123
-- end

-- case: trim_trailing_char
SELECT TRIM(TRAILING 'x' FROM 'testxxx') FROM DUAL
-- expect:
test
-- end

-- case: trim_both_char
SELECT TRIM('*' FROM '***mid***') FROM DUAL
-- expect:
mid
-- end

-- case: listagg_basic
SELECT LISTAGG(name, ', ') WITHIN GROUP (ORDER BY id) FROM people WHERE team_id = 1
-- expect:
Ada, Grace
-- end

-- case: listagg_grouped
SELECT team_id, LISTAGG(name, '|') WITHIN GROUP (ORDER BY name) FROM people WHERE team_id IS NOT NULL GROUP BY team_id ORDER BY team_id
-- expect:
1 | Ada|Grace
2 | Linus
-- end

-- case: listagg_distinct
SELECT LISTAGG(DISTINCT team_id, ',') WITHIN GROUP (ORDER BY team_id) FROM people WHERE team_id IS NOT NULL
-- expect:
1,2
-- end

-- case: translate_remove_chars
SELECT TRANSLATE('a1b2c3', '0123456789', ' ') FROM DUAL
-- expect:
abc
-- end

-- case: substr_with_length_beyond_end
SELECT SUBSTR('abc', 2, 100) FROM DUAL
-- expect:
bc
-- end

-- case: rpad_truncates_when_shorter
SELECT RPAD('abcdef', 3) FROM DUAL
-- expect:
abc
-- end

-- case: ascii_of_multichar_takes_first
SELECT ASCII('ABC') FROM DUAL
-- expect:
65
-- end

-- case: nvl_on_empty_concat
SELECT NVL(name, 'anon') || '!' FROM people WHERE id = 1
-- expect:
Ada!
-- end
