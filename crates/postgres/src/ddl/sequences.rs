//! Sequences: CREATE/ALTER/DROP SEQUENCE are intercepted here and translated to
//! the engine's internal _overlite_sequences table; nextval/currval/setval/
//! lastval calls are expanded to concrete integer literals before the SQL runs,
//! so SQLite only ever stores ordinary integers (the file stays usable without
//! overlite). currval/lastval are session-scoped, tracked on the session.

use crate::auth::rls::text_or_empty;
use crate::ddl::truncate::is_truncate;
use crate::error::{Result, errf};
use crate::rewrite::tokens::code_tokens;
use crate::session::Session;
use crate::session::utility::first_word_upper;
use crate::types::to_int64;
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted, split_words, trim_chars, trim_end_chars};
use overlite_core::{Command, SqlState, Value};
use overlite_engine::split_stored_name;

impl Session {
    /// Handles CREATE/ALTER/DROP SEQUENCE and TRUNCATE; None when it is neither.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_sequence_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        // TRUNCATE takes this route too: it is the session's, as it restarts sequences.
        if let Some(r) = self.try_truncate(sql) {
            return Some(r);
        }
        let f = split_words(sql);
        if f.len() < 2 || !eq_fold(f[1], "sequence") {
            return None;
        }
        match f[0].to_uppercase().as_str() {
            "CREATE" => Some(self.create_sequence(&f[2..]).map(|()| Command::from("CREATE SEQUENCE"))),
            "ALTER" => Some(self.alter_sequence(&f[2..]).map(|()| Command::from("ALTER SEQUENCE"))),
            "DROP" => Some(self.drop_sequence(&f[2..]).map(|()| Command::from("DROP SEQUENCE"))),
            _ => None,
        }
    }
}

/// Reports whether sql is a CREATE/ALTER/DROP SEQUENCE statement (used by the
/// extended-query path to route it like other utility DDL).
pub(crate) fn is_sequence_ddl(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::sequence_ddl).unwrap_or_else(|| is_sequence_ddl_text(sql))
}

/// `is_sequence_ddl` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_sequence_ddl_text(sql: &str) -> bool {
    if is_truncate(sql) {
        return true;
    }
    let f = split_words(sql);
    if f.len() < 2 || !eq_fold(f[1], "sequence") {
        return false;
    }
    matches!(f[0].to_uppercase().as_str(), "CREATE" | "ALTER" | "DROP")
}

/// The CREATE/ALTER SEQUENCE settings we track.
#[derive(Clone, Debug, Default)]
struct SeqOptions {
    start: i64,
    increment: i64,
    min_value: i64,
    max_value: i64,
    cache: i64,
    cycle: bool,

    start_set: bool,
    min_set: bool,
    max_set: bool,
}

fn default_seq_options() -> SeqOptions {
    SeqOptions {
        start: 1,
        increment: 1,
        min_value: 1,
        max_value: 9223372036854775807,
        cache: 1,
        ..Default::default()
    }
}

impl Session {
    pub(crate) fn create_sequence(&mut self, rest: &[&str]) -> Result<()> {
        let mut rest = rest;
        let mut if_not_exists = false;
        if rest.len() >= 3 && eq_fold(rest[0], "if") && eq_fold(rest[1], "not") && eq_fold(rest[2], "exists") {
            if_not_exists = true;
            rest = &rest[3..];
        }
        if rest.is_empty() {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in CREATE SEQUENCE"));
        }
        let name = seq_ref(rest[0]);
        let mut opts = default_seq_options();
        parse_seq_options(&rest[1..], &mut opts)?;
        // A descending sequence with no explicit MINVALUE defaults to a very low min.
        if opts.increment < 0 && !opts.min_set {
            opts.min_value = -9223372036854775808;
        }
        if opts.increment < 0 && !opts.max_set {
            opts.max_value = -1;
        }
        if opts.increment < 0 && !opts.start_set {
            opts.start = opts.max_value;
        }

        let mut verb = "INSERT";
        if if_not_exists {
            verb = "INSERT OR IGNORE";
        }
        let sql = verb.to_string()
            + " INTO _overlite_sequences"
            + " (seqname, last_value, increment, min_value, max_value, start_value, cache_size, is_cycled, is_called)"
            + " VALUES ("
            + &quote_literal(&name)
            + ", "
            + &int_text(opts.start)
            + ", "
            + &int_text(opts.increment)
            + ", "
            + &int_text(opts.min_value)
            + ", "
            + &int_text(opts.max_value)
            + ", "
            + &int_text(opts.start)
            + ", "
            + &int_text(opts.cache)
            + ", "
            + b2i(opts.cycle)
            + ", 0)";
        self.exec(&sql, &[]).map_err(|e| match e.sql_state() {
            SqlState::UNIQUE_VIOLATION => errf!(SqlState::DUPLICATE_TABLE, "relation \"{name}\" already exists"),
            _ => e,
        })?;
        Ok(())
    }

