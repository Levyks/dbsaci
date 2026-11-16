//! Structural lowerings: rules that reshape a statement (join trees,
//! hierarchical queries, multi-statement DML). They run on canonical text,
//! locate query blocks with the token navigator, and re-assemble text from
//! token ranges so comments and literals inside fragments survive.

use super::lex::{K, NONE, Ts};
use super::render::ident_text;
use super::{MariaDbContext, NOCOUNT};
use crate::error::{Error, Result};

fn unimplemented(what: &str) -> Error {
    Error::ora(3001, format!("unimplemented feature: {what} on MariaDB"))
}

/// Bind marker used while a lowering moves fragments around; resolved by
/// [`finalize_binds`].
pub fn bind_marker(n: usize) -> String {
    format!("__dbsaci_bind_{n}__")
}

/// Text of `a..b` with each `?` replaced by its ordinal marker.
fn frag(ts: &Ts, a: usize, b: usize, ord: &[usize]) -> String {
    let mut out = String::new();
    let (a, b) = ts.trim(a, b);
    for i in a..b {
        if ts.k(i) == K::Bind {
            out.push_str(&bind_marker(ord[i]));
        } else {
            out.push_str(ts.text(i));
        }
    }
    out
}

fn bind_ordinals(ts: &Ts) -> Vec<usize> {
    let mut v = vec![usize::MAX; ts.len()];
    let mut k = 0;
    for i in 0..ts.len() {
        if ts.k(i) == K::Bind {
            v[i] = k;
            k += 1;
        }
    }
    v
}

/// Replace bind markers: in order once each → `?`; otherwise hoist every bind
/// into a user variable set by a leading statement.
pub fn finalize_binds(stmts: &mut Vec<String>, total: usize) {
    if total == 0 || !stmts.iter().any(|s| s.contains("__dbsaci_bind_")) {
        return;
    }
    let mut seq = Vec::new();
    for s in stmts.iter() {
        let mut rest = s.as_str();
        while let Some(p) = rest.find("__dbsaci_bind_") {
            let after = &rest[p + 14..];
            let end = after.find("__").unwrap_or(after.len());
            if let Ok(n) = after[..end].parse::<usize>() {
                seq.push(n);
            }
            rest = &after[end..];
        }
    }
    let in_order = seq.len() == total && seq.iter().enumerate().all(|(i, n)| *n == i);
    for s in stmts.iter_mut() {
        for n in (0..total).rev() {
            let m = bind_marker(n);
            if s.contains(&m) {
                *s = s.replace(
                    &m,
                    &if in_order {
                        "?".to_string()
                    } else {
                        format!("@dbsaci_b{n}")
                    },
                );
            }
        }
    }
    if !in_order {
        let sets: Vec<String> = (0..total).map(|n| format!("@dbsaci_b{n} = ?")).collect();
        stmts.insert(0, format!("{NOCOUNT} SET {}", sets.join(", ")));
    }
}

// ---------------------------------------------------------------------------
// query blocks
// ---------------------------------------------------------------------------

/// Clause positions of one `SELECT` block. Ranges exclude the keywords.
#[derive(Debug, Default, Clone)]
pub struct Block {
    pub sel: usize,
    pub end: usize,
    pub list: (usize, usize),
    pub from: Option<(usize, usize, usize)>,
    pub where_: Option<(usize, usize, usize)>,
    pub start: Option<(usize, usize, usize)>,
    pub connect: Option<(usize, usize, usize)>,
    pub group: Option<(usize, usize, usize)>,
    pub having: Option<(usize, usize, usize)>,
    pub order: Option<(usize, usize, usize)>,
    pub siblings: bool,
    pub tail: Option<usize>,
}

