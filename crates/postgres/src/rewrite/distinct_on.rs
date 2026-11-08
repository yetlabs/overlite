//! DISTINCT ON, which SQLite lacks.

use crate::ddl::sequences::read_paren_args;
use crate::rewrite::catalog::is_word_byte;
use crate::rewrite::functions::{has_word_at, skip_spaces};
use crate::rewrite::strings::end_of_string_literal;
use overlite_core::text::quote_ident;

/// Turns "SELECT DISTINCT ON (e) cols FROM ... ORDER BY ..." into a ROW_NUMBER()
/// window over the ON expressions, keeping the first row per group. It handles
/// the top-level SELECT; a "*" select list goes through the rowid form below.
pub(crate) fn rewrite_distinct_on(sql: &str) -> String {
    if !sql.as_bytes().windows(8).any(|w| w.eq_ignore_ascii_case(b"distinct")) {
        return sql.to_string();
    }
    // A DISTINCT ON in a subquery: each SELECT that opens a parenthesis, the last
    // first, so the offsets before it hold.
    let code = overlite_engine::code_only(sql);
    let low_code = code.to_ascii_lowercase();
    let mut out = sql.to_string();
    let mut starts: Vec<usize> = Vec::new();
    let lb = low_code.as_bytes();
    for (i, _) in low_code.match_indices("select") {
        if i > 0 && is_word_byte(lb[i - 1]) || lb.get(i + 6).is_some_and(|&c| is_word_byte(c)) {
            continue;
        }
        let before = low_code[..i].trim_end();
        if before.ends_with('(') {
            starts.push(i);
        }
    }
    for &i in starts.iter().rev() {
        let Some(close) = closing_paren(&out, i) else { continue };
        let inner = &out[i..close];
        let rewritten = rewrite_distinct_on_select(inner);
        if rewritten != inner {
            out = [&out[..i], rewritten.as_str(), &out[close..]].concat();
        }
    }
    rewrite_distinct_on_select(&out)
}

/// Where the parenthesis around the SELECT at i closes.
fn closing_paren(sql: &str, i: usize) -> Option<usize> {
    let b = sql.as_bytes();
    let mut depth = 0i32;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'\'' => {
                j = end_of_string_literal(sql, j);
                continue;
            }
            b'"' => {
                j = sql[j + 1..].find('"').map_or(b.len(), |e| j + e + 2);
                continue;
            }
            b'(' => depth += 1,
            b')' if depth == 0 => return Some(j),
            b')' => depth -= 1,
            _ => {}
        }
        j += 1;
    }
    None
}