    fn alter_sequence(&mut self, rest: &[&str]) -> Result<()> {
        if rest.is_empty() {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in ALTER SEQUENCE"));
        }
        let name = seq_ref(rest[0]);
        let rest = &rest[1..];

        // ALTER SEQUENCE x OWNED BY / OWNER TO ... -- ownership, accepted as a no-op
        // (pg_dump emits these for serial-owned sequences).
        if !rest.is_empty() && (eq_fold(rest[0], "owned") || eq_fold(rest[0], "owner")) {
            return Ok(());
        }

        // ALTER SEQUENCE x RESTART [WITH n] resets the counter (start unless given).
        if !rest.is_empty() && eq_fold(rest[0], "restart") {
            let mut val = "";
            if rest.len() >= 2 && eq_fold(rest[1], "with") && rest.len() >= 3 {
                val = rest[2];
            } else if rest.len() >= 2 {
                val = rest[1];
            }
            if let Some(n) = parse_int(val) {
                self.exec(
                    &("UPDATE _overlite_sequences SET last_value = ".to_string()
                        + &int_text(n)
                        + ", is_called = 0 WHERE seqname = "
                        + &quote_literal(&name)),
                    &[],
                )?;
                return Ok(());
            }
            self.exec(
                &("UPDATE _overlite_sequences SET last_value = start_value,".to_string()
                    + " is_called = 0 WHERE seqname = "
                    + &quote_literal(&name)),
                &[],
            )?;
            return Ok(());
        }

        // Otherwise apply whatever options were named (INCREMENT/MIN/MAX/CYCLE).
        let mut opts = SeqOptions::default();
        parse_seq_options(rest, &mut opts)?;
        let mut set: Vec<String> = Vec::new();
        if opts.increment != 0 {
            set.push("increment = ".to_string() + &int_text(opts.increment));
        }
        if opts.min_set {
            set.push("min_value = ".to_string() + &int_text(opts.min_value));
        }
        if opts.max_set {
            set.push("max_value = ".to_string() + &int_text(opts.max_value));
        }
        set.push("is_cycled = ".to_string() + b2i(opts.cycle));
        if set.is_empty() {
            return Ok(());
        }
        self.exec(
            &("UPDATE _overlite_sequences SET ".to_string()
                + &set.join(", ")
                + " WHERE seqname = "
                + &quote_literal(&name)),
            &[],
        )?;
        Ok(())
    }

    fn drop_sequence(&mut self, rest: &[&str]) -> Result<()> {
        let mut rest = rest;
        let if_exists = rest.len() >= 2 && eq_fold(rest[0], "if") && eq_fold(rest[1], "exists");
        if if_exists {
            rest = &rest[2..];
        }
        for name in rest.join(" ").split(',') {
            let name = seq_ref(trim_end_chars(name, ";").trim());
            if name.is_empty() || eq_fold(&name, "cascade") || eq_fold(&name, "restrict") {
                continue;
            }
            match self.require_sequence(&name) {
                Err(_) if if_exists => {
                    self.c
                        .send_notice(&format!("sequence \"{name}\" does not exist, skipping"))?;
                    continue;
                }
                r => r?,
            }
            self.exec(
                &("DELETE FROM _overlite_sequences WHERE seqname = ".to_string() + &quote_literal(&name)),
                &[],
            )?;
            self.seq_curr.remove(&name.to_lowercase());
        }
        Ok(())
    }
}

