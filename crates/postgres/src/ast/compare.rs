//! The two routes' outputs compared: the text passes' and the AST path's SQLite SQL
//! for one statement, as tokens, so that spacing, the case of keywords, an optional AS
//! and the quotes of a name that needs none do not count. A pair that differs only in
//! parentheses is told apart: the tree keeps none of the source's, and the printer
//! writes those SQLite's precedence needs. Every other difference is either one listed
//! in `INTENDED`, with the reason the AST path is right, or unexplained.
//!
//! The differential test (`diff.rs`) holds the corpus to no unexplained difference, and
//! `shadow` compares a sample of the statements a server translates, logging any.

/// How the AST path's output for a statement stands to the text passes'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Identical,
    IdenticalButParentheses,
    /// One of `INTENDED`, by its reason.
    Intended(&'static str),
    Unexplained,
}

/// Compares the text passes' output with the AST path's, each after the passes
/// `rewrite` runs on either.
pub(crate) fn compare(text: &str, ast: &str) -> Verdict {
    let old = same_functions(same_cast_types(tokens(text)));
    let new = same_functions(same_cast_types(tokens(ast)));
    if old == new {
        return Verdict::Identical;
    }
    if without_parens(&old) == without_parens(&new) {
        return Verdict::IdenticalButParentheses;
    }
    if let Some((why, _)) = INTENDED.iter().find(|(_, f)| f(&old, &new)) {
        return Verdict::Intended(why);
    }
    // What the type environment writes on purpose, each a rewrite of the pair that
    // undoes it; the first that applies names the difference, when the pair is then
    // the same, but for labels and parentheses.
    let (mut o, mut n) = (old, new);
    let mut first = None;
    for (why, f) in TYPED {
        let (o2, n2) = f(&o, &n);
        if (&o2, &n2) != (&o, &n) {
            first = first.or(Some(*why));
            (o, n) = (o2, n2);
        }
    }
    match first {
        Some(why) if same_but_new_labels(&without_parens(&o), &without_parens(&n)) => Verdict::Intended(why),
        Some(why) if INTENDED.iter().any(|(_, f)| f(&o, &n)) => Verdict::Intended(why),
        _ => Verdict::Unexplained,
    }
}

/// The engine's names for one function: overlite_column_at_time_zone is
/// overlite_instant_at_time_zone (catalog/mod.rs registers one body for both).
fn same_functions(mut t: Vec<String>) -> Vec<String> {
    for x in &mut t {
        if x == "overlite_column_at_time_zone" {
            *x = "overlite_instant_at_time_zone".to_string();
        }
    }
    t
}

