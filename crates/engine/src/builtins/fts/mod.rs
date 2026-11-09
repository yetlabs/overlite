//! Full-text search: tsvector and tsquery, kept as the text Postgres writes for each,
//! and the functions over them, with the `simple` and `english` configurations
//! (Postgres' default parser, its English stop words and the Snowball English stemmer).
//!
//! The rewrite turns `@@` into ts_match, a `||` with a tsvector on a side into
//! ts_concat, a cast to tsvector or tsquery into tsvector_in or tsquery_in, and
//! `length` of a tsvector column into ts_length. Two stored vectors run together by
//! SQLite's own `||` read as their concatenation (vector.rs).

mod parser;
mod query;
mod rank;
mod vector;

use crate::catalog::scalar;
use crate::error::{Error, Result};
use overlite_core::{SqlState, Value};
use parser::Config;
use query::TsQuery;
use rusqlite::Connection;
use vector::{Pos, TsVector};

/// A space to a tsvector or tsquery's syntax: Unicode's, but for the no-break spaces,
/// which Postgres' iswspace does not count.
pub(crate) fn is_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}')
}

/// The configuration of a call that names none: default_text_search_config.
const DEFAULT_CONFIG: Config = Config::English;

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        other => other.to_string(),
    }
}

fn config(v: &Value) -> Result<Config> {
    let name = text(v);
    Config::named(&name).ok_or_else(|| {
        Error::with_state(
            SqlState::UNDEFINED_OBJECT,
            format!("text search configuration \"{}\" does not exist", name.trim()),
        )
    })
}

/// The configuration and the text of a call with one argument or two.
fn config_and_text(a: &[Value]) -> Result<(Config, String)> {
    match a {
        [t] => Ok((DEFAULT_CONFIG, text(t))),
        [c, t] => Ok((config(c)?, text(t))),
        _ => Err(Error::with_state(
            SqlState::UNDEFINED_FUNCTION,
            "function does not exist",
        )),
    }
}

/// to_tsvector.
pub(crate) fn to_tsvector(cfg: Config, s: &str) -> TsVector {
    let parts = parser::lexemes(cfg, s)
        .into_iter()
        .filter_map(|(p, w)| w.map(|w| (w, vec![Pos::new(p.min(u32::from(vector::MAX_POS)) as u16, 0)])))
        .collect();
    TsVector::from_parts(parts)
}

fn vector_arg(v: &Value) -> Result<TsVector> {
    TsVector::parse(&text(v))
}

fn query_arg(v: &Value) -> Result<TsQuery> {
    TsQuery::parse(&text(v))
}

fn any_null(a: &[Value]) -> bool {
    a.iter().any(Value::is_null)
}

fn weight_letter(v: &Value) -> Result<u8> {
    let w = text(v);
    match w.trim().to_ascii_lowercase().as_str() {
        "a" => Ok(3),
        "b" => Ok(2),
        "c" => Ok(1),
        "d" => Ok(0),
        _ => Err(Error::with_state(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unrecognized weight: \"{w}\""),
        )),
    }
}

/// The items of an array argument, as this server stores one (JSON) or as Postgres
/// writes it (`{a,b}`).
fn array_items(v: &Value) -> Vec<Option<String>> {
    let t = text(v);
    let t = t.trim();
    let inner = t
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .or_else(|| t.strip_prefix('{').and_then(|x| x.strip_suffix('}')))
        .unwrap_or(t);
    inner
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| {
            if x.eq_ignore_ascii_case("null") {
                None
            } else {
                Some(x.trim_matches('"').to_string())
            }
        })
        .collect()
}

/// A float4 as Postgres prints it, carried as the double of that decimal.
fn float4(f: f32) -> Value {
    Value::Float(f.to_string().parse().unwrap_or(f64::from(f)))
}

/// ts_rank and ts_rank_cd with their four argument lists: (vector, query),
/// (weights, vector, query), (vector, query, normalization) and all four.
fn rank_call(a: &[Value], cd: bool) -> Result<Value> {
    if any_null(a) {
        return Ok(Value::Null);
    }
    let (w, v, q, n) = match a {
        [v, q] => (None, v, q, 0),
        [x, y, z] if matches!(z, Value::Int(_)) => (None, x, y, int_arg(z)),
        [w, v, q] => (Some(w), v, q, 0),
        [w, v, q, n] => (Some(w), v, q, int_arg(n)),
        _ => {
            return Err(Error::with_state(
                SqlState::UNDEFINED_FUNCTION,
                "function does not exist",
            ));
        }
    };
    let weights = match w {
        Some(w) => {
            let items: Vec<Option<f32>> = array_items(w)
                .into_iter()
                .map(|x| x.and_then(|s| s.parse().ok()))
                .collect();
            rank::weights(&items)?
        }
        None => rank::DEFAULT_WEIGHTS,
    };
    let (v, q) = (vector_arg(v)?, query_arg(q)?);
    let r = if cd {
        rank::ts_rank_cd(&weights, &v, &q, n)
    } else {
        rank::ts_rank(&weights, &v, &q, n)
    };
    Ok(float4(r))
}