/// Reads the option keywords shared by CREATE and ALTER SEQUENCE.
fn parse_seq_options(tokens: &[&str], o: &mut SeqOptions) -> Result<()> {
    let mut i: usize = 0;
    while i < tokens.len() {
        match trim_end_chars(tokens[i], ";").to_uppercase().as_str() {
            "START" => {
                if i + 1 < tokens.len() && eq_fold(tokens[i + 1], "with") {
                    i += 1;
                }
                if let Some(n) = next_int(tokens, &mut i) {
                    o.start = n;
                    o.start_set = true;
                }
            }
            "INCREMENT" => {
                if i + 1 < tokens.len() && eq_fold(tokens[i + 1], "by") {
                    i += 1;
                }
                if let Some(n) = next_int(tokens, &mut i) {
                    o.increment = n;
                }
            }
            "MINVALUE" => {
                if let Some(n) = next_int(tokens, &mut i) {
                    o.min_value = n;
                    o.min_set = true;
                }
            }
            "MAXVALUE" => {
                if let Some(n) = next_int(tokens, &mut i) {
                    o.max_value = n;
                    o.max_set = true;
                }
            }
            "CACHE" => {
                if let Some(n) = next_int(tokens, &mut i) {
                    o.cache = n;
                }
            }
            "CYCLE" => o.cycle = true,
            "NO" => {
                // NO MINVALUE / NO MAXVALUE / NO CYCLE -- keep defaults / clear cycle.
                if i + 1 < tokens.len() && eq_fold(tokens[i + 1], "cycle") {
                    o.cycle = false;
                }
                i += 1;
            }
            "AS" | "OWNED" => i += 1, // AS <type> / OWNED BY <col> -- accepted, ignored
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Reads the integer token following index *i, advancing *i past it.
fn next_int(tokens: &[&str], i: &mut usize) -> Option<i64> {
    if *i + 1 >= tokens.len() {
        return None;
    }
    if let Some(n) = parse_int(tokens[*i + 1]) {
        *i += 1;
        return Some(n);
    }
    None
}

// --- nextval/currval/setval/lastval expansion --------------------------------

impl Session {
    /// Replaces every nextval/currval/setval/lastval call in sql with the integer
    /// literal it evaluates to (advancing sequences as a side effect). Skipped for
    /// DDL so a nextval() never lands in a stored schema/default.
    pub(crate) fn expand_sequences(&mut self, sql: &str) -> Result<String> {
        if !has_seq_func(sql) {
            return Ok(sql.to_string());
        }
        match first_word_upper(sql).as_str() {
            "CREATE" | "ALTER" | "DROP" | "TRUNCATE" => return Ok(sql.to_string()),
            _ => {}
        }
        // The sequence a setval names is often this call's answer, so it goes first.
        let sql = scan_func_calls(
            sql,
            |f| f == "pg_get_serial_sequence",
            |_, args| self.serial_sequence_of(args),
        )?;
        if let Some(rows) = self.setval_over_rows(&sql)? {
            return Ok(rows);
        }
        scan_func_calls(&sql, is_seq_func, |f, args| self.eval_seq_func(f, args))
    }

    /// pg_get_serial_sequence(table, column) as a literal: the sequence of a serial
    /// or identity column, schema-qualified, or NULL for any other column.
    fn serial_sequence_of(&mut self, args: &str) -> Result<String> {
        let parts = split_top_level(args);
        let literal = |a: &str| {
            let a = a.trim();
            (a.len() >= 2 && a.starts_with('\'') && a.ends_with('\'')).then(|| a[1..a.len() - 1].replace("''", "'"))
        };
        let (Some(table), Some(column)) = (
            parts.first().and_then(|a| literal(a)),
            parts.get(1).and_then(|a| literal(a)),
        ) else {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "pg_get_serial_sequence takes a table and a column, as literals"
            ));
        };
        let stored = self.stored_name(&table);
        let (_, relation) = split_stored_name(&stored);
        let q = "SELECT name FROM main.sqlite_master WHERE type = 'table' AND lower(name) = lower(".to_string()
            + &quote_literal(&stored)
            + ")";
        if self.exec(&q, &[])?.rows.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(&table)
            ));
        }
        let q = "SELECT name FROM pragma_table_info(".to_string()
            + &quote_literal(&stored)
            + ") WHERE name = "
            + &quote_literal(&column);
        if self.exec(&q, &[])?.rows.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_COLUMN,
                "column \"{column}\" of relation \"{relation}\" does not exist"
            ));
        }
        let q = "SELECT schemaname, seqname FROM _overlite_serial_sequences WHERE lower(tablename) = lower("
            .to_string()
            + &quote_literal(&stored)
            + ") AND seqname = "
            + &quote_literal(&(relation.clone() + "_" + &column + "_seq"));
        if let Some(row) = self.exec(&q, &[])?.rows.first() {
            return Ok(quote_literal(
                &(quote_seq_name(&text_or_empty(&row[0])) + "." + &quote_seq_name(&text_or_empty(&row[1]))),
            ));
        }
        // An identity column that is not a rowid has a sequence of its own, and so has a
        // serial column that is not the key: its default is its own sequence's nextval.
        let seq = relation.clone() + "_" + &column + "_seq";
        let own_default = !self
            .exec(
                &("SELECT 1 FROM main.sqlite_master WHERE type = 'table' AND lower(name) = lower(".to_string()
                    + &quote_literal(&stored)
                    + ") AND instr(overlite_column_def(sql, "
                    + &quote_literal(&column)
                    + "), '/*overlite owned sequence*/')"),
                &[],
            )?
            .rows
            .is_empty();
        if own_default || self.identity_of(&stored, &column).is_some() {
            let q = "SELECT seqname FROM _overlite_sequences WHERE seqname = ".to_string() + &quote_literal(&seq);
            if !self.exec(&q, &[])?.rows.is_empty() {
                return Ok(quote_literal(&("public.".to_string() + &quote_seq_name(&seq))));
            }
        }
        Ok("NULL".to_string())
    }

    /// `SELECT setval(seq, expr, expr) FROM ...`, Django's reset of a sequence after
    /// a load, whose value is read from the rows: the arguments are read first, and
    /// each row's setval is the answer. None for any other statement, and for one
    /// whose arguments are already literals.
    fn setval_over_rows(&mut self, sql: &str) -> Result<Option<String>> {
        let toks = code_tokens(sql);
        let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        if !(word(0, "select") && word(1, "setval") && toks.get(2).is_some_and(|t| t.is_punct(sql, "("))) {
            return Ok(None);
        }
        let Some(close) = (3..toks.len()).find(|&i| toks[i].depth == 0 && toks[i].is_punct(sql, ")")) else {
            return Ok(None);
        };
        let mut from = close + 1;
        if word(from, "as") {
            from += 2;
        }
        if !word(from, "from") || sql.contains('$') {
            return Ok(None);
        }
        let args = split_top_level(&sql[toks[2].span.end..toks[close].span.start]);
        let constant = |a: &str| {
            let a = a.trim();
            a.starts_with('\'') || parse_int(a).is_some() || ["null", "true", "false"].iter().any(|w| eq_fold(a, w))
        };
        if args.len() < 2 || args.len() > 3 || args.iter().all(|a| constant(a)) {
            return Ok(None);
        }
        let probe = "SELECT ".to_string() + &args.join(", ") + " " + &sql[toks[from].span.start..];
        let rows = self.exec(&self.rewrite_typed(&probe), &[])?.rows;
        let mut answers = Vec::with_capacity(rows.len());
        for row in &rows {
            let (Some(name), Some(value)) = (row.first(), row.get(1)) else {
                continue;
            };
            if name.is_null() || value.is_null() {
                answers.push("NULL".to_string());
                continue;
            }
            let called = row
                .get(2)
                .is_none_or(|c| !matches!(c, Value::Int(0) | Value::Bool(false)));
            let name = seq_ref(&("'".to_string() + &name.to_string().replace('\'', "''") + "'"));
            answers.push(self.set_sequence(&name, to_int64(value), called)?);
        }
        if answers.is_empty() {
            return Ok(Some("SELECT NULL WHERE 0".to_string()));
        }
        Ok(Some("SELECT ".to_string() + &answers.join(" UNION ALL SELECT ")))
    }
}