/// A rewrite of the pair (text path, AST path) that undoes one thing the typed
/// translation does on purpose.
type Typed = (&'static str, fn(&[String], &[String]) -> (Vec<String>, Vec<String>));

const TYPED: &[Typed] = &[
    (
        "the session's TimeZone is read where Postgres reads it, whatever the zone: a typed translation is \
         kept across sessions, where the engine wrote overlite_tz_in, overlite_tz_wall, overlite_tz_text and \
         overlite_current_date only in a session whose zone is not UTC",
        |o, n| {
            let mut n = unwrap_call(n, "overlite_tz_in");
            n = unwrap_call(&n, "overlite_tz_wall");
            n = replace_call(&n, "overlite_tz_text", &["cast", "("], &["text", ")"]);
            n = replace_call(&n, "overlite_current_date", &["current_date"], &[]);
            let o = replace_call(o, "overlite_tz_text", &["cast", "("], &["text", ")"]);
            (o, n)
        },
    ),
    (
        "AT TIME ZONE goes the way its operand's type says, a cast to a time type kept, and is labelled \
         timezone, as Postgres labels it: the text path dropped the cast, took an expression for an instant \
         and labelled it by its operand",
        |o, n| {
            let at = |t: &str| matches!(t, "overlite_from_time_zone" | "overlite_instant_at_time_zone");
            let mut n = n.to_vec();
            let mut i = 0;
            while i + 3 < n.len() {
                if at(&n[i]) && n[i + 1] == "(" && n[i + 2] == "overlite_cast" && n[i + 3] == "(" {
                    // overlite_cast ( X , 'type' ) -> X
                    if let Some(close) = closing(&n, i + 3)
                        && close >= 2
                        && n[close - 2] == ","
                    {
                        n.drain(close - 2..=close);
                        n.drain(i + 2..=i + 3);
                    }
                }
                i += 1;
            }
            let same = |t: &[String]| {
                t.iter()
                    .map(|x| if at(x) { "overlite_at".to_string() } else { x.clone() })
                    .collect::<Vec<_>>()
            };
            if n.iter().any(|x| at(x)) {
                (same(o), same(&n))
            } else {
                (o.to_vec(), n)
            }
        },
    ),
    (
        "an untyped literal or a parameter compared with a date or a timestamp is read as one, by the type's \
         input function, where SQLite compared its text",
        |o, n| (unwrap_value_casts(o), unwrap_value_casts(n)),
    ),
    (
        "a numeric scalar subquery is written in the numeric form, so that it compares as a number: the text \
         path left it as decimal text (NOTES.md, Limits)",
        |o, n| {
            let mut n = n.to_vec();
            let form = ["(", "cast", "(", "overlite_numeric", "(", "("];
            let mut i = 0;
            while i + form.len() < n.len() {
                if n[i..i + form.len()].iter().map(String::as_str).eq(form) && n[i + form.len()] == "select" {
                    // ( CAST ( overlite_numeric ( (SELECT ...) ) AS DECIMALTEXT ) COLLATE DECIMAL )
                    if let Some(close) = closing(&n, i + 4)
                        && n.get(close + 1..close + 6).is_some_and(|t| {
                            t.iter()
                                .map(String::as_str)
                                .eq(["decimaltext", ")", "collate", "decimal", ")"])
                        })
                    {
                        let inner: Vec<String> = n[i + 5..close].to_vec();
                        n.splice(i..=close + 5, inner);
                    }
                }
                i += 1;
            }
            (o.to_vec(), n)
        },
    ),
    (
        "a select-list item whose type the environment knows and the text pass did not read (an aggregate \
         of a subquery's column, of EXTRACT, ROUND with digits, which is numeric's) is cast to it, so that the \
         protocol sends it as one, and a numeric among them is written in the numeric form",
        |o, n| {
            let exact = |t: &[String]| -> Vec<String> {
                t.iter()
                    .map(|x| match x.as_str() {
                        "overlite_round_exact" => "round".to_string(),
                        "overlite_abs_exact" => "abs".to_string(),
                        "dec_greatest" => "greatest".to_string(),
                        "dec_least" => "least".to_string(),
                        _ => x.clone(),
                    })
                    .collect()
            };
            (
                single_casts(&unwrap_item_casts(&strip_forms(&exact(o)))),
                single_casts(&unwrap_item_casts(&strip_forms(&exact(n)))),
            )
        },
    ),
    (
        "arithmetic with a numeric side the text pass did not read as one (EXTRACT, which is a numeric, a \
         literal without its leading zero, `.5`) is exact",
        |o, n| {
            let count = |t: &[String]| t.iter().filter(|x| *x == "overlite_numeric_op").count();
            if count(n) <= count(o) {
                return (o.to_vec(), n.to_vec());
            }
            (plain_numeric_ops(o), plain_numeric_ops(n))
        },
    ),
    (
        "date, time and interval arithmetic over a term the text pass did not read as one (a CASE, a sum in \
         parentheses, a product with an interval) is the engine's, each side's kind named",
        |o, n| {
            let count = |t: &[String]| {
                t.iter()
                    .filter(|x| matches!(x.as_str(), "overlite_tmul" | "overlite_tadd" | "overlite_tsub"))
                    .count()
            };
            if count(n) <= count(o) && !o.iter().any(|x| x == "overlite_interval_cmp") {
                return (o.to_vec(), n.to_vec());
            }
            let plain = |t: &[String]| unwrap_interval_cmp(&plain_temporal(t));
            (plain(o), plain(n))
        },
    ),
    (
        "pg_trgm's % is a call around its two operands: the text path took the WHERE before it into the left one",
        |o, n| {
            let broken = o
                .windows(3)
                .any(|w| w[0] == "overlite_trgm_match" && w[1] == "(" && w[2] == "where");
            if broken {
                (n.to_vec(), n.to_vec())
            } else {
                (o.to_vec(), n.to_vec())
            }
        },
    ),
];

/// t with `overlite_cast ( overlite_cast ( X , T ) , T )` written once.
fn single_casts(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    // `overlite_cast ( ( X ) , T )`: the parentheses of an item the text pass cast.
    let mut i = 0;
    while i + 2 < t.len() {
        if t[i] == "overlite_cast"
            && t[i + 1] == "("
            && t[i + 2] == "("
            && let Some(inner) = closing(&t, i + 2)
            && t.get(inner + 1).is_some_and(|x| x == ",")
        {
            t.remove(inner);
            t.remove(i + 2);
        }
        i += 1;
    }
    let mut i = 0;
    while i + 3 < t.len() {
        if t[i] == "overlite_cast"
            && t[i + 1] == "("
            && t[i + 2] == "overlite_cast"
            && t[i + 3] == "("
            && let (Some(outer), Some(inner)) = (closing(&t, i + 1), closing(&t, i + 3))
            && inner + 3 == outer
            && t[inner + 1] == ","
            && inner >= 2
            && t[inner - 2] == ","
            && t[inner - 1] == t[inner + 2]
        {
            t.drain(inner + 1..=outer);
            t.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }
    t
}

/// t with each `overlite_cast ( F ( ... ) , T )` of a call F whose value is an
/// argument's (MIN, MAX, SUM, COALESCE...) or of a scalar subquery replaced by the call.
fn unwrap_item_casts(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 3 < t.len() {
        let f = t[i + 2].as_str();
        let passes = matches!(
            f,
            "min"
                | "max"
                | "sum"
                | "avg"
                | "coalesce"
                | "greatest"
                | "least"
                | "nullif"
                | "abs"
                | "round"
                | "ceil"
                | "ceiling"
                | "floor"
                | "date_trunc"
                | "any_value"
        ) || (f == "(" && t.get(i + 3).is_some_and(|x| x == "select"));
        if t[i] == "overlite_cast"
            && t[i + 1] == "("
            && passes
            && let Some(close) = closing(&t, i + 1)
            && close >= 2
            && t[close - 2] == ","
        {
            t.drain(close - 2..=close);
            t.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }
    t
}

/// t with each `overlite_numeric_op ( A , B , 'OP' )` written `A OP B`, a quoted number
/// unquoted, and the cast to numeric around a chain of them gone.
fn plain_numeric_ops(t: &[String]) -> Vec<String> {
    let mut t = unwrap_item_casts(&strip_forms(t));
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == "overlite_numeric_op"
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
        {
            let args = split_args(&t[i + 2..close]);
            if let [a, b, op] = args.as_slice() {
                // A number's text unquoted, as the tokens read it: `.5` is `.` and `5`.
                let unquote = |x: &[String]| -> Vec<String> {
                    match x {
                        [v] if v.starts_with('\'') && v.trim_matches('\'').parse::<f64>().is_ok() => {
                            let v = v.trim_matches('\'');
                            match v.strip_prefix('.') {
                                Some(d) => vec![".".to_string(), d.to_string()],
                                None => vec![v.to_string()],
                            }
                        }
                        _ => x.to_vec(),
                    }
                };
                let mut repl = unquote(a);
                repl.push(op.first().map_or("+", |o| o.trim_matches('\'')).to_string());
                repl.extend(unquote(b));
                t.splice(i..=close, repl);
                continue;
            }
        }
        i += 1;
    }
    // A cast to numeric left around what was a chain.
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == "overlite_cast"
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
            && close >= 2
            && t[close - 2] == ","
            && t[close - 1].starts_with("Numeric")
        {
            t.drain(close - 2..=close);
            t.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }
    t
}

/// The arguments of a call, split at its commas.
fn split_args(t: &[String]) -> Vec<Vec<String>> {
    let mut args: Vec<Vec<String>> = vec![Vec::new()];
    let mut depth = 0;
    for x in t {
        match x.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "," if depth == 0 => {
                args.push(Vec::new());
                continue;
            }
            _ => {}
        }
        if let Some(a) = args.last_mut() {
            a.push(x.clone());
        }
    }
    args
}

/// t with each `overlite_interval_cmp ( A , B ) OP 0` written `A OP B`.
fn unwrap_interval_cmp(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == "overlite_interval_cmp"
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
            && t.get(close + 2).is_some_and(|z| z == "0")
        {
            let args = split_args(&t[i + 2..close]);
            if let [a, b] = args.as_slice() {
                let mut repl = a.clone();
                repl.push(t[close + 1].clone());
                repl.extend(b.iter().cloned());
                t.splice(i..=close + 2, repl);
                continue;
            }
        }
        i += 1;
    }
    t
}

