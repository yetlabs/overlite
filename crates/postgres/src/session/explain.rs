//! EXPLAIN, answered with a plan in Postgres' shape built from SQLite's own: each
//! row of `EXPLAIN QUERY PLAN` becomes the node Postgres would name (a SCAN a Seq
//! Scan, a SEARCH an Index Scan, a temporary b-tree for ORDER BY a Sort, tables side
//! by side a Nested Loop), in the format asked for: TEXT, JSON, XML or YAML. SQLite
//! estimates no cost and no row count, so those are 0; ANALYZE runs the statement and
//! reports its real rows and times.

use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::{Column, Command, ResultSet, SqlState, Value};
use std::time::Instant;

/// Whether sql is an EXPLAIN.
pub(crate) fn is_explain(sql: &str) -> bool {
    let head = sql.trim_start().as_bytes();
    head.len() > 7 && head[..7].eq_ignore_ascii_case(b"explain") && !head[7].is_ascii_alphanumeric()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Json,
    Xml,
    Yaml,
}

struct Options {
    analyze: bool,
    costs: bool,
    timing: bool,
    summary: Option<bool>,
    format: Format,
}

/// A plan node: its type, what it reads, and its children.
#[derive(Default)]
struct Node {
    node_type: String,
    relation: Option<String>,
    alias: Option<String>,
    index: Option<String>,
    children: Vec<Node>,
    actual_rows: Option<usize>,
}

/// The options and the statement of an EXPLAIN.
fn parse(sql: &str) -> Result<(Options, &str)> {
    let mut o = Options {
        analyze: false,
        costs: true,
        timing: true,
        summary: None,
        format: Format::Text,
    };
    let mut rest = sql.trim_start()[7..].trim_start();
    if rest.starts_with('(') {
        let close = rest
            .find(')')
            .ok_or_else(|| errf!(SqlState::SYNTAX_ERROR, "syntax error at end of input"))?;
        for opt in rest[1..close].split(',') {
            let mut words = opt.split_whitespace();
            let Some(name) = words.next() else { continue };
            let value = words.next().map(str::to_ascii_lowercase);
            let on = !matches!(value.as_deref(), Some("false" | "off" | "0"));
            match name.to_ascii_lowercase().as_str() {
                "analyze" | "analyse" => o.analyze = on,
                "costs" => o.costs = on,
                "timing" => o.timing = on,
                "summary" => o.summary = Some(on),
                "format" => {
                    o.format = match value.as_deref() {
                        Some("text") => Format::Text,
                        Some("json") => Format::Json,
                        Some("xml") => Format::Xml,
                        Some("yaml") => Format::Yaml,
                        _ => {
                            return Err(errf!(
                                SqlState::INVALID_PARAMETER_VALUE,
                                "unrecognized value for EXPLAIN option \"format\": \"{}\"",
                                value.unwrap_or_default()
                            ));
                        }
                    };
                }
                "verbose" | "buffers" | "settings" | "wal" | "generic_plan" | "memory" | "serialize" => {}
                other => {
                    return Err(errf!(SqlState::SYNTAX_ERROR, "unrecognized EXPLAIN option \"{other}\""));
                }
            }
        }
        rest = rest[close + 1..].trim_start();
    } else {
        loop {
            let word: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
            match word.to_ascii_lowercase().as_str() {
                "analyze" | "analyse" => o.analyze = true,
                "verbose" => {}
                _ => break,
            }
            rest = rest[word.len()..].trim_start();
        }
    }
    Ok((o, rest))
}