/// Rewrites sequence calls to a constant 0 without advancing anything. Used for
/// Describe, where SQLite must parse the statement for its column types but must
/// not execute the sequence side effects.
pub(crate) fn neutralize_sequences(sql: &str) -> String {
    if !has_seq_func(sql) {
        return sql.to_string();
    }
    let neutral = |f: &str| f == "pg_get_serial_sequence" || is_seq_func(f);
    scan_func_calls(sql, neutral, |f, _| {
        Ok(if f == "pg_get_serial_sequence" { "''" } else { "0" }.to_string())
    })
    .unwrap_or_default()
}

/// Walks sql and replaces each call of a function `matches` accepts
/// (nextval/currval/setval/lastval, the advisory locks) with replace(fn, args),
/// respecting single-quoted strings so it never rewrites text that merely
/// contains the word.
pub(crate) fn scan_func_calls(
    sql: &str,
    matches: impl Fn(&str) -> bool,
    mut replace: impl FnMut(&str, &str) -> Result<String>,
) -> Result<String> {
    let s = sql.as_bytes();
    let mut b = String::with_capacity(sql.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' {
            let j = skip_string(sql, i);
            b.push_str(&sql[i..j]);
            i = j;
            continue;
        }
        if is_ident_start(c) {
            let mut j = i;
            while j < s.len() && is_ident_part(s[j]) {
                j += 1;
            }
            let word = &sql[i..j];
            let f = word.to_lowercase();
            let mut k = j;
            while k < s.len() && (s[k] == b' ' || s[k] == b'\t' || s[k] == b'\n' || s[k] == b'\r') {
                k += 1;
            }
            if matches(&f)
                && k < s.len()
                && s[k] == b'('
                && let Some((args, end)) = read_paren_args(sql, k)
            {
                let lit = replace(&f, &args)?;
                b.push_str(&lit);
                i = end;
                continue;
            }
            b.push_str(word);
            i = j;
            continue;
        }
        // Copy the whole char: a multi-byte one is never quote or identifier.
        let Some(ch) = sql[i..].chars().next() else { break };
        b.push(ch);
        i += ch.len_utf8();
    }
    Ok(b)
}

