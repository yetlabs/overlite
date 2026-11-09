//! What a write stores is the storage format's canonical form (ARCHITECTURE.md,
//! "Storage format"), and a column's type modifier binds it: `numeric(10,2)` rounds and
//! overflows, `varchar(3)` overflows, a uuid is lower case, a date `YYYY-MM-DD`, an array
//! a JSON array. SQLite keeps none of this, so a write is rewritten to pass each value
//! for such a column through `overlite_assign`: the values of an INSERT, whatever its
//! source, the defaults of the columns it leaves out, an UPDATE's SET, a row of them
//! included, and an ON CONFLICT DO UPDATE. A WITH before the INSERT or UPDATE stays
//! where it is. Only what is written goes through: a row already stored stays as it is.
//!
//! SQLite has no DEFAULT in a VALUES list or a SET, so the column's own default stands
//! there: its expression, a nextval of its sequence, or NULL.

use crate::auth::rls::{balanced_paren, parse_insert, text_or_empty, top_level_keyword};
use crate::ddl::sequences::split_top_level;
use crate::session::Session;
use crate::session::utility::{first_word_upper, unquote_ident};
use overlite_core::Value;
use overlite_core::lazy_regex;
use overlite_core::text::end_of_string_literal;
use overlite_core::text::{eq_fold, quote_ident, quote_literal, trim_end_chars, trim_start_chars};
use overlite_engine::{assign_value, assignment_type, blank_literals, code_only};
use std::collections::HashMap;

lazy_regex!(
    RE_UPDATE_TARGET,
    r#"(?is)^\s*UPDATE\s+(?:ONLY\s+)?((?:"(?:[^"]|"")+"|[\w$]+)(?:\s*\.\s*(?:"(?:[^"]|"")+"|[\w$]+))?)"#
);

lazy_regex!(RE_DEFAULT_WORD, r"(?i)\bdefault\b");

/// A table's columns in order, each with the type its values are made to fit and its
/// default.
struct Columns {
    names: Vec<String>,
    /// By lower-cased name.
    assign: HashMap<String, String>,
    /// SQLite's default expression, by lower-cased name.
    defaults: HashMap<String, String>,
    /// The sequence a nextval default draws from, by lower-cased name.
    sequences: HashMap<String, String>,
    /// The columns of the primary key.
    keys: Vec<KeyColumn>,
}

/// A column of the primary key. Postgres refuses a NULL in one, given or left to a
/// default that is none, where SQLite fills in a rowid for an INTEGER PRIMARY KEY and
/// takes a NULL in any other. A serial or identity key, AUTOINCREMENT here, has a
/// default, its next value, which is what SQLite writes for its NULL. So a value written
/// to a key column is refused when NULL, and one with no default left out is written as
/// NULL, refused. An INTEGER PRIMARY KEY not declared NOT NULL is the exception: left
/// out, it takes the next rowid, as clients written for SQLite expect of it.
struct KeyColumn {
    /// Lower-cased.
    column: String,
    relation: String,
    generated: bool,
}

impl KeyColumn {
    fn checked(&self, value: &str) -> String {
        [
            "overlite_not_null(",
            value.trim(),
            ", ",
            &quote_literal(&self.relation),
            ", ",
            &quote_literal(&self.column),
            ")",
        ]
        .concat()
    }
}

impl Columns {
    fn key(column: &str) -> String {
        unquote_ident(column.trim()).to_lowercase()
    }

    fn key_column(&self, column: &str) -> Option<&KeyColumn> {
        let name = Self::key(column);
        self.keys.iter().find(|k| k.column == name)
    }

    fn fit(&self, column: &str, value: &str) -> String {
        if let Some(key) = self.key_column(column) {
            return key.checked(value);
        }
        match self.assign.get(&Self::key(column)) {
            Some(ty) => ["overlite_assign(", value.trim(), ", ", &quote_literal(ty), ")"].concat(),
            None => value.trim().to_string(),
        }
    }