/// t with each `overlite_tmul ( A , B , K , K , OP )` written `A OP B`, and
/// `overlite_tadd ( A , B , K , K )` and overlite_tsub `A + B` and `A - B`.
fn plain_temporal(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 1 < t.len() {
        if matches!(t[i].as_str(), "overlite_tadd" | "overlite_tsub")
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
            && let [a, b, _, _] = split_args(&t[i + 2..close]).as_slice()
        {
            let mut repl = a.clone();
            repl.push(if t[i] == "overlite_tadd" { "+" } else { "-" }.to_string());
            repl.extend(b.iter().cloned());
            t.splice(i..=close, repl);
            continue;
        }
        if t[i] == "overlite_tmul"
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
        {
            // The five arguments at depth one.
            let mut args: Vec<Vec<String>> = vec![Vec::new()];
            let mut depth = 0;
            for x in &t[i + 2..close] {
                match x.as_str() {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    "," if depth == 0 => {
                        args.push(Vec::new());
                        continue;
                    }
                    _ => {}
                }
                if let Some(a) = args.last_mut() {
                    a.push(x.clone());
                }
            }
            if let [a, b, _, _, op] = args.as_slice() {
                let op = op.first().map_or("*", |o| if o.contains('/') { "/" } else { "*" });
                let mut repl = a.clone();
                repl.push(op.to_string());
                repl.extend(b.iter().cloned());
                t.splice(i..=close, repl);
                continue;
            }
        }
        i += 1;
    }
    t
}

