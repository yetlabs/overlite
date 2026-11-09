//! DECLARE, FETCH, MOVE and CLOSE.
//!
//! A cursor reads its query from SQLite as it is fetched: the engine keeps the
//! statement prepared, and a FETCH steps it as far as it asks, so a cursor read forward
//! holds no more rows than a FETCH returns. Going back steps the statement again from
//! its start. A cursor WITH HOLD is read whole when the transaction that declared it
//! commits, as Postgres does, and from then on fetches from those rows: from all of
//! them for a cursor that scrolls, from the rest for a NO SCROLL one.
//!
//! pg_cursors is a table of the connection's that the session keeps in step.

use crate::error::{Error, Result, errf};
use crate::session::Session;
use crate::types::{Format, oid_for_column};
use overlite_core::{Column, Oid, SqlState, Value};

/// What SCROLL said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scroll {
    Yes,
    No,
    /// Neither: Postgres scrolls when the plan can, which here it always can.
    Unsaid,
}

/// A count of FETCH or MOVE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Count {
    N(i64),
    All,
}

/// Where a FETCH or MOVE goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Forward(Count),
    Backward(Count),
    Absolute(i64),
    Relative(i64),
}

/// A cursor statement, read.
#[derive(Debug, PartialEq, Eq)]
enum Statement {
    Declare {
        name: String,
        binary: bool,
        scroll: Scroll,
        hold: bool,
        query: String,
    },
    Fetch {
        name: String,
        direction: Direction,
        moving: bool,
    },
    Close(Option<String>),
}

/// Where a cursor's rows come from.
#[derive(Debug)]
enum Source {
    /// The engine's open query, of which `read` rows have been stepped past.
    Stream { id: u64, read: i64 },
    /// The rows after the first `base`, read when its transaction committed.
    Held { rows: Vec<Vec<Value>>, base: i64 },
}

#[derive(Debug)]
pub(crate) struct Cursor {
    name: String,
    statement: String,
    hold: bool,
    binary: bool,
    scroll: Scroll,
    columns: Vec<Column>,
    oids: Vec<Oid>,
    source: Source,
    /// 0 before the first row, k on row k, and total + 1 past the last.
    pos: i64,
    /// The number of rows, once the end has been seen.
    total: Option<i64>,
    /// Declared in the open transaction, so a rollback takes it.
    fresh: bool,
    created: String,
}

/// The cursors of a session.
#[derive(Debug, Default)]
pub(crate) struct Cursors {
    by_name: Vec<Cursor>,
}

impl Cursors {
    pub(crate) fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    fn index(&self, name: &str) -> Option<usize> {
        self.by_name.iter().position(|c| c.name == name)
    }
}

/// Whether a statement is one of a cursor's.
pub(crate) fn is_cursor_statement(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::cursor).unwrap_or_else(|| is_cursor_statement_text(sql))
}

/// `is_cursor_statement` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_cursor_statement_text(sql: &str) -> bool {
    let w = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["declare", "fetch", "move", "close"]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k))
}

/// The message Postgres gives a NO SCROLL cursor asked to go back, and its hint.
const FORWARD_ONLY: &str = "cursor can only scan forward";
const FORWARD_ONLY_HINT: &str = "Declare it with SCROLL option to enable backward scan.";

/// A word or a quoted name of a cursor statement.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Quoted(String),
    Number(i64),
}

