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
        "avg of integers is a numeric, their sum over their count at Postgres' scale, where SQLite's is a double",
        |o, n| {
            let m = plain_avg(n);
            if m.len() == n.len() {
                return (o.to_vec(), n.to_vec());
            }
            (o.to_vec(), m)
        },
    ),
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
        "arithmetic with a float side is a float's, as Postgres casts a numeric beside a real: the engine's pass \
         made it exact by the numeric side alone",
        |o, n| {
            let count = |t: &[String]| t.iter().filter(|x| *x == "overlite_numeric_op").count();
            if count(n) >= count(o) {
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
        "an interval compares by its span wherever a side reads as one, an untyped literal such as `23 hours` \
         included: the engine's pass compared it so only in a statement that said `interval`, `date` or ORDER BY",
        |o, n| {
            let count = |t: &[String]| t.iter().filter(|x| *x == "overlite_interval_cmp").count();
            if count(n) <= count(o) {
                return (o.to_vec(), n.to_vec());
            }
            (unwrap_interval_cmp(o), unwrap_interval_cmp(n))
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
    (
        "a cast to an array type is the engine's assignment to it whatever its operand, which reads an array in \
         either form and makes each element its type: the text pass dropped the cast of a column, a call or a \
         constructor, and left CAST ... AS type[] to SQLite, which reads it as text",
        |o, n| {
            if !n.iter().any(|x| x == "overlite_assign") {
                return (o.to_vec(), n.to_vec());
            }
            (unwrap_array_casts(o), unwrap_array_casts(n))
        },
    ),
    (
        "an array cast to text is Postgres' text form of it (overlite_array_text, `{a,b}`): the text pass's \
         CAST gave the JSON array it is stored as, Postgres' form only where the engine's array_agg had \
         written it",
        |o, n| {
            (
                o.to_vec(),
                replace_call(n, "overlite_array_text", &["cast", "("], &["text", ")"]),
            )
        },
    ),
    (
        "array_agg is the JSON array an array is stored as, NULL over no rows: the engine's aggregate wrote \
         Postgres' text form, which the array functions, the operators and the binary format do not read",
        |o, n| {
            let m = plain_array_agg(n);
            if m.len() == n.len() {
                return (o.to_vec(), n.to_vec());
            }
            (unwrap_json_bools(o), unwrap_json_bools(&m))
        },
    ),
    (
        "a window's ORDER BY sorts NULLs as Postgres does, and EXCLUDE NO OTHERS, the default, goes unsaid: the \
         text pass left a key before a frame without its NULLS, and the clause to SQLite, which has no such words",
        |o, n| {
            if !n.iter().any(|x| x == "over") {
                return (o.to_vec(), n.to_vec());
            }
            let norm = |t: &[String]| -> Vec<String> {
                let mut out: Vec<String> = Vec::new();
                let mut i = 0;
                while i < t.len() {
                    if t[i] == "nulls" && i + 1 < t.len() {
                        i += 2;
                        continue;
                    }
                    if t[i] == "exclude" && t.get(i + 1).is_some_and(|x| x == "no") {
                        i += 3;
                        continue;
                    }
                    out.push(t[i].clone());
                    i += 1;
                }
                out
            };
            (norm(o), norm(n))
        },
    ),
    (
        "an aggregate's FILTER follows its call: the text pass wrote it after the cast to jsonb around a jsonb \
         aggregate, which SQLite rejects",
        |o, n| {
            let has = |t: &[String]| t.iter().any(|x| x == "filter");
            if !has(o) || !has(n) {
                return (o.to_vec(), n.to_vec());
            }
            (without_filters(o), without_filters(n))
        },
    ),
];

/// t with each array cast unwrapped: `overlite_assign ( X , 'T[]' )` and `CAST ( X
/// T [ ] )` (the tokens drop AS) as X.
fn unwrap_array_casts(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == "overlite_assign"
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
            && close >= 2
            && t[close - 2] == ","
            && t[close - 1].ends_with("[]")
        {
            t.drain(close - 2..=close);
            t.drain(i..=i + 1);
            continue;
        }
        if t[i] == "cast"
            && t[i + 1] == "("
            && let Some(close) = closing(&t, i + 1)
            && close >= i + 5
            && t[close - 2] == "["
            && t[close - 1] == "]"
        {
            t.drain(close - 3..=close);
            t.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }
    t
}

/// t with each `overlite_cast ( overlite_numeric_op ( sum ( X ) , count ( X ) , '/' ) ,
/// T )` written `avg ( X )`.
fn plain_avg(t: &[String]) -> Vec<String> {
    let mut n = t.to_vec();
    let head = ["overlite_cast", "(", "overlite_numeric_op", "(", "sum", "("];
    let mut i = 0;
    while i + head.len() < n.len() {
        if n[i..i + head.len()].iter().map(String::as_str).eq(head)
            && let Some(sum_close) = closing(&n, i + 5)
            && let Some(cast_close) = closing(&n, i + 1)
            && n.get(sum_close + 1..sum_close + 4)
                .is_some_and(|w| w[0] == "," && w[1] == "count" && w[2] == "(")
            && let Some(op_close) = closing(&n, i + 3)
            && n[op_close - 1] == "'/'"
        {
            let x: Vec<String> = n[i + 6..sum_close].to_vec();
            let mut repl = vec!["avg".to_string(), "(".to_string()];
            repl.extend(x);
            repl.push(")".to_string());
            n.splice(i..=cast_close, repl);
            continue;
        }
        i += 1;
    }
    n
}

/// t with `overlite_array ( nullif ( json_group_array ( ... ) [FILTER, OVER] , '[]' ) )`
/// written `array_agg ( ... ) [FILTER, OVER]`.
fn plain_array_agg(t: &[String]) -> Vec<String> {
    let mut n = t.to_vec();
    let head = ["overlite_array", "(", "nullif", "(", "json_group_array"];
    let mut i = 0;
    while i + head.len() < n.len() {
        if n[i..i + head.len()].iter().map(String::as_str).eq(head)
            && let Some(outer) = closing(&n, i + 1)
            && outer >= 4
            && n[outer - 3] == ","
            && n[outer - 2] == "'[]'"
        {
            n.drain(outer - 3..=outer);
            n.drain(i..i + 4);
            n[i] = "array_agg".to_string();
            continue;
        }
        i += 1;
    }
    n
}

/// t with each `CASE WHEN x IS NULL THEN NULL WHEN x THEN json('true') ELSE
/// json('false') END`, a boolean as a JSON array's element, written x.
fn unwrap_json_bools(t: &[String]) -> Vec<String> {
    let mut t = t.to_vec();
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == "case"
            && t[i + 1] == "when"
            && let Some(end) = t[i..].iter().position(|x| x == "end")
            && t[i..i + end].iter().any(|x| x == "'false'")
            && let Some(is) = t[i..i + end].iter().position(|x| x == "is")
        {
            let x: Vec<String> = t[i + 2..i + is].to_vec();
            t.splice(i..=i + end, x);
            continue;
        }
        i += 1;
    }
    t
}

/// t without its FILTER clauses, and the filters sorted after it, so that one written
/// elsewhere compares the same.
fn without_filters(t: &[String]) -> Vec<String> {
    let mut rest = Vec::new();
    let mut filters: Vec<Vec<String>> = Vec::new();
    let mut i = 0;
    while i < t.len() {
        if t[i] == "filter"
            && t.get(i + 1).is_some_and(|x| x == "(")
            && let Some(close) = closing(t, i + 1)
        {
            filters.push(
                t[i..=close]
                    .iter()
                    .filter(|x| *x != "(" && *x != ")")
                    .cloned()
                    .collect(),
            );
            i = close + 1;
            continue;
        }
        if t[i] != "(" && t[i] != ")" {
            rest.push(t[i].clone());
        }
        i += 1;
    }
    filters.sort();
    rest.extend(filters.into_iter().flatten());
    rest
}

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
        "a set operation on the right of another is read from a subquery, as Postgres groups it: SQLite read \
         the compound left to right, `1 UNION 2 INTERSECT 3` as `(1 UNION 2) INTERSECT 3`",
        |old, new| {
            let head = ["select", "*", "from", "(", "select"];
            let Some(at) = new.windows(5).position(|w| w.iter().map(String::as_str).eq(head)) else {
                return false;
            };
            let set_op = |t: &[String]| t.iter().any(|x| matches!(x.as_str(), "union" | "intersect" | "except"));
            let n: Vec<String> = [&new[..at], &new[at + 3..]].concat();
            set_op(old) && without_parens(old) == without_parens(&n)
        },
    ),
    (
        "label: a subscript is labelled by its array, as Postgres' FigureColname names it, where the text path \
         said ?column?",
        |old, new| {
            let (o, n) = (without_parens(old), without_parens(new));
            o.len() == n.len()
                && new.iter().any(|x| x == "json_extract")
                && o.iter().zip(&n).all(|(a, b)| a == b || *a == "\"?column?\"")
        },
    ),
    (
        "a path operator over a cast to jsonb reads the cast's value: the text pass took the cast's type name for \
         the path function's operand and wrote what SQLite rejects",
        |old, new| {
            old.iter().any(|x| x == "'overlite_jsonb_path'") && !new.iter().any(|x| x == "'overlite_jsonb_path'")
        },
    ),
    (
        "Postgres' syntax the text pass left in place, which SQLite rejects (a subscript of an expression, a \
         slice, ARRAY(subquery) in parentheses, `<> ALL (subquery)`, `@>` of arrays), is written in SQLite's",
        |old, new| {
            let pg = |t: &[String]| {
                t.iter().any(|x| x == "[")
                    || t.windows(2).any(|w| {
                        ((w[0] == "array" || w[0] == "all") && w[1] == "(")
                            || (w[0] == "@" && w[1] == ">")
                            || (w[0] == "<" && w[1] == "@")
                            || w[0] == ":"
                    })
            };
            pg(old) && !pg(new)
        },
    ),
    (
        "a row-locking clause is the session's (take_row_locks) and is dropped wherever it stands: the text pass \
         looked for ` FOR ` between spaces, and a clause on lines of its own reached SQLite, a syntax error",
        |old, new| {
            let Some(at) = old
                .windows(2)
                .rposition(|w| w[0] == "for" && matches!(w[1].as_str(), "update" | "share" | "no" | "key"))
            else {
                return false;
            };
            without_parens(&old[..at]) == without_parens(new)
        },
    ),
    (
        "ctid is the rowid of a table, and a column a subquery or another arm names ctid keeps that name: the text \
         pass renamed every ctid, a quoted one not at all",
        |old, new| {
            let norm = |t: &[String]| -> Vec<String> {
                let mut out: Vec<String> = Vec::new();
                for x in t {
                    if x == "ctid" && out.last().is_some_and(|l| l == "rowid" || l == "ctid") {
                        continue;
                    }
                    out.push(if x == "rowid" { "ctid".to_string() } else { x.clone() });
                }
                out.retain(|x| x != "(" && x != ")");
                out
            };
            new.iter().any(|x| x == "rowid" || x == "ctid") && norm(old) == norm(new)
        },
    ),
    (
        "a SELECT before ON CONFLICT takes a WHERE, as SQLite asks: without one SQLite read ON CONFLICT as a \
         join's ON, a syntax error",
        |old, new| {
            let Some(at) = new
                .windows(4)
                .position(|w| w[0] == "where" && w[1] == "true" && w[2] == "on" && w[3] == "conflict")
            else {
                return false;
            };
            let n = [&new[..at], &new[at + 2..]].concat();
            without_parens(old) == without_parens(&n)
        },
    ),
    (
        "DISTINCT ON keeps the first row of each group before LIMIT and OFFSET count rows: the text pass limited \
         the rows before choosing among them",
        |old, new| {
            let Some(close) = old
                .windows(5)
                .position(|w| w[0] == ")" && w[1] == "_ov" && w[2] == "where" && w[3] == "_ov_rn")
            else {
                return false;
            };
            let Some(limit) = old[..close].iter().rposition(|x| x == "limit") else {
                return false;
            };
            let tail: Vec<String> = old[limit..close].to_vec();
            let mut o = [&old[..limit], &old[close..]].concat();
            o.extend(tail);
            without_parens(&o) == without_parens(new)
        },
    ),
    (
        "a range operator over ranges is overlite_range_op, which reads their bounds: the text path took `@>` \
         for jsonb's containment and compared ranges as their texts",
        |old, new| {
            let has = |t: &[String], w: &str| t.iter().any(|x| x == w);
            has(new, "overlite_range_op") && !has(old, "overlite_range_op")
        },
    ),
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
        "label: FigureColname names a CASE by its ELSE and a cast by its operand, through casts and calls \
         (`CASE ... ELSE x::regtype::text END` is x, `CASE ... ELSE array_to_string(...) END` \
         array_to_string); the text path named it by the outermost cast's type or `case`",
        |old, new| {
            let (o, n) = (without_parens(old), without_parens(new));
            let mut seen = false;
            let label = |t: &[&String], i: usize| {
                i > 0
                    && matches!(t[i - 1].as_str(), "end" | ")")
                    && t.get(i + 1).is_none_or(|x| *x == "," || *x == "from")
            };
            o.len() == n.len()
                && (0..o.len()).all(|i| {
                    o[i] == n[i] || {
                        seen = true;
                        label(&o, i) && label(&n, i)
                    }
                })
                && seen
        },
    ),
    (
        "a set-returning function reading a column of a table to its left is written in place, its columns \
         read under the walker's names (stage 4), where the text pass wrapped it in a subquery naming them, \
         which reads the same row",
        |old, new| {
            let o = srf_in_place(old);
            o.len() != old.len() && without_parens(&o) == without_parens(new)
        },
    ),
    (
        "array_to_string of ARRAY(subquery) is its rows joined in their order wherever it stands: the text \
         pass joined the first such call and left the others to the engine's array_to_string over the JSON \
         array, the same string",
        |old, new| {
            let o = joined_arrays(old);
            o.len() != old.len() && without_parens(&o) == without_parens(new)
        },
    ),
    (
        "a quoted name holding a dot is a name, the stored name of a table of another schema here: the text \
         pass quoted information_schema's view names again inside its quotes, a syntax error",
        |old, new| {
            old.iter().any(|x| x == "\"\"")
                && !new.iter().any(|x| x == "\"\"")
                && new.iter().any(|x| x.starts_with("\"information_schema."))
        },
    ),
    (
        "text search queries joined by `&&` and `||` and negated by `!!` are the engine's overlite_tsquery_op: \
         the text pass took `&&` for an overlap of ranges or arrays and left `||` and `!!` to SQLite, a \
         concatenation and a syntax error",
        |old, new| {
            let has = |t: &[String], w: &str| t.iter().any(|x| x == w);
            has(new, "overlite_tsquery_op") && !has(old, "overlite_tsquery_op")
        },
    ),
    (
        "OVERRIDING SYSTEM VALUE is the session's (identity_insert checks it and takes it out before the \
         statement is translated): the text pass left it to SQLite, which has no such clause",
        |old, new| {
            let Some(at) = old
                .windows(3)
                .position(|w| w[0] == "overriding" && w[1] == "system" && w[2] == "value")
            else {
                return false;
            };
            [&old[..at], &old[at + 3..]].concat() == new
        },
    ),
    (
        "a partition set WITH ORDINALITY is an empty relation with the columns the statement names, overlite \
         having no partitions: the text pass left WITH ORDINALITY after its subquery, a syntax error that \
         failed psql's \\d of every table with a foreign key",
        |old, new| {
            let set = |t: &[String]| {
                t.windows(3).any(|w| {
                    w[0] == "null"
                        && matches!(
                            w[1].as_str(),
                            "pg_partition_ancestors" | "pg_partition_tree" | "pg_partition_root"
                        )
                }) && t.windows(2).any(|w| w[0] == "with" && w[1] == "ordinality")
            };
            set(old) && !new.iter().any(|x| x == "ordinality")
        },
    ),
    (
        "jsonb_typeof and json_typeof name a value's type as Postgres does (number, string, boolean): the text \
         pass wrote SQLite's json_type alone, which says integer, real, text, true and false",
        |old, new| {
            new.windows(2).any(|w| w[0] == "case" && w[1] == "json_type") && old.iter().any(|x| x == "json_type")
        },
    ),
    (
        "label: each statement of a string holding several is labelled as Postgres labels it: the text pass \
         labelled the last one only",
        |old, new| {
            let statements = old
                .iter()
                .enumerate()
                .filter(|(i, x)| *x == "select" && (*i == 0 || old[i - 1] != "("))
                .count();
            statements > 1 && same_but_new_labels(&without_parens(old), &without_parens(new))
        },
    ),
    (
        "a reg* literal names its object as Postgres reads a name, an unquoted one folded and compared without \
         case (the catalog keeps a name as it was created), a quoted one whole: the text pass compared it with \
         its case and cut `schema.\"Name\"` at its last quote",
        |old, new| {
            let mut n: Vec<String> = Vec::new();
            for (i, x) in new.iter().enumerate() {
                let folded = x == "collate"
                    && new.get(i + 1).is_some_and(|y| y == "nocase")
                    && i > 0
                    && new[i - 1].starts_with('\'');
                let after = x == "nocase" && i > 0 && new[i - 1] == "collate";
                if !(folded || after) {
                    n.push(x.to_lowercase());
                }
            }
            let o = without_parens(old);
            let folded = n.len() < new.len();
            let n = without_parens(&n);
            let cut = o
                .iter()
                .zip(&n)
                .any(|(o, x)| o != x && o.starts_with('\'') && o.contains('"'));
            (folded || cut)
                && n.len() == o.len()
                && o.iter()
                    .zip(&n)
                    .all(|(o, x)| o.to_lowercase() == **x || o.starts_with('\''))
        },
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

/// The text pass' subquery around a set-returning function, `( select value x , ord
/// y from overlite_elements ( ... ) ) a`, written in place as the AST path writes it:
/// the walker under the alias, `a . x` and a bare `x` as `a . value`.
fn srf_in_place(t: &[String]) -> Vec<String> {
    let mut out = t.to_vec();
    let mut i = 0;
    while i + 2 < out.len() {
        if !(out[i] == "(" && out[i + 1] == "select") {
            i += 1;
            continue;
        }
        let Some(close) = closing(&out, i) else { break };
        let Some(from) = (i + 2..close).find(|&k| out[k] == "from") else {
            i += 1;
            continue;
        };
        let walker = from + 1;
        if !matches!(out[walker].as_str(), "overlite_elements" | "overlite_pairs")
            || closing(&out, walker + 1) != Some(close - 1)
            || close + 1 >= out.len()
        {
            i += 1;
            continue;
        }
        let alias = out[close + 1].clone();
        // The items: an expression of the walker's row and the name it is given.
        let mut items: Vec<(Vec<String>, String)> = Vec::new();
        let mut cur: Vec<String> = Vec::new();
        let mut depth = 0;
        for x in &out[i + 2..from] {
            match x.as_str() {
                "(" => depth += 1,
                ")" => depth -= 1,
                "," if depth == 0 => {
                    if let Some(name) = cur.pop() {
                        items.push((std::mem::take(&mut cur), name));
                    }
                    continue;
                }
                _ => {}
            }
            cur.push(x.clone());
        }
        if let Some(name) = cur.pop() {
            items.push((cur, name));
        }
        let qualified = |e: &[String]| -> Vec<String> {
            e.iter()
                .flat_map(|x| {
                    if matches!(x.as_str(), "value" | "ord" | "key") {
                        vec![alias.clone(), ".".to_string(), x.clone()]
                    } else {
                        vec![x.clone()]
                    }
                })
                .collect()
        };
        let mut rest: Vec<String> = out[walker..close].to_vec();
        rest.push(alias.clone());
        let renamed = |part: &[String]| -> Vec<String> {
            let mut done: Vec<String> = Vec::new();
            let mut k = 0;
            while k < part.len() {
                let dotted = k + 2 < part.len() && part[k] == alias && part[k + 1] == ".";
                let name_at = if dotted { k + 2 } else { k };
                let bare = !dotted && (k == 0 || part[k - 1] != ".");
                if (dotted || bare)
                    && let Some((e, _)) = items.iter().find(|(_, n)| *n == part[name_at])
                {
                    done.extend(qualified(e));
                    k = name_at + 1;
                    continue;
                }
                done.push(part[k].clone());
                k += 1;
            }
            done
        };
        let (head, tail) = (renamed(&out[..i]), renamed(&out[close + 2..]));
        let at = head.len() + 1;
        out = [head, rest, tail].concat();
        i = at;
    }
    out
}

/// The text pass' array_to_string over ARRAY(subquery) left to the engine,
/// `array_to_string ( overlite_array ( ( select json_group_array ( _a ) from ( select
/// e _a ... ) ) ) , sep )`, as the rows joined: `( select group_concat ( _v , sep )
/// from ( select e _v ... ) )`.
fn joined_arrays(t: &[String]) -> Vec<String> {
    let head = [
        "array_to_string",
        "(",
        "overlite_array",
        "(",
        "(",
        "select",
        "json_group_array",
        "(",
        "_a",
        ")",
        "from",
        "(",
    ];
    let mut out = t.to_vec();
    let mut i = 0;
    while i + head.len() < out.len() {
        if !out[i..i + head.len()].iter().map(String::as_str).eq(head) {
            i += 1;
            continue;
        }
        let (Some(call_end), Some(array_end)) = (closing(&out, i + 1), closing(&out, i + 3)) else {
            break;
        };
        if out.get(array_end + 1).map(String::as_str) != Some(",") {
            i += 1;
            continue;
        }
        let sep: Vec<String> = out[array_end + 2..call_end].to_vec();
        let mut inner: Vec<String> = out[i + head.len() - 1..array_end - 1].to_vec();
        for x in &mut inner {
            if x == "_a" {
                *x = "_v".to_string();
            }
        }
        let mut repl: Vec<String> = ["(", "select", "group_concat", "(", "_v", ","]
            .iter()
            .map(|x| (*x).to_string())
            .collect();
        repl.extend(sep);
        repl.extend([")".to_string(), "from".to_string()]);
        repl.extend(inner);
        repl.push(")".to_string());
        out = [&out[..i], &repl[..], &out[call_end + 1..]].concat();
        i += 1;
    }
    out
}

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

/// The type name of each overlite_cast, and of each array overlite_assign, as the engine
/// reads it: `'UUID'`, `'int'` and `'integer'` are one type to it, `'int[]'` and
/// `'integer[]'` one array type.
fn same_cast_types(mut t: Vec<String>) -> Vec<String> {
    for i in 0..t.len() {
        if t[i] == "overlite_assign"
            && let Some(close) = closing(&t, i + 1)
            && close >= 1
            && let Some(name) = t[close - 1].strip_prefix('\'').and_then(|n| n.strip_suffix("[]'"))
        {
            let name = match name {
                "dec" => "numeric",
                "bool" => "boolean",
                n => n,
            };
            t[close - 1] = match overlite_engine::CastType::parse(name) {
                Some(ty) => format!("{ty:?}[]"),
                None => format!("{name}[]"),
            };
            continue;
        }
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