    /// value, made to fit column; DEFAULT is the column's default.
    fn wrap(&self, column: &str, value: &str) -> String {
        if value.trim().eq_ignore_ascii_case("default") {
            return self.default_of(column);
        }
        self.fit(column, value)
    }

    /// What DEFAULT stands for in column.
    fn default_of(&self, column: &str) -> String {
        let key = Self::key(column);
        if let Some(seq) = self.sequences.get(&key) {
            return ["nextval(", &quote_literal(seq), ")"].concat();
        }
        match self.defaults.get(&key) {
            Some(d) => self.fit(column, d),
            // A generated key's next value, which SQLite gives a NULL.
            None if self.key_column(column).is_some_and(|k| k.generated) => "NULL".to_string(),
            None => self.fit(column, "NULL"),
        }
    }

    /// The default an INSERT that leaves column out has to write itself, because SQLite
    /// would store it as it is: one of a column whose values are fitted, unless it is a
    /// constant already in its form.
    fn written_default(&self, column: &str) -> Option<String> {
        let key = Self::key(column);
        if let Some(k) = self.key_column(column) {
            return (!k.generated && !self.defaults.contains_key(&key)).then(|| k.checked("NULL"));
        }
        let ty = self.assign.get(&key)?;
        let d = self.defaults.get(&key)?;
        if let Some(v) = constant(d)
            && assign_value(&v, ty).is_ok_and(|fitted| fitted == v)
        {
            return None;
        }
        Some(self.fit(column, d))
    }
}

/// The value of a default that is a constant: a string or a number.
fn constant(expr: &str) -> Option<Value> {
    let e = expr.trim();
    if e.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if let Some(inner) = e.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))
        && !inner.replace("''", "").contains('\'')
    {
        return Some(Value::Text(inner.replace("''", "'")));
    }
    if let Ok(n) = e.parse::<i64>() {
        return Some(Value::Int(n));
    }
    None
}

impl Session {
    /// Writes a statement so what it stores fits the columns it stores to.
    pub(crate) fn fit_written_values(&mut self, sql: &str) -> String {
        match first_word_upper(sql).as_str() {
            "INSERT" => self.fit_insert(sql).unwrap_or_else(|| sql.to_string()),
            "UPDATE" => self.fit_update(sql).unwrap_or_else(|| sql.to_string()),
            "WITH" => self.fit_after_with(sql).unwrap_or_else(|| sql.to_string()),
            _ => sql.to_string(),
        }
    }

    /// `WITH ... INSERT` and `WITH ... UPDATE`: the write after the WITH, fitted.
    fn fit_after_with(&mut self, sql: &str) -> Option<String> {
        let code = blank_literals(sql);
        let at = ["insert", "update"]
            .iter()
            .filter_map(|w| top_level_keyword(&code, w))
            .min()?;
        let write = &sql[at..];
        let fitted = if first_word_upper(write) == "INSERT" {
            let write = insert_source_for_sqlite(write);
            self.fit_insert(&write).unwrap_or(write)
        } else {
            self.fit_update(write)?
        };
        Some([&sql[..at], &fitted].concat())
    }