impl Session {
    /// Answers an EXPLAIN.
    pub(crate) fn explain(&mut self, sql: &str) -> Result<ResultSet> {
        let (o, inner) = parse(sql)?;
        let inner = inner.trim_end().trim_end_matches(';');
        let started = Instant::now();
        let rewritten = self.rewrite_for_exec(inner, &[])?;
        let planned = started.elapsed();
        let rows = self.exec(&["EXPLAIN QUERY PLAN ", &rewritten].concat(), &[])?;
        let mut root = plan(inner, &rows.rows);
        let mut executed = None;
        if o.analyze {
            let t = Instant::now();
            let rs = self.exec(&rewritten, &[])?;
            executed = Some(t.elapsed());
            root.actual_rows = Some(if rs.is_query { rs.rows.len() } else { 0 });
        }
        let summary = o.summary.unwrap_or(o.analyze);
        let times = summary.then(|| {
            (
                planned.as_secs_f64() * 1000.0,
                executed.map_or(0.0, |e| e.as_secs_f64() * 1000.0),
            )
        });
        let ms = executed.map_or(0.0, |e| e.as_secs_f64() * 1000.0);
        let (lines, oid_type) = match o.format {
            Format::Text => (text(&root, &o, ms, times), "TEXT"),
            Format::Json => (vec![json(&root, &o, ms, times)], "JSON"),
            Format::Xml => (vec![xml(&root, &o, ms, times)], "TEXT"),
            Format::Yaml => (vec![yaml(&root, &o, ms, times)], "TEXT"),
        };
        Ok(ResultSet {
            is_query: true,
            columns: vec![Column {
                name: "QUERY PLAN".to_string(),
                decl_type: oid_type.to_string(),
            }],
            rows_affected: lines.len() as i64,
            rows: lines.into_iter().map(|l| vec![Value::Text(l)]).collect(),
            command: Command::from("EXPLAIN"),
            ..Default::default()
        })
    }
}

/// The plan of a statement from SQLite's rows: (id, parent, notused, detail).
fn plan(statement: &str, rows: &[Vec<Value>]) -> Node {
    let rows: Vec<(i64, i64, String)> = rows
        .iter()
        .map(|r| {
            let int = |v: &Value| if let Value::Int(i) = v { *i } else { 0 };
            (
                int(&r[0]),
                int(&r[1]),
                r.get(3).map(Value::to_text_lossy).unwrap_or_default(),
            )
        })
        .collect();
    let body = level(&rows, 0);
    let first = statement
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let verb = match first.as_str() {
        "insert" => Some("Insert"),
        "update" => Some("Update"),
        "delete" => Some("Delete"),
        _ => None,
    };
    match verb {
        Some(v) => {
            let table = target_table(statement, &first);
            Node {
                node_type: "ModifyTable".into(),
                relation: Some(table),
                alias: Some(v.into()),
                children: vec![body.unwrap_or_else(|| Node {
                    node_type: "Result".into(),
                    ..Node::default()
                })],
                ..Node::default()
            }
        }
        None => body.unwrap_or_else(|| Node {
            node_type: "Result".into(),
            ..Node::default()
        }),
    }
}

/// The table an INSERT, UPDATE or DELETE writes.
fn target_table(statement: &str, verb: &str) -> String {
    let words: Vec<&str> = statement.split_whitespace().collect();
    let after = match verb {
        "insert" => "into",
        "delete" => "from",
        _ => "update",
    };
    words
        .iter()
        .position(|w| w.eq_ignore_ascii_case(after))
        .and_then(|i| words.get(i + 1))
        .map(|w| w.split('(').next().unwrap_or(w).trim_matches('"').to_string())
        .unwrap_or_default()
}