/// rewrite_distinct_on for one SELECT, the whole of sql.
fn rewrite_distinct_on_select(sql: &str) -> String {
    let low = sql.to_ascii_lowercase();
    let b = sql.as_bytes();
    let s = skip_spaces(sql, 0);
    if !has_word_at(&low, s, "select") {
        return sql.to_string();
    }
    let mut p = skip_spaces(sql, s + "select".len());
    if !has_word_at(&low, p, "distinct") {
        return sql.to_string();
    }
    p = skip_spaces(sql, p + "distinct".len());
    if !has_word_at(&low, p, "on") {
        return sql.to_string();
    }
    p = skip_spaces(sql, p + "on".len());
    if p >= b.len() || b[p] != b'(' {
        return sql.to_string();
    }
    let Some((on_args, after_on)) = read_paren_args(sql, p) else {
        return sql.to_string();
    };
    let Some(from_idx) = index_top_level_word(sql, after_on, "from") else {
        return sql.to_string();
    };
    let select_list = sql[after_on..from_idx].trim();
    if select_list.is_empty() {
        return sql.to_string();
    }
    let star = select_list.contains('*');

    let mut from_etc = &sql[from_idx..];
    let mut window_order = String::new();
    let mut outer_tail = String::new();
    if let Some(ob_idx) = index_top_level_word(sql, from_idx, "order") {
        from_etc = &sql[from_idx..ob_idx];
        outer_tail = [" ", sql[ob_idx..].trim()].concat();
        let mut by = skip_spaces(sql, ob_idx + "order".len());
        if has_word_at(&low, by, "by") {
            by = skip_spaces(sql, by + "by".len());
        }
        let mut end_order = sql.len();
        for kw in ["limit", "offset"] {
            if let Some(k) = index_top_level_word(sql, by, kw)
                && k < end_order
            {
                end_order = k;
            }
        }
        window_order = [" ORDER BY ", sql[by..end_order].trim()].concat();
    }

    if star {
        // A "*" select list can't drop the helper column, so instead keep the
        // first row per group by its rowid. Only a single base table has an
        // unambiguous rowid; a join/multi-table "*" is left to error as before.
        let Some((ref_, qual, where_)) = single_table_ref(from_etc) else {
            return sql.to_string();
        };
        let ref_ = ref_.trim();
        let mut inner = [
            "SELECT ",
            &qual,
            ".rowid AS rid, ROW_NUMBER() OVER (PARTITION BY ",
            on_args.trim(),
            &window_order,
            ") AS _ov_rn ",
            ref_,
        ]
        .concat();
        if !where_.is_empty() {
            inner.push(' ');
            inner.push_str(&where_);
        }
        return [
            "SELECT ",
            select_list,
            " ",
            ref_,
            " WHERE ",
            &qual,
            ".rowid IN (SELECT rid FROM (",
            &inner,
            ") WHERE _ov_rn = 1)",
            &outer_tail,
        ]
        .concat();
    }

    windowed(sql, on_args.trim(), select_list, from_etc.trim(), &outer_tail).unwrap_or_else(|| sql.to_string())
}

/// The general form: each item of the select list and each ORDER BY term computed
/// in an inner SELECT beside ROW_NUMBER() over the ON expressions, and the outer
/// SELECT keeping the first row of each group under the names the client gave, so
/// a qualified name (`"t"."id"`) still means its column inside. The ORDER BY of the
/// window and of the result are the statement's, with a position or an output name
/// read as the item it stands for.
fn windowed(sql: &str, on: &str, select_list: &str, from_etc: &str, outer_tail: &str) -> Option<String> {
    let items = crate::ddl::sequences::split_top_level(select_list);
    let mut inner_items = Vec::new();
    let mut outer_items = Vec::new();
    let mut names: Vec<(String, String)> = Vec::new(); // output name -> expression
    for (k, item) in items.iter().enumerate() {
        let (expr, name) = split_alias(item.trim());
        inner_items.push([expr, " AS \"_ov_c", &k.to_string(), "\""].concat());
        let out = ["\"_ov_c", &k.to_string(), "\""].concat();
        match name {
            Some(n) => {
                names.push((n.clone(), expr.to_string()));
                outer_items.push([out, " AS ".to_string(), quote_ident(&n)].concat());
            }
            None => outer_items.push(out),
        }
    }
    let (order_terms, rest) = if outer_tail.trim().is_empty() {
        (Vec::new(), String::new())
    } else {
        let by = index_top_level_word(outer_tail, 0, "by")? + 2;
        let mut end = outer_tail.len();
        for kw in ["limit", "offset"] {
            if let Some(k) = index_top_level_word(outer_tail, by, kw)
                && k < end
            {
                end = k;
            }
        }
        (
            crate::ddl::sequences::split_top_level(&outer_tail[by..end]),
            outer_tail[end..].to_string(),
        )
    };
    // A position or an output name, in ON as in ORDER BY, is the item it stands for.
    let resolve = |expr: &str| -> Option<String> {
        Some(match expr.parse::<usize>() {
            Ok(n) if n >= 1 && n <= items.len() => split_alias(items[n - 1].trim()).0.to_string(),
            Ok(_) => return None,
            Err(_) => names
                .iter()
                .find(|(n, _)| unquote(expr) == *n)
                .map_or_else(|| expr.to_string(), |(_, e)| e.clone()),
        })
    };
    let on_terms: Option<Vec<String>> = crate::ddl::sequences::split_top_level(on)
        .iter()
        .map(|t| resolve(t.trim()))
        .collect();
    let on = on_terms?.join(", ");
    let mut window_terms = Vec::new();
    let mut outer_order = Vec::new();
    for (k, term) in order_terms.iter().enumerate() {
        let (expr, dir) = split_direction(term.trim());
        let expr = resolve(expr)?;
        inner_items.push([&expr, " AS \"_ov_o", &k.to_string(), "\""].concat());
        window_terms.push([expr.as_str(), dir].concat());
        outer_order.push(["\"_ov_o", &k.to_string(), "\"", dir].concat());
    }
    let _ = sql;
    let window_order = if window_terms.is_empty() {
        String::new()
    } else {
        [" ORDER BY ", &window_terms.join(", ")].concat()
    };
    let outer_order = if outer_order.is_empty() {
        String::new()
    } else {
        [" ORDER BY ", &outer_order.join(", ")].concat()
    };
    Some(
        [
            "SELECT ",
            &outer_items.join(", "),
            " FROM (SELECT ",
            &inner_items.join(", "),
            ", ROW_NUMBER() OVER (PARTITION BY ",
            &on,
            &window_order,
            ") AS _ov_rn ",
            from_etc,
            ") AS _ov WHERE _ov_rn = 1",
            &outer_order,
            " ",
            rest.trim(),
        ]
        .concat()
        .trim_end()
        .to_string(),
    )
}

