//! The token/expression rule pass: one walk over a canonical statement that
//! copies every byte no rule touches and rewrites function calls,
//! pseudo-columns, typed arithmetic, `ORDER BY` NULL placement, DDL types,
//! reserved-word identifiers and dictionary names.

use std::cell::RefCell;

use super::datefmt;
use super::lex::{K, KwSet, NONE, Ts};
use super::{MariaDbContext, StmtKind};
use crate::error::{Error, Result};
use crate::mariadb::dictionary;
use crate::translate::schema::ValueKind;

/// Value class of an expression, as far as the translator can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vk {
    Num,
    Text,
    Date,
    Ts,
    Bin,
    Interval,
    Bool,
    Unknown,
}

impl Vk {
    fn dateish(self) -> bool {
        matches!(self, Vk::Date | Vk::Ts)
    }
    fn from_value(v: ValueKind) -> Vk {
        match v {
            ValueKind::Number => Vk::Num,
            ValueKind::Text => Vk::Text,
            ValueKind::Date => Vk::Date,
            ValueKind::Timestamp => Vk::Ts,
            ValueKind::Binary => Vk::Bin,
            ValueKind::Unknown => Vk::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub a: usize,
    pub b: usize,
    pub kind: Nk,
    pub kids: Vec<Node>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nk {
    /// One significant token: literal, bind, variable, `*`, NULL.
    Atom,
    /// `a.b.c` identifier chain.
    Path,
    /// `name(args)` plus trailing `WITHIN GROUP (…)`, `KEEP (…)`, `OVER …`.
    Call { open: usize, close: usize },
    /// `( … )`, also `ANY (…)` / `ALL (…)` / `EXISTS (…)`.
    Paren { open: usize, close: usize },
    /// `CASE … END`.
    Case { end: usize },
    /// `DATE '…'` / `TIMESTAMP '…'`.
    TypedLit,
    /// `INTERVAL '…' unit [TO unit]`.
    Interval,
    /// Prefix operator (kid 0).
    Unary { op: usize },
    /// Binary operator `op` (kids 0, 1).
    Bin { op: usize },
    /// Comparison / logical / postfix construct; kids rendered in place.
    Other,
}

static STOP_SET: KwSet = KwSet::new(STOP);
static RESERVED_SET: KwSet = KwSet::new(MARIADB_EXTRA_RESERVED);

/// Words that never start an expression operand.
const STOP: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "UNION",
    "INTERSECT",
    "MINUS",
    "EXCEPT",
    "DISTINCT",
    "UNIQUE",
    "AS",
    "ON",
    "JOIN",
    "LEFT",
    "RIGHT",
    "FULL",
    "INNER",
    "OUTER",
    "CROSS",
    "NATURAL",
    "USING",
    "INTO",
    "VALUES",
    "SET",
    "UPDATE",
    "DELETE",
    "INSERT",
    "MERGE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "AND",
    "OR",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "ESCAPE",
    "FOR",
    "CONNECT",
    "START",
    "WITH",
    "NOCYCLE",
    "FETCH",
    "OFFSET",
    "LIMIT",
    "ASC",
    "DESC",
    "NULLS",
    "RETURNING",
    "RETURN",
    "BEGIN",
    "DECLARE",
    "CREATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "GRANT",
    "REVOKE",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "PARTITION",
    "OVER",
    "WITHIN",
    "KEEP",
    "PIVOT",
    "UNPIVOT",
    "LATERAL",
    "CHECK",
    "CONSTRAINT",
    "PRIMARY",
    "FOREIGN",
    "REFERENCES",
    "DEFAULT",
    "IF",
    "LOOP",
    "WHILE",
    "EXIT",
    "ELSIF",
    "EXCEPTION",
    "RAISE",
    "PRAGMA",
    "CURSOR",
    "OPEN",
    "CLOSE",
    "GOTO",
    "SEPARATOR",
    "ALL",
    "ANY",
    "SOME",
    "EXISTS",
    "LOCK",
    "MODEL",
    "WINDOW",
    "FORALL",
    "BULK",
    "COLLECT",
    "EXECUTE",
    "IMMEDIATE",
];

/// Words MariaDB's `SQL_MODE=ORACLE` reserves that Oracle allows as plain
/// identifiers; back-quoted when they appear in an identifier position.
const MARIADB_EXTRA_RESERVED: &[&str] = &[
    "BODY",
    "OPTION",
    "RANK",
    "ROWS",
    "GROUPS",
    "SYSTEM",
    "LEAD",
    "LAG",
    "OVER",
    "WINDOW",
    "RECURSIVE",
    "EXCEPT",
    "INTERSECT",
    "DIV",
    "KEYS",
    "READS",
    "RELEASE",
    "REPEAT",
    "REQUIRE",
    "SPATIAL",
    "SQL",
    "STARTING",
    "STRAIGHT_JOIN",
    "TERMINATED",
    "UNDO",
    "UNLOCK",
    "UNSIGNED",
    "USAGE",
    "UTC_DATE",
    "WRITE",
    "XOR",
    "YEAR_MONTH",
    "ZEROFILL",
    "CHANGE",
    "DATABASE",
    "DATABASES",
    "DUAL_X",
    "FULLTEXT",
    "IGNORE",
    "INFILE",
    "KILL",
    "LINES",
    "LOAD",
    "LONGBLOB",
    "LONGTEXT",
    "MEDIUMINT",
    "MOD",
    "PURGE",
    "RANGE",
    "READ",
    "REGEXP",
    "RENAME",
    "REPLACE",
    "SCHEMA",
    "SCHEMAS",
    "SHOW",
    "STATS_AUTO_RECALC",
];

/// Clause a token sits in (per paren level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clause {
    None,
    Select,
    From,
    On,
    Other,
}

pub struct Render<'a> {
    pub ts: &'a Ts<'a>,
    pub cx: &'a MariaDbContext<'a>,
    pub stmt: StmtKind,
    /// Ordinal of each `?` token among the statement's binds.
    bind_ord: Vec<u32>,
    clause: Vec<Clause>,
    /// `(paren block, correlation name, schema, table)`.
    scopes: Vec<(usize, String, Option<String>, Option<String>)>,
    pub err: RefCell<Option<Error>>,
    kinds: RefCell<std::collections::HashMap<(usize, usize), Vk>>,
}

fn up(s: &str) -> String {
    s.to_ascii_uppercase()
}