/// The nodes under parent, as one node: siblings that read tables are joined by
/// Nested Loops, and a temporary b-tree over them is the Sort, Aggregate or Unique
/// above them.
fn level(rows: &[(i64, i64, String)], parent: i64) -> Option<Node> {
    let mut scans = Vec::new();
    let mut wrappers = Vec::new();
    for (id, _, detail) in rows.iter().filter(|(_, p, _)| *p == parent) {
        let up = detail.to_ascii_uppercase();
        if let Some(what) = up.strip_prefix("USE TEMP B-TREE FOR ") {
            wrappers.push(match what {
                w if w.starts_with("ORDER BY") || w.starts_with("RIGHT PART OF ORDER BY") => "Sort",
                w if w.starts_with("GROUP BY") => "GroupAggregate",
                _ => "Unique",
            });
            continue;
        }
        let mut node = node_of(detail);
        if let Some(sub) = level(rows, *id) {
            node.children.push(sub);
        }
        scans.push(node);
    }
    let mut it = scans.into_iter();
    let mut acc = it.next()?;
    for next in it {
        acc = Node {
            node_type: "Nested Loop".into(),
            children: vec![acc, next],
            ..Node::default()
        };
    }
    for w in wrappers.into_iter().rev() {
        acc = Node {
            node_type: w.into(),
            children: vec![acc],
            ..Node::default()
        };
    }
    Some(acc)
}

/// The node of one row of SQLite's plan.
fn node_of(detail: &str) -> Node {
    let words: Vec<&str> = detail.split_whitespace().collect();
    let up: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
    let is = |i: usize, w: &str| up.get(i).is_some_and(|u| u == w);
    if (is(0, "SCAN") || is(0, "SEARCH")) && !is(1, "CONSTANT") {
        let table = words.get(1).copied().unwrap_or("").to_string();
        let alias = is(2, "AS").then(|| words.get(3).copied().unwrap_or("").to_string());
        let using = up.iter().position(|w| w == "USING");
        let (node_type, index) = match using {
            Some(u) if is(u + 1, "INTEGER") || (is(u + 1, "PRIMARY") && is(u + 2, "KEY")) => {
                ("Index Scan", Some(format!("{table}_pkey")))
            }
            Some(u) if is(u + 1, "COVERING") => ("Index Only Scan", words.get(u + 3).map(|s| (*s).to_string())),
            Some(u) if is(u + 1, "INDEX") => ("Index Scan", words.get(u + 2).map(|s| (*s).to_string())),
            _ => ("Seq Scan", None),
        };
        return Node {
            node_type: node_type.into(),
            relation: Some(table),
            alias,
            index,
            ..Node::default()
        };
    }
    let node_type = if is(0, "SCAN") {
        "Result"
    } else if up.iter().any(|w| w == "SUBQUERY") {
        "SubPlan"
    } else if is(0, "MATERIALIZE") || is(0, "CO-ROUTINE") {
        "Subquery Scan"
    } else if is(0, "COMPOUND") || up.iter().any(|w| w == "UNION") {
        "Append"
    } else {
        "Result"
    };
    Node {
        node_type: node_type.into(),
        ..Node::default()
    }
}

/// How the text format names a node: "Seq Scan on t x", "Insert on t".
fn label(n: &Node) -> String {
    if n.node_type == "ModifyTable" {
        return format!(
            "{} on {}",
            n.alias.as_deref().unwrap_or(""),
            n.relation.as_deref().unwrap_or("")
        );
    }
    let mut s = n.node_type.clone();
    if let Some(i) = &n.index {
        s.push_str(" using ");
        s.push_str(i);
    }
    if let Some(r) = &n.relation {
        s.push_str(" on ");
        s.push_str(r);
        if let Some(a) = &n.alias {
            s.push(' ');
            s.push_str(a);
        }
    }
    s
}

fn text(root: &Node, o: &Options, ms: f64, times: Option<(f64, f64)>) -> Vec<String> {
    fn walk(n: &Node, depth: usize, o: &Options, ms: f64, out: &mut Vec<String>) {
        let mut line = if depth == 0 {
            String::new()
        } else {
            " ".repeat(2 + 6 * (depth - 1)) + "->  "
        };
        line.push_str(&label(n));
        if o.costs {
            line.push_str("  (cost=0.00..0.00 rows=0 width=0)");
        }
        if let Some(rows) = n.actual_rows {
            if o.timing {
                line.push_str(&format!(" (actual time=0.000..{ms:.3} rows={rows} loops=1)"));
            } else {
                line.push_str(&format!(" (actual rows={rows} loops=1)"));
            }
        }
        out.push(line);
        for c in &n.children {
            walk(c, depth + 1, o, ms, out);
        }
    }
    let mut out = Vec::new();
    walk(root, 0, o, ms, &mut out);
    if let Some((p, e)) = times {
        out.push(format!("Planning Time: {p:.3} ms"));
        out.push(format!("Execution Time: {e:.3} ms"));
    }
    out
}

