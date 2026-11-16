//! Oracle datetime format models, compiled to MariaDB expressions.
//!
//! `TO_CHAR(<datetime>, '<model>')` becomes a single `DATE_FORMAT` call when the
//! model maps 1:1 onto `DATE_FORMAT` specifiers (the common case), an
//! `UPPER(DATE_FORMAT(…))` when every alphabetic element is upper case, and a
//! `CONCAT` of pieces otherwise. `TO_DATE` models become `STR_TO_DATE`
//! specifiers. Elements with no MariaDB form make compilation fail, and the
//! caller raises ORA-01821 instead of guessing.

/// One compiled output piece of a `TO_CHAR` model.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// `DATE_FORMAT` specifier text (already `%`-escaped), with a case fold.
    Fmt(String, Case),
    /// An expression over `{x}` (the value), e.g. `QUARTER({x})`.
    Expr(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// Output has no letters, or DATE_FORMAT's own case is right.
    Keep,
    Upper,
    Lower,
}

/// Case requested by an alphabetic format element (`MON`, `Mon`, `mon`).
fn elem_case(tok: &str) -> Case {
    let mut letters = tok.chars().filter(|c| c.is_ascii_alphabetic());
    let first = letters.next();
    let rest_upper = tok
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .skip(1)
        .all(|c| c.is_ascii_uppercase());
    match first {
        Some(c) if c.is_ascii_uppercase() && rest_upper => Case::Upper,
        Some(c) if c.is_ascii_uppercase() => Case::Keep, // `Mon` == DATE_FORMAT's
        _ => Case::Lower,
    }
}

/// Does a `DATE_FORMAT` spec produce letters (so a case fold changes it)?
fn has_letters(spec: &str) -> bool {
    let b = spec.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if matches!(b.get(i + 1), Some(b'b' | b'M' | b'W' | b'a' | b'p')) {
                return true;
            }
            i += 2;
            continue;
        }
        if b[i].is_ascii_alphabetic() {
            return true;
        }
        i += 1;
    }
    false
}

fn push_fmt(out: &mut Vec<Piece>, spec: &str, case: Case) {
    if let Some(Piece::Fmt(s, c)) = out.last_mut() {
        let new_letters = has_letters(spec);
        if *c == case || !new_letters {
            s.push_str(spec);
            return;
        }
        if !has_letters(s) {
            s.push_str(spec);
            *c = case;
            return;
        }
    }
    out.push(Piece::Fmt(spec.to_string(), case));
}

fn literal_spec(text: &str) -> String {
    text.replace('%', "%%")
}

