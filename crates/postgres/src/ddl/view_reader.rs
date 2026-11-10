//! A view's query read by Postgres' grammar for the engine's catalog: which relations
//! it reads (pg_depend, so that pg_dump orders a view after them) and the query with
//! each of them named as the session's search_path finds it (pg_get_viewdef, so that
//! a dump, made under an empty path, names them qualified).

use overlite_engine::ViewReader;
use pg_query::NodeRef;

/// The reader the engine is given.
pub(crate) fn reader() -> ViewReader {
    ViewReader { qualify, relations }
}

/// Each relation of the query: where its name starts, and its stored name.
fn range_vars(query: &str) -> Option<Vec<(usize, String)>> {
    let parsed = pg_query::parse(query, 0).ok()?;
    let nodes = parsed.protobuf.nodes();
    let ctes: Vec<&str> = nodes
        .iter()
        .filter_map(|(n, ..)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.as_str()),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for (n, ..) in &nodes {
        let NodeRef::RangeVar(r) = n else { continue };
        if r.schemaname.is_empty() && ctes.contains(&r.relname.as_str()) {
            continue;
        }
        let stored = if r.schemaname.is_empty() || r.schemaname == "public" {
            r.relname.clone()
        } else {
            [r.schemaname.as_str(), ".", r.relname.as_str()].concat()
        };
        out.push((usize::try_from(r.location).ok()?, stored));
    }
    out.sort_by_key(|(at, _)| *at);
    out.dedup_by_key(|(at, _)| *at);
    Some(out)
}

fn relations(query: &str) -> Vec<String> {
    let mut names: Vec<String> = range_vars(query)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    names.sort();
    names.dedup();
    names
}

fn qualify(query: &str, relation: &dyn Fn(&str) -> String) -> Option<String> {
    let vars = range_vars(query)?;
    if vars.is_empty() {
        return Some(query.to_string());
    }
    let tokens = pg_query::scan_tokens(query).ok()?;
    let text = |i: usize| -> Option<&str> {
        let t = tokens.get(i)?;
        query.get(usize::try_from(t.start).ok()?..usize::try_from(t.end).ok()?)
    };
    let mut out = String::with_capacity(query.len() + 8 * vars.len());
    let mut done = 0;
    for (at, stored) in vars {
        let first = tokens.iter().position(|t| usize::try_from(t.start).ok() == Some(at))?;
        // The name runs on through each `.` and the name after it.
        let mut last = first;
        while text(last + 1) == Some(".") && text(last + 2).is_some() {
            last += 2;
        }
        let end = usize::try_from(tokens[last].end).ok()?;
        if at < done {
            return None;
        }
        out.push_str(query.get(done..at)?);
        out.push_str(&relation(&stored));
        done = end;
    }
    out.push_str(query.get(done..)?);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_public(t: &str) -> String {
        match t.split_once('.') {
            Some((s, n)) => format!("{s}.{n}"),
            None => format!("public.{t}"),
        }
    }

    #[test]
    fn names_each_relation() {
        assert_eq!(
            qualify("SELECT a FROM books b JOIN \"s.t\" AS t ON t.id = b.id", &bare_public).as_deref(),
            Some("SELECT a FROM public.books b JOIN s.t AS t ON t.id = b.id")
        );
        assert_eq!(
            qualify("WITH c AS (SELECT 1 FROM x) SELECT * FROM c, public.y", &bare_public).as_deref(),
            Some("WITH c AS (SELECT 1 FROM public.x) SELECT * FROM c, public.y")
        );
        assert_eq!(relations("SELECT * FROM b, a, s.c, b"), ["a", "b", "s.c"]);
    }
}