/// t with each call of f with one argument, `f ( X )`, replaced by X.
fn unwrap_call(t: &[String], f: &str) -> Vec<String> {
    replace_call(t, f, &[], &[])
}

/// t with each `f ( X )` replaced by `before X after`.
fn replace_call(t: &[String], f: &str, before: &[&str], after: &[&str]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == f
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
        {
            let inner: Vec<String> = t[i + 2..close].to_vec();
            let mut repl: Vec<String> = before.iter().map(|s| (*s).to_string()).collect();
            repl.extend(inner);
            repl.extend(after.iter().map(|s| (*s).to_string()));
            t.splice(i..=close, repl);
            continue;
        }
        i += 1;
    }
    t
}

/// The index of the parenthesis closing the one at open.
fn closing(t: &[String], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (k, x) in t.iter().enumerate().skip(open) {
        match x.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// t with `overlite_cast ( V , 'date' | 'timestamp' )` of a literal or a parameter V
/// replaced by V.
fn unwrap_value_casts(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 5 < t.len() {
        let v = &t[i + 2];
        if t[i] == "overlite_cast"
            && t[i + 1] == "("
            && (v.starts_with('\'') || v.starts_with('$'))
            && t[i + 3] == ","
            && matches!(t[i + 4].as_str(), "Date" | "Timestamp(None)" | "'date'" | "'timestamp'")
            && t[i + 5] == ")"
        {
            let v = v.clone();
            t.splice(i..=i + 5, [v]);
        }
        i += 1;
    }
    t
}

/// t without the numeric form around anything: `( CAST ( overlite_numeric ( X ) AS
/// DECIMALTEXT ) COLLATE DECIMAL )` replaced by X.
fn strip_forms(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let head = ["(", "cast", "(", "overlite_numeric", "("];
    let mut i = 0;
    while i + head.len() < t.len() {
        if t[i..i + head.len()].iter().map(String::as_str).eq(head)
            && let Some(close) = closing(&t, i + 4)
            && t.get(close + 1..close + 6).is_some_and(|x| {
                x.iter()
                    .map(String::as_str)
                    .eq(["decimaltext", ")", "collate", "decimal", ")"])
            })
        {
            let inner: Vec<String> = t[i + 5..close].to_vec();
            t.splice(i..=close + 5, inner);
            continue;
        }
        i += 1;
    }
    t
}

/// The differences the AST path makes on purpose, each a test over the pair of
/// normalized token lists (text path, AST path) and the reason.
type Intended = (&'static str, fn(&[String], &[String]) -> bool);

const INTENDED: &[Intended] = &[
    (
        "a regular expression operator is REGEXP whatever its operands: over the corpus' text of two Django \
         statements (a cast on the left, a subquery on the right) rewrite_full left the `~` as it was",
        |old, new| old.iter().any(|t| t == "~") && !new.iter().any(|t| t == "~") && new.iter().any(|t| t == "regexp"),
    ),
    (
        "jsonb compares as jsonb however its name is spelled: the text path gave `::jsonb` its \
         COLLATE JSONB and not `::JSONB`",
        |old, new| {
            fn strip(t: &[String]) -> Vec<&String> {
                let mut n: Vec<&String> = Vec::new();
                for t in without_parens(t) {
                    if t == "jsonb" && n.last().is_some_and(|l| *l == "collate") {
                        n.pop();
                    } else {
                        n.push(t);
                    }
                }
                n
            }
            let n = strip(new);
            n.len() < without_parens(new).len() && same_but_labels(&strip(old), &n)
        },
    ),
    (
        "a cast of a cast is two casts: the text path left `::` in the SQLite SQL or cast a whole \
         VALUES row, neither of which SQLite reads",
        |old, new| {
            let broken = old.iter().any(|t| t == "::") || old.windows(2).any(|w| w[0] == "values" && w[1] == "cast");
            broken && !new.iter().any(|t| t == "::")
        },
    ),
    (
        "label: FigureColname names an expression over a cast or a column ?column?, and a CASE case; \
         the text path labeled it by its operand",
        |old, new| same_but_labels(&without_parens(old), &without_parens(new)),
    ),
    (
        "a LIKE's pattern is the whole expression: the text path cut a CASE at its first word",
        |old, new| {
            let cut = |t: &[String]| {
                t.windows(4).any(|w| {
                    matches!(w[0].as_str(), "overlite_like_glob" | "overlite_ilike_pattern")
                        && w[1] == "("
                        && w[2] == "case"
                        && w[3] == ","
                })
            };
            cut(old) && !cut(new)
        },
    ),
    (
        "a quoted name is a name: the text path rewrote an operator inside \"a?b\"",
        |old, new| {
            old.len() == new.len()
                && old
                    .iter()
                    .zip(new)
                    .all(|(o, n)| o == n || (o.starts_with('"') && n.starts_with('"')))
        },
    ),
    (
        "NOT (x IN (subquery)) is written x NOT IN (subquery): one node in Postgres' tree",
        |old, new| {
            let strip = |t: &[String]| -> Vec<String> {
                t.iter()
                    .filter(|t| !matches!(t.as_str(), "(" | ")" | "not"))
                    .cloned()
                    .collect()
            };
            let nots = |t: &[String]| t.iter().filter(|t| *t == "not").count();
            strip(old) == strip(new) && nots(old) == nots(new)
        },
    ),
    (
        "a set member in parentheses without ORDER BY or LIMIT needs no SELECT * FROM wrapper",
        |old, new| {
            let mut o: Vec<&String> = Vec::new();
            let mut i = 0;
            while i < old.len() {
                if old[i..].starts_with(&["select".into(), "*".into(), "from".into(), "(".into()]) {
                    i += 3;
                    continue;
                }
                o.push(&old[i]);
                i += 1;
            }
            o.retain(|t| *t != "(" && *t != ")");
            o == without_parens(new)
        },
    ),
    (
        "a parenthesized column is a column: SQLite names it, the text path wrote the same name as a label",
        |old, new| {
            let o = without_parens(old);
            let n = without_parens(new);
            o.len() > n.len() && (1..o.len()).any(|i| o[i] == o[i - 1] && [&o[..i], &o[i + 1..]].concat() == n)
        },
    ),
];

/// The tokens of a SQLite statement, normalized as the module says.
fn tokens(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() || c == b';' {
            i += 1;
        } else if c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push(sql[start..i].to_string());
        } else if c == b'"' {
            let start = i + 1;
            let end = sql[start..].find('"').map_or(b.len(), |p| start + p);
            let name = &sql[start..end];
            let plain = !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c == b'_' || c.is_ascii_lowercase() || c.is_ascii_digit());
            out.push(if plain { name.to_string() } else { format!("\"{name}\"") });
            i = (end + 1).min(b.len());
        } else if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            // A decimal point inside a number stays with it.
            if c.is_ascii_digit() && b.get(i) == Some(&b'.') {
                i += 1;
                while i < b.len() && b[i].is_ascii_alphanumeric() {
                    i += 1;
                }
            }
            let w = sql[start..i].to_ascii_lowercase();
            if !matches!(w.as_str(), "as" | "inner" | "outer") {
                out.push(w);
            }
        } else {
            let two = sql.get(i..i + 2).unwrap_or("");
            if matches!(two, "<>" | "!=" | "<=" | ">=" | "||" | "::" | "->" | "==") {
                out.push(if two == "!=" { "<>".to_string() } else { two.to_string() });
                i += 2;
            } else {
                out.push((c as char).to_string());
                i += 1;
            }
        }
    }
    out
}