/// Compile a `TO_CHAR` datetime model. `None` when an element is unknown.
pub fn compile_to_char(model: &str) -> Option<Vec<Piece>> {
    let m: Vec<char> = model.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut fm = false;
    let mut i = 0;
    let up = |s: &[char]| -> String { s.iter().collect::<String>().to_ascii_uppercase() };
    while i < m.len() {
        let c = m[i];
        if c == '"' {
            let mut j = i + 1;
            let mut lit = String::new();
            while j < m.len() && m[j] != '"' {
                lit.push(m[j]);
                j += 1;
            }
            // Literal text: its own case, never folded.
            push_fmt(&mut out, &literal_spec(&lit), Case::Keep);
            i = j + 1;
            continue;
        }
        if !c.is_ascii_alphanumeric() {
            push_fmt(&mut out, &literal_spec(&c.to_string()), Case::Keep);
            i += 1;
            continue;
        }
        let rest = up(&m[i..m.len().min(i + 6)]);
        let take = |n: usize| -> String { m[i..i + n].iter().collect() };
        macro_rules! fmt {
            ($n:expr, $spec:expr) => {{
                push_fmt(&mut out, $spec, Case::Keep);
                i += $n;
                continue;
            }};
        }
        macro_rules! expr {
            ($n:expr, $e:expr) => {{
                out.push(Piece::Expr($e.to_string()));
                i += $n;
                continue;
            }};
        }
        if rest.starts_with("FM") {
            fm = !fm;
            i += 2;
            continue;
        }
        if rest.starts_with("FX") {
            i += 2;
            continue;
        }
        if rest.starts_with("SYYYY") {
            expr!(
                5,
                "CONCAT(IF(YEAR({x}) < 0, '-', ' '), DATE_FORMAT({x}, '%Y'))"
            )
        }
        if rest.starts_with("YYYY") || rest.starts_with("RRRR") || rest.starts_with("IYYY") {
            let spec = if rest.starts_with("IYYY") { "%x" } else { "%Y" };
            fmt!(4, spec)
        }
        if rest.starts_with("Y,YYY") {
            expr!(5, "INSERT(DATE_FORMAT({x}, '%Y'), 2, 0, ',')")
        }
        if rest.starts_with("YEAR") {
            return None; // spelled-out year
        }
        if rest.starts_with("YYY") || rest.starts_with("IYY") {
            let y = if rest.starts_with('I') { "%x" } else { "%Y" };
            expr!(3, format!("RIGHT(DATE_FORMAT({{x}}, '{y}'), 3)"))
        }
        if rest.starts_with("YY") || rest.starts_with("RR") || rest.starts_with("IY") {
            let y = if rest.starts_with('I') { "%x" } else { "%Y" };
            expr!(2, format!("RIGHT(DATE_FORMAT({{x}}, '{y}'), 2)"))
        }
        if rest.starts_with("HH24") {
            fmt!(4, if fm { "%k" } else { "%H" })
        }
        if rest.starts_with("HH12") {
            fmt!(4, if fm { "%l" } else { "%h" })
        }
        if rest.starts_with("HH") {
            fmt!(2, if fm { "%l" } else { "%h" })
        }
        if rest.starts_with("MONTH") {
            let case = elem_case(&take(5));
            let spec = if fm { "%M" } else { "" };
            if fm {
                push_fmt(&mut out, spec, case);
            } else {
                out.push(Piece::Expr(case_wrap(
                    "RPAD(DATE_FORMAT({x}, '%M'), 9, ' ')",
                    case,
                )));
            }
            i += 5;
            continue;
        }
        if rest.starts_with("MON") {
            let case = elem_case(&take(3));
            push_fmt(&mut out, "%b", case);
            i += 3;
            continue;
        }
        if rest.starts_with("MI") {
            if fm {
                expr!(2, "CAST(MINUTE({x}) AS CHAR)")
            }
            fmt!(2, "%i")
        }
        if rest.starts_with("MM") {
            fmt!(2, if fm { "%c" } else { "%m" })
        }
        if rest.starts_with("RM") {
            expr!(
                2,
                "ELT(MONTH({x}), 'I', 'II', 'III', 'IV', 'V', 'VI', 'VII', 'VIII', 'IX', 'X', 'XI', 'XII')"
            )
        }
        if rest.starts_with("DAY") {
            let case = elem_case(&take(3));
            if fm {
                push_fmt(&mut out, "%W", case);
            } else {
                out.push(Piece::Expr(case_wrap(
                    "RPAD(DATE_FORMAT({x}, '%W'), 9, ' ')",
                    case,
                )));
            }
            i += 3;
            continue;
        }
        if rest.starts_with("DDD") {
            if fm {
                expr!(3, "CAST(DAYOFYEAR({x}) AS CHAR)")
            }
            fmt!(3, "%j")
        }
        if rest.starts_with("DD") {
            fmt!(2, if fm { "%e" } else { "%d" })
        }
        if rest.starts_with("DY") {
            let case = elem_case(&take(2));
            push_fmt(&mut out, "%a", case);
            i += 2;
            continue;
        }
        if rest.starts_with('D') {
            expr!(1, "CAST(DAYOFWEEK({x}) AS CHAR)")
        }
        if rest.starts_with("SSSSS") {
            expr!(5, "LPAD(TIME_TO_SEC(TIME({x})), 5, '0')")
        }
        if rest.starts_with("SS") {
            if fm {
                expr!(2, "CAST(SECOND({x}) AS CHAR)")
            }
            fmt!(2, "%s")
        }
        if rest.starts_with("FF") {
            let digits = m.get(i + 2).and_then(|c| c.to_digit(10));
            let (n, len) = match digits {
                Some(d) if (1..=9).contains(&d) => (d as usize, 3),
                _ => (6, 2),
            };
            if n >= 6 {
                out.push(Piece::Expr(format!(
                    "RPAD(DATE_FORMAT({{x}}, '%f'), {n}, '0')"
                )));
            } else {
                out.push(Piece::Expr(format!("LEFT(DATE_FORMAT({{x}}, '%f'), {n})")));
            }
            i += len;
            continue;
        }
        if rest.starts_with("A.M.") || rest.starts_with("P.M.") {
            let case = elem_case(&take(4));
            out.push(Piece::Expr(case_wrap(
                "IF(HOUR({x}) < 12, 'A.M.', 'P.M.')",
                case,
            )));
            i += 4;
            continue;
        }
        if rest.starts_with("AM") || rest.starts_with("PM") {
            let case = elem_case(&take(2));
            push_fmt(
                &mut out,
                "%p",
                if case == Case::Keep {
                    Case::Upper
                } else {
                    case
                },
            );
            i += 2;
            continue;
        }
        if rest.starts_with("IW") {
            fmt!(2, "%v")
        }
        if rest.starts_with("WW") {
            expr!(2, "LPAD(FLOOR((DAYOFYEAR({x}) - 1) / 7) + 1, 2, '0')")
        }
        if rest.starts_with('W') {
            expr!(1, "CAST(FLOOR((DAY({x}) - 1) / 7) + 1 AS CHAR)")
        }
        if rest.starts_with('Q') {
            expr!(1, "CAST(QUARTER({x}) AS CHAR)")
        }
        if rest.starts_with('J') {
            expr!(1, "CAST(TO_DAYS({x}) + 1721060 AS CHAR)")
        }
        if rest.starts_with("CC") || rest.starts_with("SCC") {
            let n = if rest.starts_with('S') { 3 } else { 2 };
            expr!(n, "LPAD(FLOOR((YEAR({x}) - 1) / 100) + 1, 2, '0')")
        }
        if rest.starts_with("TZH") || rest.starts_with("TZM") || rest.starts_with("TZR") {
            // No time-zone datatypes on MariaDB: render the session offset.
            let part = &rest[..3];
            // The session's UTC offset in minutes, from the server clock (works
            // for named zones and needs no helper routine).
            let e = match part {
                "TZH" => {
                    "CONCAT(IF(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()) < 0, '-', '+'), \
                     LPAD(FLOOR(ABS(ROUND(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()) / 60)) / 60), 2, '0'))"
                }
                "TZM" => {
                    "LPAD(MOD(ABS(ROUND(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()) / 60)), 60), 2, '0')"
                }
                _ => {
                    "CONCAT(IF(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()) < 0, '-', '+'), \
                     LPAD(FLOOR(ABS(ROUND(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()) / 60)) / 60), 2, '0'), ':', \
                     LPAD(MOD(ABS(ROUND(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()) / 60)), 60), 2, '0'))"
                }
            };
            expr!(3, e)
        }
        if rest.starts_with("TZD") {
            // A fixed-offset session has no daylight-saving abbreviation.
            i += 3;
            continue;
        }
        if rest.starts_with('Y') {
            expr!(1, "RIGHT(DATE_FORMAT({x}, '%Y'), 1)")
        }
        if c.is_ascii_digit() {
            push_fmt(&mut out, &c.to_string(), Case::Keep);
            i += 1;
            continue;
        }
        return None;
    }
    Some(out)
}