/// An item and the name the result gives it: its alias, or the column it names.
fn split_alias(item: &str) -> (&str, Option<String>) {
    let code = overlite_engine::code_only(item);
    let low = code.to_ascii_lowercase();
    // `expr AS name`
    if let Some(i) = low.rfind(" as ") {
        let name = item[i + 4..].trim();
        if !name.is_empty() && !name.contains(['(', ')', ' ']) {
            return (item[..i].trim(), Some(unquote(name)));
        }
    }
    // A name, maybe qualified: its last part.
    let simple = item
        .bytes()
        .all(|c| is_word_byte(c) || c == b'.' || c == b'"' || c == b' ');
    if simple && !item.contains(' ') {
        let last = item.rsplit('.').next().unwrap_or(item);
        return (item, Some(unquote(last)));
    }
    (item, None)
}

/// An ORDER BY term and its direction, ` ASC`, ` DESC`, ` NULLS FIRST`... as written.
fn split_direction(term: &str) -> (&str, &str) {
    let low = term.to_ascii_lowercase();
    let mut cut = term.len();
    for suffix in [" nulls first", " nulls last"] {
        if low[..cut].ends_with(suffix) {
            cut -= suffix.len();
        }
    }
    for suffix in [" asc", " desc"] {
        if low[..cut].ends_with(suffix) {
            cut -= suffix.len();
        }
    }
    (term[..cut].trim_end(), &term[cut..])
}

fn unquote(name: &str) -> String {
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(n) => n.replace("\"\"", "\""),
        None => name.to_ascii_lowercase(),
    }
}

/// Parses a FROM clause that is a single base table, returning the "FROM <table>
/// [alias]" prefix (ref), the name to qualify rowid with (qual, the alias or
/// bare table name), and any trailing WHERE/GROUP/… (where). None for joins,
/// comma lists, subqueries or table functions.
fn single_table_ref(from_etc: &str) -> Option<(String, String, String)> {
    let low = from_etc.to_ascii_lowercase();
    let p = skip_spaces(from_etc, 0);
    if !has_word_at(&low, p, "from") {
        return None;
    }
    let tbl_start = skip_spaces(from_etc, p + "from".len());
    // The table list ends at the first top-level WHERE/GROUP/HAVING/WINDOW.
    let mut end = from_etc.len();
    for kw in ["where", "group", "having", "window"] {
        if let Some(k) = index_top_level_word(from_etc, tbl_start, kw)
            && k < end
        {
            end = k;
        }
    }
    let table_list = &from_etc[tbl_start..end];
    // A join or a comma means more than one table.
    if has_top_level_comma(table_list) {
        return None;
    }
    for kw in ["join", "inner", "left", "right", "full", "cross", "natural"] {
        if index_top_level_word(table_list, 0, kw).is_some() {
            return None;
        }
    }
    let (name, alias) = parse_table_and_alias(table_list.trim())?;
    let mut qual = alias;
    if qual.is_empty() {
        // bare table name
        qual = match name.rfind('.') {
            Some(d) => name[d + 1..].to_string(),
            None => name,
        };
    }
    Some((from_etc[p..end].to_string(), qual, from_etc[end..].trim().to_string()))
}