impl Tok {
    fn is(&self, w: &str) -> bool {
        matches!(self, Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }
}

fn syntax_error(near: &str) -> Error {
    errf!(SqlState::SYNTAX_ERROR, "syntax error at or near \"{near}\"")
}

/// Splits the head of a statement into words, quoted names and signed integers, up to
/// `stop` words (the FOR of a DECLARE), and answers where it stopped.
fn tokens(sql: &str, stop: Option<&str>) -> Result<(Vec<Tok>, usize)> {
    let b = sql.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'"' {
            let mut name = String::new();
            let mut j = i + 1;
            loop {
                match sql[j..].find('"') {
                    None => return Err(syntax_error(&sql[i..])),
                    Some(k) => {
                        name.push_str(&sql[j..j + k]);
                        j += k + 1;
                        if b.get(j) == Some(&b'"') {
                            name.push('"');
                            j += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            toks.push(Tok::Quoted(name));
            i = j;
        } else if c.is_ascii_digit() || ((c == b'-' || c == b'+') && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let n = sql[i..j].parse().map_err(|_| syntax_error(&sql[i..j]))?;
            toks.push(Tok::Number(n));
            i = j;
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$' || b[j] >= 0x80) {
                j += 1;
            }
            let w = &sql[i..j];
            if stop.is_some_and(|s| w.eq_ignore_ascii_case(s)) {
                return Ok((toks, i));
            }
            toks.push(Tok::Word(w.to_string()));
            i = j;
        } else {
            let end = sql[i..].find(char::is_whitespace).map_or(sql.len(), |k| i + k);
            return Err(syntax_error(&sql[i..end]));
        }
    }
    Ok((toks, sql.len()))
}

/// A name as Postgres reads it: folded when bare, whole when quoted.
fn name_of(t: &Tok) -> Option<String> {
    match t {
        Tok::Word(w) => Some(w.to_lowercase()),
        Tok::Quoted(q) => Some(q.clone()),
        Tok::Number(_) => None,
    }
}

fn near(t: Option<&Tok>) -> String {
    match t {
        None => String::new(),
        Some(Tok::Word(w) | Tok::Quoted(w)) => w.clone(),
        Some(Tok::Number(n)) => n.to_string(),
    }
}

fn parse(sql: &str) -> Result<Statement> {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    let lead = sql.len() - sql.trim_start().len() + first.len();
    let rest = &sql[lead..];
    match first.to_ascii_lowercase().as_str() {
        "declare" => parse_declare(rest),
        "close" => {
            let (toks, _) = tokens(rest, None)?;
            match toks.as_slice() {
                [t] if t.is("all") => Ok(Statement::Close(None)),
                [t] => name_of(t)
                    .map(|n| Statement::Close(Some(n)))
                    .ok_or_else(|| syntax_error(&near(Some(t)))),
                other => Err(syntax_error(&near(other.get(1).or(other.first())))),
            }
        }
        moving => parse_fetch(rest, moving == "move"),
    }
}

fn parse_declare(rest: &str) -> Result<Statement> {
    let (toks, at) = tokens(rest, Some("for"))?;
    if at >= rest.len() {
        return Err(syntax_error(""));
    }
    let query = rest[at + 3..].trim().to_string();
    let mut it = toks.iter().peekable();
    let name = it.next().and_then(name_of).ok_or_else(|| syntax_error("FOR"))?;
    let (mut binary, mut scroll, mut hold) = (false, Scroll::Unsaid, false);
    let mut cursor = false;
    while let Some(t) = it.next() {
        if t.is("binary") && !cursor {
            binary = true;
        } else if (t.is("insensitive") || t.is("asensitive")) && !cursor {
        } else if t.is("scroll") && !cursor {
            scroll = Scroll::Yes;
        } else if t.is("no") && !cursor && it.peek().is_some_and(|n| n.is("scroll")) {
            it.next();
            scroll = Scroll::No;
        } else if t.is("cursor") && !cursor {
            cursor = true;
        } else if (t.is("with") || t.is("without")) && cursor && it.peek().is_some_and(|n| n.is("hold")) {
            hold = t.is("with");
            it.next();
        } else {
            return Err(syntax_error(&near(Some(t))));
        }
    }
    if !cursor {
        return Err(syntax_error("FOR"));
    }
    Ok(Statement::Declare {
        name,
        binary,
        scroll,
        hold,
        query,
    })
}

fn parse_fetch(rest: &str, moving: bool) -> Result<Statement> {
    let (toks, _) = tokens(rest, None)?;
    let mut t = toks.as_slice();
    // The cursor's name is the last token, after an optional FROM or IN.
    let Some((last, head)) = t.split_last() else {
        return Err(syntax_error(""));
    };
    let name = name_of(last).ok_or_else(|| syntax_error(&near(Some(last))))?;
    t = head;
    if let Some((x, h)) = t.split_last()
        && (x.is("from") || x.is("in"))
    {
        t = h;
    }
    let count = |x: Option<&Tok>| -> Result<Count> {
        match x {
            None => Ok(Count::N(1)),
            Some(Tok::Number(n)) => Ok(Count::N(*n)),
            Some(w) if w.is("all") => Ok(Count::All),
            Some(other) => Err(syntax_error(&near(Some(other)))),
        }
    };
    let number = |x: Option<&Tok>| -> Result<i64> {
        match x {
            Some(Tok::Number(n)) => Ok(*n),
            other => Err(syntax_error(&near(other))),
        }
    };
    let direction = match t {
        [] => Direction::Forward(Count::N(1)),
        [Tok::Number(n)] => Direction::Forward(Count::N(*n)),
        [w] if w.is("next") => Direction::Forward(Count::N(1)),
        [w] if w.is("prior") => Direction::Backward(Count::N(1)),
        [w] if w.is("first") => Direction::Absolute(1),
        [w] if w.is("last") => Direction::Absolute(-1),
        [w] if w.is("all") => Direction::Forward(Count::All),
        [w, rest @ ..] if w.is("forward") && rest.len() <= 1 => Direction::Forward(count(rest.first())?),
        [w, rest @ ..] if w.is("backward") && rest.len() <= 1 => Direction::Backward(count(rest.first())?),
        [w, n] if w.is("absolute") => Direction::Absolute(number(Some(n))?),
        [w, n] if w.is("relative") => Direction::Relative(number(Some(n))?),
        other => return Err(syntax_error(&near(other.first()))),
    };
    Ok(Statement::Fetch {
        name,
        direction,
        moving,
    })
}

/// What a cursor statement gave: rows to send, with their columns, and a tag.
pub(crate) struct Outcome {
    pub(crate) rows: Option<Fetched>,
    pub(crate) tag: String,
}

/// The rows of a FETCH: its columns, their types, the rows, and whether they go out in
/// binary.
pub(crate) struct Fetched {
    pub(crate) columns: Vec<Column>,
    pub(crate) oids: Vec<Oid>,
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) binary: bool,
}

impl Session {
    /// Runs a cursor statement, simple protocol: its rows and tag go out here.
    pub(crate) fn simple_cursor_statement(&mut self, sql: &str) -> Result<()> {
        match self.cursor_statement(sql, &[]) {
            Ok(out) => {
                if let Some(f) = &out.rows {
                    let formats = if f.binary { vec![Format::Binary] } else { Vec::new() };
                    self.c.send_row_description_formats(&f.columns, &f.oids, &formats)?;
                    if let Some(why) = self.c.send_data_rows(&f.rows, &f.oids, &formats)? {
                        self.mark_tx_failed();
                        return self
                            .c
                            .send_error(SqlState::INVALID_BINARY_REPRESENTATION, &why.to_string());
                    }
                }
                self.c.send_command_complete(&out.tag)
            }
            Err(e) => {
                self.mark_tx_failed();
                self.send_cursor_error(&e)
            }
        }
    }

    /// An error of a cursor statement, with Postgres' hint where it gives one.
    pub(crate) fn send_cursor_error(&mut self, e: &Error) -> Result<()> {
        if e.message() == FORWARD_ONLY {
            return self.c.send_error_hint(e.sql_state(), FORWARD_ONLY, FORWARD_ONLY_HINT);
        }
        self.send_exec_error(e)
    }

    /// Runs a cursor statement with its parameters.
    pub(crate) fn cursor_statement(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        let stmt = parse(sql)?;
        match stmt {
            Statement::Declare {
                name,
                binary,
                scroll,
                hold,
                query,
            } => {
                self.declare(name, sql, binary, scroll, hold, &query, params)?;
                Ok(Outcome {
                    rows: None,
                    tag: "DECLARE CURSOR".into(),
                })
            }
            Statement::Fetch {
                name,
                direction,
                moving,
            } => {
                let i = self.cursor_index(&name)?;
                let (rows, n) = self.fetch(i, direction, !moving)?;
                if moving {
                    return Ok(Outcome {
                        rows: None,
                        tag: format!("MOVE {n}"),
                    });
                }
                let c = &self.cursors.by_name[i];
                Ok(Outcome {
                    rows: Some(Fetched {
                        columns: c.columns.clone(),
                        oids: c.oids.clone(),
                        rows,
                        binary: c.binary,
                    }),
                    tag: format!("FETCH {n}"),
                })
            }
            Statement::Close(name) => {
                let tag = if let Some(name) = name {
                    let i = self.cursor_index(&name)?;
                    let c = self.cursors.by_name.remove(i);
                    self.drop_source(&c);
                    "CLOSE CURSOR"
                } else {
                    for c in std::mem::take(&mut self.cursors.by_name) {
                        self.drop_source(&c);
                    }
                    "CLOSE CURSOR ALL"
                };
                self.list_cursors();
                Ok(Outcome {
                    rows: None,
                    tag: tag.into(),
                })
            }
        }
    }

    /// The columns of a cursor, for a Describe of it as a portal or of a FETCH from it.
    pub(crate) fn cursor_description(&self, sql_or_name: &str, is_name: bool) -> Option<(Vec<Column>, Vec<Oid>)> {
        let name = if is_name {
            sql_or_name.to_string()
        } else {
            match parse(sql_or_name).ok()? {
                Statement::Fetch {
                    name, moving: false, ..
                } => name,
                _ => return None,
            }
        };
        let c = self.cursors.by_name.iter().find(|c| c.name == name)?;
        Some((c.columns.clone(), c.oids.clone()))
    }

    /// Describe of a prepared cursor statement: a DECLARE's parameters are its query's,
    /// and a FETCH has the cursor's columns.
    pub(crate) fn describe_cursor_statement(&mut self, sql: &str, declared: &[Oid]) -> Result<()> {
        match parse(sql) {
            Ok(Statement::Declare { query, .. }) => {
                let n = super::count_params(&query);
                let oids = self.param_types(&query, n, declared);
                self.c.send_parameter_types(&oids)?;
                self.c.send(crate::wire::MSG_NO_DATA, &[])
            }
            Ok(Statement::Fetch { moving: false, .. }) => {
                self.c.send_parameter_description(0)?;
                match self.cursor_description(sql, false) {
                    Some((cols, oids)) => self.c.send_row_description(&cols, &oids),
                    None => self.c.send(crate::wire::MSG_NO_DATA, &[]),
                }
            }
            _ => {
                self.c.send_parameter_description(0)?;
                self.c.send(crate::wire::MSG_NO_DATA, &[])
            }
        }
    }

    fn cursor_index(&self, name: &str) -> Result<usize> {
        self.cursors
            .index(name)
            .ok_or_else(|| errf!(SqlState::INVALID_CURSOR_NAME, "cursor \"{name}\" does not exist"))
    }

    #[allow(clippy::too_many_arguments)] // one per clause of a DECLARE
    fn declare(
        &mut self,
        name: String,
        statement: &str,
        binary: bool,
        scroll: Scroll,
        hold: bool,
        query: &str,
        params: &[Value],
    ) -> Result<()> {
        if !hold && !self.in_tx() {
            return Err(errf!(
                SqlState::NO_ACTIVE_SQL_TRANSACTION,
                "DECLARE CURSOR can only be used in transaction blocks"
            ));
        }
        if self.cursors.index(&name).is_some() {
            return Err(errf!(SqlState::DUPLICATE_CURSOR, "cursor \"{name}\" already exists"));
        }
        let first = query
            .trim_start()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        if !["select", "values", "with", "table"]
            .iter()
            .any(|w| first.eq_ignore_ascii_case(w))
            && !query.trim_start().starts_with('(')
        {
            return Err(syntax_error(first));
        }
        self.check_privileges(query)?;
        let sql = self.rewrite_for_exec(query, params)?;
        self.canceler.arm();
        let opened = self.db.open_query(&sql, params, self.search_path.as_deref());
        self.canceler.disarm();
        let (id, columns) = opened?;
        let oids = columns
            .iter()
            .enumerate()
            .map(|(i, c)| oid_for_column(c, &[], i))
            .collect();
        let mut cursor = Cursor {
            name,
            statement: statement.to_string(),
            hold,
            binary,
            scroll,
            columns,
            oids,
            source: Source::Stream { id, read: 0 },
            pos: 0,
            total: None,
            fresh: self.in_tx(),
            created: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
        };
        if !self.in_tx() {
            // Its transaction is the statement's, and it has ended.
            if let Err(e) = self.hold_rows(&mut cursor) {
                self.drop_source(&cursor);
                return Err(e);
            }
        }
        self.cursors.by_name.push(cursor);
        self.list_cursors();
        Ok(())
    }

    fn drop_source(&self, c: &Cursor) {
        if let Source::Stream { id, .. } = c.source {
            self.db.close_query(id);
        }
    }

    /// Reads a held cursor's rows into it, as its transaction commits.
    fn hold_rows(&self, c: &mut Cursor) -> Result<()> {
        let Source::Stream { id, .. } = c.source else {
            return Ok(());
        };
        let base = if c.scroll == Scroll::No {
            c.pos.min(c.total.unwrap_or(i64::MAX))
        } else {
            0
        };
        let rows = self.rows(c, base + 1, i64::MAX)?;
        self.db.close_query(id);
        c.source = Source::Held { rows, base };
        Ok(())
    }

    /// Rows a to b of a cursor, 1-based and inclusive, in order; fewer at the end, whose
    /// place it learns then.
    fn rows(&self, c: &mut Cursor, a: i64, b: i64) -> Result<Vec<Vec<Value>>> {
        self.rows_or_count(c, a, b, true).map(|(rows, _)| rows)
    }

    /// rows, or with keep false only their number, read past without building them.
    fn rows_or_count(&self, c: &mut Cursor, a: i64, b: i64, keep: bool) -> Result<(Vec<Vec<Value>>, i64)> {
        if a > b || c.total.is_some_and(|t| a > t) {
            return Ok((Vec::new(), 0));
        }
        let want = usize::try_from(b - a + 1).unwrap_or(usize::MAX);
        let total = &mut c.total;
        match &mut c.source {
            Source::Held { rows, base } => {
                let from = usize::try_from(a - 1 - *base).unwrap_or(0);
                let n = rows.len().saturating_sub(from).min(want);
                let got = if keep {
                    rows[from..from + n].to_vec()
                } else {
                    Vec::new()
                };
                Ok((got, n as i64))
            }
            Source::Stream { id, read } => {
                self.canceler.arm();
                let r = (|| {
                    if a <= *read {
                        self.db.rewind_query(*id);
                        *read = 0;
                    }
                    let gap = usize::try_from(a - 1 - *read).unwrap_or(0);
                    let passed = self.db.skip_query(*id, gap)?;
                    *read += passed as i64;
                    if passed < gap {
                        *total = Some(*read);
                        return Ok((Vec::new(), 0));
                    }
                    let (got, n) = if keep {
                        let got = self.db.fetch_query(*id, want)?;
                        let n = got.len();
                        (got, n)
                    } else {
                        (Vec::new(), self.db.skip_query(*id, want)?)
                    };
                    *read += n as i64;
                    if n < want {
                        *total = Some(*read);
                    }
                    Ok((got, n as i64))
                })();
                self.canceler.disarm();
                r
            }
        }
    }

    /// The number of a cursor's rows, read to its end if need be.
    fn total(&self, c: &mut Cursor) -> Result<i64> {
        if let Some(t) = c.total {
            return Ok(t);
        }
        if let Source::Stream { id, read } = &mut c.source {
            self.canceler.arm();
            let passed = self.db.skip_query(*id, usize::MAX);
            self.canceler.disarm();
            *read += passed? as i64;
            c.total = Some(*read);
        }
        Ok(c.total.unwrap_or(0))
    }

    /// FETCH (keep) or MOVE: the rows, and their number.
    fn fetch(&mut self, i: usize, direction: Direction, keep: bool) -> Result<(Vec<Vec<Value>>, i64)> {
        let mut c = std::mem::replace(&mut self.cursors.by_name[i], placeholder());
        let r = self.fetch_in(&mut c, direction, keep);
        self.cursors.by_name[i] = c;
        r
    }

    fn fetch_in(&self, c: &mut Cursor, direction: Direction, keep: bool) -> Result<(Vec<Vec<Value>>, i64)> {
        let direction = match direction {
            Direction::Forward(Count::N(n)) if n < 0 => Direction::Backward(Count::N(-n)),
            Direction::Backward(Count::N(n)) if n < 0 => Direction::Forward(Count::N(-n)),
            d => d,
        };
        if c.scroll == Scroll::No {
            forward_only(c, direction, keep)?;
        }
        let after = |c: &Cursor| c.total.is_some_and(|t| c.pos > t);
        match direction {
            Direction::Forward(Count::N(0)) | Direction::Backward(Count::N(0)) | Direction::Relative(0) => {
                if c.pos == 0 || after(c) {
                    return Ok((Vec::new(), 0));
                }
                let rows = if keep { self.rows(c, c.pos, c.pos)? } else { Vec::new() };
                Ok((rows, 1))
            }
            Direction::Forward(count) => {
                if after(c) {
                    return Ok((Vec::new(), 0));
                }
                let b = match count {
                    Count::N(n) => c.pos.saturating_add(n),
                    Count::All => i64::MAX,
                };
                let (rows, n) = self.rows_or_count(c, c.pos + 1, b, keep)?;
                if c.total.is_some_and(|t| b > t) {
                    c.pos = c.total.unwrap_or(0) + 1;
                } else {
                    c.pos = b;
                }
                Ok((rows, n))
            }
            Direction::Backward(count) => {
                if c.pos == 0 {
                    return Ok((Vec::new(), 0));
                }
                let hi = c.pos - 1;
                let lo = match count {
                    Count::N(n) => (hi - n + 1).max(1),
                    Count::All => 1,
                };
                let n = (hi - lo + 1).max(0);
                let mut rows = if keep { self.rows(c, lo, hi)? } else { Vec::new() };
                rows.reverse();
                c.pos = if matches!(count, Count::N(k) if hi - k + 1 >= 1) {
                    lo
                } else {
                    0
                };
                Ok((rows, n))
            }
            Direction::Absolute(n) => {
                let target = if n < 0 { self.total(c)? + 1 + n } else { n };
                self.land(c, target, keep)
            }
            Direction::Relative(n) => {
                let from = if after(c) { c.total.unwrap_or(0) + 1 } else { c.pos };
                if n > 0 && after(c) {
                    return Ok((Vec::new(), 0));
                }
                self.land(c, from + n, keep)
            }
        }
    }

    /// Goes to row target, answering it if there is one.
    fn land(&self, c: &mut Cursor, target: i64, keep: bool) -> Result<(Vec<Vec<Value>>, i64)> {
        if target <= 0 {
            c.pos = 0;
            return Ok((Vec::new(), 0));
        }
        let (rows, n) = self.rows_or_count(c, target, target, keep)?;
        if n == 0 {
            c.pos = c.total.unwrap_or(target - 1) + 1;
            return Ok((Vec::new(), 0));
        }
        c.pos = target;
        Ok((rows, 1))
    }

    /// Writes pg_cursors from the cursors held.
    fn list_cursors(&self) {
        let rows: Vec<Vec<Value>> = self
            .cursors
            .by_name
            .iter()
            .map(|c| {
                vec![
                    Value::Text(c.name.clone()),
                    Value::Text(c.statement.clone()),
                    Value::Int(i64::from(c.hold)),
                    Value::Int(i64::from(c.binary)),
                    Value::Int(i64::from(c.scroll != Scroll::No)),
                    Value::Text(c.created.clone()),
                ]
            })
            .collect();
        if let Err(e) = self.db.list_cursors(&rows) {
            log::debug!("pg_cursors: {e}");
        }
    }

    /// What the end of a transaction does to its cursors: a commit reads each held
    /// one's rows and closes the rest; a rollback closes what it declared.
    pub(crate) fn end_cursors(&mut self, commit: bool) {
        let mut kept = Vec::new();
        for mut c in std::mem::take(&mut self.cursors.by_name) {
            let stays = if commit { c.hold } else { c.hold && !c.fresh };
            if stays && self.hold_rows(&mut c).is_ok() {
                c.fresh = false;
                kept.push(c);
            } else {
                self.drop_source(&c);
            }
        }
        self.cursors.by_name = kept;
    }

    /// After the transaction has ended: pg_cursors as the cursors now stand.
    pub(crate) fn relist_cursors(&self) {
        self.list_cursors();
    }
}

fn placeholder() -> Cursor {
    Cursor {
        name: String::new(),
        statement: String::new(),
        hold: false,
        binary: false,
        scroll: Scroll::No,
        columns: Vec::new(),
        oids: Vec::new(),
        source: Source::Held {
            rows: Vec::new(),
            base: 0,
        },
        pos: 0,
        total: None,
        fresh: false,
        created: String::new(),
    }
}

/// Refuses what would take a NO SCROLL cursor back, by the steps Postgres takes: it
/// may use any direction so long as it ends up moving forward.
fn forward_only(c: &Cursor, direction: Direction, keep: bool) -> Result<()> {
    let at_end = c.total.is_some_and(|t| c.pos > t);
    let at_start = c.pos == 0;
    let back = || Err(errf!(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "{FORWARD_ONLY}"));
    match direction {
        Direction::Forward(Count::N(0)) | Direction::Backward(Count::N(0)) | Direction::Relative(0) => {
            // Re-reading the row it is on steps back to it; a MOVE does not read.
            if keep && !at_start && !at_end {
                return back();
            }
            Ok(())
        }
        Direction::Forward(_) => Ok(()),
        Direction::Backward(_) => back(),
        Direction::Relative(n) => {
            if n < 0 {
                return back();
            }
            Ok(())
        }
        Direction::Absolute(n) => {
            // The steps of DoPortalRunFetch: a rewind, then forward or back.
            let pos = if at_end { c.total.unwrap_or(0) } else { c.pos };
            if n < 0 {
                return back();
            }
            if n == 0 || n - 1 <= pos / 2 {
                if at_start && !at_end {
                    return Ok(());
                }
                return back();
            }
            let p = if at_end { pos + 1 } else { pos };
            if n <= p {
                return back();
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements() {
        assert_eq!(
            parse(r#"DECLARE "c1" NO SCROLL CURSOR WITH HOLD FOR SELECT 1"#).unwrap(),
            Statement::Declare {
                name: "c1".into(),
                binary: false,
                scroll: Scroll::No,
                hold: true,
                query: "SELECT 1".into()
            }
        );
        assert_eq!(
            parse("declare X binary insensitive scroll cursor without hold for values (1)").unwrap(),
            Statement::Declare {
                name: "x".into(),
                binary: true,
                scroll: Scroll::Yes,
                hold: false,
                query: "values (1)".into()
            }
        );
        let fetch = |sql: &str| match parse(sql).unwrap() {
            Statement::Fetch {
                name,
                direction,
                moving,
            } => (name, direction, moving),
            other => panic!("{other:?}"),
        };
        assert_eq!(fetch("FETCH c"), ("c".into(), Direction::Forward(Count::N(1)), false));
        assert_eq!(
            fetch("FETCH -2 IN c"),
            ("c".into(), Direction::Forward(Count::N(-2)), false)
        );
        assert_eq!(
            fetch("MOVE BACKWARD ALL FROM c"),
            ("c".into(), Direction::Backward(Count::All), true)
        );
        assert_eq!(
            fetch("FETCH ABSOLUTE -1 \"C\""),
            ("C".into(), Direction::Absolute(-1), false)
        );
        assert_eq!(
            fetch("fetch forward from c"),
            ("c".into(), Direction::Forward(Count::N(1)), false)
        );
        assert_eq!(parse("CLOSE ALL").unwrap(), Statement::Close(None));
        assert_eq!(parse("close \"a b\"").unwrap(), Statement::Close(Some("a b".into())));
        assert!(parse("FETCH ABSOLUTE FROM c").is_err());
        assert!(parse("DECLARE c CURSOR").is_err());
    }
}