    fn fit_columns(&mut self, reference: &str, sequences: bool) -> Option<Columns> {
        let stored = self.stored_name(reference);
        let q = "SELECT name, type, dflt_value, pk, (SELECT sql FROM sqlite_master WHERE type = 'table' AND name = "
            .to_string()
            + &quote_literal(&stored)
            + ") LIKE '%AUTOINCREMENT%', \"notnull\" FROM pragma_table_info("
            + &quote_literal(&stored)
            + ")";
        let rs = self.exec(&q, &[]).ok()?;
        let mut columns = Columns {
            names: Vec::with_capacity(rs.rows.len()),
            assign: HashMap::new(),
            defaults: HashMap::new(),
            sequences: HashMap::new(),
            keys: Vec::new(),
        };
        let keys: Vec<&Vec<Value>> = rs.rows.iter().filter(|r| !matches!(r[3], Value::Int(0))).collect();
        let rowid = keys.len() == 1 && text_or_empty(&keys[0][1]).eq_ignore_ascii_case("integer");
        let relation = stored.rsplit('.').next().unwrap_or(&stored).to_string();
        columns.keys = keys
            .iter()
            .map(|key| KeyColumn {
                column: text_or_empty(&key[0]).to_lowercase(),
                relation: relation.clone(),
                generated: rowid && (matches!(key[4], Value::Int(1)) || matches!(key[5], Value::Int(0))),
            })
            .collect();
        for row in &rs.rows {
            let name = text_or_empty(&row[0]);
            if let Some(ty) = assignment_type(&text_or_empty(&row[1])) {
                columns.assign.insert(name.to_lowercase(), ty);
            }
            if !row[2].is_null() {
                columns.defaults.insert(name.to_lowercase(), text_or_empty(&row[2]));
            }
            columns.names.push(name);
        }
        if columns.names.is_empty() {
            return None;
        }
        if sequences {
            let q = "SELECT colname, seqname FROM _overlite_defaults WHERE lower(tablename) = lower(".to_string()
                + &quote_literal(&stored)
                + ")";
            if let Ok(rs) = self.exec(&q, &[]) {
                for row in &rs.rows {
                    columns
                        .sequences
                        .insert(text_or_empty(&row[0]).to_lowercase(), text_or_empty(&row[1]));
                }
            }
        }
        Some(columns)
    }

    fn fit_insert(&mut self, sql: &str) -> Option<String> {
        let (target, cols, src_start) = parse_insert(sql);
        let src_start = usize::try_from(src_start).ok()?;
        if target.is_empty() {
            return None;
        }
        let mut src_start = src_start;
        // OVERRIDING SYSTEM VALUE belongs before the source, where identity_insert reads it.
        let lead = code_only(&sql[src_start..]).to_ascii_lowercase();
        if lead.trim_start().starts_with("overriding") {
            src_start += lead.find(" value")? + " value".len();
        }
        let source = &sql[src_start..];
        let low = code_only(source).to_ascii_lowercase();
        let named_default = RE_DEFAULT_WORD.is_match(&low);
        let columns = self.fit_columns(&target, named_default)?;
        let default_values = low.trim_start().starts_with("default");
        let targets = if cols.is_empty() && !default_values {
            columns.names.clone()
        } else {
            cols
        };
        // The columns left out whose defaults have to be written here.
        let omitted: Vec<(String, String)> = if targets.len() < columns.names.len() {
            columns
                .names
                .iter()
                .filter(|n| !targets.iter().any(|t| Columns::key(t) == n.to_lowercase()))
                .filter_map(|n| columns.written_default(n).map(|d| (n.clone(), d)))
                .collect()
        } else {
            Vec::new()
        };
        let names_key = targets.iter().any(|t| columns.key_column(t).is_some());
        if columns.assign.is_empty() && !named_default && omitted.is_empty() && !names_key {
            return None;
        }
        let (body, tail) = source.split_at(tail_of_source(source, &low));
        let fitted = if default_values {
            if omitted.is_empty() {
                body.to_string()
            } else {
                let values: Vec<&str> = omitted.iter().map(|(_, d)| d.as_str()).collect();
                format!("VALUES ({}) ", values.join(", "))
            }
        } else if low.trim_start().starts_with("values") {
            fit_values(body, &targets, &columns, &omitted)?
        } else {
            fit_select(body, &targets, &columns, &omitted)
        };
        let head = &sql[..src_start];
        let head = if omitted.is_empty() {
            head.to_string()
        } else {
            let added: Vec<&str> = omitted.iter().map(|(c, _)| c.as_str()).collect();
            let added: Vec<String> = added.iter().map(|c| quote_ident(c)).collect();
            match head.rfind(')').filter(|_| !targets.is_empty()) {
                Some(close) => [&head[..close], ", ", &added.join(", "), &head[close..]].concat(),
                None => [head.trim_end(), " (", &added.join(", "), ") "].concat(),
            }
        };
        Some([&head, fitted.as_str(), " ", &fit_conflict(tail, &columns)].concat())
    }