/// A node's fields in Postgres' order, each a key and its value already in JSON.
fn fields(n: &Node, o: &Options, ms: f64) -> Vec<(&'static str, String)> {
    let q = |s: &str| ["\"", &s.replace('\\', "\\\\").replace('"', "\\\""), "\""].concat();
    let mut f = Vec::new();
    if n.node_type == "ModifyTable" {
        f.push(("Node Type", q("ModifyTable")));
        f.push(("Operation", q(&n.alias.as_deref().unwrap_or("").to_ascii_lowercase())));
    } else {
        f.push(("Node Type", q(&n.node_type)));
    }
    f.push(("Parallel Aware", "false".into()));
    f.push(("Async Capable", "false".into()));
    if let Some(i) = &n.index {
        f.push(("Index Name", q(i)));
    }
    if let Some(r) = &n.relation {
        f.push(("Relation Name", q(r)));
        let alias = if n.node_type == "ModifyTable" {
            r
        } else {
            n.alias.as_ref().unwrap_or(r)
        };
        f.push(("Alias", q(alias)));
    }
    if o.costs {
        f.push(("Startup Cost", "0.00".into()));
        f.push(("Total Cost", "0.00".into()));
        f.push(("Plan Rows", "0".into()));
        f.push(("Plan Width", "0".into()));
    }
    if let Some(rows) = n.actual_rows {
        if o.timing {
            f.push(("Actual Startup Time", "0.000".into()));
            f.push(("Actual Total Time", format!("{ms:.3}")));
        }
        f.push(("Actual Rows", rows.to_string()));
        f.push(("Actual Loops", "1".into()));
    }
    f
}

fn json(root: &Node, o: &Options, ms: f64, times: Option<(f64, f64)>) -> String {
    fn node(n: &Node, o: &Options, ms: f64, ind: usize, out: &mut String) {
        let pad = " ".repeat(ind);
        out.push_str("{\n");
        let mut items: Vec<String> = fields(n, o, ms)
            .into_iter()
            .map(|(k, v)| format!("{pad}  \"{k}\": {v}"))
            .collect();
        if !n.children.is_empty() {
            let mut plans = format!("{pad}  \"Plans\": [\n");
            for (i, c) in n.children.iter().enumerate() {
                plans.push_str(&" ".repeat(ind + 4));
                node(c, o, ms, ind + 4, &mut plans);
                plans.push_str(if i + 1 < n.children.len() { ",\n" } else { "\n" });
            }
            plans.push_str(&format!("{pad}  ]"));
            items.push(plans);
        }
        out.push_str(&items.join(",\n"));
        out.push('\n');
        out.push_str(&pad);
        out.push('}');
    }
    let mut out = String::from("[\n  {\n    \"Plan\": ");
    node(root, o, ms, 4, &mut out);
    if let Some((p, e)) = times {
        out.push_str(&format!(
            ",\n    \"Planning Time\": {p:.3},\n    \"Execution Time\": {e:.3}"
        ));
    }
    out.push_str("\n  }\n]");
    out
}