fn case_wrap(e: &str, case: Case) -> String {
    match case {
        Case::Keep => e.to_string(),
        Case::Upper => format!("UPPER({e})"),
        Case::Lower => format!("LOWER({e})"),
    }
}

/// Render compiled pieces against the value expression `x`. `x` is inserted
/// once per piece, so callers pass a column/literal or a captured variable.
pub fn render_pieces(pieces: &[Piece], x: &str) -> String {
    let parts: Vec<String> = pieces
        .iter()
        .filter(|p| !matches!(p, Piece::Fmt(s, _) if s.is_empty()))
        .map(|p| match p {
            Piece::Fmt(spec, case) => case_wrap(
                &format!("DATE_FORMAT({x}, '{}')", spec.replace('\'', "''")),
                *case,
            ),
            Piece::Expr(e) => e.replace("{x}", x),
        })
        .collect();
    match parts.len() {
        0 => "NULL".to_string(),
        1 => parts.into_iter().next().unwrap(),
        _ => format!("CONCAT({})", parts.join(", ")),
    }
}

/// How many times `render_pieces` mentions the value.
pub fn value_uses(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Fmt(s, _) if s.is_empty() => 0,
            Piece::Fmt(..) => 1,
            Piece::Expr(e) => e.matches("{x}").count(),
        })
        .sum()
}