fn int_arg(v: &Value) -> i32 {
    match v {
        Value::Int(n) => *n as i32,
        other => text(other).trim().parse().unwrap_or(0),
    }
}

/// A stored tsvector or tsquery in its canonical text.
///
/// # Errors
///
/// 42601 for text that is neither.
pub(crate) fn canonical(v: &Value, decl: &str) -> Result<Value> {
    let Value::Text(t) = v else {
        return Ok(v.clone());
    };
    Ok(Value::Text(if decl == "tsquery" {
        TsQuery::parse(t)?.render()
    } else {
        TsVector::parse(t)?.render()
    }))
}

/// Registers the functions of full-text search on a connection.
pub(crate) fn register(conn: &Connection) -> Result<()> {
    scalar(conn, "to_tsvector", -1, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        let (cfg, s) = config_and_text(a)?;
        Ok(Value::Text(to_tsvector(cfg, &s).render()))
    })?;
    type Make = fn(Config, &str) -> Result<TsQuery>;
    let queries: [(&str, Make); 4] = [
        ("to_tsquery", TsQuery::to_tsquery),
        ("plainto_tsquery", |c, s| Ok(TsQuery::plainto(c, s))),
        ("phraseto_tsquery", |c, s| Ok(TsQuery::phraseto(c, s))),
        ("websearch_to_tsquery", |c, s| Ok(TsQuery::websearch(c, s))),
    ];
    for (name, make) in queries {
        scalar(conn, name, -1, move |a| {
            if any_null(a) {
                return Ok(Value::Null);
            }
            let (cfg, s) = config_and_text(a)?;
            Ok(Value::Text(make(cfg, &s)?.render()))
        })?;
    }
    scalar(conn, "tsvector_in", 1, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        Ok(Value::Text(vector_arg(&a[0])?.render()))
    })?;
    // tsquery's operators: `&&` and `||` join two queries ('&', '|'), `!!` negates one
    // ('!', the third argument unread); an empty query leaves the other as it is.
    scalar(conn, "overlite_tsquery_op", 3, |a| {
        if a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        let op = a[0].to_string();
        let x = query_arg(&a[1])?.root;
        let root = if op == "!" {
            x.map(|n| query::Node::Not(Box::new(n)))
        } else {
            if a[2].is_null() {
                return Ok(Value::Null);
            }
            match (x, query_arg(&a[2])?.root) {
                (Some(l), Some(r)) if op == "&" => Some(query::Node::And(Box::new(l), Box::new(r))),
                (Some(l), Some(r)) => Some(query::Node::Or(Box::new(l), Box::new(r))),
                (l, r) => l.or(r),
            }
        };
        Ok(Value::Text(TsQuery { root }.render()))
    })?;
    scalar(conn, "tsquery_in", 1, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        Ok(Value::Text(query_arg(&a[0])?.render()))
    })?;
    scalar(conn, "setweight", -1, |a| {
        if a.len() < 2 || any_null(&a[..2]) {
            return Ok(Value::Null);
        }
        let w = weight_letter(&a[1])?;
        let only: Option<Vec<String>> = a.get(2).map(|l| array_items(l).into_iter().flatten().collect());
        Ok(Value::Text(vector_arg(&a[0])?.setweight(w, only.as_deref()).render()))
    })?;
    scalar(conn, "strip", 1, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        Ok(Value::Text(vector_arg(&a[0])?.strip().render()))
    })?;
    scalar(conn, "ts_length", 1, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        Ok(Value::Int(vector_arg(&a[0])?.entries.len() as i64))
    })?;
    scalar(conn, "ts_concat", 2, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        Ok(Value::Text(vector_arg(&a[0])?.concat(&vector_arg(&a[1])?).render()))
    })?;
    scalar(conn, "ts_match", 2, |a| {
        if any_null(a) {
            return Ok(Value::Null);
        }
        Ok(Value::Int(i64::from(query::matches(
            &vector_arg(&a[0])?,
            &query_arg(&a[1])?,
        ))))
    })?;
    scalar(conn, "ts_rank", -1, |a| rank_call(a, false))?;
    scalar(conn, "ts_rank_cd", -1, |a| rank_call(a, true))?;
    scalar(conn, "get_current_ts_config", 0, |_| Ok(Value::Text("english".into())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_as_postgres_writes_them() {
        assert_eq!(
            to_tsvector(Config::English, "Cookbook: Christmas Mince Pies (Meat-Free)").render(),
            "'christma':2 'cookbook':1 'free':7 'meat':6 'meat-fre':5 'minc':3 'pie':4"
        );
        assert_eq!(
            to_tsvector(Config::Simple, "The Greeks in 6th century AD").render(),
            "'6th':4 'ad':6 'century':5 'greeks':2 'in':3 'the':1"
        );
    }

    #[test]
    fn ranks_as_postgres_gives_them() {
        let v = TsVector::parse("'bread':1,3 'yeast':2 'wild':4").unwrap();
        let q = TsQuery::parse("bread").unwrap();
        assert!(query::matches(&v, &q));
        assert!(rank::ts_rank(&rank::DEFAULT_WEIGHTS, &v, &q, 0) > 0.0);
    }
}