    fn fit_update(&mut self, sql: &str) -> Option<String> {
        let target = RE_UPDATE_TARGET.captures(sql)?.get(1)?.as_str().to_string();
        let set = top_level_keyword(sql, "set")?;
        let named_default = RE_DEFAULT_WORD.is_match(&code_only(&sql[set..]));
        let columns = self.fit_columns(&target, named_default)?;
        let set_code = code_only(&sql[set..]).to_lowercase();
        let sets_key = columns.keys.iter().any(|k| set_code.contains(&k.column));
        if columns.assign.is_empty() && !named_default && !sets_key {
            return None;
        }
        Some([&sql[..set], &fit_assignments(&sql[set..], &columns)].concat())
    }
}

/// Where the source of an INSERT ends and its ON CONFLICT or RETURNING starts.
fn tail_of_source(source: &str, low: &str) -> usize {
    let mut end = source.len();
    for word in ["returning", "on"] {
        // ON at the top level of a source that is a SELECT opens a join's condition
        // only after a JOIN, and what the conflict clause says next tells them apart.
        let mut from = 0;
        while let Some(at) = top_level_keyword(&source[from..], word) {
            let at = from + at;
            let next = trim_start_chars(&low[at + word.len()..], " \t\r\n");
            if word == "returning" || next.starts_with("conflict") {
                end = end.min(at);
                break;
            }
            from = at + word.len();
        }
    }
    end
}

/// Wraps the values of each tuple of a VALUES, and adds the defaults written here.
fn fit_values(body: &str, targets: &[String], columns: &Columns, omitted: &[(String, String)]) -> Option<String> {
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 32);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                let j = end_of_string_literal(body, i);
                out.push_str(&body[i..j]);
                i = j;
            }
            b'(' => {
                let (inner, end) = balanced_paren(body, i);
                let items = split_top_level(&inner);
                let mut fitted: Vec<String> = items
                    .iter()
                    .enumerate()
                    .map(|(k, item)| match targets.get(k) {
                        Some(column) => columns.wrap(column, item),
                        None => item.clone(),
                    })
                    .collect();
                fitted.extend(omitted.iter().map(|(_, d)| d.clone()));
                out.push('(');
                out.push_str(&fitted.join(", "));
                out.push(')');
                i = end;
            }
            _ => {
                let c = body[i..].chars().next()?;
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    Some(out)
}

/// Wraps what a SELECT source gives, by position, through a CTE, and adds the defaults
/// written here. `WHERE true` keeps SQLite from reading an ON CONFLICT that follows as a
/// join's.
fn fit_select(body: &str, targets: &[String], columns: &Columns, omitted: &[(String, String)]) -> String {
    let names: Vec<String> = (0..targets.len()).map(|i| format!("c{i}")).collect();
    let mut values: Vec<String> = targets
        .iter()
        .zip(&names)
        .map(|(target, name)| columns.fit(target, name))
        .collect();
    values.extend(omitted.iter().map(|(_, d)| d.clone()));
    format!(
        "WITH _written({}) AS ({}) SELECT {} FROM _written WHERE true",
        names.join(", "),
        body.trim(),
        values.join(", ")
    )
}

/// Wraps what an ON CONFLICT DO UPDATE sets.
fn fit_conflict(tail: &str, columns: &Columns) -> String {
    let code = code_only(tail).to_ascii_lowercase();
    match top_level_keyword(tail, "set").filter(|_| code.contains("do update")) {
        Some(set) => [&tail[..set], &fit_assignments(&tail[set..], columns)].concat(),
        None => tail.to_string(),
    }
}