impl Session {
    fn eval_seq_func(&mut self, f: &str, args: &str) -> Result<String> {
        match f {
            "nextval" => self.nextval(&seq_ref(args)),
            "currval" => self.currval(&seq_ref(args)),
            "lastval" => self.lastval(),
            "setval" => self.setval(args),
            _ => Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "unknown sequence function {:?}",
                quoted(f)
            )),
        }
    }

    fn nextval(&mut self, name: &str) -> Result<String> {
        if let Some(table) = self.serial_table(name)? {
            return self.serial_nextval(name, &table);
        }
        let rs = self.exec(
            &("SELECT last_value, increment, min_value, max_value, is_cycled, is_called".to_string()
                + " FROM _overlite_sequences WHERE seqname = "
                + &quote_literal(name)),
            &[],
        )?;
        if rs.rows.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(name)
            ));
        }
        let row = &rs.rows[0];
        let last = to_int64(&row[0]);
        let incr = to_int64(&row[1]);
        let min_v = to_int64(&row[2]);
        let max_v = to_int64(&row[3]);
        let cycled = to_int64(&row[4]) != 0;
        let called = to_int64(&row[5]) != 0;

        let mut next = last;
        if called {
            next = last.wrapping_add(incr);
            if incr > 0 && next > max_v {
                if !cycled {
                    return Err(errf!(
                        SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                        "nextval: reached maximum value of sequence {:?} ({})",
                        quoted(name),
                        max_v
                    ));
                }
                next = min_v;
            } else if incr < 0 && next < min_v {
                if !cycled {
                    return Err(errf!(
                        SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                        "nextval: reached minimum value of sequence {:?} ({})",
                        quoted(name),
                        min_v
                    ));
                }
                next = max_v;
            }
        }
        self.exec(
            &("UPDATE _overlite_sequences SET last_value = ".to_string()
                + &int_text(next)
                + ", is_called = 1 WHERE seqname = "
                + &quote_literal(name)),
            &[],
        )?;
        self.remember_seq(name, next);
        Ok(int_text(next))
    }

    /// A sequence function's name that names nothing is Postgres' 42P01.
    fn require_sequence(&mut self, name: &str) -> Result<()> {
        if self.serial_table(name)?.is_some() {
            return Ok(());
        }
        let q = "SELECT 1 FROM _overlite_sequences WHERE seqname = ".to_string() + &quote_literal(name);
        if self.exec(&q, &[])?.rows.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(name)
            ));
        }
        Ok(())
    }

    fn currval(&mut self, name: &str) -> Result<String> {
        if let Some(v) = self.seq_curr.get(&name.to_lowercase()) {
            return Ok(int_text(*v));
        }
        self.require_sequence(name)?;
        Err(errf!(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "currval of sequence {:?} is not yet defined in this session",
            quoted(name)
        ))
    }

    fn lastval(&mut self) -> Result<String> {
        if self.seq_last.is_empty() {
            return Err(errf!(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "lastval is not yet defined in this session"
            ));
        }
        Ok(int_text(self.seq_curr.get(&self.seq_last).copied().unwrap_or(0)))
    }

    fn setval(&mut self, args: &str) -> Result<String> {
        let parts = split_top_level(args);
        if parts.len() < 2 {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "setval requires (sequence, value[, is_called])"
            ));
        }
        let name = seq_ref(&parts[0]);
        if parts[0].trim().eq_ignore_ascii_case("null") || parts[1].trim().eq_ignore_ascii_case("null") {
            return Ok("NULL".to_string()); // strict, as in Postgres
        }
        let Some(val) = parse_int(parts[1].trim()) else {
            return Err(errf!(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "setval: invalid value {:?}",
                quoted(parts[1].trim())
            ));
        };
        let mut is_called = true;
        if parts.len() >= 3 {
            is_called = parse_seq_bool(parts[2].trim());
        }
        self.set_sequence(&name, val, is_called)
    }

    /// setval of a sequence by its name, val as a literal.
    fn set_sequence(&mut self, name: &str, val: i64, is_called: bool) -> Result<String> {
        if let Some(table) = self.serial_table(name)? {
            // SQLite's next rowid is past both sqlite_sequence and the highest row.
            let seq = if is_called { val } else { val - 1 };
            self.set_sqlite_sequence(&table, seq)?;
            if is_called {
                self.remember_seq(name, val);
            }
            return Ok(int_text(val));
        }
        self.require_sequence(name)?;
        let rs = self.exec(
            &("UPDATE _overlite_sequences SET last_value = ".to_string()
                + &int_text(val)
                + ", is_called = "
                + b2i(is_called)
                + " WHERE seqname = "
                + &quote_literal(name)),
            &[],
        )?;
        if rs.rows_affected == 0 {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(name)
            ));
        }
        if is_called {
            self.remember_seq(name, val);
        }
        Ok(int_text(val))
    }

    /// The table of a serial key's sequence, which is SQLite's AUTOINCREMENT, when
    /// name is one and no sequence of that name is.
    fn serial_table(&mut self, name: &str) -> Result<Option<String>> {
        let q = "SELECT tablename FROM _overlite_serial_sequences WHERE lower(seqname) = lower(".to_string()
            + &quote_literal(name)
            + ")";
        Ok(self.exec(&q, &[])?.rows.first().map(|row| text_or_empty(&row[0])))
    }

    /// Where table's AUTOINCREMENT goes on from.
    fn set_sqlite_sequence(&mut self, table: &str, seq: i64) -> Result<()> {
        let t = quote_literal(table);
        let rs = self.exec(
            &("UPDATE main.sqlite_sequence SET seq = ".to_string() + &int_text(seq) + " WHERE name = " + &t),
            &[],
        )?;
        if rs.rows_affected == 0 {
            self.exec(
                &("INSERT INTO main.sqlite_sequence (name, seq) VALUES (".to_string()
                    + &t
                    + ", "
                    + &int_text(seq)
                    + ")"),
                &[],
            )?;
        }
        Ok(())
    }

    /// nextval of a serial key's sequence: the id the next row would get.
    fn serial_nextval(&mut self, name: &str, table: &str) -> Result<String> {
        let q = "SELECT max(COALESCE((SELECT seq FROM main.sqlite_sequence WHERE name = ".to_string()
            + &quote_literal(table)
            + "), 0), COALESCE((SELECT max(rowid) FROM "
            + &quote_ident(table)
            + "), 0)) + 1";
        let next = self.exec(&q, &[])?.rows.first().map_or(1, |row| to_int64(&row[0]));
        self.set_sqlite_sequence(table, next)?;
        self.remember_seq(name, next);
        Ok(int_text(next))
    }

    fn remember_seq(&mut self, name: &str, v: i64) {
        let key = name.to_lowercase();
        self.seq_curr.insert(key.clone(), v);
        self.seq_last = key;
    }
}