/// Reports whether s has a comma outside parentheses/strings.
fn has_top_level_comma(s: &str) -> bool {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => i = end_of_string_literal(s, i) - 1,
            b'(' => depth += 1,
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b',' if depth == 0 => {
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Splits "<table> [AS] [alias]" into its name and alias. It rejects a table
/// function or subquery (a parenthesis in the reference).
fn parse_table_and_alias(s: &str) -> Option<(String, String)> {
    if s.is_empty() || s.contains(['(', ')']) {
        return None;
    }
    let (tok, rest) = read_ref_token(s);
    if tok.is_empty() {
        return None;
    }
    let mut rest = rest.trim();
    let mut alias = "";
    if !rest.is_empty() {
        let low = rest.to_ascii_lowercase();
        if has_word_at(&low, 0, "as") {
            rest = rest[2..].trim();
        }
        alias = read_ref_token(rest).0;
    }
    Some((tok.to_string(), alias.to_string()))
}

/// Reads one identifier token (allowing "quoted" parts and dotted qualifiers)
/// from the front of s, returning it and the remainder.
fn read_ref_token(s: &str) -> (&str, &str) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r' {
            break;
        }
        i += 1;
    }
    s.split_at(i.min(s.len()))
}

/// Finds the first whole-word occurrence of word at paren depth zero and outside
/// string literals, at or after from.
pub(crate) fn index_top_level_word(sql: &str, from: usize, word: &str) -> Option<usize> {
    let low = sql.to_ascii_lowercase();
    let b = sql.as_bytes();
    let l = low.as_bytes();
    let mut depth = 0;
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i = end_of_string_literal(sql, i);
                continue;
            }
            b'(' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0 && (i == 0 || !is_word_byte(l[i - 1])) && has_word_at(&low, i, word) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_on_forms() {
        assert_eq!(
            rewrite_distinct_on(
                r#"SELECT DISTINCT ON ("rn"."name") "rn"."id", "rn"."name" FROM "rn" ORDER BY "rn"."name", "rn"."id" DESC LIMIT 2"#
            ),
            r#"SELECT "_ov_c0" AS "id", "_ov_c1" AS "name" FROM (SELECT "rn"."id" AS "_ov_c0", "rn"."name" AS "_ov_c1", "rn"."name" AS "_ov_o0", "rn"."id" AS "_ov_o1", ROW_NUMBER() OVER (PARTITION BY "rn"."name" ORDER BY "rn"."name", "rn"."id" DESC) AS _ov_rn FROM "rn") AS _ov WHERE _ov_rn = 1 ORDER BY "_ov_o0", "_ov_o1" DESC LIMIT 2"#
        );
        let sub = rewrite_distinct_on(
            "SELECT id FROM rn WHERE id IN (SELECT DISTINCT ON (name) id FROM rn ORDER BY name, id DESC)",
        );
        assert!(
            sub.starts_with("SELECT id FROM rn WHERE id IN (SELECT \"_ov_c0\" AS \"id\" FROM (SELECT id AS"),
            "{sub}"
        );
        assert!(!sub.to_ascii_lowercase().contains("distinct on"), "{sub}");
    }
}