/// Wraps the values of a SET, which text opens with: `a = v`, and `(a, b) = (v, w)`.
fn fit_assignments(text: &str, columns: &Columns) -> String {
    let start = "set".len();
    let mut end = trim_end_chars(text, "; \t\r\n").len();
    for word in ["where", "from", "returning"] {
        if let Some(at) = top_level_keyword(&text[start..], word) {
            end = end.min(start + at);
        }
    }
    let fitted: Vec<String> = split_top_level(&text[start..end])
        .iter()
        .map(|assignment| {
            let code = code_only(assignment);
            if code.trim_start().starts_with('(') {
                return fit_row_assignment(assignment, columns).unwrap_or_else(|| assignment.clone());
            }
            match code.find('=').filter(|at| !code[..*at].contains('(')) {
                Some(eq) => {
                    let (column, value) = (&assignment[..eq], &assignment[eq + 1..]);
                    let plain = column.rsplit('.').next().unwrap_or(column);
                    if eq_fold(plain.trim(), "") {
                        assignment.clone()
                    } else {
                        [column.trim(), " = ", &columns.wrap(plain, value)].concat()
                    }
                }
                None => assignment.clone(),
            }
        })
        .collect();
    ["SET ", &fitted.join(", "), " ", &text[end..]].concat()
}

/// `(a, b) = (v, w)` or `= ROW(v, w)`, each value fitted to its column. A row from a
/// subquery is left as it is.
fn fit_row_assignment(assignment: &str, columns: &Columns) -> Option<String> {
    let code = code_only(assignment);
    let open = code.find('(')?;
    let (targets, after) = balanced_paren(assignment, open);
    let rest = assignment[after..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = match rest.get(..3) {
        Some(w) if w.eq_ignore_ascii_case("row") && rest[3..].trim_start().starts_with('(') => rest[3..].trim_start(),
        _ => rest,
    };
    if !rest.starts_with('(') {
        return None;
    }
    let (values, end) = balanced_paren(rest, 0);
    if !rest[end..].trim().is_empty() {
        return None;
    }
    let targets = split_top_level(&targets);
    let values = split_top_level(&values);
    let first = values.first()?.trim_start();
    if targets.len() != values.len() || first.get(..6).is_some_and(|w| w.eq_ignore_ascii_case("select")) {
        return None;
    }
    let fitted: Vec<String> = targets.iter().zip(&values).map(|(t, v)| columns.wrap(t, v)).collect();
    let targets: Vec<&str> = targets.iter().map(|t| t.trim()).collect();
    Some(format!("({}) = ({})", targets.join(", "), fitted.join(", ")))
}

/// An INSERT as SQLite's grammar takes it. Postgres reads a source in parentheses,
/// `INSERT INTO t (a) (VALUES (1))`, as the query inside them, and SQLite has no such
/// form, so the parentheses go. And SQLite reads the ON of an ON CONFLICT after a
/// SELECT's FROM as a join's, so such a SELECT with no WHERE gets `WHERE true`, as
/// SQLite's documentation of upsert says to.
pub(crate) fn insert_source_for_sqlite(sql: &str) -> String {
    let sql = unwrap_insert_source(sql);
    where_before_conflict(&sql).unwrap_or(sql)
}

fn where_before_conflict(sql: &str) -> Option<String> {
    let src = usize::try_from(parse_insert(sql).2).ok()?;
    let source = &sql[src..];
    let low = code_only(source).to_ascii_lowercase();
    if !word_at(&low, 0, "select") {
        return None;
    }
    let tail = tail_of_source(source, &low);
    if !word_at(&low, tail, "on") {
        return None;
    }
    let body = &source[..tail];
    let has = |w: &str| top_level_keyword(body, w).is_some();
    if !has("from") || has("where") || ["union", "intersect", "except"].iter().any(|w| has(w)) {
        return None;
    }
    let at = ["group", "having", "window", "order", "limit", "offset", "fetch"]
        .iter()
        .filter_map(|w| top_level_keyword(body, w))
        .min()
        .unwrap_or(tail);
    let at = src + at;
    Some([&sql[..at], " WHERE true ", &sql[at..]].concat())
}

fn unwrap_insert_source(sql: &str) -> String {
    let code = blank_literals(sql);
    let Some(into) = top_level_keyword(&code, "into") else {
        return sql.to_string();
    };
    let b = code.as_bytes();
    // Past the target, its alias, and a column list.
    let mut i = skip_name(b, skip_blank(b, into + "into".len()));
    i = skip_blank(b, i);
    if word_at(&code, i, "as") {
        i = skip_blank(b, skip_name(b, skip_blank(b, i + 2)));
    }
    for _ in 0..2 {
        if b.get(i) != Some(&b'(') {
            return sql.to_string();
        }
        let (inner, end) = balanced_paren(&code, i);
        let start = inner.len() - inner.trim_start().len();
        let opens = i + 1 + start;
        if ["select", "values", "with"].iter().any(|w| word_at(&code, opens, w)) {
            let close = end - 1;
            return [&sql[..i], " ", &sql[i + 1..close], " ", &sql[end..]].concat();
        }
        i = skip_blank(b, end);
    }
    sql.to_string()
}

/// Past a name, qualified or quoted, that starts at i.
fn skip_name(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'(' {
        if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
        }
        i += 1;
    }
    i
}

