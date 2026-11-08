//! A member of a UNION, INTERSECT or EXCEPT in parentheses. Postgres takes
//! `(SELECT ...) UNION ALL (SELECT ... ORDER BY x LIMIT 1)`, and Django writes every
//! member of a union of querysets so. SQLite's grammar has neither the parentheses nor
//! a member's own ORDER BY and LIMIT: a plain member loses its parentheses, and one with
//! clauses of its own is read through a subquery, `SELECT * FROM (...)`, which keeps
//! them and the names of its columns.

use overlite_engine::blank_literals;

/// The statement with each parenthesised member of a set operation made one SQLite
/// reads, and a statement that is a query in parentheses unwrapped.
pub(crate) fn rewrite_set_members(sql: &str) -> String {
    let code = blank_literals(sql);
    let low = code.to_ascii_lowercase();
    let starts_paren = code.trim_start().starts_with('(');
    if !starts_paren && !["union", "intersect", "except"].iter().any(|w| low.contains(w)) {
        return sql.to_string();
    }
    let walked = walk(sql);
    // At the top, a lone query in parentheses is the statement.
    query_expression(&walked, true).unwrap_or(walked)
}

/// s with every group in parentheses that holds a set operation over a member in
/// parentheses rewritten, innermost first.
fn walk(s: &str) -> String {
    let code = blank_literals(s);
    let b = code.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'(' {
            let Some(close) = matching(b, i) else { break };
            let inner = walk(&s[i + 1..close]);
            let inner = query_expression(&inner, false).unwrap_or(inner);
            out.push_str(&s[last..=i]);
            out.push_str(&inner);
            out.push(')');
            last = close + 1;
            i = close + 1;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// The index of the parenthesis closing the one at open, in blanked code.
fn matching(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in b.iter().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// A word of blanked code at the top level: where it starts and ends.
fn top_words(code: &str) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut words = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_word(b[i - 1])) {
            let start = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            words.push((start, i));
            continue;
        }
        i += 1;
    }
    words
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// The words of a query's own clauses after its body.
const TAIL: [&str; 5] = ["order", "limit", "offset", "fetch", "for"];

/// s, a query expression with a member in parentheses, as SQLite reads it; None for
/// anything else. A lone member in parentheses counts only at the top of a statement:
/// elsewhere it is SQLite's subquery already.
fn query_expression(s: &str, top: bool) -> Option<String> {
    let code = blank_literals(s);
    let low = code.to_ascii_lowercase();
    let words = top_words(&low);
    // The set operators, each with the ALL or DISTINCT after it.
    let mut ops: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < words.len() {
        let (a, e) = words[k];
        if matches!(&low[a..e], "union" | "intersect" | "except") {
            let mut end = e;
            if let Some(&(na, ne)) = words.get(k + 1)
                && matches!(&low[na..ne], "all" | "distinct")
            {
                end = ne;
                k += 1;
            }
            ops.push((a, end));
        }
        k += 1;
    }
    if ops.is_empty() && !top {
        return None;
    }
    // The members, and what follows the last when it is in parentheses.
    let mut bounds = Vec::with_capacity(ops.len() + 1);
    let mut from = 0;
    for &(a, e) in &ops {
        bounds.push((from, a));
        from = e;
    }
    bounds.push((from, s.len()));
    let n = bounds.len();
    let mut members: Vec<Member> = Vec::with_capacity(n);
    for (idx, &(a, e)) in bounds.iter().enumerate() {
        let m = member(s, &code[a..e], &low[a..e], a, idx + 1 == n)?;
        members.push(m);
    }
    if !members.iter().any(|m| m.inner.is_some()) {
        return None;
    }
    let mut out = String::with_capacity(s.len() + 32);
    for (idx, m) in members.iter().enumerate() {
        match &m.inner {
            Some(inner) => {
                let own = has_own_clauses(inner);
                out.push(' ');
                if own && (n > 1 || !m.tail.trim().is_empty()) {
                    out.push_str("SELECT * FROM (");
                    out.push_str(inner);
                    out.push(')');
                } else {
                    out.push_str(inner);
                }
                out.push(' ');
                out.push_str(&m.tail);
            }
            None => out.push_str(&m.text),
        }
        if let Some(&(a, e)) = ops.get(idx) {
            out.push(' ');
            out.push_str(&s[a..e]);
            out.push(' ');
        }
    }
    Some(out)
}

/// A member: its text, and for one in parentheses what they hold and what follows.
struct Member {
    text: String,
    inner: Option<String>,
    tail: String,
}

/// A member of the expression, None when it is no query, which leaves the whole alone.
fn member(s: &str, code: &str, low: &str, at: usize, last: bool) -> Option<Member> {
    let lead = code.len() - code.trim_start().len();
    let text = &s[at..at + code.len()];
    let first = low[lead..]
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()?;
    if !code[lead..].starts_with('(') {
        return matches!(first, "select" | "values" | "with").then(|| Member {
            text: text.to_string(),
            inner: None,
            tail: String::new(),
        });
    }
    let close = matching(code.as_bytes(), lead)?;
    let rest = &low[close + 1..];
    let tail_ok = if last {
        let w = rest
            .trim_start()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or("");
        rest.trim().is_empty() || TAIL.contains(&w)
    } else {
        rest.trim().is_empty()
    };
    if !tail_ok {
        return None;
    }
    let inner = &text[lead + 1..close];
    if !is_query(inner) {
        return None;
    }
    Some(Member {
        text: text.to_string(),
        inner: Some(inner.trim().to_string()),
        tail: text[close + 1..].to_string(),
    })
}

/// Whether text is a query: it opens with SELECT, VALUES or WITH, perhaps in
/// parentheses.
fn is_query(text: &str) -> bool {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix('(') {
        return is_query(rest);
    }
    let w: String = t.chars().take_while(char::is_ascii_alphabetic).collect();
    matches!(w.to_ascii_lowercase().as_str(), "select" | "values" | "with")
}

/// Whether a member's query has clauses SQLite takes only on a whole statement: an
/// ORDER BY, LIMIT, OFFSET or FETCH, a set operation, a WITH, or parentheses of its own.
fn has_own_clauses(inner: &str) -> bool {
    let code = blank_literals(inner).to_ascii_lowercase();
    if code.trim_start().starts_with('(') {
        return true;
    }
    top_words(&code).iter().any(|&(a, e)| {
        matches!(
            &code[a..e],
            "order" | "limit" | "offset" | "fetch" | "union" | "intersect" | "except" | "with"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn squash(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn members() {
        for (sql, want) in [
            (
                "SELECT COUNT(*) FROM ((SELECT a FROM t) UNION ALL (SELECT a FROM u WHERE a > 2)) q",
                "SELECT COUNT(*) FROM ( SELECT a FROM t UNION ALL SELECT a FROM u WHERE a > 2 ) q",
            ),
            (
                "(SELECT a FROM t) UNION ALL (SELECT a FROM u) ORDER BY 1 LIMIT 2",
                "SELECT a FROM t UNION ALL SELECT a FROM u ORDER BY 1 LIMIT 2",
            ),
            (
                "(SELECT a FROM t ORDER BY a LIMIT 1) UNION (SELECT ')' FROM u)",
                "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 1) UNION SELECT ')' FROM u",
            ),
            ("(SELECT a FROM t ORDER BY a)", "SELECT a FROM t ORDER BY a"),
            (
                "(SELECT a FROM t ORDER BY a) LIMIT 1",
                "SELECT * FROM (SELECT a FROM t ORDER BY a) LIMIT 1",
            ),
            (
                "SELECT * FROM t WHERE a IN ((SELECT 1) UNION (SELECT 2))",
                "SELECT * FROM t WHERE a IN ( SELECT 1 UNION SELECT 2 )",
            ),
            (
                "((SELECT 1) UNION (SELECT 2)) EXCEPT (SELECT 2)",
                "SELECT * FROM (SELECT 1 UNION SELECT 2) EXCEPT SELECT 2",
            ),
        ] {
            assert_eq!(squash(&rewrite_set_members(sql)), squash(want), "{sql}");
        }
        for sql in [
            "SELECT a FROM t UNION SELECT b FROM u",
            "SELECT (a + 1) * 2 FROM t UNION SELECT 1",
            "SELECT 'union (select 1)'",
            "SELECT * FROM ((SELECT 1)) x",
            "SELECT ((SELECT max(a) FROM t) + 1) UNION SELECT 2",
        ] {
            assert_eq!(rewrite_set_members(sql), sql, "{sql}");
        }
    }
}