/// Compile a `TO_DATE`/`TO_TIMESTAMP` model to a `STR_TO_DATE` specifier.
/// Returns `(spec, uses_rr)`; `None` for unsupported elements.
pub fn compile_to_date(model: &str) -> Option<(String, bool)> {
    let m: Vec<char> = model.chars().collect();
    let mut out = String::new();
    let mut rr = false;
    let mut i = 0;
    while i < m.len() {
        let c = m[i];
        if c == '"' {
            let mut j = i + 1;
            while j < m.len() && m[j] != '"' {
                if m[j] == '%' {
                    out.push('%');
                }
                out.push(m[j]);
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if !c.is_ascii_alphanumeric() {
            if c == '%' {
                out.push('%');
            }
            out.push(c);
            i += 1;
            continue;
        }
        let rest: String = m[i..m.len().min(i + 5)]
            .iter()
            .collect::<String>()
            .to_ascii_uppercase();
        let table: &[(&str, &str)] = &[
            ("SYYYY", "%Y"),
            ("YYYY", "%Y"),
            ("RRRR", "%Y"),
            ("HH24", "%H"),
            ("HH12", "%h"),
            ("MONTH", "%M"),
            ("A.M.", "%p"),
            ("P.M.", "%p"),
            ("SSSSS", ""),
            ("FF", "%f"),
            ("MON", "%b"),
            ("DAY", "%W"),
            ("DDD", "%j"),
            ("DD", "%d"),
            ("DY", "%a"),
            ("MM", "%m"),
            ("MI", "%i"),
            ("SS", "%s"),
            ("HH", "%h"),
            ("AM", "%p"),
            ("PM", "%p"),
            ("YY", "%y"),
            ("RR", "%y"),
            ("FM", ""),
            ("FX", ""),
        ];
        let Some((tok, spec)) = table.iter().find(|(t, _)| rest.starts_with(t)) else {
            if c.is_ascii_digit() {
                out.push(c);
                i += 1;
                continue;
            }
            return None;
        };
        if *tok == "SSSSS" {
            return None;
        }
        if *tok == "RR" {
            rr = true;
        }
        out.push_str(spec);
        i += tok.len();
        // `FF1`..`FF9`: the digit is part of the element.
        if *tok == "FF" && m.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    }
    Some((out, rr))
}

/// Default `NLS_DATE_FORMAT` (`DD-MON-RR`) as a `STR_TO_DATE` spec.
pub const DEFAULT_DATE_SPEC: &str = "%d-%b-%y";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_date_format_when_possible() {
        let p = compile_to_char("YYYY-MM-DD HH24:MI:SS").unwrap();
        assert_eq!(
            render_pieces(&p, "x"),
            "DATE_FORMAT(x, '%Y-%m-%d %H:%i:%s')"
        );
        let p = compile_to_char("DD-MON-YYYY").unwrap();
        assert_eq!(render_pieces(&p, "x"), "UPPER(DATE_FORMAT(x, '%d-%b-%Y'))");
        let p = compile_to_char("Mon dy").unwrap();
        assert_eq!(
            render_pieces(&p, "x"),
            "CONCAT(DATE_FORMAT(x, '%b '), LOWER(DATE_FORMAT(x, '%a')))"
        );
        let p = compile_to_char("Mon").unwrap();
        assert_eq!(render_pieces(&p, "x"), "DATE_FORMAT(x, '%b')");
    }

    #[test]
    fn fm_and_padding() {
        let p = compile_to_char("FMDD Month YYYY").unwrap();
        assert_eq!(render_pieces(&p, "x"), "DATE_FORMAT(x, '%e %M %Y')");
        let p = compile_to_char("DAY").unwrap();
        assert_eq!(
            render_pieces(&p, "x"),
            "UPPER(RPAD(DATE_FORMAT(x, '%W'), 9, ' '))"
        );
        let p = compile_to_char("FF3").unwrap();
        assert_eq!(render_pieces(&p, "x"), "LEFT(DATE_FORMAT(x, '%f'), 3)");
        let p = compile_to_char("YYYY\"%Q\"Q").unwrap();
        assert_eq!(
            render_pieces(&p, "x"),
            "CONCAT(DATE_FORMAT(x, '%Y%%Q'), CAST(QUARTER(x) AS CHAR))"
        );
    }

    #[test]
    fn to_date_specs() {
        assert_eq!(
            compile_to_date("YYYY-MM-DD HH24:MI:SS"),
            Some(("%Y-%m-%d %H:%i:%s".into(), false))
        );
        assert_eq!(
            compile_to_date("DD-MON-RR"),
            Some(("%d-%b-%y".into(), true))
        );
        assert_eq!(compile_to_date("SSSSS"), None);
    }
}