fn skip_blank(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Whether code holds the keyword w, whole, at i.
fn word_at(code: &str, i: usize, w: &str) -> bool {
    let b = code.as_bytes();
    code.get(i..i + w.len()).is_some_and(|s| s.eq_ignore_ascii_case(w))
        && b.get(i + w.len())
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_sources() {
        for (sql, want) in [
            (
                "INSERT INTO t (a, b) (VALUES (1, 'x)')) ON CONFLICT (a) DO NOTHING",
                "INSERT INTO t (a, b)  VALUES (1, 'x)')  ON CONFLICT (a) DO NOTHING",
            ),
            ("INSERT INTO t (SELECT 1)", "INSERT INTO t  SELECT 1 "),
            (
                "INSERT INTO \"s\".\"t\" AS x (a) ( select 1 )",
                "INSERT INTO \"s\".\"t\" AS x (a)   select 1  ",
            ),
            ("INSERT INTO t (a) VALUES (1)", "INSERT INTO t (a) VALUES (1)"),
            ("INSERT INTO t (a) SELECT (1)", "INSERT INTO t (a) SELECT (1)"),
            ("INSERT INTO t DEFAULT VALUES", "INSERT INTO t DEFAULT VALUES"),
            (
                "INSERT INTO t SELECT * FROM unnest($1) ON CONFLICT DO NOTHING",
                "INSERT INTO t SELECT * FROM unnest($1)  WHERE true ON CONFLICT DO NOTHING",
            ),
            (
                "INSERT INTO t SELECT a FROM u ORDER BY a LIMIT 2 ON CONFLICT DO NOTHING",
                "INSERT INTO t SELECT a FROM u  WHERE true ORDER BY a LIMIT 2 ON CONFLICT DO NOTHING",
            ),
            (
                "INSERT INTO t SELECT a FROM u WHERE b ON CONFLICT DO NOTHING",
                "INSERT INTO t SELECT a FROM u WHERE b ON CONFLICT DO NOTHING",
            ),
            ("INSERT INTO t SELECT a FROM u", "INSERT INTO t SELECT a FROM u"),
            (
                "INSERT INTO t SELECT 1 ON CONFLICT DO NOTHING",
                "INSERT INTO t SELECT 1 ON CONFLICT DO NOTHING",
            ),
        ] {
            assert_eq!(insert_source_for_sqlite(sql), want, "{sql}");
        }
    }
}