fn yaml(root: &Node, o: &Options, ms: f64, times: Option<(f64, f64)>) -> String {
    fn node(n: &Node, o: &Options, ms: f64, ind: usize, out: &mut Vec<String>, first_dash: bool) {
        for (i, (k, v)) in fields(n, o, ms).into_iter().enumerate() {
            let lead = if first_dash && i == 0 {
                " ".repeat(ind - 2) + "- "
            } else {
                " ".repeat(ind)
            };
            out.push(format!("{lead}{k}: {v}"));
        }
        if !n.children.is_empty() {
            out.push(format!("{}Plans: ", " ".repeat(ind)));
            for c in &n.children {
                node(c, o, ms, ind + 4, out, true);
            }
        }
    }
    let mut out = vec!["- Plan: ".to_string()];
    node(root, o, ms, 4, &mut out, false);
    if let Some((p, e)) = times {
        out.push(format!("  Planning Time: {p:.3}"));
        out.push(format!("  Execution Time: {e:.3}"));
    }
    out.join("\n")
}

fn xml(root: &Node, o: &Options, ms: f64, times: Option<(f64, f64)>) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
    fn node(n: &Node, o: &Options, ms: f64, ind: usize, out: &mut Vec<String>) {
        let pad = " ".repeat(ind);
        out.push(format!("{pad}<Plan>"));
        for (k, v) in fields(n, o, ms) {
            let tag = k.replace(' ', "-");
            let v = v.trim_matches('"').replace("\\\"", "\"").replace("\\\\", "\\");
            out.push(format!("{pad}  <{tag}>{}</{tag}>", esc(&v)));
        }
        if !n.children.is_empty() {
            out.push(format!("{pad}  <Plans>"));
            for c in &n.children {
                node(c, o, ms, ind + 4, out);
            }
            out.push(format!("{pad}  </Plans>"));
        }
        out.push(format!("{pad}</Plan>"));
    }
    let mut out = vec![
        "<explain xmlns=\"http://www.postgresql.org/2009/explain\">".to_string(),
        "  <Query>".to_string(),
    ];
    node(root, o, ms, 4, &mut out);
    if let Some((p, e)) = times {
        out.push(format!("    <Planning-Time>{p:.3}</Planning-Time>"));
        out.push(format!("    <Execution-Time>{e:.3}</Execution-Time>"));
    }
    out.push("  </Query>".to_string());
    out.push("</explain>".to_string());
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: &[(i64, i64, &str)]) -> Vec<Vec<Value>> {
        r.iter()
            .map(|(a, b, d)| vec![Value::Int(*a), Value::Int(*b), Value::Int(0), Value::Text((*d).into())])
            .collect()
    }

    #[test]
    fn plans_in_postgres_shape() {
        let o = Options {
            analyze: false,
            costs: false,
            timing: true,
            summary: None,
            format: Format::Text,
        };
        let p = plan(
            "SELECT * FROM a JOIN b ON a.id = b.a ORDER BY a.x",
            &rows(&[
                (3, 0, "SCAN a"),
                (5, 0, "SEARCH b USING INDEX b_a (a=?)"),
                (9, 0, "USE TEMP B-TREE FOR ORDER BY"),
            ]),
        );
        assert_eq!(
            text(&p, &o, 0.0, None),
            [
                "Sort",
                "  ->  Nested Loop",
                "        ->  Seq Scan on a",
                "        ->  Index Scan using b_a on b"
            ]
        );
        let j = json(&p, &o, 0.0, None);
        assert!(
            j.starts_with("[\n  {\n    \"Plan\": {\n      \"Node Type\": \"Sort\""),
            "{j}"
        );
        let p = plan("INSERT INTO t VALUES (1)", &[]);
        assert_eq!(text(&p, &o, 0.0, None), ["Insert on t", "  ->  Result"]);
    }

    #[test]
    fn options() {
        let (o, rest) = parse("EXPLAIN (FORMAT JSON, ANALYZE true, COSTS OFF) SELECT 1").unwrap();
        assert!(o.analyze && !o.costs && o.format == Format::Json);
        assert_eq!(rest, "SELECT 1");
        let (o, rest) = parse("explain analyze verbose select 1").unwrap();
        assert!(o.analyze);
        assert_eq!(rest, "select 1");
        assert!(parse("EXPLAIN (FORMAT CSV) SELECT 1").is_err());
    }
}