/// The type name of each overlite_cast, as the engine reads it: `'UUID'`, `'int'` and
/// `'integer'` are one type to it.
fn same_cast_types(mut t: Vec<String>) -> Vec<String> {
    for i in 0..t.len() {
        if t[i] != "overlite_cast" {
            continue;
        }
        let mut depth = 0;
        for j in i + 1..t.len() {
            match t[j].as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(name) = t[j - 1].strip_prefix('\'').and_then(|n| n.strip_suffix('\''))
                            && let Some(ty) = overlite_engine::CastType::parse(name)
                        {
                            t[j - 1] = format!("{ty:?}");
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    t
}

/// Whether two token lists differ at most where the second has a label FigureColname
/// gives and the text path did not: `?column?` or `case`.
fn same_but_labels(o: &[&String], n: &[&String]) -> bool {
    o.len() == n.len()
        && o.iter()
            .zip(n)
            .all(|(o, n)| o == n || *n == "\"?column?\"" || *n == "case")
}

/// Whether two token lists differ at most in the labels the second gives where
/// Postgres gives them (`?column?`, `case`, `timezone`), the first giving another or
/// none there.
fn same_but_new_labels(o: &[&String], n: &[&String]) -> bool {
    let label = |t: &str| matches!(t, "\"?column?\"" | "case" | "timezone");
    let (mut i, mut j) = (0, 0);
    while i < o.len() && j < n.len() {
        if o[i] == n[j] {
            i += 1;
            j += 1;
        } else if label(n[j]) {
            if n.get(j + 1).is_some_and(|x| *x == o[i]) {
                j += 1;
            } else {
                i += 1;
                j += 1;
            }
        } else {
            return false;
        }
    }
    i == o.len() && (j == n.len() || (j + 1 == n.len() && label(n[j])))
}

fn without_parens(t: &[String]) -> Vec<&String> {
    t.iter().filter(|t| *t != "(" && *t != ")").collect()
}