pub fn parse_block(ts: &Ts, sel: usize) -> Block {
    let limit = match ts.parent[sel] {
        NONE => ts.len(),
        p => ts.pair[p],
    };
    let mut marks: Vec<(usize, &'static str, usize)> = vec![]; // (kw, name, content start)
    let mut end = limit;
    let mut i = sel + 1;
    while i < limit {
        match ts.k(i) {
            K::LParen if ts.pair[i] != NONE => {
                i = ts.pair[i] + 1;
                continue;
            }
            K::RParen | K::Semi => {
                end = i;
                break;
            }
            K::Word => {
                let nx = |j: usize| ts.next(j, limit);
                let w = ts.text(i).to_ascii_uppercase();
                match w.as_str() {
                    "UNION" | "MINUS" | "INTERSECT" | "EXCEPT" => {
                        end = i;
                        break;
                    }
                    "FROM" => marks.push((i, "FROM", i + 1)),
                    "WHERE" => marks.push((i, "WHERE", i + 1)),
                    "HAVING" => marks.push((i, "HAVING", i + 1)),
                    "START" if ts.is(nx(i), "WITH") => marks.push((i, "START", nx(i) + 1)),
                    "CONNECT" if ts.is(nx(i), "BY") => marks.push((i, "CONNECT", nx(i) + 1)),
                    "GROUP" if ts.is(nx(i), "BY") => marks.push((i, "GROUP", nx(i) + 1)),
                    "ORDER" if ts.is(nx(i), "BY") => marks.push((i, "ORDER", nx(i) + 1)),
                    "ORDER" if ts.is(nx(i), "SIBLINGS") && ts.is(nx(nx(i)), "BY") => {
                        marks.push((i, "SIBLINGS", nx(nx(i)) + 1))
                    }
                    "FETCH" | "OFFSET" | "LIMIT" | "MODEL" | "WINDOW" => marks.push((i, "TAIL", i)),
                    "FOR" if ts.is(nx(i), "UPDATE") => marks.push((i, "TAIL", i)),
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
    let mut b = Block {
        sel,
        end,
        ..Default::default()
    };
    let first_mark = marks.first().map_or(end, |m| m.0);
    b.list = (sel + 1, first_mark);
    for (n, &(kw, name, start)) in marks.iter().enumerate() {
        let stop = marks.get(n + 1).map_or(end, |m| m.0);
        let r = Some((kw, start, stop));
        match name {
            "FROM" => b.from = r,
            "WHERE" => b.where_ = r,
            "HAVING" => b.having = r,
            "START" => b.start = r,
            "CONNECT" => b.connect = r,
            "GROUP" => b.group = r,
            "ORDER" => b.order = r,
            "SIBLINGS" => {
                b.order = r;
                b.siblings = true;
            }
            "TAIL" => {
                if b.tail.is_none() {
                    b.tail = Some(kw);
                }
            }
            _ => {}
        }
    }
    b
}

/// The `SELECT` keyword of the block containing token `t`.
pub fn enclosing_select(ts: &Ts, t: usize) -> Option<usize> {
    let p = ts.parent[t];
    let (a, _) = if p == NONE {
        (0, ts.len())
    } else {
        (p + 1, ts.pair[p])
    };
    let mut best = None;
    let mut i = a;
    while i < t {
        match ts.k(i) {
            K::LParen if ts.pair[i] != NONE && ts.pair[i] < t => {
                i = ts.pair[i] + 1;
                continue;
            }
            K::Word if ts.is(i, "SELECT") => best = Some(i),
            _ => {}
        }
        i += 1;
    }
    best
}

/// One FROM-list table reference.
#[derive(Debug, Clone)]
pub struct TRef {
    pub a: usize,
    pub b: usize,
    /// correlation name (alias, or the table's last name part)
    pub key: String,
    pub has_alias: bool,
}

/// Parse a single table reference occupying `a..b` exactly.
pub fn table_ref(ts: &Ts, a: usize, b: usize) -> Option<TRef> {
    let (a, b) = ts.trim(a, b);
    if a >= b {
        return None;
    }
    let (name_end, base) = if ts.k(a) == K::LParen {
        let c = ts.pair[a];
        if c == NONE || c >= b {
            return None;
        }
        (c + 1, None)
    } else {
        let mut j = a;
        loop {
            if !matches!(ts.k(j), K::Word | K::Quoted | K::Back) {
                return None;
            }
            if j + 2 < b && ts.k(j + 1) == K::Dot {
                j += 2;
                continue;
            }
            break;
        }
        let last = j;
        (last + 1, Some(last))
    };
    let mut j = ts.sig(name_end, b);
    if ts.is(j, "AS") {
        j = ts.next(j, b);
    }
    if j < b && matches!(ts.k(j), K::Word | K::Quoted | K::Back) && ts.sig(j + 1, b) >= b {
        return Some(TRef {
            a,
            b,
            key: ident_text(ts.text(j)).to_string(),
            has_alias: true,
        });
    }
    if ts.sig(name_end, b) < b {
        return None;
    }
    Some(TRef {
        a,
        b,
        key: base.map(|t| ident_text(ts.text(t)).to_string())?,
        has_alias: false,
    })
}

const JOIN_WORDS: &[&str] = &[
    "JOIN",
    "LEFT",
    "RIGHT",
    "FULL",
    "INNER",
    "CROSS",
    "NATURAL",
    "STRAIGHT_JOIN",
    "OUTER",
];

fn has_join(ts: &Ts, a: usize, b: usize) -> bool {
    ts.find_kw(a, b, JOIN_WORDS).is_some()
}

fn replace_range(ts: &Ts, a: usize, b: usize, with: &str) -> String {
    let s = ts.t.get(a).map_or(ts.src.len(), |t| t.s as usize);
    let e = if b == 0 {
        s
    } else {
        ts.t.get(b - 1).map_or(ts.src.len(), |t| t.e as usize)
    };
    let mut out = String::with_capacity(ts.src.len() + with.len());
    out.push_str(&ts.src[..s]);
    out.push_str(with);
    out.push_str(&ts.src[e.max(s)..]);
    out
}

/// Rename qualifier `old.` → `new.` in a fragment.
fn requalify(ts: &Ts, a: usize, b: usize, old: &str, new: &str) -> String {
    let mut out = String::new();
    for i in a..b {
        if matches!(ts.k(i), K::Word | K::Quoted | K::Back)
            && ident_text(ts.text(i)) == old
            && i + 1 < ts.len()
            && ts.k(i + 1) == K::Dot
            && (i == 0 || ts.k(i - 1) != K::Dot)
        {
            out.push_str(new);
        } else {
            out.push_str(ts.text(i));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// legacy outer join `(+)`
// ---------------------------------------------------------------------------

fn outer_marker_at(ts: &Ts, i: usize) -> bool {
    ts.k(i) == K::LParen && ts.pair[i] != NONE && {
        let p = ts.next(i, ts.len());
        ts.is_op(p, "+") && ts.next(p, ts.len()) == ts.pair[i]
    }
}

pub fn lower_outer_joins(sql: String) -> Result<String> {
    let mut sql = sql;
    for _ in 0..64 {
        let ts = Ts::new(&sql);
        let Some(m) = (0..ts.len()).find(|&i| outer_marker_at(&ts, i)) else {
            return Ok(sql);
        };
        let sel = enclosing_select(&ts, m)
            .ok_or_else(|| Error::ora(1719, "outer join operator (+) not allowed here"))?;
        let blk = parse_block(&ts, sel);
        let (Some((_, fa, fb)), Some((wkw, wa, wb))) = (blk.from, blk.where_) else {
            return Err(Error::ora(936, "missing expression"));
        };
        if !(wa..wb).contains(&m) {
            return Err(Error::ora(
                25156,
                "old style outer join (+) cannot be used with ANSI joins",
            ));
        }
        if has_join(&ts, fa, fb) {
            return Err(Error::ora(
                25156,
                "old style outer join (+) cannot be used with ANSI joins",
            ));
        }
        let items: Vec<TRef> = ts
            .split_commas(fa, fb)
            .into_iter()
            .map(|(a, b)| table_ref(&ts, a, b))
            .collect::<Option<_>>()
            .ok_or_else(|| unimplemented("(+) with this FROM list"))?;
        let keys: Vec<String> = items.iter().map(|t| t.key.to_ascii_uppercase()).collect();

        // conjuncts
        let mut conj: Vec<(usize, usize)> = vec![];
        split_and(&ts, wa, wb, &mut conj);
        let mut filters: Vec<String> = vec![];
        // per outer table: (conditions, deps)
        let mut outer: Vec<(usize, Vec<String>, Vec<usize>)> = vec![];
        for &(ca, cb) in &conj {
            let markers: Vec<usize> = (ca..cb).filter(|&i| outer_marker_at(&ts, i)).collect();
            if markers.is_empty() {
                filters.push(ts.tslice(ca, cb).to_string());
                continue;
            }
            if (ca..cb).any(|i| ts.is(i, "OR") || ts.is(i, "IN")) {
                return Err(Error::ora(
                    1719,
                    "outer join operator (+) not allowed in operand of OR or IN",
                ));
            }
            // marked tables
            let mut marked: Vec<usize> = vec![];
            for &mk in &markers {
                let col = ts.prev(mk, ca);
                if col == NONE {
                    return Err(Error::ora(936, "missing expression"));
                }
                let q = ts.prev(col, ca);
                let table = if q != NONE && ts.k(q) == K::Dot {
                    let qn = ts.prev(q, ca);
                    keys.iter()
                        .position(|k| k.eq_ignore_ascii_case(ident_text(ts.text(qn))))
                        .ok_or_else(|| {
                            Error::ora(
                                1705,
                                "an outer join cannot be specified on a correlation column",
                            )
                        })?
                } else {
                    return Err(unimplemented("unqualified column with (+)"));
                };
                if !marked.contains(&table) {
                    marked.push(table);
                }
            }
            if marked.len() > 1 {
                return Err(Error::ora(
                    1468,
                    "a predicate may reference only one outer-joined table",
                ));
            }
            let target = marked[0];
            // other referenced tables
            let mut deps = vec![];
            for i in ca..cb {
                if matches!(ts.k(i), K::Word | K::Quoted)
                    && i + 1 < cb
                    && ts.k(i + 1) == K::Dot
                    && (i == ca || ts.k(i - 1) != K::Dot)
                    && let Some(p) = keys
                        .iter()
                        .position(|k| k.eq_ignore_ascii_case(ident_text(ts.text(i))))
                    && p != target
                    && !deps.contains(&p)
                {
                    deps.push(p);
                }
            }
            // stripped text
            let mut s = String::new();
            let mut i = ca;
            while i < cb {
                if outer_marker_at(&ts, i) {
                    i = ts.pair[i] + 1;
                    continue;
                }
                s.push_str(ts.text(i));
                i += 1;
            }
            let s = s.trim().to_string();
            match outer.iter_mut().find(|o| o.0 == target) {
                Some(o) => {
                    o.1.push(s);
                    for d in deps {
                        if !o.2.contains(&d) {
                            o.2.push(d);
                        }
                    }
                }
                None => outer.push((target, vec![s], deps)),
            }
        }
        // mutual outer join
        for o in &outer {
            for d in &o.2 {
                if outer.iter().any(|p| p.0 == *d && p.2.contains(&o.0)) {
                    return Err(Error::ora(
                        1416,
                        "two tables cannot be outer-joined to each other",
                    ));
                }
            }
        }
        // order: inner tables first, then outer tables once their deps are placed
        let mut placed: Vec<usize> = (0..items.len())
            .filter(|i| !outer.iter().any(|o| o.0 == *i))
            .collect();
        let mut from = placed
            .iter()
            .map(|&i| ts.tslice(items[i].a, items[i].b).to_string())
            .collect::<Vec<_>>()
            .join(" CROSS JOIN ");
        let mut pending: Vec<usize> = (0..outer.len()).collect();
        while !pending.is_empty() {
            let pos = pending
                .iter()
                .position(|&o| outer[o].2.iter().all(|d| placed.contains(d)))
                .ok_or_else(|| unimplemented("this (+) join graph"))?;
            let o = pending.remove(pos);
            let (t, conds, _) = &outer[o];
            let item = ts.tslice(items[*t].a, items[*t].b);
            if from.is_empty() {
                // every table is outer-joined: Oracle treats it as inner
                from = item.to_string();
                filters.extend(conds.iter().cloned());
            } else {
                from.push_str(&format!(" LEFT JOIN {item} ON {}", conds.join(" AND ")));
            }
            placed.push(*t);
        }
        // `SELECT *` keeps Oracle's FROM-order columns
        let (la, lb) = ts.trim(blk.list.0, blk.list.1);
        let star = lb == la + 1 && ts.is_op(la, "*");
        let mut text = String::new();
        if star {
            text.push_str(&keys_star(&items));
            text.push(' ');
            text.push_str(ts.slice(lb, fa - 1));
        } else {
            text.push_str(ts.slice(blk.list.0, fa - 1));
        }
        text.push_str("FROM ");
        text.push_str(&from);
        if !filters.is_empty() {
            text.push_str(" WHERE ");
            text.push_str(&filters.join(" AND "));
        }
        if wb < ts.len() {
            text.push(' ');
        }
        let start = if star { la } else { blk.list.0 };
        let _ = wkw;
        sql = replace_range(&ts, start, wb, &text);
    }
    Ok(sql)
}

fn keys_star(items: &[TRef]) -> String {
    items
        .iter()
        .map(|t| format!("{}.*", quote_key(&t.key)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_key(k: &str) -> String {
    if k.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#')
    {
        k.to_string()
    } else {
        format!("\"{}\"", k.replace('"', "\"\""))
    }
}

/// Split a WHERE range on top-level AND (respecting BETWEEN) and flatten
/// parenthesised conjunctions that carry `(+)`.
fn split_and(ts: &Ts, a: usize, b: usize, out: &mut Vec<(usize, usize)>) {
    let (a, b) = ts.trim(a, b);
    if a >= b {
        return;
    }
    // a fully parenthesised group with a marker inside: recurse
    if ts.k(a) == K::LParen
        && ts.pair[a] == b - 1
        && (a..b).any(|i| outer_marker_at(ts, i))
        && ts.find_kw(a + 1, b - 1, &["OR"]).is_none()
        && ts.find_kw(a + 1, b - 1, &["AND"]).is_some()
    {
        split_and(ts, a + 1, b - 1, out);
        return;
    }
    let mut start = a;
    let mut i = a;
    let mut between = false;
    while i < b {
        match ts.k(i) {
            K::LParen if ts.pair[i] != NONE && ts.pair[i] < b => {
                i = ts.pair[i] + 1;
                continue;
            }
            K::Word if ts.is(i, "BETWEEN") => between = true,
            K::Word if ts.is(i, "AND") => {
                if between {
                    between = false;
                } else {
                    split_and(ts, start, i, out);
                    start = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if start == a {
        out.push((a, b));
    } else {
        split_and(ts, start, b, out);
    }
}

// ---------------------------------------------------------------------------
// FULL [OUTER] JOIN
// ---------------------------------------------------------------------------

pub fn lower_full_joins(sql: String) -> Result<String> {
    let mut sql = sql;
    for n in 0..16 {
        let ts = Ts::new(&sql);
        let Some(full) = (0..ts.len()).find(|&i| {
            ts.is(i, "FULL") && {
                let j = ts.next(i, ts.len());
                ts.is(j, "JOIN") || (ts.is(j, "OUTER") && ts.is(ts.next(j, ts.len()), "JOIN"))
            }
        }) else {
            return Ok(sql);
        };
        let mut join = ts.next(full, ts.len());
        if ts.is(join, "OUTER") {
            join = ts.next(join, ts.len());
        }
        let sel = enclosing_select(&ts, full).ok_or_else(|| unimplemented("FULL OUTER JOIN"))?;
        let blk = parse_block(&ts, sel);
        let (fkw, fa, fb) = blk.from.ok_or_else(|| unimplemented("FULL OUTER JOIN"))?;
        let _ = fkw;
        // the comma item containing the FULL JOIN
        let (ia, ib) = ts
            .split_commas(fa, fb)
            .into_iter()
            .find(|(a, b)| (*a..*b).contains(&full))
            .unwrap();
        let (ia, ib) = ts.trim(ia, ib);
        let left = (ia, full);
        let on = ts
            .find_kw(join + 1, ib, &["ON"])
            .ok_or_else(|| unimplemented("FULL OUTER JOIN … USING"))?;
        let right = table_ref(&ts, join + 1, on)
            .ok_or_else(|| unimplemented("FULL OUTER JOIN with this right operand"))?;
        // condition ends at the next join keyword
        let cend = ts.find_kw(on + 1, ib, JOIN_WORDS).unwrap_or(ib);
        if ts.find_kw(cend, ib, &["FULL"]).is_some() {
            return Err(unimplemented("chained FULL OUTER JOINs"));
        }
        let (ca, cb) = ts.trim(on + 1, cend);
        if ts.has(left.0, cb, K::Bind) {
            return Err(unimplemented("bind variable in a FULL OUTER JOIN operand"));
        }
        let cond = ts.slice(ca, cb);
        let left_txt = ts.tslice(left.0, left.1);
        let left_wrapped = if has_join(&ts, left.0, left.1) {
            format!("({left_txt})")
        } else {
            left_txt.to_string()
        };
        let right_txt = ts.tslice(right.a, right.b);
        let d = format!("__dbsaci_foj{n}");
        let b2 = format!("__dbsaci_foj{n}_r");
        // right operand under a fresh correlation name, for the "unmatched"
        // existence test
        let right_base = if right.has_alias {
            // drop alias: text up to the alias token
            let (ra, rb) = ts.trim(right.a, right.b);
            let alias_tok = ts.prev(rb, ra);
            let mut e = alias_tok;
            let p = ts.prev(alias_tok, ra);
            if p != NONE && ts.is(p, "AS") {
                e = p;
            }
            ts.tslice(ra, e).to_string()
        } else {
            right_txt.to_string()
        };
        let cond_b2 = requalify(&ts, ca, cb, &right.key, &b2);
        let item = format!(
            "(SELECT 1 AS __k UNION ALL SELECT 2) {d} LEFT JOIN {left_wrapped} ON {d}.__k = 1 \
             LEFT JOIN {right_txt} ON (CASE WHEN {d}.__k = 1 THEN ({cond}) \
             ELSE NOT EXISTS (SELECT 1 FROM {left_txt} WHERE {cond}) END){rest}",
            rest = ts.slice(cend, ib).trim_end().to_string().chars().fold(
                String::new(),
                |mut s, c| {
                    if s.is_empty() && !c.is_whitespace() {
                        s.push(' ');
                    }
                    s.push(c);
                    s
                }
            )
        );
        let filter = format!(
            "({d}.__k = 1 AND EXISTS (SELECT 1 FROM {left_txt}) OR {d}.__k = 2 AND EXISTS \
             (SELECT 1 FROM {right_base} {b2} WHERE NOT EXISTS (SELECT 1 FROM {left_txt} WHERE {cond_b2})))"
        );
        // assemble: list (star expansion), FROM with item, WHERE + filter
        let mut text = String::new();
        let (la, lb) = ts.trim(blk.list.0, blk.list.1);
        if lb == la + 1 && ts.is_op(la, "*") {
            // all FROM items except the driver, in order
            let mut keys = vec![];
            for (a, b) in ts.split_commas(fa, fb) {
                let (a, b) = ts.trim(a, b);
                let mut j = a;
                let mut seg_start = a;
                while j <= b {
                    let at_end = j == b;
                    let boundary = !at_end && ts.is_any(j, JOIN_WORDS);
                    if at_end || boundary {
                        // seg_start..j holds `[ON …]` and/or a table ref
                        let seg_end = ts.find_kw(seg_start, j, &["ON", "USING"]).unwrap_or(j);
                        if let Some(t) = table_ref(&ts, seg_start, seg_end) {
                            keys.push(quote_key(&t.key));
                        }
                        // skip run of join words
                        let mut k = j;
                        while k < b && (ts.is_any(k, JOIN_WORDS) || ts.is_trivia(k)) {
                            k += 1;
                        }
                        seg_start = k;
                        j = k.max(j + 1);
                        continue;
                    }
                    if ts.k(j) == K::LParen && ts.pair[j] != NONE {
                        j = ts.pair[j] + 1;
                        continue;
                    }
                    j += 1;
                }
            }
            text.push_str(ts.slice(blk.list.0, la));
            text.push_str(
                &keys
                    .iter()
                    .map(|k| format!("{k}.*"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            text.push(' ');
        } else {
            text.push_str(ts.slice(blk.list.0, blk.list.1));
        }
        text.push_str(ts.slice(blk.list.1, ia));
        text.push_str(&item);
        text.push_str(ts.slice(ib, fb));
        let (end_tok, where_txt) = match blk.where_ {
            Some((_, wa, wb)) => (wb, format!(" WHERE {filter} AND ({})", ts.tslice(wa, wb))),
            None => (fb, format!(" WHERE {filter}")),
        };
        let mut text = format!("{}{where_txt}", text.trim_end());
        if end_tok < ts.len() {
            text.push(' ');
        }
        sql = replace_range(&ts, blk.list.0, end_tok, &text);
    }
    Ok(sql)
}

// ---------------------------------------------------------------------------
// CONNECT BY
// ---------------------------------------------------------------------------

pub fn lower_connect_by(sql: String, cx: &MariaDbContext) -> Result<String> {
    let mut sql = sql;
    for _ in 0..16 {
        let ts = Ts::new(&sql);
        let Some(cb) =
            (0..ts.len()).find(|&i| ts.is(i, "CONNECT") && ts.is(ts.next(i, ts.len()), "BY"))
        else {
            return Ok(sql);
        };
        let sel = enclosing_select(&ts, cb).ok_or_else(|| unimplemented("CONNECT BY"))?;
        let blk = parse_block(&ts, sel);
        let text = connect_by_block(&ts, &blk, cx)?;
        let start = if ts.prev(sel, 0) != NONE && ts.parent[sel] == NONE {
            sel
        } else {
            sel
        };
        let sep = if blk.end < ts.len() { " " } else { "" };
        sql = replace_range(&ts, start, blk.end, &format!("{text}{sep}"));
    }
    Ok(sql)
}

fn connect_by_block(ts: &Ts, blk: &Block, cx: &MariaDbContext) -> Result<String> {
    let (_, fa, fb) = blk
        .from
        .ok_or_else(|| unimplemented("CONNECT BY without FROM"))?;
    let (_, ca, cb) = blk.connect.unwrap();
    let mut ca = ts.sig(ca, cb);
    let nocycle = ts.is(ca, "NOCYCLE");
    if nocycle {
        ca = ts.next(ca, cb);
    }
    let cond_range = ts.trim(ca, cb);
    let items = ts.split_commas(fa, fb);
    if items.len() != 1 || has_join(ts, fa, fb) {
        return Err(unimplemented("CONNECT BY over a join"));
    }
    let tref = table_ref(ts, fa, fb).ok_or_else(|| unimplemented("CONNECT BY source"))?;
    let alias = quote_key(&tref.key);
    let table_txt = if tref.has_alias {
        let (ra, rb) = ts.trim(tref.a, tref.b);
        let alias_tok = ts.prev(rb, ra);
        let mut e = alias_tok;
        let p = ts.prev(alias_tok, ra);
        if p != NONE && ts.is(p, "AS") {
            e = p;
        }
        ts.tslice(ra, e).to_string()
    } else {
        ts.tslice(tref.a, tref.b).to_string()
    };
    let is_dual = table_txt.eq_ignore_ascii_case("DUAL");
    let order_txt = blk.order.map(|(_, a, b)| (a, b));
    let tail_txt = blk
        .tail
        .map(|t| ts.slice(t, blk.end).to_string())
        .unwrap_or_default();

    // ---- row generator: SELECT … FROM DUAL CONNECT BY LEVEL <= n --------
    if is_dual {
        let (a, b) = cond_range;
        let lvl = ts.sig(a, b);
        if !(ts.is(lvl, "LEVEL") || ts.is(lvl, "ROWNUM")) {
            return Err(unimplemented("this CONNECT BY over DUAL"));
        }
        let op = ts.next(lvl, b);
        let opt = ts.text(op);
        if !matches!(opt, "<=" | "<") {
            return Err(unimplemented("this CONNECT BY over DUAL"));
        }
        let limit = ts.tslice(op + 1, b);
        let proj = replace_words(
            ts,
            blk.list.0,
            blk.list.1,
            &[("LEVEL", "__cb.lvl"), ("ROWNUM", "__cb.lvl")],
        );
        let mut out = format!(
            "WITH RECURSIVE __cb(lvl) AS (SELECT 1 UNION ALL SELECT lvl + 1 FROM __cb WHERE lvl + 1 {opt} ({limit})) SELECT{proj}FROM __cb"
        );
        if let Some((_, wa, wb)) = blk.where_ {
            out.push_str(" WHERE ");
            out.push_str(&replace_words(ts, wa, wb, &[("LEVEL", "__cb.lvl")]));
        }
        if let Some((a, b)) = order_txt {
            out.push_str(" ORDER BY ");
            out.push_str(&replace_words(ts, a, b, &[("LEVEL", "__cb.lvl")]));
        }
        if !tail_txt.is_empty() {
            out.push(' ');
            out.push_str(&tail_txt);
        }
        return Ok(out);
    }

    // ---- hierarchy over a table -------------------------------------------
    let (a, b) = cond_range;
    if ts.has(a, b, K::Bind) {
        return Err(unimplemented("bind variable in CONNECT BY condition"));
    }
    // link columns: for each `=`, the non-PRIOR side's column (child link)
    // and the PRIOR side (parent key)
    let mut child_cols: Vec<String> = vec![];
    let mut key_cols: Vec<String> = vec![];
    for i in a..b {
        if ts.is(i, "PRIOR") {
            let c = ts.next(i, b);
            let (parts, _) = path(ts, c);
            if let Some(&l) = parts.last() {
                key_cols.push(ident_text(ts.text(l)).to_string());
            }
        }
    }
    for i in a..b {
        if ts.is_op(i, "=") {
            let l = ts.prev(i, a);
            let r = ts.next(i, b);
            // side without PRIOR
            // left operand: back to the previous AND/OR (or the start)
            let mut s = i;
            while s > a && !ts.is_any(s - 1, &["AND", "OR"]) {
                s -= 1;
            }
            let lhs_prior = (s..i).any(|t| ts.is(t, "PRIOR"));
            let side = if lhs_prior { r } else { l };
            let mut c = side;
            if !lhs_prior {
                // l is last token of left operand: walk to path start
                while c > a && ts.k(c - 1) == K::Dot {
                    c -= 2;
                }
            }
            let (parts, _) = path(ts, c);
            if let Some(&last) = parts.last() {
                child_cols.push(ident_text(ts.text(last)).to_string());
            }
            let _ = l;
        }
    }
    if key_cols.is_empty() {
        return Err(unimplemented("CONNECT BY without PRIOR"));
    }
    let child = "__c";
    let cond_step = rewrite_cond(ts, a, b, &tref.key, child, "__cb");
    let ident_cols: Vec<String> = {
        let mut v = key_cols.clone();
        for c in &child_cols {
            if !v.contains(c) {
                v.push(c.clone());
            }
        }
        v
    };
    let key_of = |q: &str| -> String {
        let parts: Vec<String> = ident_cols
            .iter()
            .map(|c| format!("COALESCE(CAST({q}.{} AS CHAR), '~')", quote_key(c)))
            .collect();
        format!("CONCAT(CHAR(1), {}, CHAR(1))", parts.join(", '|', "))
    };
    // sibling ordering → rank per parent
    let sib_order = if blk.siblings {
        let (oa, ob) = order_txt.unwrap();
        ts.slice(oa, ob).to_string()
    } else {
        key_cols
            .iter()
            .map(|c| quote_key(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let partition = if child_cols.is_empty() {
        "1".to_string()
    } else {
        child_cols
            .iter()
            .map(|c| quote_key(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let src = format!(
        "(SELECT __t.*, ROW_NUMBER() OVER (PARTITION BY {partition} ORDER BY {sib_order}) AS __rn FROM {table_txt} __t)"
    );
    // projection rewriting
    let mut extra_seed = String::new();
    let mut extra_step = String::new();
    let mut proj = String::new();
    let mut n_extra = 0;
    let (la, lb) = blk.list;
    let mut i = la;
    while i < lb {
        if ts.is(i, "CONNECT_BY_ROOT") {
            let c = ts.next(i, lb);
            let (parts, end) = path(ts, c);
            let col = ident_text(ts.text(*parts.last().unwrap())).to_string();
            let h = format!("__root{n_extra}");
            n_extra += 1;
            extra_seed.push_str(&format!(", {alias}.{} AS {h}", quote_key(&col)));
            extra_step.push_str(&format!(", __cb.{h}"));
            proj.push_str(&format!("{alias}.{h}"));
            i = end;
            continue;
        }
        if ts.is(i, "SYS_CONNECT_BY_PATH") {
            let p = ts.next(i, lb);
            let close = ts.pair[p];
            let args = ts.split_commas(p + 1, close);
            if args.len() != 2 {
                return Err(unimplemented("SYS_CONNECT_BY_PATH arguments"));
            }
            let col_seed = ts.tslice(args[0].0, args[0].1).to_string();
            let col_step = rewrite_cond(ts, args[0].0, args[0].1, &tref.key, child, child);
            let sep = ts.tslice(args[1].0, args[1].1);
            let h = format!("__path{n_extra}");
            n_extra += 1;
            extra_seed.push_str(&format!(
                ", CAST(CONCAT({sep}, {col_seed}) AS CHAR(4000)) AS {h}"
            ));
            extra_step.push_str(&format!(", CONCAT(__cb.{h}, {sep}, {})", col_step.trim()));
            proj.push_str(&format!("{alias}.{h}"));
            i = close + 1;
            continue;
        }
        if ts.is(i, "LEVEL") {
            proj.push_str(&format!("{alias}.__level"));
            i += 1;
            continue;
        }
        if ts.is(i, "CONNECT_BY_ISLEAF") {
            proj.push_str(&format!(
                "(CASE WHEN EXISTS (SELECT 1 FROM {table_txt} __l WHERE {}) THEN 0 ELSE 1 END)",
                rewrite_cond(ts, a, b, &tref.key, "__l", &alias)
            ));
            i += 1;
            continue;
        }
        if ts.is(i, "CONNECT_BY_ISCYCLE") {
            if !nocycle {
                return Err(Error::ora(
                    30930,
                    "NOCYCLE keyword is required with CONNECT_BY_ISCYCLE pseudo column",
                ));
            }
            proj.push_str(&format!(
                "(CASE WHEN EXISTS (SELECT 1 FROM {table_txt} __l WHERE {} AND INSTR({alias}.__ids, {}) > 0) THEN 1 ELSE 0 END)",
                rewrite_cond(ts, a, b, &tref.key, "__l", &alias),
                key_of("__l")
            ));
            i += 1;
            continue;
        }
        if ts.is(i, "PRIOR") {
            return Err(unimplemented("PRIOR in the select list"));
        }
        proj.push_str(ts.text(i));
        i += 1;
    }
    let seed_where = blk
        .start
        .map(|(_, sa, sb)| format!(" WHERE {}", ts.tslice(sa, sb)))
        .unwrap_or_default();
    let cycle = if nocycle || !cx.route_dictionary {
        format!("INSTR(__cb.__ids, {}) = 0", key_of(child))
    } else {
        format!(
            "(CASE WHEN INSTR(__cb.__ids, {}) > 0 THEN {}(1436, 'CONNECT BY loop in user data') ELSE 1 END) = 1",
            key_of(child),
            fname(cx, "dbsaci_raise")
        )
    };
    let mut out = format!(
        "WITH RECURSIVE __cb AS (\
         SELECT {alias}.*, 1 AS __level, CAST({ids_seed} AS CHAR(4000)) AS __ids, \
         CAST(LPAD({alias}.__rn, 10, '0') AS CHAR(4000)) AS __ord{extra_seed} \
         FROM {src} {alias}{seed_where} \
         UNION ALL \
         SELECT {child}.*, __cb.__level + 1, CONCAT(__cb.__ids, {ids_step}), \
         CONCAT(__cb.__ord, LPAD({child}.__rn, 10, '0')){extra_step} \
         FROM {src} {child} JOIN __cb ON {cond_step} WHERE {cycle}\
         ) SELECT{proj}FROM __cb {alias}",
        ids_seed = key_of(&alias),
        ids_step = key_of(child),
    );
    if let Some((_, wa, wb)) = blk.where_ {
        out.push_str(" WHERE ");
        out.push_str(&replace_words(
            ts,
            wa,
            wb,
            &[("LEVEL", &format!("{alias}.__level"))],
        ));
    }
    match (order_txt, blk.siblings) {
        (Some((oa, ob)), false) => {
            out.push_str(" ORDER BY ");
            out.push_str(&replace_words(
                ts,
                oa,
                ob,
                &[("LEVEL", &format!("{alias}.__level"))],
            ));
        }
        _ => out.push_str(&format!(" ORDER BY {alias}.__ord")),
    }
    if !tail_txt.is_empty() {
        out.push(' ');
        out.push_str(&tail_txt);
    }
    Ok(out)
}

fn fname(cx: &MariaDbContext, name: &str) -> String {
    if cx.route_dictionary {
        format!("`{}`.`{name}`", crate::mariadb::dictionary::SCHEMA)
    } else {
        name.to_string()
    }
}

fn path(ts: &Ts, i: usize) -> (Vec<usize>, usize) {
    let mut parts = vec![];
    let mut j = i;
    while j < ts.len() && matches!(ts.k(j), K::Word | K::Quoted | K::Back) {
        parts.push(j);
        if j + 2 < ts.len() && ts.k(j + 1) == K::Dot {
            j += 2;
        } else {
            return (parts, j + 1);
        }
    }
    (parts, j)
}

/// Replace bare words (not qualified, not calls).
fn replace_words(ts: &Ts, a: usize, b: usize, map: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for i in a..b {
        if ts.k(i) == K::Word
            && (i == 0 || ts.k(i - 1) != K::Dot)
            && let Some((_, r)) = map.iter().find(|(w, _)| ts.is(i, w))
        {
            out.push_str(r);
        } else {
            out.push_str(ts.text(i));
        }
    }
    out
}

/// CONNECT BY condition for the recursive step: `PRIOR x` → `parent.x`,
/// other column references → `child.x`.
fn rewrite_cond(ts: &Ts, a: usize, b: usize, key: &str, child: &str, parent: &str) -> String {
    let mut out = String::new();
    let mut i = a;
    while i < b {
        match ts.k(i) {
            K::Word if ts.is(i, "PRIOR") => {
                let c = ts.next(i, b);
                let (parts, end) = path(ts, c);
                if let Some(&l) = parts.last() {
                    out.push_str(&format!("{parent}.{}", ts.text(l)));
                    i = end;
                    continue;
                }
                i += 1;
            }
            K::Word | K::Quoted | K::Back => {
                let (parts, end) = path(ts, i);
                let nx = ts.sig(end, b);
                let is_call = nx < b && ts.k(nx) == K::LParen;
                let keyword = ts.k(i) == K::Word
                    && ts.is_any(
                        i,
                        &[
                            "AND", "OR", "NOT", "IS", "NULL", "IN", "LIKE", "BETWEEN", "CASE",
                            "WHEN", "THEN", "ELSE", "END", "TRUE", "FALSE", "ESCAPE",
                        ],
                    );
                if is_call || keyword {
                    out.push_str(ts.slice(i, end));
                    i = end;
                    continue;
                }
                let col = ts.text(*parts.last().unwrap());
                let qualified_other = parts.len() >= 2
                    && !ident_text(ts.text(parts[parts.len() - 2])).eq_ignore_ascii_case(key);
                if qualified_other {
                    out.push_str(ts.slice(i, end));
                } else {
                    out.push_str(&format!("{child}.{col}"));
                }
                i = end;
            }
            _ => {
                out.push_str(ts.text(i));
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// MERGE
// ---------------------------------------------------------------------------

pub fn lower_merge(sql: &str, cx: &MariaDbContext) -> Result<Option<Vec<String>>> {
    let ts = Ts::new(sql);
    let n = ts.len();
    let m = ts.sig(0, n);
    if !ts.is(m, "MERGE") {
        return Ok(None);
    }
    let ord = bind_ordinals(&ts);
    let mut i = ts.next(m, n);
    if !ts.is(i, "INTO") {
        return Err(Error::ora(905, "missing keyword"));
    }
    i = ts.next(i, n);
    let using = ts
        .find_kw(i, n, &["USING"])
        .ok_or_else(|| Error::ora(905, "missing keyword"))?;
    let target = table_ref(&ts, i, using).ok_or_else(|| unimplemented("MERGE target"))?;
    let on = ts
        .find_kw(using + 1, n, &["ON"])
        .ok_or_else(|| Error::ora(905, "missing keyword"))?;
    let source = table_ref(&ts, using + 1, on).ok_or_else(|| unimplemented("MERGE source"))?;
    let c0 = ts.next(on, n);
    let (ca, cb, after_cond) = if ts.k(c0) == K::LParen && ts.pair[c0] != NONE {
        (c0 + 1, ts.pair[c0], ts.pair[c0] + 1)
    } else {
        let w = ts.find_kw(c0, n, &["WHEN"]).unwrap_or(n);
        (c0, w, w)
    };
    let tkey = quote_key(&target.key);
    let skey = quote_key(&source.key);
    let target_name = strip_alias(&ts, &target);
    let source_name = strip_alias(&ts, &source);
    let cond = frag(&ts, ca, cb, &ord);

    let mut update: Option<(
        Vec<(String, String, String)>,
        Option<String>,
        Option<String>,
    )> = None;
    let mut update_where_range: Option<(usize, usize)> = None;
    let mut insert: Option<(Option<String>, String, Option<String>)> = None;
    let mut j = ts.sig(after_cond, n);
    while j < n && ts.k(j) != K::Semi {
        if !ts.is(j, "WHEN") {
            return Err(Error::ora(905, "missing keyword"));
        }
        let mut k = ts.next(j, n);
        let not = ts.is(k, "NOT");
        if not {
            k = ts.next(k, n);
        }
        if !ts.is(k, "MATCHED") || !ts.is(ts.next(k, n), "THEN") {
            return Err(Error::ora(905, "missing keyword"));
        }
        let act = ts.next(ts.next(k, n), n);
        let clause_end = ts.find_kw(act, n, &["WHEN"]).unwrap_or(n);
        if !not {
            if ts.is(act, "DELETE") {
                return Err(unimplemented("MERGE … WHEN MATCHED THEN DELETE"));
            }
            if !ts.is(act, "UPDATE") || !ts.is(ts.next(act, n), "SET") {
                return Err(Error::ora(905, "missing keyword"));
            }
            let set_a = ts.next(act, n) + 1;
            let w = ts.find_kw(set_a, clause_end, &["WHERE"]);
            let d = ts.find_kw(set_a, clause_end, &["DELETE"]);
            let set_b = [w, d].into_iter().flatten().min().unwrap_or(clause_end);
            let mut sets = vec![];
            for (x, y) in ts.split_commas(set_a, set_b) {
                let eq = (x..y)
                    .find(|&t| ts.is_op(t, "="))
                    .ok_or_else(|| Error::ora(927, "missing equal sign"))?;
                let lhs = ts.tslice(x, eq).to_string();
                let col = lhs.rsplit('.').next().unwrap_or(&lhs).to_string();
                sets.push((lhs, col, frag(&ts, eq + 1, y, &ord)));
            }
            let upd_where = w.map(|w| {
                let e = d.filter(|d| *d > w).unwrap_or(clause_end);
                update_where_range = Some((w + 1, e));
                frag(&ts, w + 1, e, &ord)
            });
            let del_where = d.map(|d| {
                let wk = ts.next(d, clause_end);
                frag(&ts, wk + 1, clause_end, &ord)
            });
            update = Some((sets, upd_where, del_where));
        } else {
            if !ts.is(act, "INSERT") {
                return Err(Error::ora(905, "missing keyword"));
            }
            let mut p = ts.next(act, n);
            let cols = if ts.k(p) == K::LParen {
                let c = ts.pair[p];
                let s = ts.slice(p, c + 1).to_string();
                p = ts.next(c, n);
                Some(s)
            } else {
                None
            };
            if !ts.is(p, "VALUES") {
                return Err(Error::ora(926, "missing VALUES keyword"));
            }
            let vp = ts.next(p, n);
            let vc = ts.pair[vp];
            let vals = frag(&ts, vp + 1, vc, &ord);
            let w = ts.find_kw(vc, clause_end, &["WHERE"]);
            let ins_where = w.map(|w| frag(&ts, w + 1, clause_end, &ord));
            insert = Some((cols, vals, ins_where));
        }
        j = clause_end;
    }
    if update.is_none() && insert.is_none() {
        return Err(Error::ora(905, "missing keyword"));
    }
    // DELETE WHERE may only see rows the UPDATE touched; the UPDATE's WHERE is
    // re-evaluated after the update, so it must not read a column the SET
    // changes.
    if let (Some((sets, Some(_), Some(_))), Some((wa, wb))) = (&update, update_where_range) {
        for t in wa..wb {
            if matches!(ts.k(t), K::Word | K::Quoted)
                && sets
                    .iter()
                    .any(|(_, c, _)| c.eq_ignore_ascii_case(ident_text(ts.text(t))))
            {
                let q = ts.prev(t, wa);
                let qualified_source = q != NONE
                    && ts.k(q) == K::Dot
                    && ident_text(ts.text(ts.prev(q, wa))).eq_ignore_ascii_case(&source.key);
                if !qualified_source {
                    return Err(unimplemented(
                        "MERGE UPDATE … WHERE on an updated column combined with DELETE WHERE",
                    ));
                }
            }
        }
    }

    let tmp = "__dbsaci_merge";
    let mut out = vec![];
    out.push(format!("{NOCOUNT} DROP TEMPORARY TABLE IF EXISTS {tmp}"));
    let matched_col = if insert.is_some() {
        format!(", EXISTS (SELECT 1 FROM {target_name} {tkey} WHERE {cond}) AS __dbsaci_matched")
    } else {
        String::new()
    };
    out.push(format!(
        "{NOCOUNT} CREATE TEMPORARY TABLE {tmp} AS SELECT {skey}.*{matched_col} FROM {source_name} {skey}"
    ));
    if let Some((sets, upd_where, del_where)) = &update {
        let uw = upd_where
            .as_ref()
            .map(|w| format!(" AND ({w})"))
            .unwrap_or_default();
        if cx.route_dictionary {
            out.push(format!(
            "{NOCOUNT} SET @dbsaci_chk = (CASE WHEN (SELECT COUNT(*) FROM {target_name} {tkey} JOIN {tmp} {skey} ON ({cond}){where_uw}) \
             > (SELECT COUNT(*) FROM {target_name} {tkey} WHERE EXISTS (SELECT 1 FROM {tmp} {skey} WHERE ({cond}){uw})) \
             THEN {}(30926, 'unable to get a stable set of rows in the source tables') END)",
            fname(cx, "dbsaci_raise"),
            where_uw = upd_where
                .as_ref()
                .map(|w| format!(" WHERE ({w})"))
                .unwrap_or_default(),
        ));
        }
        let assigns: Vec<String> = sets
            .iter()
            .map(|(lhs, col, rhs)| {
                let l = if lhs.contains('.') {
                    lhs.clone()
                } else {
                    format!("{tkey}.{col}")
                };
                format!("{l} = {rhs}")
            })
            .collect();
        out.push(format!(
            "UPDATE {target_name} {tkey} JOIN {tmp} {skey} ON ({cond}) SET {}{}",
            assigns.join(", "),
            upd_where
                .as_ref()
                .map(|w| format!(" WHERE {w}"))
                .unwrap_or_default()
        ));
        if let Some(dw) = del_where {
            out.push(format!(
                "{NOCOUNT} DELETE {tkey} FROM {target_name} {tkey} JOIN {tmp} {skey} ON ({cond}) WHERE ({dw}){uw}"
            ));
        }
    }
    if let Some((cols, vals, ins_where)) = &insert {
        out.push(format!(
            "INSERT INTO {target_name}{} SELECT {vals} FROM {tmp} {skey} WHERE NOT {skey}.__dbsaci_matched{}",
            cols.as_ref().map(|c| format!(" {c}")).unwrap_or_default(),
            ins_where
                .as_ref()
                .map(|w| format!(" AND ({w})"))
                .unwrap_or_default()
        ));
    }
    out.push(format!("{NOCOUNT} DROP TEMPORARY TABLE IF EXISTS {tmp}"));
    let total = ord.iter().filter(|o| **o != usize::MAX).count();
    finalize_binds(&mut out, total);
    Ok(Some(out))
}

fn strip_alias(ts: &Ts, t: &TRef) -> String {
    let (ra, rb) = ts.trim(t.a, t.b);
    if !t.has_alias {
        return ts.slice(ra, rb).to_string();
    }
    let alias_tok = ts.prev(rb, ra);
    let mut e = alias_tok;
    let p = ts.prev(alias_tok, ra);
    if p != NONE && ts.is(p, "AS") {
        e = p;
    }
    ts.tslice(ra, e).to_string()
}

// ---------------------------------------------------------------------------
// INSERT ALL / INSERT FIRST
// ---------------------------------------------------------------------------

pub fn lower_insert_multi(sql: &str) -> Result<Option<Vec<String>>> {
    let ts = Ts::new(sql);
    let n = ts.len();
    let ins = ts.sig(0, n);
    if !ts.is(ins, "INSERT") {
        return Ok(None);
    }
    let mode = ts.next(ins, n);
    let first = ts.is(mode, "FIRST");
    if !first && !ts.is(mode, "ALL") {
        return Ok(None);
    }
    let ord = bind_ordinals(&ts);
    // source subquery: the top-level SELECT/WITH after the last INTO clause
    let mut i = ts.next(mode, n);
    let mut targets: Vec<(String, Option<String>, Option<String>, Option<String>)> = vec![];
    let mut cond: Option<String> = None;
    let mut prior: Vec<String> = vec![];
    let src_start;
    loop {
        if i >= n {
            return Err(Error::ora(928, "missing SELECT keyword"));
        }
        if ts.is(i, "WHEN") {
            let then = ts
                .find_kw(i + 1, n, &["THEN"])
                .ok_or_else(|| Error::ora(905, "missing keyword"))?;
            let c = frag(&ts, i + 1, then, &ord);
            let full = if first {
                let mut parts = vec![format!("({c}) IS TRUE")];
                parts.extend(prior.iter().map(|p| format!("({p}) IS NOT TRUE")));
                parts.join(" AND ")
            } else {
                format!("({c}) IS TRUE")
            };
            prior.push(c);
            cond = Some(full);
            i = ts.next(then, n);
            continue;
        }
        if ts.is(i, "ELSE") {
            cond = Some(if prior.is_empty() {
                "1 = 1".into()
            } else {
                prior
                    .iter()
                    .map(|p| format!("({p}) IS NOT TRUE"))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            });
            i = ts.next(i, n);
            continue;
        }
        if ts.is(i, "INTO") {
            let t = ts.next(i, n);
            let (_, tend) = path(&ts, t);
            let table = ts.slice(t, tend).to_string();
            let mut p = ts.sig(tend, n);
            let mut cols = None;
            if p < n && ts.k(p) == K::LParen && ts.pair[p] != NONE {
                cols = Some(ts.slice(p, ts.pair[p] + 1).to_string());
                p = ts.next(ts.pair[p], n);
            }
            let mut vals = None;
            if ts.is(p, "VALUES") {
                let vp = ts.next(p, n);
                let vc = ts.pair[vp];
                vals = Some(frag(&ts, vp + 1, vc, &ord));
                p = ts.next(vc, n);
            }
            targets.push((table, cols, vals, cond.clone()));
            i = p;
            continue;
        }
        if ts.is(i, "SELECT") || ts.is(i, "WITH") || ts.k(i) == K::LParen {
            src_start = i;
            break;
        }
        return Err(Error::ora(928, "missing SELECT keyword"));
    }
    let src_end = if ts.k(n - 1) == K::Semi { n - 1 } else { n };
    let mut source = frag(&ts, src_start, src_end, &ord);
    // `SELECT * FROM DUAL` has no columns in MariaDB
    let st = Ts::new(&source);
    let sn = st.len();
    let s0 = st.sig(0, sn);
    if st.is(s0, "SELECT") && st.is_op(st.next(s0, sn), "*") {
        let f = st.next(st.next(s0, sn), sn);
        if st.is(f, "FROM") && st.is(st.next(f, sn), "DUAL") && st.next(st.next(f, sn), sn) >= sn {
            source = "SELECT 'X' AS DUMMY FROM DUAL".into();
        }
    }
    let tmp = "__dbsaci_insert";
    let mut out = vec![
        format!("{NOCOUNT} DROP TEMPORARY TABLE IF EXISTS {tmp}"),
        format!("{NOCOUNT} CREATE TEMPORARY TABLE {tmp} AS {source}"),
    ];
    for (table, cols, vals, cond) in targets {
        out.push(format!(
            "INSERT INTO {table}{} SELECT {} FROM {tmp}{}",
            cols.map(|c| format!(" {c}")).unwrap_or_default(),
            vals.unwrap_or_else(|| "*".into()),
            cond.map(|c| format!(" WHERE {c}")).unwrap_or_default()
        ));
    }
    out.push(format!("{NOCOUNT} DROP TEMPORARY TABLE IF EXISTS {tmp}"));
    let total = ord.iter().filter(|o| **o != usize::MAX).count();
    finalize_binds(&mut out, total);
    Ok(Some(out))
}