// --- small parsing helpers ----------------------------------------------------

pub(crate) fn has_seq_func(sql: &str) -> bool {
    let l = sql.to_lowercase();
    l.contains("nextval")
        || l.contains("currval")
        || l.contains("setval")
        || l.contains("lastval")
        || l.contains("pg_get_serial_sequence")
}

fn is_seq_func(f: &str) -> bool {
    f == "nextval" || f == "currval" || f == "setval" || f == "lastval"
}

pub(crate) fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_lowercase() || c.is_ascii_uppercase()
}

pub(crate) fn is_ident_part(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

/// The index just past the single-quoted string starting at i.
pub(crate) fn skip_string(sql: &str, i: usize) -> usize {
    let s = sql.as_bytes();
    let mut j = i + 1;
    while j < s.len() {
        if s[j] == b'\'' {
            if j + 1 < s.len() && s[j + 1] == b'\'' {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    j
}

/// Reads a parenthesized argument list starting at the '(' at open, honoring
/// nested parens and quoted strings: the inner text and the index just past ')'.
pub(crate) fn read_paren_args(sql: &str, open: usize) -> Option<(String, usize)> {
    let s = sql.as_bytes();
    let mut depth = 0;
    let mut i = open;
    let start = open + 1;
    while i < s.len() {
        match s[i] {
            b'\'' => {
                i = skip_string(sql, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((sql[start..i].to_string(), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Extracts the sequence name from a nextval/currval argument, stripping quotes,
/// a ::regclass/::text cast, and any schema qualifier.
fn seq_ref(arg: &str) -> String {
    let mut a = arg.trim();
    if let Some(i) = a.find("::") {
        a = a[..i].trim();
    }
    a = trim_chars(a, "'").trim();
    a = trim_chars(a, "\"");
    if let Some(i) = a.rfind('.') {
        a = &a[i + 1..];
    }
    a.to_string()
}

/// Splits on commas that are not inside quotes, parentheses or brackets.
pub(crate) fn split_top_level(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => i = skip_string(s, i) - 1,
            b'"' => i = s[i + 1..].find('"').map_or(b.len(), |j| i + j + 1),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].to_string());
    out
}

pub(crate) fn parse_int(s: &str) -> Option<i64> {
    trim_end_chars(s, ";").trim().parse::<i64>().ok()
}

/// A boolean sequence option: anything but a spelled-out false is true, which is
/// looser than `overlite_core::parse_bool`.
fn parse_seq_bool(s: &str) -> bool {
    !matches!(
        s.trim().to_lowercase().as_str(),
        "false" | "f" | "0" | "'false'" | "'f'"
    )
}

pub(crate) fn int_text(v: i64) -> String {
    v.to_string()
}

fn b2i(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

/// A sequence's name part as pg_get_serial_sequence writes it: bare when it reads
/// back the same. Unlike `quote_ident_if_needed` in the engine it keeps a `$` bare.
fn quote_seq_name(s: &str) -> String {
    let bare = s.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'$');
    if bare { s.to_string() } else { quote_ident(s) }
}