/// Strip `"…"` / `` `…` `` quoting from an identifier token's text.
pub fn ident_text(t: &str) -> &str {
    if t.len() >= 2 && (t.starts_with('"') || t.starts_with('`')) {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

impl<'a> Render<'a> {
    pub fn new(ts: &'a Ts<'a>, cx: &'a MariaDbContext<'a>, stmt: StmtKind, bind_base: u32) -> Self {
        let n = ts.len();
        let mut bind_ord = vec![u32::MAX; n];
        let mut k = bind_base;
        for (i, t) in ts.t.iter().enumerate() {
            if t.k == K::Bind {
                bind_ord[i] = k;
                k += 1;
            }
        }
        let mut r = Render {
            ts,
            cx,
            stmt,
            bind_ord,
            clause: vec![Clause::None; n],
            scopes: Vec::new(),
            err: RefCell::new(None),
            kinds: RefCell::new(std::collections::HashMap::new()),
        };
        r.analyze();
        r
    }

    fn fail(&self, e: Error) {
        let mut slot = self.err.borrow_mut();
        if slot.is_none() {
            *slot = Some(e);
        }
    }

    // -----------------------------------------------------------------------
    // pre-pass: clause state and FROM-item scopes
    // -----------------------------------------------------------------------

    fn analyze(&mut self) {
        let ts = self.ts;
        let n = ts.len();
        // state per paren (index NONE == top level) — kept in a small stack.
        let mut stack: Vec<(usize, Clause, bool)> = vec![(NONE, Clause::None, false)];
        let mut i = 0;
        while i < n {
            let k = ts.k(i);
            match k {
                K::LParen => {
                    self.clause[i] = stack.last().unwrap().1;
                    stack.push((i, Clause::None, false));
                    i += 1;
                    continue;
                }
                K::RParen => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                    self.clause[i] = stack.last().unwrap().1;
                    i += 1;
                    continue;
                }
                _ => {}
            }
            let top = stack.last_mut().unwrap();
            if k == K::Word {
                let w = ts.text(i);
                let is = |kw: &str| w.eq_ignore_ascii_case(kw);
                if is("SELECT") || is("DELETE") || is("UPDATE") || is("INSERT") || is("MERGE") {
                    top.2 = true;
                }
                if is("SELECT") {
                    top.1 = Clause::Select;
                } else if (is("FROM") && top.2) || (is("JOIN") && top.1 == Clause::On) {
                    top.1 = Clause::From;
                } else if is("ON") && top.1 == Clause::From {
                    top.1 = Clause::On;
                } else if [
                    "WHERE",
                    "GROUP",
                    "ORDER",
                    "HAVING",
                    "CONNECT",
                    "START",
                    "UNION",
                    "MINUS",
                    "INTERSECT",
                    "EXCEPT",
                    "SET",
                    "VALUES",
                    "FETCH",
                    "OFFSET",
                    "LIMIT",
                    "FOR",
                    "PIVOT",
                    "UNPIVOT",
                    "RETURNING",
                    "WINDOW",
                    "MODEL",
                    "USING",
                ]
                .iter()
                .any(|kw| is(kw))
                    && top.1 != Clause::None
                {
                    top.1 = Clause::Other;
                }
            } else if k == K::Comma && top.1 == Clause::On {
                top.1 = Clause::From;
            }
            self.clause[i] = top.1;
            i += 1;
        }
        // scopes: table references in table position
        for i in 0..n {
            if !matches!(ts.k(i), K::Word | K::Quoted | K::Back | K::LParen) {
                continue;
            }
            if !self.table_position(i) {
                continue;
            }
            let block = ts.parent[i];
            if ts.k(i) == K::LParen {
                let close = ts.pair[i];
                if close == NONE {
                    continue;
                }
                if let Some((alias, _)) = self.alias_after(close + 1) {
                    self.scopes.push((block, alias, None, None));
                }
                continue;
            }
            let (parts, end) = self.path_parts(i);
            if parts.is_empty() {
                continue;
            }
            let table = ident_text(ts.text(*parts.last().unwrap())).to_string();
            let schema =
                (parts.len() >= 2).then(|| ident_text(ts.text(parts[parts.len() - 2])).to_string());
            let alias = self
                .alias_after(end)
                .map(|(a, _)| a)
                .unwrap_or_else(|| table.clone());
            self.scopes.push((block, alias, schema, Some(table)));
        }
    }

    /// Identifier tokens of the dotted path starting at `i`, and its end.
    pub fn path_parts(&self, i: usize) -> (Vec<usize>, usize) {
        let ts = self.ts;
        let mut parts = vec![];
        let mut j = i;
        loop {
            if !matches!(ts.k(j), K::Word | K::Quoted | K::Back) {
                break;
            }
            parts.push(j);
            let d = j + 1;
            if d < ts.len() && ts.k(d) == K::Dot && d + 1 < ts.len() {
                if matches!(ts.k(d + 1), K::Word | K::Quoted | K::Back) {
                    j = d + 1;
                    continue;
                }
                if ts.is_op(d + 1, "*") {
                    return (parts, d + 2);
                }
            }
            return (parts, j + 1);
        }
        (parts, j)
    }

    /// `[AS] alias` after a table reference ending at `end`.
    pub fn alias_after(&self, end: usize) -> Option<(String, usize)> {
        let ts = self.ts;
        let mut j = ts.sig(end, ts.len());
        if ts.is(j, "AS") {
            j = ts.next(j, ts.len());
        }
        if j >= ts.len() {
            return None;
        }
        match ts.k(j) {
            K::Quoted | K::Back => Some((ident_text(ts.text(j)).to_string(), j + 1)),
            K::Word if !self.is_clause_word(j) => Some((ts.text(j).to_string(), j + 1)),
            _ => None,
        }
    }

    fn is_clause_word(&self, j: usize) -> bool {
        self.ts.in_set(j, &STOP_SET)
            || self.ts.is_any(
                j,
                &[
                    "CONNECT",
                    "START",
                    "SAMPLE",
                    "PARTITION",
                    "NATURAL",
                    "STRAIGHT_JOIN",
                    "FORCE",
                    "IGNORE",
                    "USE",
                    "PIVOT",
                    "UNPIVOT",
                    "LOG",
                    "MODEL",
                ],
            )
    }

    /// Is token `i` a relation name position (`FROM x`, `JOIN x`, …)?
    pub fn table_position(&self, i: usize) -> bool {
        let ts = self.ts;
        let p = ts.prev(i, 0);
        if p == NONE {
            return false;
        }
        if ts.is_any(p, &["FROM", "JOIN", "STRAIGHT_JOIN"]) {
            // `EXTRACT(x FROM y)`, `TRIM(… FROM y)`: not a FROM clause.
            return self.clause[p] == Clause::From;
        }
        if ts.is_any(p, &["INTO", "UPDATE"]) {
            return matches!(
                self.stmt,
                StmtKind::Insert | StmtKind::Update | StmtKind::Merge | StmtKind::Other
            ) && !self.stmt_is_plsql();
        }
        if ts.k(p) == K::Comma {
            return self.clause[p] == Clause::From && self.clause[i] == Clause::From;
        }
        false
    }

    fn stmt_is_plsql(&self) -> bool {
        matches!(self.stmt, StmtKind::Plsql)
    }

    // -----------------------------------------------------------------------
    // expression parser
    // -----------------------------------------------------------------------

    /// Parse `a..end` fully as one expression.
    pub fn parse_full(&self, a: usize, end: usize) -> Option<Node> {
        let (a, end) = self.ts.trim(a, end);
        if a >= end {
            return None;
        }
        let n = self.parse_expr(a, end, 0)?;
        (self.ts.sig(n.b, end) >= end).then_some(n)
    }

    fn primary(&self, i: usize, end: usize) -> Option<Node> {
        let ts = self.ts;
        let i = ts.sig(i, end);
        if i >= end {
            return None;
        }
        let atom = |b| Node {
            a: i,
            b,
            kind: Nk::Atom,
            kids: vec![],
        };
        match ts.k(i) {
            K::Str | K::QStr | K::Num | K::Bind | K::Var | K::Colon => Some(atom(i + 1)),
            K::Op if ts.text(i) == "*" => Some(atom(i + 1)),
            K::Op if matches!(ts.text(i), "-" | "+") => {
                let e = self.parse_expr(i + 1, end, 13)?;
                Some(Node {
                    a: i,
                    b: e.b,
                    kind: Nk::Unary { op: i },
                    kids: vec![e],
                })
            }
            K::LParen => {
                let close = ts.pair[i];
                if close == NONE || close >= end {
                    return None;
                }
                Some(Node {
                    a: i,
                    b: close + 1,
                    kind: Nk::Paren { open: i, close },
                    kids: vec![],
                })
            }
            K::Quoted | K::Back => self.path_or_call(i, end),
            K::Word => {
                let w = ts.text(i);
                let is = |kw: &str| w.eq_ignore_ascii_case(kw);
                if is("CASE") {
                    // CASE … END at this depth, not a PL/SQL CASE statement.
                    let mut depth = 0usize;
                    let mut j = i + 1;
                    while j < end {
                        match ts.k(j) {
                            K::LParen if ts.pair[j] != NONE => {
                                j = ts.pair[j] + 1;
                                continue;
                            }
                            K::Semi => return None,
                            K::Word if ts.is(j, "CASE") => depth += 1,
                            K::Word if ts.is(j, "END") => {
                                if depth == 0 {
                                    return Some(Node {
                                        a: i,
                                        b: j + 1,
                                        kind: Nk::Case { end: j },
                                        kids: vec![],
                                    });
                                }
                                depth -= 1;
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    return None;
                }
                if is("NOT") {
                    let e = self.parse_expr(i + 1, end, 5)?;
                    return Some(Node {
                        a: i,
                        b: e.b,
                        kind: Nk::Unary { op: i },
                        kids: vec![e],
                    });
                }
                if is("PRIOR") || is("CONNECT_BY_ROOT") {
                    let e = self.parse_expr(i + 1, end, 13)?;
                    return Some(Node {
                        a: i,
                        b: e.b,
                        kind: Nk::Unary { op: i },
                        kids: vec![e],
                    });
                }
                let nx = ts.next(i, end);
                if (is("EXISTS") || is("ANY") || is("ALL") || is("SOME"))
                    && nx < end
                    && ts.k(nx) == K::LParen
                    && ts.pair[nx] != NONE
                    && ts.pair[nx] < end
                {
                    return Some(Node {
                        a: i,
                        b: ts.pair[nx] + 1,
                        kind: Nk::Paren {
                            open: nx,
                            close: ts.pair[nx],
                        },
                        kids: vec![],
                    });
                }
                if (is("DATE") || is("TIMESTAMP")) && nx < end && ts.k(nx) == K::Str {
                    return Some(Node {
                        a: i,
                        b: nx + 1,
                        kind: Nk::TypedLit,
                        kids: vec![],
                    });
                }
                if is("INTERVAL") && nx < end && matches!(ts.k(nx), K::Str | K::Num) {
                    return Some(self.interval(i, nx, end));
                }
                if is("NULL") || is("TRUE") || is("FALSE") {
                    return Some(atom(i + 1));
                }
                if ts.in_set(i, &STOP_SET) {
                    return None;
                }
                self.path_or_call(i, end)
            }
            _ => None,
        }
    }

    fn interval(&self, kw: usize, lit: usize, end: usize) -> Node {
        let ts = self.ts;
        let units = [
            "YEAR",
            "MONTH",
            "DAY",
            "HOUR",
            "MINUTE",
            "SECOND",
            "MICROSECOND",
            "WEEK",
            "QUARTER",
            "DAY_SECOND",
            "DAY_MINUTE",
            "DAY_HOUR",
            "HOUR_SECOND",
            "HOUR_MINUTE",
            "MINUTE_SECOND",
            "YEAR_MONTH",
        ];
        let mut j = lit + 1;
        let mut b = j;
        loop {
            let s = ts.sig(j, end);
            if s < end && ts.is_any(s, &units) {
                b = s + 1;
                j = s + 1;
                let p = ts.sig(j, end);
                if p < end && ts.k(p) == K::LParen && ts.pair[p] != NONE && ts.pair[p] < end {
                    b = ts.pair[p] + 1;
                    j = b;
                }
                let t = ts.sig(j, end);
                if t < end && ts.is(t, "TO") {
                    j = t + 1;
                    continue;
                }
            }
            break;
        }
        Node {
            a: kw,
            b,
            kind: Nk::Interval,
            kids: vec![],
        }
    }

    fn path_or_call(&self, i: usize, end: usize) -> Option<Node> {
        let ts = self.ts;
        let (parts, pend) = self.path_parts(i);
        if parts.is_empty() || pend > end {
            return None;
        }
        let nx = ts.sig(pend, end);
        if nx < end && ts.k(nx) == K::LParen && ts.pair[nx] != NONE && ts.pair[nx] < end {
            let close = ts.pair[nx];
            // `col(+)` is the legacy outer-join marker, not a call.
            if close == ts.next(ts.next(nx, end), end) && ts.is_op(ts.next(nx, end), "+") {
                return Some(Node {
                    a: i,
                    b: close + 1,
                    kind: Nk::Other,
                    kids: vec![Node {
                        a: i,
                        b: pend,
                        kind: Nk::Path,
                        kids: vec![],
                    }],
                });
            }
            let mut b = close + 1;
            // trailing analytic / ordered-set clauses
            loop {
                let s = ts.sig(b, end);
                if s >= end {
                    break;
                }
                if ts.is(s, "WITHIN") || ts.is(s, "KEEP") || ts.is(s, "FILTER") {
                    let mut p = ts.next(s, end);
                    if ts.is(p, "GROUP") {
                        p = ts.next(p, end);
                    }
                    if p < end && ts.k(p) == K::LParen && ts.pair[p] != NONE && ts.pair[p] < end {
                        b = ts.pair[p] + 1;
                        continue;
                    }
                    break;
                }
                if (ts.is(s, "IGNORE") || ts.is(s, "RESPECT")) && ts.is(ts.next(s, end), "NULLS") {
                    b = ts.next(s, end) + 1;
                    continue;
                }
                if ts.is(s, "OVER") {
                    let p = ts.next(s, end);
                    if p < end && ts.k(p) == K::LParen && ts.pair[p] != NONE && ts.pair[p] < end {
                        b = ts.pair[p] + 1;
                    } else if p < end && matches!(ts.k(p), K::Word | K::Quoted) {
                        b = p + 1;
                    }
                    break;
                }
                break;
            }
            return Some(Node {
                a: i,
                b,
                kind: Nk::Call { open: nx, close },
                kids: vec![],
            });
        }
        Some(Node {
            a: i,
            b: pend,
            kind: Nk::Path,
            kids: vec![],
        })
    }

    pub fn parse_expr(&self, i: usize, end: usize, min_bp: u8) -> Option<Node> {
        let ts = self.ts;
        let mut lhs = self.primary(i, end)?;
        loop {
            let j = ts.sig(lhs.b, end);
            if j >= end {
                break;
            }
            // infix operator?
            let (lbp, rbp, op_end, kind): (u8, u8, usize, u8) = match ts.k(j) {
                K::Op => match ts.text(j) {
                    "+" | "-" | "||" => (9, 10, j + 1, 0),
                    "*" | "/" | "%" => (11, 12, j + 1, 0),
                    "=" | "<>" | "!=" | "^=" | "~=" | "<" | ">" | "<=" | ">=" => {
                        // `<` `=` written apart is not handled; fine.
                        (7, 8, j + 1, 1)
                    }
                    _ => break,
                },
                K::Word => {
                    let w = ts.text(j);
                    let is = |kw: &str| w.eq_ignore_ascii_case(kw);
                    if is("OR") {
                        (1, 2, j + 1, 1)
                    } else if is("AND") {
                        (3, 4, j + 1, 1)
                    } else if is("IS") {
                        // postfix: IS [NOT] NULL | TRUE | FALSE | UNKNOWN | NAN …
                        if 7 < min_bp {
                            break;
                        }
                        let mut k = ts.next(j, end);
                        if ts.is(k, "NOT") {
                            k = ts.next(k, end);
                        }
                        if k >= end || ts.k(k) != K::Word {
                            break;
                        }
                        let b = k + 1;
                        lhs = Node {
                            a: lhs.a,
                            b,
                            kind: Nk::Other,
                            kids: vec![lhs],
                        };
                        continue;
                    } else {
                        let (neg, k) = if is("NOT") {
                            (true, ts.next(j, end))
                        } else {
                            (false, j)
                        };
                        let _ = neg;
                        if ts.is_any(k, &["LIKE", "LIKE2", "LIKE4", "LIKEC", "REGEXP"]) {
                            if 7 < min_bp {
                                break;
                            }
                            let rhs = self.parse_expr(k + 1, end, 8)?;
                            let mut b = rhs.b;
                            let mut kids = vec![lhs, rhs];
                            let e = ts.sig(b, end);
                            if ts.is(e, "ESCAPE")
                                && let Some(esc) = self.parse_expr(e + 1, end, 8)
                            {
                                b = esc.b;
                                kids.push(esc);
                            }
                            let a = kids[0].a;
                            lhs = Node {
                                a,
                                b,
                                kind: Nk::Other,
                                kids,
                            };
                            continue;
                        }
                        if ts.is(k, "BETWEEN") {
                            if 7 < min_bp {
                                break;
                            }
                            let lo = self.parse_expr(k + 1, end, 9)?;
                            let and = ts.sig(lo.b, end);
                            if !ts.is(and, "AND") {
                                return Some(lhs);
                            }
                            let hi = self.parse_expr(and + 1, end, 9)?;
                            let a = lhs.a;
                            let b = hi.b;
                            lhs = Node {
                                a,
                                b,
                                kind: Nk::Other,
                                kids: vec![lhs, lo, hi],
                            };
                            continue;
                        }
                        if ts.is(k, "IN") {
                            if 7 < min_bp {
                                break;
                            }
                            let rhs = self.primary(k + 1, end)?;
                            let a = lhs.a;
                            let b = rhs.b;
                            lhs = Node {
                                a,
                                b,
                                kind: Nk::Other,
                                kids: vec![lhs, rhs],
                            };
                            continue;
                        }
                        break;
                    }
                }
                _ => break,
            };
            if lbp < min_bp {
                break;
            }
            let rhs = self.parse_expr(op_end, end, rbp)?;
            let a = lhs.a;
            let b = rhs.b;
            lhs = Node {
                a,
                b,
                kind: if kind == 0 {
                    Nk::Bin { op: j }
                } else {
                    Nk::Other
                },
                kids: vec![lhs, rhs],
            };
        }
        Some(lhs)
    }

    // -----------------------------------------------------------------------
    // typing
    // -----------------------------------------------------------------------

    pub fn kind(&self, n: &Node) -> Vk {
        if let Some(k) = self.kinds.borrow().get(&(n.a, n.b)) {
            return *k;
        }
        let k = self.kind_uncached(n);
        self.kinds.borrow_mut().insert((n.a, n.b), k);
        k
    }

    fn kind_uncached(&self, n: &Node) -> Vk {
        let ts = self.ts;
        match n.kind {
            Nk::Atom => match ts.k(n.a) {
                K::Str | K::QStr => Vk::Text,
                K::Num => Vk::Num,
                K::Bind => {
                    let o = self.bind_ord[n.a] as usize;
                    self.cx
                        .bind_kinds
                        .get(o)
                        .map_or(Vk::Unknown, |k| Vk::from_value(*k))
                }
                K::Word if ts.is_any(n.a, &["TRUE", "FALSE"]) => Vk::Bool,
                _ => Vk::Unknown,
            },
            Nk::TypedLit => {
                if ts.is(n.a, "DATE") {
                    Vk::Date
                } else {
                    Vk::Ts
                }
            }
            Nk::Interval => Vk::Interval,
            Nk::Path => self.path_kind(n),
            Nk::Paren { open, close } => self
                .parse_full(open + 1, close)
                .map_or(Vk::Unknown, |e| self.kind(&e)),
            Nk::Case { end } => {
                let then = ts.find_kw(n.a + 1, end, &["THEN"]);
                then.and_then(|t| {
                    let e = self.parse_expr(t + 1, end, 0)?;
                    Some(self.kind(&e))
                })
                .unwrap_or(Vk::Unknown)
            }
            Nk::Unary { op } => {
                if ts.is_any(op, &["NOT"]) {
                    Vk::Bool
                } else if ts.is_any(op, &["PRIOR", "CONNECT_BY_ROOT"]) {
                    self.kind(&n.kids[0])
                } else {
                    match self.kind(&n.kids[0]) {
                        Vk::Interval => Vk::Interval,
                        _ => Vk::Num,
                    }
                }
            }
            Nk::Bin { op } => {
                let l = self.kind(&n.kids[0]);
                let r = self.kind(&n.kids[1]);
                match ts.text(op) {
                    "||" => Vk::Text,
                    "+" => match (l, r) {
                        (Vk::Date | Vk::Ts, Vk::Num | Vk::Text) | (Vk::Num, Vk::Date | Vk::Ts) => {
                            Vk::Date
                        }
                        (Vk::Date | Vk::Ts, Vk::Interval) => l,
                        (Vk::Interval, Vk::Date | Vk::Ts) => r,
                        (Vk::Num | Vk::Text, Vk::Num | Vk::Text) => Vk::Num,
                        _ => Vk::Unknown,
                    },
                    "-" => match (l, r) {
                        (Vk::Date | Vk::Ts, Vk::Num) => Vk::Date,
                        (Vk::Date, Vk::Date) => Vk::Num,
                        (Vk::Date | Vk::Ts, Vk::Date | Vk::Ts) => Vk::Interval,
                        (Vk::Date | Vk::Ts, Vk::Interval) => l,
                        (Vk::Num | Vk::Text, Vk::Num | Vk::Text) => Vk::Num,
                        _ => Vk::Unknown,
                    },
                    _ => {
                        if matches!(l, Vk::Num | Vk::Text) || matches!(r, Vk::Num | Vk::Text) {
                            Vk::Num
                        } else {
                            Vk::Unknown
                        }
                    }
                }
            }
            Nk::Other => Vk::Bool,
            Nk::Call { open, close } => self.call_kind(n, open, close),
        }
    }

    fn path_kind(&self, n: &Node) -> Vk {
        let ts = self.ts;
        let (parts, _) = self.path_parts(n.a);
        if parts.len() == 1 && ts.k(parts[0]) == K::Word {
            let w = ts.text(parts[0]);
            let is = |k: &str| w.eq_ignore_ascii_case(k);
            if is("SYSDATE") || is("CURRENT_DATE") {
                return Vk::Date;
            }
            if is("SYSTIMESTAMP") || is("CURRENT_TIMESTAMP") || is("LOCALTIMESTAMP") {
                return Vk::Ts;
            }
            if is("ROWNUM") || is("LEVEL") || is("UID") {
                return Vk::Num;
            }
            if is("USER") {
                return Vk::Text;
            }
        }
        if parts.len() == 2 && ts.is_any(parts[1], &["NEXTVAL", "CURRVAL"]) {
            return Vk::Num;
        }
        self.column_kind(n.a, &parts).unwrap_or(Vk::Unknown)
    }

    fn column_kind(&self, at: usize, parts: &[usize]) -> Option<Vk> {
        let ts = self.ts;
        let col = ident_text(ts.text(*parts.last()?));
        let qual = (parts.len() >= 2).then(|| ident_text(ts.text(parts[parts.len() - 2])));
        let mut block = ts.parent[at];
        loop {
            let in_block = self.scopes.iter().filter(|s| s.0 == block);
            let mut found: Option<Vk> = None;
            let mut any = false;
            for (_, alias, schema, table) in in_block {
                any = true;
                if let Some(q) = qual
                    && !alias.eq_ignore_ascii_case(q)
                {
                    continue;
                }
                let Some(table) = table else {
                    if qual.is_some() {
                        return None;
                    }
                    continue;
                };
                if let Some(k) = self.cx.schema.column_kind(schema.as_deref(), table, col) {
                    let k = Vk::from_value(k);
                    if qual.is_some() {
                        return Some(k);
                    }
                    match found {
                        None => found = Some(k),
                        Some(f) if f == k => {}
                        Some(_) => return None,
                    }
                }
            }
            if found.is_some() {
                return found;
            }
            if any && qual.is_some() {
                // qualifier not found here: look outward
            }
            if block == NONE {
                return None;
            }
            block = ts.parent[block];
        }
    }

    pub fn call_name(&self, n: &Node) -> (Option<String>, String) {
        let (parts, _) = self.path_parts(n.a);
        let ts = self.ts;
        let name = up(ident_text(ts.text(*parts.last().unwrap())));
        let pkg = (parts.len() >= 2).then(|| up(ident_text(ts.text(parts[parts.len() - 2]))));
        (pkg, name)
    }

    pub fn args(&self, open: usize, close: usize) -> Vec<(usize, usize)> {
        let (a, b) = self.ts.trim(open + 1, close);
        if a >= b {
            return vec![];
        }
        self.ts
            .split_commas(open + 1, close)
            .into_iter()
            .map(|(x, y)| self.ts.trim(x, y))
            .collect()
    }

    fn arg_kind(&self, r: (usize, usize)) -> Vk {
        self.parse_full(r.0, r.1)
            .map_or(Vk::Unknown, |e| self.kind(&e))
    }

    fn call_kind(&self, n: &Node, open: usize, close: usize) -> Vk {
        let (_, name) = self.call_name(n);
        let args = self.args(open, close);
        let first = || args.first().map_or(Vk::Unknown, |a| self.arg_kind(*a));
        match name.as_str() {
            "TO_DATE" | "ADD_MONTHS" | "LAST_DAY" | "NEXT_DAY" | "NEW_TIME" | "SYSDATE" | "NOW" => {
                Vk::Date
            }
            "TO_TIMESTAMP" | "CURRENT_TIMESTAMP" | "LOCALTIMESTAMP" | "SYSTIMESTAMP" => Vk::Ts,
            "TRUNC" | "ROUND" => {
                let k = first();
                if k.dateish() || (args.len() == 2 && self.is_date_unit_lit(args[1])) {
                    Vk::Date
                } else {
                    Vk::Num
                }
            }
            "TO_CHAR" | "SUBSTR" | "SUBSTRB" | "UPPER" | "LOWER" | "INITCAP" | "LPAD" | "RPAD"
            | "LTRIM" | "RTRIM" | "TRIM" | "REPLACE" | "TRANSLATE" | "CONCAT" | "CHR"
            | "REGEXP_SUBSTR" | "REGEXP_REPLACE" | "LISTAGG" | "SYS_CONTEXT" | "RAWTOHEX"
            | "USERENV" | "TO_NCHAR" | "NLS_UPPER" | "NLS_LOWER" | "SOUNDEX" | "DUMP" => Vk::Text,
            "TO_NUMBER" | "LENGTH" | "LENGTHB" | "INSTR" | "ABS" | "MOD" | "CEIL" | "FLOOR"
            | "SIGN" | "POWER" | "SQRT" | "EXP" | "LN" | "LOG" | "COUNT" | "SUM" | "AVG"
            | "MONTHS_BETWEEN" | "EXTRACT" | "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE"
            | "ASCII" | "REGEXP_INSTR" | "REGEXP_COUNT" | "BITAND" | "REMAINDER" | "STDDEV"
            | "VARIANCE" | "MEDIAN" | "CUME_DIST" | "PERCENT_RANK" | "RATIO_TO_REPORT" => Vk::Num,
            "NVL" | "NVL2" | "COALESCE" | "GREATEST" | "LEAST" | "MIN" | "MAX" | "NULLIF"
            | "FIRST_VALUE" | "LAST_VALUE" | "LAG" | "LEAD" => {
                let idx = usize::from(name == "NVL2");
                args.get(idx).map_or(Vk::Unknown, |a| self.arg_kind(*a))
            }
            "DECODE" => args.get(2).map_or(Vk::Unknown, |a| self.arg_kind(*a)),
            "CAST" => {
                let ts = self.ts;
                let as_kw = ts.find_kw(open + 1, close, &["AS"]);
                match as_kw.map(|a| ts.sig(a + 1, close)) {
                    Some(t) if ts.is(t, "DATE") => Vk::Date,
                    Some(t) if ts.is(t, "TIMESTAMP") => Vk::Ts,
                    Some(t)
                        if ts.is_any(
                            t,
                            &[
                                "NUMBER",
                                "INTEGER",
                                "INT",
                                "DECIMAL",
                                "NUMERIC",
                                "FLOAT",
                                "BINARY_DOUBLE",
                                "BINARY_FLOAT",
                                "SMALLINT",
                                "REAL",
                                "DOUBLE",
                            ],
                        ) =>
                    {
                        Vk::Num
                    }
                    Some(t)
                        if ts.is_any(
                            t,
                            &["VARCHAR2", "VARCHAR", "CHAR", "NVARCHAR2", "NCHAR", "CLOB"],
                        ) =>
                    {
                        Vk::Text
                    }
                    Some(t) if ts.is(t, "INTERVAL") => Vk::Interval,
                    _ => Vk::Unknown,
                }
            }
            "NUMTODSINTERVAL" | "NUMTOYMINTERVAL" => Vk::Interval,
            _ => Vk::Unknown,
        }
    }

    fn is_date_unit_lit(&self, r: (usize, usize)) -> bool {
        let ts = self.ts;
        if r.1 != r.0 + 1 || ts.k(r.0) != K::Str {
            return false;
        }
        let v = up(str_lit_value(ts.text(r.0)).trim());
        !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphabetic())
    }

    // -----------------------------------------------------------------------
    // rendering
    // -----------------------------------------------------------------------

    /// Render tokens `a..b`.
    pub fn range(&self, a: usize, b: usize) -> String {
        let ts = self.ts;
        let mut out = String::with_capacity(ts.slice(a, b).len() + 8);
        let mut i = a;
        while i < b {
            let k = ts.k(i);
            if matches!(k, K::Ws | K::Comment) {
                out.push_str(ts.text(i));
                i += 1;
                continue;
            }
            if k == K::Word
                && let Some(next) = self.word_rule(i, b, &mut out)
            {
                i = next;
                continue;
            }
            if matches!(k, K::Word | K::Quoted | K::Back) && self.table_position(i) {
                i = self.relation(i, b, &mut out);
                continue;
            }
            if k == K::Comma
                && self.clause[i] == Clause::From
                && let Some(next) = self.skip_comma_dual(i, b)
            {
                i = next;
                continue;
            }
            if let Some(n) = self.parse_expr(i, b, 0) {
                out.push_str(&self.node(&n));
                i = n.b;
                continue;
            }
            match k {
                K::LParen if ts.pair[i] != NONE && ts.pair[i] <= b => {
                    let close = ts.pair[i];
                    out.push('(');
                    out.push_str(&self.range(i + 1, close));
                    out.push(')');
                    i = close + 1;
                }
                _ => {
                    out.push_str(&self.token(i));
                    i += 1;
                }
            }
        }
        out
    }

    /// Text of a single token (QStr normalised, row refs in triggers).
    pub fn token(&self, i: usize) -> String {
        let ts = self.ts;
        let t = ts.text(i);
        match ts.k(i) {
            K::QStr => normalize_q_quote(t),
            K::Colon if self.stmt == StmtKind::Trigger => {
                let name = &t[1..];
                if name.eq_ignore_ascii_case("NEW") || name.eq_ignore_ascii_case("OLD") {
                    up(name)
                } else {
                    t.to_string()
                }
            }
            K::Word if self.needs_backquote(i) => format!("`{t}`"),
            _ => t.to_string(),
        }
    }

    fn needs_backquote(&self, i: usize) -> bool {
        let ts = self.ts;
        if !ts.in_set(i, &RESERVED_SET) {
            return false;
        }
        let n = ts.len();
        let nx = ts.next(i, n);
        if nx < n && ts.k(nx) == K::LParen {
            return false; // a call
        }
        if nx < n && ts.k(nx) == K::Dot {
            return true;
        }
        let p = ts.prev(i, 0);
        if p == NONE {
            return false;
        }
        if ts.is_any(i, &["ROWS", "RANGE", "GROUPS"])
            && (ts.is_any(nx, &["BETWEEN", "UNBOUNDED", "CURRENT", "ONLY"])
                || (nx < n && ts.k(nx) == K::Num))
        {
            return false;
        }
        if ts.k(p) == K::Dot {
            return true;
        }
        if ts.is_any(p, &["SELECT", "BY", "SET", "DISTINCT", "AS"]) {
            return true;
        }
        if matches!(ts.k(p), K::Comma | K::LParen) {
            return true;
        }
        // column definition `… , rank NUMBER` handled above; alias `expr rank`
        matches!(
            ts.k(p),
            K::Word | K::Quoted | K::Back | K::RParen | K::Num | K::Str
        ) && self.clause[i] == Clause::Select
            && (nx >= n || ts.k(nx) == K::Comma || ts.is(nx, "FROM"))
    }

    /// `, DUAL` inside a FROM list (MariaDB's DUAL cannot be joined): drop it.
    fn skip_comma_dual(&self, i: usize, b: usize) -> Option<usize> {
        let ts = self.ts;
        let d = ts.next(i, b);
        if !ts.is(d, "DUAL") {
            return None;
        }
        let after = ts.next(d, b);
        if after < b && (ts.k(after) == K::Word && !self.is_clause_word(after)) {
            return None; // aliased: keep, handled by `relation`
        }
        Some(d + 1)
    }

    /// Render a relation reference starting at `i`.
    fn relation(&self, i: usize, b: usize, out: &mut String) -> usize {
        let ts = self.ts;
        let (parts, end) = self.path_parts(i);
        if parts.is_empty() {
            out.push_str(&self.token(i));
            return i + 1;
        }
        let names: Vec<String> = parts.iter().map(|&p| up(ident_text(ts.text(p)))).collect();
        let last = names.last().unwrap();
        let qualified_sys = names.len() == 2 && matches!(names[0].as_str(), "SYS" | "PUBLIC");
        if last == "DUAL"
            && (names.len() == 1 || qualified_sys)
            && ts.k(*parts.last().unwrap()) == K::Word
        {
            let alias = self.alias_after(end);
            match alias {
                Some((a, aend)) => {
                    out.push_str("(SELECT 'X' AS DUMMY) ");
                    out.push_str(&a);
                    return aend;
                }
                None => {
                    // `FROM DUAL, t` -> drop `DUAL,`
                    let nx = ts.next(end - 1, b);
                    if nx < b && ts.k(nx) == K::Comma && self.clause[nx] == Clause::From {
                        let after = ts.next(nx, b);
                        return after;
                    }
                    out.push_str("DUAL");
                    return end;
                }
            }
        }
        if self.cx.route_dictionary
            && (names.len() == 1 || qualified_sys)
            && dictionary::is_relation(last)
        {
            let lower = last.to_ascii_lowercase();
            out.push_str(&format!(
                "`{}`.`{}`",
                dictionary::SCHEMA,
                lower.replace('`', "``")
            ));
            if self.alias_after(end).is_none() {
                out.push(' ');
                out.push_str(ts.text(*parts.last().unwrap()));
            }
            return end;
        }
        for p in i..end {
            out.push_str(&self.token(p));
        }
        end
    }

    /// Statement-shape word rules that consume tokens; `None` = not applicable.
    fn word_rule(&self, i: usize, b: usize, out: &mut String) -> Option<usize> {
        let ts = self.ts;
        let w = ts.text(i);
        let is = |kw: &str| w.eq_ignore_ascii_case(kw);
        if is("ORDER") {
            let by = ts.next(i, b);
            if ts.is(by, "BY") {
                // ordered-set aggregates (PERCENTILE_*, MEDIAN …) take exactly
                // one sort key and ignore NULLs: no NULL-placement key
                let p = ts.parent[i];
                let within_group = p != NONE && {
                    let g = ts.prev(p, 0);
                    g != NONE && ts.is(g, "GROUP") && ts.is(ts.prev(g, 0), "WITHIN")
                };
                if within_group {
                    return None;
                }
                out.push_str(ts.slice(i, by + 1));
                let (s, e) = self.order_items_range(by + 1, b);
                out.push_str(&self.order_items(s, e));
                return Some(e);
            }
            return None;
        }
        if is("WITH") {
            return self.with_clause(i, b, out);
        }
        if is("EXECUTE") && ts.is(ts.next(i, b), "IMMEDIATE") {
            return self.execute_immediate(i, b, out);
        }
        if is("FOR") {
            let u = ts.next(i, b);
            if ts.is(u, "UPDATE") {
                let of = ts.next(u, b);
                if ts.is(of, "OF") {
                    out.push_str(ts.slice(i, u + 1));
                    let mut j = of + 1;
                    while j < b && !ts.is_any(j, &["NOWAIT", "WAIT", "SKIP"]) && ts.k(j) != K::Semi
                    {
                        j += 1;
                    }
                    if j < b {
                        out.push(' ');
                    }
                    return Some(j);
                }
            }
            return None;
        }
        if self.is_type_position(i)
            && let Some((text, next)) = self.ddl_type(i, b)
        {
            out.push_str(&text);
            return Some(next);
        }
        if is("DEFAULT") && matches!(self.stmt, StmtKind::Ddl) {
            // `DEFAULT ON NULL x` -> `DEFAULT x`
            let on = ts.next(i, b);
            if ts.is(on, "ON") && ts.is(ts.next(on, b), "NULL") {
                out.push_str("DEFAULT");
                return Some(ts.next(on, b) + 1);
            }
            let v = ts.next(i, b);
            if v < b && ts.k(v) == K::Word && ts.next(v, b) < b && ts.k(ts.next(v, b)) == K::LParen
            {
                return None;
            }
            if ts.is(v, "SYSDATE") {
                out.push_str("DEFAULT CURRENT_TIMESTAMP");
                return Some(v + 1);
            }
            if ts.is(v, "SYSTIMESTAMP") {
                out.push_str("DEFAULT CURRENT_TIMESTAMP(6)");
                return Some(v + 1);
            }
            if ts.is(v, "USER") {
                out.push_str("DEFAULT (UPPER(SUBSTRING_INDEX(SESSION_USER(), '@', 1)))");
                return Some(v + 1);
            }
            return None;
        }
        None
    }

    /// `EXECUTE IMMEDIATE '<sql>' [|| '<sql>' …]`: the literal is an Oracle
    /// statement too — translate it and re-quote. A lowering that needs a
    /// batch becomes one `EXECUTE IMMEDIATE` per statement. Non-literal
    /// dynamic SQL is left alone.
    fn execute_immediate(&self, i: usize, b: usize, out: &mut String) -> Option<usize> {
        let ts = self.ts;
        let imm = ts.next(i, b);
        let mut j = ts.next(imm, b);
        let mut text = String::new();
        let mut end;
        loop {
            if j >= b || !matches!(ts.k(j), K::Str | K::QStr) || ts.text(j).starts_with(['x', 'X'])
            {
                return None;
            }
            let lit = if ts.k(j) == K::QStr {
                normalize_q_quote(ts.text(j))
            } else {
                ts.text(j).to_string()
            };
            text.push_str(&str_lit_value(&lit));
            end = j + 1;
            let nx = ts.next(j, b);
            if ts.is_op(nx, "||") {
                j = ts.next(nx, b);
                continue;
            }
            break;
        }
        // the dynamic statement may itself be followed by INTO/USING
        let inner_cx = super::MariaDbContext {
            case: self.cx.case,
            schema: self.cx.schema,
            bind_kinds: &[],
            route_dictionary: self.cx.route_dictionary,
        };
        let translated = super::oracle_to_mariadb_ctx(&text, &inner_cx).ok()?;
        let parts: Vec<String> = translated
            .split(crate::translate::MARIADB_BATCH_SEP)
            .map(|p| format!("{} '{}'", ts.slice(i, imm + 1), p.replace('\'', "''")))
            .collect();
        if parts.len() > 1
            && ts.sig(end, b) < b
            && !ts.is(ts.sig(end, b), "END")
            && ts.k(ts.sig(end, b)) != K::Semi
        {
            return None; // INTO/USING on a batch: leave unchanged
        }
        out.push_str(&parts.join("; "));
        Some(end)
    }

    /// `WITH name [(cols)] AS (…), …`: add RECURSIVE when a CTE refers to itself.
    fn with_clause(&self, i: usize, b: usize, out: &mut String) -> Option<usize> {
        let ts = self.ts;
        let name = ts.next(i, b);
        if name >= b || !matches!(ts.k(name), K::Word | K::Quoted | K::Back) {
            return None;
        }
        if ts.is(name, "RECURSIVE") {
            return None;
        }
        let mut j = ts.next(name, b);
        if j < b && ts.k(j) == K::LParen && ts.pair[j] != NONE {
            j = ts.next(ts.pair[j], b);
        }
        if !ts.is(j, "AS") {
            return None;
        }
        // walk the CTE list
        let mut recursive = false;
        let mut cur = name;
        loop {
            let cte = ident_text(ts.text(cur));
            let mut k = ts.next(cur, b);
            if k < b && ts.k(k) == K::LParen && ts.pair[k] != NONE {
                k = ts.next(ts.pair[k], b);
            }
            if !ts.is(k, "AS") {
                break;
            }
            let body = ts.next(k, b);
            if body >= b || ts.k(body) != K::LParen || ts.pair[body] == NONE {
                break;
            }
            let close = ts.pair[body];
            if (body + 1..close).any(|t| {
                matches!(ts.k(t), K::Word | K::Quoted | K::Back)
                    && ident_text(ts.text(t)) == cte
                    && ts.k(ts.next(t, close)) != K::Dot
            }) {
                recursive = true;
            }
            let c = ts.next(close, b);
            if c < b && ts.k(c) == K::Comma {
                cur = ts.next(c, b);
                continue;
            }
            break;
        }
        if recursive {
            out.push_str(ts.text(i));
            out.push_str(" RECURSIVE");
            return Some(i + 1);
        }
        None
    }

    /// End of an ORDER BY item list starting at `s`.
    fn order_items_range(&self, s: usize, b: usize) -> (usize, usize) {
        let ts = self.ts;
        let mut j = s;
        while j < b {
            match ts.k(j) {
                K::LParen if ts.pair[j] != NONE => {
                    j = ts.pair[j] + 1;
                    continue;
                }
                K::RParen | K::Semi => break,
                K::Word
                    if ts.is_any(
                        j,
                        &[
                            "FETCH",
                            "OFFSET",
                            "LIMIT",
                            "FOR",
                            "ROWS",
                            "RANGE",
                            "GROUPS",
                            "SEPARATOR",
                            "UNION",
                            "MINUS",
                            "INTERSECT",
                            "EXCEPT",
                            "ON",
                        ],
                    ) =>
                {
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        (s, j)
    }

    /// Oracle NULL placement: ASC sorts NULLs last, DESC first (MariaDB: the
    /// opposite). Each item gets a leading `expr IS NULL [DESC]` key.
    pub fn order_items(&self, s: usize, e: usize) -> String {
        let ts = self.ts;
        let frame_range = ts.is(ts.sig(e, ts.len()), "RANGE");
        let mut parts = Vec::new();
        for (a, b) in ts.split_commas(s, e) {
            let lead = &ts.slice(a, ts.trim(a, b).0);
            let (a, b) = ts.trim(a, b);
            if a >= b {
                parts.push(String::new());
                continue;
            }
            // trailing [ASC|DESC] [NULLS FIRST|LAST]
            let mut end = b;
            let mut nulls: Option<bool> = None; // Some(true) = FIRST
            let last = ts.prev(end, a.saturating_sub(1));
            if last != NONE && ts.is_any(last, &["FIRST", "LAST"]) {
                let n = ts.prev(last, a.saturating_sub(1));
                if n != NONE && ts.is(n, "NULLS") {
                    nulls = Some(ts.is(last, "FIRST"));
                    end = n;
                }
            }
            let (ea, eb) = ts.trim(a, end);
            let mut desc = false;
            let mut dir_end = eb;
            let d = ts.prev(eb, ea.saturating_sub(1));
            if d != NONE && d >= ea && ts.is_any(d, &["ASC", "DESC"]) {
                desc = ts.is(d, "DESC");
                dir_end = d;
            }
            let (xa, xb) = ts.trim(ea, dir_end);
            let expr = self.range(xa, xb);
            let dir = ts.slice(dir_end, eb);
            let nulls_first = nulls.unwrap_or(desc);
            // MariaDB default: NULLs first for ASC, last for DESC.
            let native = nulls_first != desc;
            // positional item: the key is the select-list expression (or alias)
            let positional = (xb == xa + 1 && ts.k(xa) == K::Num)
                .then(|| self.select_item_key(s, ts.text(xa)))
                .flatten();
            let skip = native
                || frame_range
                || (xb == xa + 1 && matches!(ts.k(xa), K::Num | K::Str) && positional.is_none())
                || ts.has(xa, xb, K::Bind)
                || (xb == xa + 1 && ts.is_any(xa, &["ROWNUM", "LEVEL"]))
                || self.not_null_expr(xa, xb);
            let mut item = String::from(*lead);
            if !skip {
                item.push_str(positional.as_deref().unwrap_or(&expr));
                item.push_str(if nulls_first {
                    " IS NULL DESC, "
                } else {
                    " IS NULL, "
                });
            }
            item.push_str(&expr);
            let dir_t = dir.trim();
            if !dir_t.is_empty() {
                item.push(' ');
                item.push_str(dir_t);
            }
            parts.push(item);
        }
        let mut out = parts.join(",");
        // keep the whitespace that preceded the clause terminator
        let (_, te) = ts.trim(s, e);
        out.push_str(ts.slice(te, e));
        if !out.starts_with(' ') && !out.is_empty() {
            out.insert(0, ' ');
        }
        out
    }

    /// Sort key for `ORDER BY <n>`: the n-th select item's alias, or its
    /// expression. `None` for set operations, `*`, binds, or out of range.
    fn select_item_key(&self, order_items_start: usize, pos: &str) -> Option<String> {
        let ts = self.ts;
        let n: usize = pos.parse().ok()?;
        let sel = super::structural::enclosing_select(ts, order_items_start)?;
        // a set operation's ORDER BY sorts the combined result
        let p = ts.parent[sel];
        let (a, b) = if p == NONE {
            (0, ts.len())
        } else {
            (p + 1, ts.pair[p])
        };
        let setop = ts
            .find_kw(a, b, &["UNION", "MINUS", "INTERSECT", "EXCEPT"])
            .is_some();
        // for a set operation the result columns are named by the first branch
        let sel = if setop {
            ts.find_kw(a, b, &["SELECT"])?
        } else {
            sel
        };
        let blk = super::structural::parse_block(ts, sel);
        let mut la = ts.sig(blk.list.0, blk.list.1);
        if ts.is_any(la, &["DISTINCT", "UNIQUE", "ALL"]) {
            la += 1;
        }
        let items = ts.split_commas(la, blk.list.1);
        let (ia, ib) = ts.trim(items.get(n.checked_sub(1)?)?.0, items[n - 1].1);
        if ia >= ib || ts.has(ia, ib, K::Bind) || ts.is_op(ib - 1, "*") {
            return None;
        }
        let e = self.parse_expr(ia, ib, 0)?;
        let rest = ts.sig(e.b, ib);
        if rest < ib {
            let mut al = rest;
            if ts.is(al, "AS") {
                al = ts.next(al, ib);
            }
            return (al < ib).then(|| self.token(al));
        }
        if setop {
            // only a plain column names the result column
            if e.kind != Nk::Path {
                return None;
            }
            let (parts, _) = self.path_parts(e.a);
            return parts.last().map(|&l| self.token(l));
        }
        if self.not_null_expr(ia, ib) {
            return None;
        }
        Some(self.range(ia, ib))
    }

    fn not_null_expr(&self, a: usize, b: usize) -> bool {
        let Some(n) = self.parse_full(a, b) else {
            return false;
        };
        if let Nk::Call { .. } = n.kind {
            let (_, name) = self.call_name(&n);
            return matches!(
                name.as_str(),
                "COUNT" | "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE"
            );
        }
        false
    }

    /// Render one parsed node.
    pub fn node(&self, n: &Node) -> String {
        match n.kind {
            Nk::Call { open, close } => self.call(n, open, close),
            Nk::Bin { op } => self.binary(n, op),
            Nk::Path => self.path(n),
            Nk::Interval => self.interval_text(n, true),
            Nk::TypedLit => self.ts.slice(n.a, n.b).to_string(),
            Nk::Case { end } => {
                let mut s = self.token(n.a);
                s.push_str(&self.range(n.a + 1, end));
                s.push_str(self.ts.text(end));
                s
            }
            _ => self.generic(n),
        }
    }

    /// Copy `n`'s tokens, rendering kids and paren contents in place.
    pub fn generic(&self, n: &Node) -> String {
        let ts = self.ts;
        let mut out = String::new();
        let mut i = n.a;
        let mut kid = 0;
        while i < n.b {
            if kid < n.kids.len() && n.kids[kid].a == i {
                out.push_str(&self.node(&n.kids[kid]));
                i = n.kids[kid].b;
                kid += 1;
                continue;
            }
            if ts.k(i) == K::LParen && ts.pair[i] != NONE && ts.pair[i] < n.b {
                let c = ts.pair[i];
                // `(+)` markers survive only if the structural pass missed them
                out.push('(');
                out.push_str(&self.range(i + 1, c));
                out.push(')');
                i = c + 1;
                continue;
            }
            out.push_str(&self.token(i));
            i += 1;
        }
        out
    }

    fn select_item_boundary(&self, n: &Node) -> bool {
        let ts = self.ts;
        if self.clause[n.a] != Clause::Select {
            return false;
        }
        let p = ts.prev(n.a, 0);
        let nx = ts.sig(n.b, ts.len());
        (p != NONE
            && (ts.is_any(p, &["SELECT", "DISTINCT", "UNIQUE", "ALL"]) || ts.k(p) == K::Comma))
            && (nx >= ts.len()
                || ts.k(nx) == K::Comma
                || ts.is(nx, "FROM")
                || ts.k(nx) == K::RParen && self.clause[nx] != Clause::Select)
    }

    fn path(&self, n: &Node) -> String {
        let ts = self.ts;
        let (parts, _) = self.path_parts(n.a);
        if parts.len() == 1 && ts.k(parts[0]) == K::Word && n.b == n.a + 1 {
            let w = up(ts.text(n.a));
            let ddl = matches!(self.stmt, StmtKind::Ddl);
            let rep = match w.as_str() {
                "SYSTIMESTAMP" => Some("CURRENT_TIMESTAMP(6)"),
                "SYSDATE" if !ddl => Some("NOW()"),
                "CURRENT_DATE" if !ddl => Some("NOW()"),
                "CURRENT_TIMESTAMP" if !ddl => Some("CURRENT_TIMESTAMP(6)"),
                "LOCALTIMESTAMP" if !ddl => Some("LOCALTIMESTAMP(6)"),
                "SESSIONTIMEZONE" => Some("SESSIONTIMEZONE()"),
                "DBTIMEZONE" => Some("DBTIMEZONE()"),
                "UID" => Some("CONNECTION_ID()"),
                "USER" => Some("SYS_CONTEXT('USERENV', 'SESSION_USER')"),
                _ => None,
            };
            if let Some(rep) = rep {
                let rep = match w.as_str() {
                    "USER" => format!("{}('USERENV', 'SESSION_USER')", self.fname("sys_context")),
                    "SESSIONTIMEZONE" => self.fname("sessiontimezone") + "()",
                    "DBTIMEZONE" => self.fname("dbtimezone") + "()",
                    _ => rep.to_string(),
                };
                if self.select_item_boundary(n) {
                    return format!("{rep} AS \"{w}\"");
                }
                return rep;
            }
        }
        let mut s = String::new();
        for i in n.a..n.b {
            s.push_str(&self.token(i));
        }
        s
    }

    /// Name of a compatibility routine, qualified when dictionary routing is on.
    pub fn fname(&self, name: &str) -> String {
        let lower = name.to_ascii_lowercase();
        if self.cx.route_dictionary
            && (dictionary::is_function(&lower) || super::helpers::is_helper(&lower))
        {
            format!("`{}`.`{lower}`", dictionary::SCHEMA)
        } else {
            lower
        }
    }

    // ----- intervals ---------------------------------------------------------

    /// `INTERVAL` literal. In arithmetic (`arith`) → MariaDB `INTERVAL n UNIT`;
    /// as a value → `numtodsinterval`/`numtoyminterval` (interval text).
    fn interval_text(&self, n: &Node, value_ctx: bool) -> String {
        let ts = self.ts;
        let lit_i = ts.next(n.a, n.b);
        let raw = ts.text(lit_i);
        let val = if ts.k(lit_i) == K::Str {
            str_lit_value(raw)
        } else {
            raw.to_string()
        };
        let units: Vec<String> = (lit_i + 1..n.b)
            .filter(|&t| ts.k(t) == K::Word && !ts.is(t, "TO"))
            .map(|t| up(ts.text(t)))
            .collect();
        let (from, to) = (
            units.first().cloned().unwrap_or_default(),
            units.get(1).cloned(),
        );
        let (sign, body) = match val.trim().strip_prefix('-') {
            Some(b) => (-1i64, b.trim().to_string()),
            None => (1, val.trim().to_string()),
        };
        // year-month
        if from == "YEAR" || (from == "MONTH" && to.is_none()) {
            let months = if to.as_deref() == Some("MONTH") {
                let (y, m) = body.split_once('-').unwrap_or((&body, "0"));
                y.trim().parse::<i64>().unwrap_or(0) * 12 + m.trim().parse::<i64>().unwrap_or(0)
            } else if from == "YEAR" {
                body.parse::<i64>().unwrap_or(0) * 12
            } else {
                body.parse::<i64>().unwrap_or(0)
            };
            let months = sign * months;
            return if value_ctx {
                format!("{}({months}, 'MONTH')", self.fname("numtoyminterval"))
            } else {
                format!("INTERVAL {months} MONTH")
            };
        }
        // day-second: compute seconds (with fraction)
        let secs = day_second_seconds(&body, &from, to.as_deref());
        match secs {
            Some(s) => {
                let s = if sign < 0 { -s } else { s };
                let txt = fmt_seconds(s);
                if value_ctx {
                    format!("{}({txt}, 'SECOND')", self.fname("numtodsinterval"))
                } else if s.fract() == 0.0 {
                    format!("INTERVAL {txt} SECOND")
                } else {
                    format!("INTERVAL {} MICROSECOND", (s * 1e6).round() as i64)
                }
            }
            None => ts.slice(n.a, n.b).to_string(),
        }
    }

    // ----- binary operators ----------------------------------------------------

    fn binary(&self, n: &Node, op: usize) -> String {
        let ts = self.ts;
        let (l, r) = (&n.kids[0], &n.kids[1]);
        let o = ts.text(op);
        let lk = self.kind(l);
        let rk = self.kind(r);
        let ws_l = ts.slice(l.b, op);
        let ws_r = ts.slice(op + 1, r.a);
        let lt = || self.operand(l, false);
        let rt = || self.operand(r, false);
        match o {
            "+" | "-" => {
                let minus = o == "-";
                // date ± number
                if lk.dateish() && matches!(rk, Vk::Num | Vk::Text) {
                    return self.date_add(l, lk, r, minus);
                }
                if !minus && rk.dateish() && matches!(lk, Vk::Num | Vk::Text) {
                    return self.date_add(r, rk, l, false);
                }
                // date ± interval (literal or NUMTO*INTERVAL)
                if lk.dateish() && (rk == Vk::Interval) {
                    return format!("{}{ws_l}{o}{ws_r}{}", lt(), self.operand(r, true));
                }
                if !minus && rk.dateish() && lk == Vk::Interval {
                    return format!("{}{ws_l}{o}{ws_r}{}", self.operand(l, true), rt());
                }
                // date - date
                if minus && lk.dateish() && rk.dateish() {
                    if lk == Vk::Date && rk == Vk::Date {
                        if self.date_only_literal(l) && self.date_only_literal(r) {
                            return format!("DATEDIFF({}, {})", lt(), rt());
                        }
                        return format!(
                            "(CAST(TIMESTAMPDIFF(SECOND, {}, {}) AS DECIMAL(65,30)) / 86400)",
                            rt(),
                            lt()
                        );
                    }
                    return format!(
                        "{}(TIMESTAMPDIFF(MICROSECOND, {}, {}) / 1000000, 'SECOND')",
                        self.fname("numtodsinterval"),
                        rt(),
                        lt()
                    );
                }
                self.arith_coerce(l, lk, r, rk, o, ws_l, ws_r)
            }
            "/" => {
                if ts.k(r.a) == K::Num
                    && r.b == r.a + 1
                    && ts.text(r.a).parse::<f64>().is_ok_and(|v| v == 0.0)
                {
                    return format!("{}({}, {})", self.fname("dbsaci_div"), lt(), rt());
                }
                self.arith_coerce(l, lk, r, rk, o, ws_l, ws_r)
            }
            "*" => self.arith_coerce(l, lk, r, rk, o, ws_l, ws_r),
            _ => self.generic(n),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn arith_coerce(
        &self,
        l: &Node,
        lk: Vk,
        r: &Node,
        rk: Vk,
        o: &str,
        ws_l: &str,
        ws_r: &str,
    ) -> String {
        let coerce = matches!(lk, Vk::Num | Vk::Text)
            && matches!(rk, Vk::Num | Vk::Text)
            && (lk == Vk::Text || rk == Vk::Text);
        let side = |n: &Node, k: Vk| -> String {
            let t = self.operand(n, false);
            if coerce && k == Vk::Text && !self.numeric_literal_text(n) {
                format!("{}({t})", self.fname("dbsaci_to_number"))
            } else {
                t
            }
        };
        format!("{}{ws_l}{o}{ws_r}{}", side(l, lk), side(r, rk))
    }

    fn numeric_literal_text(&self, n: &Node) -> bool {
        let ts = self.ts;
        n.b == n.a + 1
            && ts.k(n.a) == K::Str
            && is_oracle_number(str_lit_value(ts.text(n.a)).trim())
    }

    fn date_only_literal(&self, n: &Node) -> bool {
        let ts = self.ts;
        n.kind == Nk::TypedLit && ts.is(n.a, "DATE") && {
            let v = str_lit_value(ts.text(ts.next(n.a, n.b)));
            !v.trim().contains(' ')
        }
    }

    /// Render an operand; `interval_arith` renders an interval in MariaDB
    /// arithmetic form.
    fn operand(&self, n: &Node, interval_arith: bool) -> String {
        if interval_arith {
            match n.kind {
                Nk::Interval => return self.interval_text(n, false),
                Nk::Call { open, close } => {
                    let (_, name) = self.call_name(n);
                    let args = self.args(open, close);
                    if (name == "NUMTODSINTERVAL" || name == "NUMTOYMINTERVAL")
                        && args.len() == 2
                        && self.ts.k(args[1].0) == K::Str
                    {
                        let unit = up(str_lit_value(self.ts.text(args[1].0)).trim());
                        let x = self.range(args[0].0, args[0].1);
                        let (mult, u) = match unit.as_str() {
                            "DAY" => ("86400", "SECOND"),
                            "HOUR" => ("3600", "SECOND"),
                            "MINUTE" => ("60", "SECOND"),
                            "SECOND" => ("1", "SECOND"),
                            "YEAR" => ("12", "MONTH"),
                            _ => ("1", "MONTH"),
                        };
                        return format!("INTERVAL ({x}) * {mult} {u}");
                    }
                }
                Nk::Paren { open, close } => {
                    if let Some(inner) = self.parse_full(open + 1, close) {
                        return self.operand(&inner, true);
                    }
                }
                _ => {}
            }
        }
        self.node(n)
    }

    fn date_add(&self, d: &Node, dk: Vk, nnode: &Node, minus: bool) -> String {
        let ts = self.ts;
        let dt = self.node(d);
        let dt = if dk == Vk::Ts {
            format!("CAST({dt} AS DATETIME)")
        } else {
            dt
        };
        let f = if minus { "DATE_SUB" } else { "DATE_ADD" };
        let is_int = nnode.b == nnode.a + 1
            && ts.k(nnode.a) == K::Num
            && ts.text(nnode.a).bytes().all(|b| b.is_ascii_digit());
        let n = self.node(nnode);
        if is_int {
            format!("{f}({dt}, INTERVAL {n} DAY)")
        } else {
            let n = if self.kind(nnode) == Vk::Text && !self.numeric_literal_text(nnode) {
                format!("{}({n})", self.fname("dbsaci_to_number"))
            } else {
                n
            };
            format!("{f}({dt}, INTERVAL ROUND(({n}) * 86400) SECOND)")
        }
    }

    // ----- DDL types ---------------------------------------------------------

    fn is_type_position(&self, i: usize) -> bool {
        let ts = self.ts;
        if !matches!(
            self.stmt,
            StmtKind::Ddl | StmtKind::Plsql | StmtKind::Trigger
        ) {
            return false;
        }
        let p = ts.prev(i, 0);
        if p == NONE {
            return false;
        }
        match ts.k(p) {
            K::Quoted | K::Back => true,
            K::Word => {
                ts.is_any(p, &["RETURN", "IN", "OUT", "NOCOPY", "RETURNS"])
                    || !(ts.in_set(p, &STOP_SET)
                        || ts.is_any(
                            p,
                            &[
                                "NOT",
                                "NULL",
                                "IS",
                                "TYPE",
                                "SUBTYPE",
                                "CONSTANT",
                                "TABLE",
                                "VIEW",
                                "INDEX",
                                "SEQUENCE",
                                "TRIGGER",
                                "PROCEDURE",
                                "FUNCTION",
                                "PACKAGE",
                                "BODY",
                                "REPLACE",
                                "OR",
                                "COLUMN",
                                "MODIFY",
                                "ADD",
                                "RENAME",
                                "TO",
                                "KEY",
                                "EACH",
                                "ROW",
                            ],
                        ))
            }
            _ => false,
        }
    }

    /// Map an Oracle type starting at `i`; `None` when `i` is not a type.
    pub fn ddl_type(&self, i: usize, b: usize) -> Option<(String, usize)> {
        let ts = self.ts;
        let w = up(ts.text(i));
        let nx = ts.next(i, b);
        let (args, after): (Vec<String>, usize) =
            if nx < b && ts.k(nx) == K::LParen && ts.pair[nx] != NONE {
                let c = ts.pair[nx];
                let a: Vec<String> = ts
                    .split_commas(nx + 1, c)
                    .into_iter()
                    .map(|(x, y)| ts.tslice(x, y).to_string())
                    .collect();
                (a, c + 1)
            } else {
                (vec![], i + 1)
            };
        let number = |args: &[String]| -> String {
            match args {
                [] => "DECIMAL(65,30)".into(),
                [p] if p == "*" => "DECIMAL(65,30)".into(),
                [p] => format!("DECIMAL({},0)", p.trim()),
                [p, s] => {
                    let s: i64 = s.trim().parse().unwrap_or(0);
                    let p: i64 = if p.trim() == "*" {
                        38
                    } else {
                        p.trim().parse().unwrap_or(38)
                    };
                    if s < 0 {
                        format!("DECIMAL({p},0)")
                    } else if s > p {
                        format!("DECIMAL({s},{s})")
                    } else {
                        format!("DECIMAL({p},{s})")
                    }
                }
                _ => "DECIMAL(65,30)".into(),
            }
        };
        let strip_sem = |args: &[String]| -> String {
            args.first()
                .map(|a| a.split_whitespace().next().unwrap_or("").to_string())
                .unwrap_or_default()
        };
        let mut next = after;
        let text = match w.as_str() {
            "NUMBER" | "NUMERIC" | "DECIMAL" | "DEC" => {
                if args.is_empty() && w != "NUMBER" {
                    "DECIMAL(38,0)".to_string()
                } else {
                    number(&args)
                }
            }
            "INTEGER" | "INT" | "SMALLINT" => "DECIMAL(38,0)".into(),
            "FLOAT" | "REAL" => "DECIMAL(65,30)".into(),
            "DOUBLE" if ts.is(nx, "PRECISION") => {
                next = nx + 1;
                "DECIMAL(65,30)".into()
            }
            "BINARY_DOUBLE" => "DOUBLE".into(),
            "BINARY_FLOAT" => "FLOAT".into(),
            "PLS_INTEGER" | "BINARY_INTEGER" | "SIMPLE_INTEGER" | "NATURAL" | "POSITIVE" => {
                "INT".into()
            }
            "VARCHAR2" | "VARCHAR" if !args.is_empty() => {
                format!("VARCHAR({})", strip_sem(&args))
            }
            "NVARCHAR2" => {
                if args.is_empty() {
                    "VARCHAR(4000)".into()
                } else {
                    format!("VARCHAR({})", strip_sem(&args))
                }
            }
            "CHAR" | "NCHAR" | "CHARACTER" if !args.is_empty() => {
                format!("{w}({})", strip_sem(&args))
            }
            "NCLOB" => "LONGTEXT".into(),
            "TIMESTAMP" => {
                let p = args.first().cloned().unwrap_or_else(|| "6".into());
                // WITH [LOCAL] TIME ZONE: no zone-aware type on MariaDB.
                let mut j = ts.next(after - 1, b);
                if ts.is(j, "WITH") {
                    let mut k = ts.next(j, b);
                    if ts.is(k, "LOCAL") {
                        k = ts.next(k, b);
                    }
                    if ts.is(k, "TIME") && ts.is(ts.next(k, b), "ZONE") {
                        j = ts.next(k, b);
                        next = j + 1;
                    }
                }
                format!("DATETIME({p})")
            }
            "LONG" if ts.is(nx, "RAW") => {
                next = nx + 1;
                "LONGBLOB".into()
            }
            _ => return None,
        };
        // identity column
        let g = ts.sig(next, b);
        if ts.is(g, "GENERATED") {
            let mut j = g;
            let mut ident_end = None;
            while j < b && ts.k(j) != K::Comma && ts.k(j) != K::RParen {
                if ts.is(j, "IDENTITY") {
                    let mut e = j + 1;
                    let p = ts.sig(e, b);
                    if p < b && ts.k(p) == K::LParen && ts.pair[p] != NONE {
                        e = ts.pair[p] + 1;
                    }
                    ident_end = Some(e);
                    break;
                }
                if ts.k(j) == K::LParen {
                    break; // GENERATED ALWAYS AS (expr): virtual column
                }
                j += 1;
            }
            if let Some(e) = ident_end {
                let mut s = "BIGINT AUTO_INCREMENT".to_string();
                if !self.identity_is_keyed(i, e, b) {
                    s.push_str(" UNIQUE");
                }
                return Some((s, e));
            }
        }
        Some((text, next))
    }

    /// Is the identity column defined at `col_type` already a key?
    fn identity_is_keyed(&self, col_type: usize, def_end: usize, b: usize) -> bool {
        let ts = self.ts;
        // rest of this column definition
        let mut j = def_end;
        while j < b && ts.k(j) != K::Comma && ts.k(j) != K::RParen {
            if ts.is_any(j, &["PRIMARY", "UNIQUE"]) {
                return true;
            }
            if ts.k(j) == K::LParen && ts.pair[j] != NONE {
                j = ts.pair[j];
            }
            j += 1;
        }
        // table-level PRIMARY KEY (…, col, …)
        let col = ts.prev(col_type, 0);
        if col == NONE {
            return false;
        }
        let name = ident_text(ts.text(col));
        let list = ts.parent[col_type];
        if list == NONE {
            return false;
        }
        let close = ts.pair[list];
        let mut k = list + 1;
        while k < close {
            if ts.is(k, "PRIMARY") || ts.is(k, "UNIQUE") {
                let mut p = k + 1;
                while p < close && ts.k(p) != K::LParen {
                    p += 1;
                }
                if p < close && ts.pair[p] != NONE {
                    if (p + 1..ts.pair[p]).any(|t| {
                        matches!(ts.k(t), K::Word | K::Quoted) && ident_text(ts.text(t)) == name
                    }) && !(p + 1..ts.pair[p]).any(|t| t == col)
                    {
                        return true;
                    }
                    k = ts.pair[p];
                }
            }
            k += 1;
        }
        false
    }

    /// `CAST(x AS <type>)` target.
    fn cast_target(&self, a: usize, b: usize) -> String {
        let ts = self.ts;
        let (a, b) = ts.trim(a, b);
        let t = up(ts.text(a));
        let nx = ts.next(a, b);
        let args: Vec<String> = if nx < b && ts.k(nx) == K::LParen && ts.pair[nx] != NONE {
            ts.split_commas(nx + 1, ts.pair[nx])
                .into_iter()
                .map(|(x, y)| ts.tslice(x, y).to_string())
                .collect()
        } else {
            vec![]
        };
        let len = |args: &[String]| {
            args.first()
                .and_then(|a| a.split_whitespace().next())
                .map(|s| format!("CHAR({s})"))
                .unwrap_or_else(|| "CHAR".into())
        };
        match t.as_str() {
            "NUMBER" | "NUMERIC" | "DECIMAL" | "DEC" => {
                if let Some((s, _)) = self.ddl_type(a, b) {
                    s
                } else {
                    "DECIMAL(65,30)".into()
                }
            }
            "INTEGER" | "INT" | "SMALLINT" => "DECIMAL(38,0)".into(),
            "BIGINT" => "SIGNED".into(),
            "BINARY_DOUBLE" | "REAL" => "DOUBLE".into(),
            "FLOAT" if args.is_empty() => "DOUBLE".into(),
            "DOUBLE" => "DOUBLE".into(),
            "BINARY_FLOAT" => "FLOAT".into(),
            "VARCHAR2" | "VARCHAR" | "NVARCHAR2" | "CHAR" | "NCHAR" => len(&args),
            "CLOB" | "NCLOB" | "TEXT" | "LONG" => "CHAR".into(),
            "DATE" => "DATETIME".into(),
            "TIMESTAMP" => format!("DATETIME({})", args.first().map_or("6", |s| s.as_str())),
            "RAW" => args
                .first()
                .map(|n| format!("BINARY({n})"))
                .unwrap_or_else(|| "BINARY".into()),
            _ => self.range(a, b),
        }
    }

    // ----- function calls ------------------------------------------------------

    fn call(&self, n: &Node, open: usize, close: usize) -> String {
        match self.call_rule(n, open, close) {
            Some(s) => s,
            None => self.call_default(n, open, close),
        }
    }

    /// The call rendered unchanged except for its name (dictionary routing)
    /// and its contents.
    fn call_default(&self, n: &Node, open: usize, close: usize) -> String {
        let ts = self.ts;
        let (pkg, name) = self.call_name(n);
        let mut out = String::new();
        let routed = if let Some(pkg) = &pkg {
            self.cx
                .route_dictionary
                .then(|| dictionary::package_routine(pkg, &name))
                .flatten()
                .map(|r| format!("`{}`.`{r}`", dictionary::SCHEMA))
        } else if self.cx.route_dictionary
            && (dictionary::is_function(&name) || super::helpers::is_helper(&name))
        {
            Some(self.fname(&name))
        } else {
            None
        };
        match &routed {
            Some(r) => out.push_str(r),
            None => {
                for i in n.a..open {
                    if !ts.is_trivia(i) || i > n.a {
                        out.push_str(&self.token(i));
                    }
                }
                // tokens between name and `(` (rare whitespace) kept above
            }
        }
        if routed.is_some() {
            out.push_str(ts.slice(self.path_parts(n.a).1, open));
        }
        out.push('(');
        out.push_str(&self.range(open + 1, close));
        out.push(')');
        out.push_str(&self.range(close + 1, n.b));
        out
    }

    fn arg(&self, r: (usize, usize)) -> String {
        self.range(r.0, r.1)
    }

    fn str_arg(&self, r: (usize, usize)) -> Option<String> {
        let ts = self.ts;
        (r.1 == r.0 + 1 && matches!(ts.k(r.0), K::Str | K::QStr)).then(|| {
            if ts.k(r.0) == K::QStr {
                str_lit_value(&normalize_q_quote(ts.text(r.0)))
            } else {
                str_lit_value(ts.text(r.0))
            }
        })
    }

    /// Does the argument range contain a bind or column reference (unsafe to
    /// evaluate more than once / move into a derived table)?
    fn has_bind(&self, a: usize, b: usize) -> bool {
        self.ts.has(a, b, K::Bind)
    }

    fn call_rule(&self, n: &Node, open: usize, close: usize) -> Option<String> {
        let ts = self.ts;
        let (pkg, name) = self.call_name(n);
        let args = self.args(open, close);
        let tail = self.range(close + 1, n.b);
        if let Some(pkg) = pkg.as_deref() {
            return self.package_call(pkg, &name, &args, n);
        }
        let quoted_name = ts.k(n.a) != K::Word;
        if quoted_name {
            return None;
        }
        let a = |k: usize| self.arg(args[k]);
        let s = match (name.as_str(), args.len()) {
            ("TO_NUMBER", 1) => {
                if let Some(v) = self.str_arg(args[0])
                    && is_oracle_number(v.trim())
                {
                    return Some(v.trim().to_string());
                }
                format!("{}({})", self.fname("dbsaci_to_number"), a(0))
            }
            ("TO_NUMBER", 2 | 3) => format!(
                "{}(REPLACE(REPLACE(REPLACE({}, ',', ''), '$', ''), ' ', ''))",
                self.fname("dbsaci_to_number"),
                a(0)
            ),
            ("TO_DATE" | "TO_TIMESTAMP", 1..=3) => {
                let spec = if args.len() >= 2 {
                    let m = self.str_arg(args[1])?;
                    match datefmt::compile_to_date(&m) {
                        Some(s) => s,
                        None => {
                            self.fail(Error::ora(1821, "date format not recognized"));
                            return None;
                        }
                    }
                } else if name == "TO_TIMESTAMP" {
                    ("%d-%b-%y %h.%i.%s.%f %p".to_string(), true)
                } else {
                    (datefmt::DEFAULT_DATE_SPEC.to_string(), true)
                };
                let call = if self.cx.route_dictionary {
                    format!(
                        "{}({}, '{}', {})",
                        self.fname("dbsaci_str_to_date"),
                        a(0),
                        spec.0.replace('\'', "''"),
                        u8::from(spec.1)
                    )
                } else {
                    // without the helper: no validation / RR century fix
                    format!("STR_TO_DATE({}, '{}')", a(0), spec.0.replace('\'', "''"))
                };
                if name == "TO_DATE" {
                    format!("CAST({call} AS DATETIME)")
                } else {
                    call
                }
            }
            ("TO_CHAR", 1) => match self.arg_kind(args[0]) {
                Vk::Text => a(0),
                Vk::Num if self.cx.route_dictionary => {
                    format!("{}({}, 'TM9')", self.fname("dbsaci_to_char_num"), a(0))
                }
                _ => format!("CAST({} AS CHAR(4000))", a(0)),
            },
            ("TO_CHAR", 2 | 3) => return self.to_char(n, &args, &tail),
            ("TRUNC" | "ROUND", 1 | 2) => return self.trunc_round(&name, &args, &tail),
            ("REGEXP_LIKE", 2 | 3) => {
                let flags = if args.len() == 3 {
                    self.str_arg(args[2])?
                } else {
                    String::new()
                };
                format!("({} REGEXP {})", a(0), self.regex_pattern(args[1], &flags))
            }
            ("REGEXP_REPLACE", 3) => format!(
                "REGEXP_REPLACE({}, {}, {})",
                a(0),
                self.regex_pattern(args[1], ""),
                a(2)
            ),
            ("REGEXP_REPLACE", 2) => {
                format!(
                    "REGEXP_REPLACE({}, {}, '')",
                    a(0),
                    self.regex_pattern(args[1], "")
                )
            }
            ("REGEXP_REPLACE", 4..=6) => {
                let pos_one = self.str_or_num(args[3]).as_deref() == Some("1");
                let occ_zero = args.len() < 5 || self.str_or_num(args[4]).as_deref() == Some("0");
                let flags = if args.len() == 6 {
                    self.str_arg(args[5])?
                } else {
                    String::new()
                };
                if !(pos_one && occ_zero) {
                    self.fail(Error::ora(
                        3001,
                        "unimplemented feature: REGEXP_REPLACE position/occurrence on MariaDB",
                    ));
                    return None;
                }
                format!(
                    "REGEXP_REPLACE({}, {}, {})",
                    a(0),
                    self.regex_pattern(args[1], &flags),
                    a(2)
                )
            }
            ("REGEXP_INSTR", 2) => {
                format!(
                    "REGEXP_INSTR({}, {})",
                    a(0),
                    self.regex_pattern(args[1], "")
                )
            }
            ("REGEXP_SUBSTR", 2) => {
                format!(
                    "REGEXP_SUBSTR({}, {})",
                    a(0),
                    self.regex_pattern(args[1], "")
                )
            }
            ("REGEXP_SUBSTR", 3..=6) => {
                let flags = if args.len() >= 5 {
                    self.str_arg(args[4]).unwrap_or_default()
                } else {
                    String::new()
                };
                let occ = if args.len() >= 4 { a(3) } else { "1".into() };
                let grp = if args.len() == 6 { a(5) } else { "NULL".into() };
                format!(
                    "{}({}, {}, {}, {occ}, {grp})",
                    self.fname("oracle_regexp_substr"),
                    a(0),
                    self.regex_pattern(args[1], &flags),
                    a(2)
                )
            }
            ("REGEXP_COUNT", 2..=4) => {
                let flags = if args.len() == 4 {
                    self.str_arg(args[3])?
                } else {
                    String::new()
                };
                let subj = if args.len() >= 3 && self.str_or_num(args[2]).as_deref() != Some("1") {
                    format!("SUBSTR({}, {})", a(0), a(2))
                } else {
                    a(0)
                };
                format!(
                    "{}({subj}, {})",
                    self.fname("regexp_count"),
                    self.regex_pattern(args[1], &flags)
                )
            }
            ("LTRIM" | "RTRIM", 2) => {
                let set = self.str_arg(args[1])?;
                let kw = if name == "LTRIM" {
                    "LEADING"
                } else {
                    "TRAILING"
                };
                if set.chars().count() == 1 {
                    format!("TRIM({kw} {} FROM {})", a(1), a(0))
                } else if set.is_empty() {
                    "NULL".into()
                } else {
                    let class: String = set
                        .chars()
                        .flat_map(|c| {
                            if matches!(c, '\\' | ']' | '^' | '-' | '[') {
                                vec!['\\', c]
                            } else {
                                vec![c]
                            }
                        })
                        .collect::<String>()
                        .replace('\'', "''");
                    let pat = if name == "LTRIM" {
                        format!("'(?-i)^[{class}]+'")
                    } else {
                        format!("'(?-i)[{class}]+$'")
                    };
                    format!("REGEXP_REPLACE({}, {pat}, '')", a(0))
                }
            }
            ("INSTR", 3) => format!(
                "{}({}, {}, {}, 1)",
                self.fname("oracle_instr"),
                a(0),
                a(1),
                a(2)
            ),
            ("INSTR", 4) => format!(
                "{}({}, {}, {}, {})",
                self.fname("oracle_instr"),
                a(0),
                a(1),
                a(2),
                a(3)
            ),
            ("TRANSLATE", 3) => format!(
                "{}({}, {}, {})",
                self.fname("oracle_translate"),
                a(0),
                a(1),
                a(2)
            ),
            ("BITAND", 2) => format!("({} & {})", a(0), a(1)),
            ("REMAINDER", 2) if !self.has_bind(open, close) => {
                let (x, y) = (a(0), a(1));
                format!("(({x}) - ({y}) * ROUND(({x}) / ({y})))")
            }
            ("LNNVL", 1) => format!("(({}) IS NOT TRUE)", a(0)),
            ("NANVL", 2) => format!("({})", a(0)),
            ("RAWTOHEX", 1) => format!("HEX({})", a(0)),
            ("HEXTORAW", 1) => format!("UNHEX({})", a(0)),
            ("DECODE", 2)
                if self
                    .str_arg(args[1])
                    .is_some_and(|v| v.eq_ignore_ascii_case("hex")) =>
            {
                format!("UNHEX({})", a(0))
            }
            ("PG_SLEEP", 1) => format!("SLEEP({})", a(0)),
            // DATABASE() inside a stored routine is the routine's own schema,
            // so the current schema is computed inline, in the caller's context
            // (same Oracle-name mapping as `dbsaci_sys.oname`).
            ("SYS_CONTEXT", 2)
                if self.str_arg(args[1]).is_some_and(|k| {
                    k.eq_ignore_ascii_case("CURRENT_SCHEMA")
                        || k.eq_ignore_ascii_case("SESSION_SCHEMA")
                }) =>
            {
                "CAST(IF(@dbsaci_lower_names = 1 AND CAST(DATABASE() AS BINARY) = \
                 CAST(LOWER(DATABASE()) AS BINARY), UPPER(DATABASE()), DATABASE()) AS CHAR(128))"
                    .to_string()
            }
            ("RAISE_APPLICATION_ERROR", 2 | 3) => {
                let code = self
                    .str_or_num(args[0])
                    .and_then(|c| c.trim_start_matches('-').parse::<u32>().ok());
                return Some(match (code, self.str_arg(args[1])) {
                    (Some(c), Some(m)) => format!(
                        "SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = '{}'",
                        format!("ORA-{c:05}: {m}").replace('\'', "''")
                    ),
                    _ => format!(
                        "SET @dbsaci_err = CONCAT('ORA-', LPAD(ABS({}), 5, '0'), ': ', {}); \
                         SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = @dbsaci_err",
                        a(0),
                        a(1)
                    ),
                });
            }
            ("CAST", _) => {
                let as_kw = ts.find_kw(open + 1, close, &["AS"])?;
                let (va, vb) = ts.trim(open + 1, as_kw);
                let (ta, tb) = ts.trim(as_kw + 1, close);
                if ts.is(ta, "INTERVAL") {
                    return self.cast_interval(va, vb, ta, tb);
                }
                let target = self.cast_target(ta, tb);
                if target == "DATETIME" && vb == va + 1 && ts.k(va) == K::Str {
                    return Some(format!(
                        "{}({})",
                        self.fname("dbsaci_to_date"),
                        self.range(va, vb)
                    ));
                }
                format!("CAST({} AS {target})", self.range(va, vb))
            }
            ("EXTRACT", _) => return self.extract(open, close),
            ("LISTAGG", 1..=2) => {
                if ts.find_kw(close + 1, n.b, &["OVER"]).is_some() {
                    return None; // windowed: handled structurally
                }
                return self.group_concat(n, &args, close);
            }
            ("STRING_AGG", 2) => {
                let ob = ts.find_kw(args[1].0, args[1].1, &["ORDER"]);
                let (sa, sb) = (args[1].0, ob.unwrap_or(args[1].1));
                let order = ob
                    .map(|o| {
                        let by = ts.next(o, args[1].1);
                        format!(" ORDER BY{}", self.order_items(by + 1, args[1].1))
                    })
                    .unwrap_or_default();
                format!(
                    "GROUP_CONCAT({}{order} SEPARATOR {})",
                    a(0),
                    ts.tslice(sa, sb)
                )
            }
            ("LAG" | "LEAD", 3) => {
                let over = ts.find_kw(close + 1, n.b, &["OVER"])?;
                if self.has_bind(open, close) {
                    return None;
                }
                let win = self.range(over, n.b);
                format!(
                    "CASE WHEN {name}(1, {}) {win} IS NOT NULL THEN {name}({}, {}) {win} ELSE {} END",
                    a(1),
                    a(0),
                    a(1),
                    a(2)
                )
            }
            ("RATIO_TO_REPORT", 1) => {
                let over = ts.find_kw(close + 1, n.b, &["OVER"])?;
                let win = self.range(over, n.b);
                format!("(({}) / SUM({}) {win})", a(0), a(0))
            }
            _ => return None,
        };
        Some(s)
    }

    fn str_or_num(&self, r: (usize, usize)) -> Option<String> {
        let ts = self.ts;
        if r.1 == r.0 + 1 && ts.k(r.0) == K::Num {
            return Some(ts.text(r.0).to_string());
        }
        if r.1 == r.0 + 2 && ts.is_op(r.0, "-") && ts.k(r.0 + 1) == K::Num {
            return Some(format!("-{}", ts.text(r.0 + 1)));
        }
        self.str_arg(r)
    }

    /// Pattern with Oracle's match parameters as an inline PCRE flag group.
    /// Oracle matching is case-sensitive by default (`(?-i)`).
    fn regex_pattern(&self, r: (usize, usize), flags: &str) -> String {
        let mut inline = String::new();
        let mut ci = false;
        for c in flags.chars() {
            match c {
                'i' => ci = true,
                'c' => ci = false,
                'n' => inline.push('s'),
                'm' => inline.push('m'),
                'x' => inline.push('x'),
                _ => {}
            }
        }
        let prefix = if ci {
            format!("(?i{inline})")
        } else if inline.is_empty() {
            "(?-i)".to_string()
        } else {
            format!("(?{inline}-i)")
        };
        match self.str_arg(r) {
            Some(p) => format!("'{}{}'", prefix, p.replace('\'', "''")),
            None => format!("CONCAT('{prefix}', {})", self.arg(r)),
        }
    }

    fn package_call(
        &self,
        pkg: &str,
        name: &str,
        args: &[(usize, usize)],
        n: &Node,
    ) -> Option<String> {
        let a = |k: usize| self.arg(args[k]);
        match (pkg, name, args.len()) {
            ("DBMS_LOB", "GETLENGTH", 1) => Some(format!("LENGTH({})", a(0))),
            ("DBMS_LOB", "SUBSTR", 1) => Some(format!("SUBSTR({}, 1, 32767)", a(0))),
            ("DBMS_LOB", "SUBSTR", 2) => Some(format!("SUBSTR({}, 1, {})", a(0), a(1))),
            ("DBMS_LOB", "SUBSTR", 3) => Some(format!("SUBSTR({}, {}, {})", a(0), a(2), a(1))),
            ("DBMS_LOB", "INSTR", 2) => Some(format!("INSTR({}, {})", a(0), a(1))),
            ("DBMS_LOB", "INSTR", 3) => Some(format!("LOCATE({}, {}, {})", a(1), a(0), a(2))),
            ("DBMS_LOB", "INSTR", 4) => Some(format!(
                "{}({}, {}, {}, {})",
                self.fname("oracle_instr"),
                a(0),
                a(1),
                a(2),
                a(3)
            )),
            ("DBMS_OUTPUT", "PUT_LINE", _) if !self.cx.route_dictionary => {
                let _ = n;
                Some("NULL".into())
            }
            _ => None,
        }
    }

    fn cast_interval(&self, va: usize, vb: usize, ta: usize, tb: usize) -> Option<String> {
        let ts = self.ts;
        let lit = self.str_arg((va, vb))?;
        let ty = up(&ts
            .slice(ta, tb)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "));
        let (sign, body) = match lit.trim().strip_prefix('-') {
            Some(b) => (-1i64, b.trim().to_string()),
            None => (1, lit.trim().to_string()),
        };
        if ty.starts_with("INTERVAL YEAR") {
            let (y, m) = body.split_once('-')?;
            let months =
                sign * (y.trim().parse::<i64>().ok()? * 12 + m.trim().parse::<i64>().ok()?);
            return Some(format!(
                "{}({months}, 'MONTH')",
                self.fname("numtoyminterval")
            ));
        }
        if ty.starts_with("INTERVAL DAY") {
            let secs = day_second_seconds(&body, "DAY", Some("SECOND"))?;
            let secs = if sign < 0 { -secs } else { secs };
            return Some(format!(
                "{}({}, 'SECOND')",
                self.fname("numtodsinterval"),
                fmt_seconds(secs)
            ));
        }
        None
    }

    /// `EXTRACT(unit FROM (a - b))` over a datetime difference: the interval's
    /// component. Plain `EXTRACT(unit FROM date)` is native.
    fn extract(&self, open: usize, close: usize) -> Option<String> {
        let ts = self.ts;
        let from = ts.find_kw(open + 1, close, &["FROM"])?;
        let unit = up(ts.tslice(open + 1, from));
        let (ea, eb) = ts.trim(from + 1, close);
        let e = self.parse_full(ea, eb)?;
        let inner = match e.kind {
            Nk::Paren { open, close } => self.parse_full(open + 1, close)?,
            _ => e,
        };
        let Nk::Bin { op } = inner.kind else {
            return Some(format!("EXTRACT({unit} FROM {})", self.range(ea, eb)));
        };
        if ts.text(op) != "-"
            || !self.kind(&inner.kids[0]).dateish()
            || !self.kind(&inner.kids[1]).dateish()
            || self.has_bind(ea, eb)
        {
            return None;
        }
        let a = self.node(&inner.kids[0]);
        let b = self.node(&inner.kids[1]);
        let secs = format!("TIMESTAMPDIFF(SECOND, {b}, {a})");
        Some(match unit.as_str() {
            "DAY" => format!("TRUNCATE({secs} / 86400, 0)"),
            "HOUR" => format!("TRUNCATE(MOD({secs}, 86400) / 3600, 0)"),
            "MINUTE" => format!("TRUNCATE(MOD({secs}, 3600) / 60, 0)"),
            "SECOND" => format!("(MOD(TIMESTAMPDIFF(MICROSECOND, {b}, {a}), 60000000) / 1000000)"),
            _ => return None,
        })
    }

    fn group_concat(&self, n: &Node, args: &[(usize, usize)], close: usize) -> Option<String> {
        let ts = self.ts;
        // `ON OVERFLOW …` sits inside the argument list after the separator.
        let (a0, b0) = args[0];
        let (expr_a, distinct) = if ts.is(a0, "DISTINCT") {
            (ts.next(a0, b0), "DISTINCT ")
        } else if ts.is(a0, "ALL") {
            (ts.next(a0, b0), "")
        } else {
            (a0, "")
        };
        let expr = self.range(expr_a, b0);
        let sep = if args.len() == 2 {
            let (sa, sb) = args[1];
            let sb = ts.find_kw(sa, sb, &["ON"]).unwrap_or(sb);
            let (sa, sb) = ts.trim(sa, sb);
            if sb == sa + 1 && matches!(ts.k(sa), K::Str | K::QStr) {
                self.token(sa)
            } else if sb == sa + 1 && ts.is(sa, "NULL") {
                "''".into()
            } else {
                self.fail(Error::ora(
                    3001,
                    "unimplemented feature: LISTAGG delimiter must be a literal on MariaDB",
                ));
                return None;
            }
        } else {
            let (a0, b0) = args[0];
            if let Some(on) = ts.find_kw(a0, b0, &["ON"]) {
                let _ = on;
            }
            "''".into()
        };
        let mut order = String::new();
        if let Some(w) = ts.find_kw(close + 1, n.b, &["WITHIN"]) {
            let g = ts.next(w, n.b);
            let p = ts.next(g, n.b);
            if p < n.b && ts.k(p) == K::LParen && ts.pair[p] != NONE {
                let ob = ts.next(p, ts.pair[p]);
                if ts.is(ob, "ORDER") {
                    let by = ts.next(ob, ts.pair[p]);
                    order = format!(" ORDER BY{}", self.order_items(by + 1, ts.pair[p]));
                }
            }
        }
        Some(format!(
            "GROUP_CONCAT({distinct}{expr}{} SEPARATOR {sep})",
            order.trim_end()
        ))
    }

    fn trunc_round(&self, name: &str, args: &[(usize, usize)], tail: &str) -> Option<String> {
        let k = self.arg_kind(args[0]);
        let date = k.dateish() || (args.len() == 2 && self.is_date_unit_lit(args[1]));
        let x = self.arg(args[0]);
        if !date {
            if name == "ROUND" {
                return None; // native numeric ROUND
            }
            let d = if args.len() == 2 {
                self.arg(args[1])
            } else {
                "0".into()
            };
            return Some(format!("TRUNCATE({x}, {d}){tail}"));
        }
        let unit = if args.len() == 2 {
            self.str_arg(args[1]).map(|u| up(u.trim()))
        } else {
            Some("DD".into())
        };
        let has_bind = self.has_bind(args[0].0, args[0].1);
        let Some(unit) = unit else {
            // non-literal format: only the stored helper can interpret it
            if !self.cx.route_dictionary {
                self.fail(Error::ora(
                    3001,
                    "unimplemented feature: non-literal TRUNC/ROUND date format on MariaDB",
                ));
                return None;
            }
            let f = if name == "TRUNC" {
                "dbsaci_trunc_date"
            } else {
                "dbsaci_round_date"
            };
            return Some(format!(
                "{}({x}, {}){tail}",
                self.fname(f),
                self.arg(args[1])
            ));
        };
        let Some(formula) = date_unit_formula(name == "TRUNC", &unit) else {
            self.fail(Error::ora(1821, "date format not recognized"));
            return None;
        };
        let uses = formula.matches("{x}").count();
        let s = if uses <= 1 || !has_bind {
            formula.replace("{x}", &x)
        } else if self.cx.route_dictionary {
            let f = if name == "TRUNC" {
                "dbsaci_trunc_date"
            } else {
                "dbsaci_round_date"
            };
            format!("{}({x}, '{unit}')", self.fname(f))
        } else {
            formula
                .replacen("{x}", &format!("(@dbsaci_v := {x})"), 1)
                .replace("{x}", "@dbsaci_v")
        };
        Some(format!("{s}{tail}"))
    }

    fn to_char(&self, n: &Node, args: &[(usize, usize)], tail: &str) -> Option<String> {
        let _ = n;
        let model = self.str_arg(args[1])?;
        let k = self.arg_kind(args[0]);
        let x = self.arg(args[0]);
        if is_number_model(&model) && !k.dateish() && !self.cx.route_dictionary {
            let (xa, xb) = args[0];
            let inline = number_format_inline(&model)?;
            let out = if inline.matches("{x}").count() <= 1 || !self.has_bind(xa, xb) {
                inline.replace("{x}", &format!("({x})"))
            } else {
                inline
                    .replacen("{x}", &format!("(@dbsaci_v := {x})"), 1)
                    .replace("{x}", "@dbsaci_v")
            };
            return Some(format!("{out}{tail}"));
        }
        if is_number_model(&model) && !k.dateish() {
            return Some(format!(
                "{}({x}, '{}'){tail}",
                self.fname("dbsaci_to_char_num"),
                model.replace('\'', "''")
            ));
        }
        if k == Vk::Num || k == Vk::Text && !is_date_model(&model) {
            return None;
        }
        let Some(pieces) = datefmt::compile_to_char(&model) else {
            self.fail(Error::ora(1821, "date format not recognized"));
            return None;
        };
        let uses = datefmt::value_uses(&pieces);
        let (xa, xb) = args[0];
        let simple = xb == xa + 1
            || self
                .parse_full(xa, xb)
                .is_some_and(|e| matches!(e.kind, Nk::Path | Nk::TypedLit | Nk::Atom));
        let out = if uses <= 1 || (simple && !self.has_bind(xa, xb)) {
            datefmt::render_pieces(&pieces, &x)
        } else if !self.has_bind(xa, xb) {
            // non-trivial expression: evaluate once through a user variable
            let first = datefmt::render_pieces(&pieces, "@dbsaci_v");
            first.replacen("@dbsaci_v", &format!("(@dbsaci_v := {x})"), 1)
        } else {
            let first = datefmt::render_pieces(&pieces, "@dbsaci_v");
            first.replacen("@dbsaci_v", &format!("(@dbsaci_v := {x})"), 1)
        };
        Some(format!("{out}{tail}"))
    }
}

/// Inline MariaDB expression (over `{x}`) for `TRUNC`/`ROUND(date, unit)`.
fn date_unit_formula(trunc: bool, unit: &str) -> Option<String> {
    let t = |u: &str| date_unit_formula(true, u);
    let f = match (trunc, unit) {
        (true, "DD" | "DDD" | "J") => "DATE({x})".into(),
        (true, "MM" | "MON" | "MONTH" | "RM") => "CAST(DATE_FORMAT({x}, '%Y-%m-01') AS DATE)".into(),
        (true, "YYYY" | "SYYYY" | "YEAR" | "SYEAR" | "YYY" | "YY" | "Y") => {
            "CAST(DATE_FORMAT({x}, '%Y-01-01') AS DATE)".into()
        }
        (true, "Q") => "(MAKEDATE(YEAR({x}), 1) + INTERVAL (QUARTER({x}) - 1) QUARTER)".into(),
        (true, "IW") => "(DATE({x}) - INTERVAL WEEKDAY({x}) DAY)".into(),
        (true, "IYYY" | "IYY" | "IY" | "I") => "(MAKEDATE(FLOOR(YEARWEEK({x}, 3) / 100), 4) - INTERVAL WEEKDAY(MAKEDATE(FLOOR(YEARWEEK({x}, 3) / 100), 4)) DAY)"
            .into(),
        (true, "WW") => {
            "(MAKEDATE(YEAR({x}), 1) + INTERVAL (FLOOR((DAYOFYEAR({x}) - 1) / 7) * 7) DAY)".into()
        }
        (true, "W") => "(DATE({x}) - INTERVAL MOD(DAY({x}) - 1, 7) DAY)".into(),
        (true, "DAY" | "DY" | "D") => "(DATE({x}) - INTERVAL (DAYOFWEEK({x}) - 1) DAY)".into(),
        (true, "CC" | "SCC") => "MAKEDATE(FLOOR((YEAR({x}) - 1) / 100) * 100 + 1, 1)".into(),
        (true, "HH" | "HH12" | "HH24") => {
            "CAST(DATE_FORMAT({x}, '%Y-%m-%d %H:00:00') AS DATETIME)".into()
        }
        (true, "MI") => "CAST(DATE_FORMAT({x}, '%Y-%m-%d %H:%i:00') AS DATETIME)".into(),
        (false, "DD" | "DDD" | "J") => "DATE({x} + INTERVAL 12 HOUR)".into(),
        (false, "MM" | "MON" | "MONTH" | "RM") => "CAST(DATE_FORMAT(DATE_ADD({x}, INTERVAL IF(DAYOFMONTH({x}) >= 16, 1, 0) MONTH), '%Y-%m-01') AS DATE)"
            .into(),
        (false, "YYYY" | "SYYYY" | "YEAR" | "SYEAR" | "YYY" | "YY" | "Y") => {
            "MAKEDATE(YEAR({x}) + IF(MONTH({x}) >= 7, 1, 0), 1)".into()
        }
        (false, "Q") => t("Q")?.replace(
            "{x}",
            "IF(MOD(MONTH({x}) - 1, 3) = 2 OR (MOD(MONTH({x}) - 1, 3) = 1 AND DAY({x}) >= 16), {x} + INTERVAL 1 QUARTER, {x})",
        ),
        (false, "IW" | "WW" | "W" | "DAY" | "DY" | "D" | "IYYY" | "IYY" | "IY" | "I") => {
            t(unit)?.replace("{x}", "({x} + INTERVAL 84 HOUR)")
        }
        (false, "CC" | "SCC") => "MAKEDATE(FLOOR((YEAR({x}) - 1) / 100) * 100 + 1              + IF(MOD(YEAR({x}) - 1, 100) >= 50, 100, 0), 1)"
            .into(),
        (false, "HH" | "HH12" | "HH24") => t("HH")?.replace("{x}", "({x} + INTERVAL 30 MINUTE)"),
        (false, "MI") => t("MI")?.replace("{x}", "({x} + INTERVAL 30 SECOND)"),
        _ => return None,
    };
    Some(f)
}

/// Seconds of a day-to-second interval literal body (`'1 02:03:04.5'`).
fn day_second_seconds(body: &str, from: &str, to: Option<&str>) -> Option<f64> {
    let body = body.trim();
    let mul = |u: &str| match u {
        "DAY" => 86400.0,
        "HOUR" => 3600.0,
        "MINUTE" => 60.0,
        _ => 1.0,
    };
    if to.is_none() {
        return body.parse::<f64>().ok().map(|v| v * mul(from));
    }
    // split into fields: days, then h:m:s
    let (days, rest) = if from == "DAY" {
        match body.split_once(' ') {
            Some((d, r)) => (d.trim().parse::<f64>().ok()?, r.trim().to_string()),
            None => (body.parse::<f64>().ok()?, String::new()),
        }
    } else {
        (0.0, body.to_string())
    };
    let mut secs = days * 86400.0;
    if !rest.is_empty() {
        let fields: Vec<&str> = rest.split(':').collect();
        let order: &[f64] = match from {
            "DAY" | "HOUR" => &[3600.0, 60.0, 1.0],
            "MINUTE" => &[60.0, 1.0],
            _ => &[1.0],
        };
        for (f, m) in fields.iter().zip(order) {
            secs += f.trim().parse::<f64>().ok()? * m;
        }
    } else if from != "DAY" {
        secs = body.parse::<f64>().ok()? * mul(from);
    }
    Some(secs)
}

fn fmt_seconds(s: f64) -> String {
    if s.fract() == 0.0 {
        format!("{}", s as i64)
    } else {
        let t = format!("{s:.6}");
        t.trim_end_matches('0').to_string()
    }
}

/// Content of a `'…'` literal (prefix `n`/`x` dropped, `''` unescaped).
pub fn str_lit_value(t: &str) -> String {
    let start = t.find('\'').map_or(0, |p| p + 1);
    let end = if t.ends_with('\'') && t.len() > start {
        t.len() - 1
    } else {
        t.len()
    };
    t.get(start..end).unwrap_or("").replace("''", "'")
}

/// `q'[…]'` → `'…'`.
pub fn normalize_q_quote(t: &str) -> String {
    let q = t.find('\'').unwrap_or(0);
    let inner = t.get(q + 2..t.len().saturating_sub(2)).unwrap_or("");
    let n = if t[..q].len() == 2 { "N" } else { "" };
    format!("{n}'{}'", inner.replace('\'', "''"))
}

/// Is `s` an Oracle numeric literal (what `TO_NUMBER` accepts by default)?
pub fn is_oracle_number(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(p) => (&s[..p], Some(&s[p + 1..])),
        None => (s, None),
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    let digits_ok = !(ip.is_empty() && fp.is_empty())
        && ip.bytes().all(|b| b.is_ascii_digit())
        && fp.bytes().all(|b| b.is_ascii_digit());
    let exp_ok = exp.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
    });
    digits_ok && exp_ok
}

/// Inline form of a number format model (over `{x}`) for the common
/// layouts: `9`/`0` positions, `,`/`G` grouping by threes, `.`/`D`, `$`, `FM`.
fn number_format_inline(model: &str) -> Option<String> {
    let mut m = model.trim().to_ascii_uppercase();
    let fm = m.starts_with("FM");
    if fm {
        m = m[2..].to_string();
    }
    let dollar = m.contains('$');
    let m = m.replace('$', "").replace('G', ",").replace('D', ".");
    if m.chars().any(|c| !matches!(c, '9' | '0' | ',' | '.')) {
        return None;
    }
    let (ip, fp) = m.split_once('.').unwrap_or((&m, ""));
    let d = fp.len();
    let grouped = ip.contains(',');
    let slots: String = ip.chars().filter(|c| *c != ',').collect();
    let zfrom = slots.find('0');
    let width = 1 + m.len() + usize::from(dollar);
    let core = if grouped {
        format!("FORMAT(ROUND(ABS({{x}}), {d}), {d}, 'en_US')")
    } else {
        format!("CAST(CAST(ROUND(ABS({{x}}), {d}) AS DECIMAL(65,{d})) AS CHAR)")
    };
    let core = match zfrom {
        Some(z) if !grouped => format!(
            "LPAD({core}, {}, '0')",
            slots.len() - z + if d > 0 { d + 1 } else { 0 }
        ),
        None if d > 0 => format!("IF(ABS({{x}}) < 1, SUBSTRING({core}, 2), {core})"),
        _ => core,
    };
    let body = format!(
        "CONCAT(IF({{x}} < 0, '-', ''), {}{core})",
        if dollar { "'$', " } else { "" }
    );
    Some(if fm {
        body
    } else {
        format!("LPAD({body}, {width}, ' ')")
    })
}

/// Oracle number format model?
fn is_number_model(m: &str) -> bool {
    let u = m.trim().to_ascii_uppercase();
    let u = u.strip_prefix("FM").unwrap_or(&u);
    !u.is_empty()
        && (u.contains('9') || u.contains('0'))
        && u.chars().all(|c| {
            matches!(
                c,
                '9' | '0' | ',' | '.' | '$' | 'G' | 'D' | 'S' | 'M' | 'I' | 'P' | 'R'
            )
        })
        || u == "TM9"
        || u == "TM"
}

fn is_date_model(m: &str) -> bool {
    let u = m.to_ascii_uppercase();
    [
        "Y", "MM", "MON", "DD", "HH", "MI", "SS", "DAY", "DY", "Q", "J", "IW", "WW", "AM", "PM",
        "FF", "RR",
    ]
    .iter()
    .any(|t| u.contains(t))
}

/// Error raised by a rule (first one wins).
pub fn take_err(r: &Render) -> Result<()> {
    match r.err.borrow_mut().take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
