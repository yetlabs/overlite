//! COPY implements the bulk-data sub-protocol used by pg_dump and psql's \copy.
//! Supported: COPY `<table>` [(cols)] FROM STDIN, COPY `<table>` [(cols)] TO STDOUT,
//! and COPY (query) TO STDOUT, in text (default) and CSV formats.

use crate::error::{Error, Result, errf};
use crate::rewrite::rewrite;
use crate::session::Session;
use crate::session::utility::unquote_ident;
use crate::wire::{
    MSG_COPY_DATA, MSG_COPY_DONE, MSG_COPY_FAIL, MSG_COPY_IN_RESPONSE, MSG_COPY_OUT_RESPONSE, WireConn, append_int16,
};
use overlite_core::text::{
    compile_or_panic, eq_fold, quote_ident, quote_literal, split_words, string_from_cut_utf8, text_from_bytes,
    trim_end_chars, unescape_vec,
};
use overlite_core::{SqlState, Value, lazy_regex};
use overlite_engine::blank_literals;

#[derive(Clone, Debug, Default)]
pub(crate) struct CopyStmt {
    table: String,
    columns: Vec<String>, // empty = all columns
    query: String,        // set for COPY (query) TO STDOUT
    from_stdin: bool,     // FROM STDIN (true) vs TO STDOUT (false)
    csv: bool,
    delimiter: u8,
    null: String,
    header: bool,
}

lazy_regex!(RE_COPY, r"(?is)^\s*COPY\s+(.*?)\s+(FROM\s+STDIN|TO\s+STDOUT)\s*(.*)$");

impl CopyStmt {
    /// The query of `COPY (query) TO STDOUT`, which the session translates alone.
    #[cfg(test)]
    pub(crate) fn query(&self) -> Option<&str> {
        (!self.from_stdin && !self.query.is_empty()).then_some(self.query.as_str())
    }
}

/// Parses a COPY statement, or returns None if it isn't one.
pub(crate) fn parse_copy(sql: &str) -> Option<CopyStmt> {
    let code = blank_literals(sql);
    let m = RE_COPY.captures(&code)?;
    let mut cs = CopyStmt {
        from_stdin: eq_fold(split_words(&m[2])[0], "from"),
        delimiter: b'\t',
        null: r"\N".to_string(),
        ..Default::default()
    };

    let target = sql[m.get(1)?.range()].trim();
    if target.starts_with('(') {
        let t = target.strip_prefix('(').unwrap_or(target);
        cs.query = t.strip_suffix(')').unwrap_or(t).trim().to_string();
    } else if let Some(i) = target.find('(') {
        cs.table = target[..i].trim().to_string();
        let c = target[i + 1..].trim();
        let cols = c.strip_suffix(')').unwrap_or(c);
        for c in cols.split(',') {
            cs.columns.push(unquote_ident(c.trim()));
        }
    } else {
        cs.table = target.to_string();
    }

    parse_copy_options(&mut cs, &m[3]);
    Some(cs)
}

fn parse_copy_options(cs: &mut CopyStmt, opts: &str) {
    let low = opts.to_lowercase();
    if low.contains("csv") || regexp_contains(&low, r"format\s+csv") {
        cs.csv = true;
        cs.delimiter = b',';
        cs.null = String::new();
    }
    if low.contains("header") {
        cs.header = true;
    }
    if let Some(m) = compile_or_panic(r"(?i)delimiter\s+'(.)'").captures(opts) {
        cs.delimiter = m[1].as_bytes()[0];
    }
    if let Some(m) = compile_or_panic(r"(?i)null\s+'([^']*)'").captures(opts) {
        cs.null = m[1].to_string();
    }
}

fn regexp_contains(s: &str, pat: &str) -> bool {
    compile_or_panic(pat).is_match(s)
}

impl Session {
    /// Runs a COPY statement, driving the copy sub-protocol on the wire.
    pub(crate) fn handle_copy(&mut self, cs: &CopyStmt) -> Result<()> {
        // Held to what the same read or write would need sent directly, column by
        // column: a COPY with no list reads or writes them all.
        let listed = || cs.columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
        let needs = match (cs.from_stdin, cs.query.is_empty()) {
            (true, _) => {
                let width = if cs.columns.is_empty() {
                    let table = self.stored_name(&cs.table);
                    let q = "SELECT count(*) FROM pragma_table_info(".to_string() + &quote_literal(&table) + ")";
                    match self
                        .exec(&q, &[])
                        .ok()
                        .and_then(|rs| rs.rows.first().map(|r| r[0].clone()))
                    {
                        Some(Value::Int(n)) => usize::try_from(n).unwrap_or(0),
                        _ => 0,
                    }
                } else {
                    cs.columns.len()
                };
                let mut sql = "INSERT INTO ".to_string() + &cs.table;
                if width > 0 {
                    if !cs.columns.is_empty() {
                        sql += &[" (", &listed(), ")"].concat();
                    }
                    sql += &[" VALUES (", &vec!["NULL"; width].join(", "), ")"].concat();
                }
                sql
            }
            (false, true) if cs.columns.is_empty() => "SELECT * FROM ".to_string() + &cs.table,
            (false, true) => ["SELECT ", &listed(), " FROM ", &cs.table].concat(),
            (false, false) => cs.query.clone(),
        };
        if let Err(e) = self.check_privileges(&needs) {
            self.mark_tx_failed();
            return self.c.send_error(e.sql_state(), e.message());
        }
        if cs.from_stdin {
            // As in Postgres, whose COPY TO reads through the policies like a query.
            let table = self.stored_name(&cs.table);
            if self.rls_check_expr(&table).is_some() {
                self.mark_tx_failed();
                return self.c.send_error(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "COPY FROM not supported with row-level security",
                );
            }
            return self.copy_in(cs);
        }
        self.copy_out(cs)
    }

    // --- COPY ... TO STDOUT ---------------------------------------------------

    fn copy_out(&mut self, cs: &CopyStmt) -> Result<()> {
        let mut query = cs.query.clone();
        if query.is_empty() {
            let mut cols = "*".to_string();
            if !cs.columns.is_empty() {
                cols = quote_cols(&cs.columns);
            }
            query = "SELECT ".to_string() + &cols + " FROM " + &cs.table;
        }
        let query = match self.rewrite_for_exec(&query, &[]) {
            Ok(q) => q,
            Err(e) => return self.c.send_error(e.sql_state(), e.message()),
        };
        let rs = match self.exec(&query, &[]) {
            Ok(rs) => rs,
            Err(e) => {
                return self.c.send_error(e.sql_state(), e.message());
            }
        };

        // CopyOutResponse: text format (0), one format code per column.
        self.c.send_copy_response(MSG_COPY_OUT_RESPONSE, rs.columns.len())?;

        let w = new_copy_writer(cs);
        if cs.header {
            let names: Vec<String> = rs.columns.iter().map(|c| c.name.clone()).collect();
            self.c.send(MSG_COPY_DATA, &w.line(&names))?;
        }
        for row in &rs.rows {
            self.c.send(MSG_COPY_DATA, &w.row(row))?;
        }
        self.c.send(MSG_COPY_DONE, &[])?;
        self.c.send_command_complete(&format!("COPY {}", rs.rows.len()))
    }

    // --- COPY ... FROM STDIN --------------------------------------------------

    fn copy_in(&mut self, cs: &CopyStmt) -> Result<()> {
        let mut cols = cs.columns.clone();
        if cols.is_empty() {
            let describe = match self
                .db
                .describe(&("SELECT * FROM ".to_string() + &rewrite(&cs.table)), &[], None)
            {
                Ok(d) => d,
                Err(e) => {
                    return self.c.send_error(e.sql_state(), e.message());
                }
            };
            for c in describe.unwrap_or_default() {
                cols.push(c.name);
            }
        }

        self.c.send_copy_response(MSG_COPY_IN_RESPONSE, cols.len())?;
        self.c.flush()?;

        // Accumulate all CopyData until CopyDone (or abort on CopyFail).
        let mut data: Vec<u8> = Vec::new();
        loop {
            let (typ, body) = self.c.read_message()?;
            if typ == MSG_COPY_DATA {
                data.extend_from_slice(&body);
            } else if typ == MSG_COPY_DONE {
                return match self.copy_insert(cs, &cols, &data) {
                    Err(e) => {
                        self.mark_tx_failed();
                        self.c.send_error(e.sql_state(), e.message())
                    }
                    Ok(n) => self.c.send_command_complete(&format!("COPY {n}")),
                };
            } else if typ == MSG_COPY_FAIL {
                let msg = "COPY from stdin failed: ".to_string() + trim_end_chars(&text_from_bytes(&body), "\x00");
                return self.c.send_error(SqlState::QUERY_CANCELED, &msg);
            } else {
                return self
                    .c
                    .send_error(SqlState::PROTOCOL_VIOLATION, "unexpected message during COPY");
            }
        }
    }

    /// Parses the received data and inserts it, atomically when not already inside
    /// a transaction.
    fn copy_insert(&mut self, cs: &CopyStmt, cols: &[String], data: &[u8]) -> Result<usize> {
        let rows = parse_copy_data(cs, cols.len(), data)?;

        let insert = "INSERT INTO ".to_string()
            + &rewrite(&cs.table)
            + " ("
            + &quote_cols(cols)
            + ") VALUES ("
            + &placeholders(cols.len())
            + ")";
        let insert = self.fit_written_values(&insert);

        // Wrap in a transaction so a bad row doesn't leave a partial load.
        let own_tx = !self.in_tx();
        if own_tx {
            self.db.begin()?;
        }

        for row in &rows {
            let res = if own_tx {
                self.db.tx_execute(&insert, row, None).map_err(Error::from)
            } else {
                self.exec(&insert, row)
            };
            if let Err(e) = res {
                if own_tx {
                    let _ = self.db.rollback();
                }
                return Err(e);
            }
        }
        if own_tx {
            self.db.commit()?;
        }
        Ok(rows.len())
    }
}

// --- wire helpers -------------------------------------------------------------

impl WireConn {
    fn send_copy_response(&mut self, typ: u8, num_cols: usize) -> Result<()> {
        let mut b: Vec<u8> = Vec::new();
        b.push(0); // overall format: 0 = text
        append_int16(&mut b, num_cols as i32);
        for _ in 0..num_cols {
            append_int16(&mut b, 0); // per-column format: text
        }
        self.send(typ, &b)
    }
}

fn quote_cols(cols: &[String]) -> String {
    let q: Vec<String> = cols.iter().map(|c| "\"".to_string() + c + "\"").collect();
    q.join(", ")
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

// --- text/CSV encoding --------------------------------------------------------

#[derive(Debug)]
struct CopyWriter<'a> {
    cs: &'a CopyStmt,
}

fn new_copy_writer(cs: &CopyStmt) -> CopyWriter<'_> {
    CopyWriter { cs }
}

impl CopyWriter<'_> {
    /// Encodes one result row as a copy line (with trailing newline).
    fn row(&self, cells: &[Value]) -> Vec<u8> {
        let mut f: Vec<String> = Vec::with_capacity(cells.len());
        for cell in cells {
            if cell.is_null() {
                f.push(self.cs.null.clone());
                continue;
            }
            f.push(value_to_text(cell));
        }
        if self.cs.csv {
            return self.line(&f);
        }
        // In text the NULL marker is written as it is: escaped, it is data.
        let esc: Vec<String> = cells
            .iter()
            .zip(&f)
            .map(|(c, x)| if c.is_null() { x.clone() } else { escape_copy_text(x) })
            .collect();
        self.join_text(&esc)
    }

    fn line(&self, f: &[String]) -> Vec<u8> {
        // The delimiter byte read as a code point.
        let delim = self.cs.delimiter as char;
        if self.cs.csv {
            return unescape_vec(csv_line(f, delim).into_bytes());
        }
        let esc: Vec<String> = f.iter().map(|x| escape_copy_text(x)).collect();
        self.join_text(&esc)
    }

    fn join_text(&self, esc: &[String]) -> Vec<u8> {
        let delim = self.cs.delimiter as char;
        unescape_vec((esc.join(delim.to_string().as_str()) + "\n").into_bytes())
    }
}

/// Whether a char can delimit CSV, for a delimiter that came from a byte.
fn valid_delim(r: char) -> bool {
    r != '\0' && r != '"' && r != '\r' && r != '\n'
}

/// Whether a CSV field needs quotes.
fn csv_field_needs_quotes(field: &str, delim: char) -> bool {
    if field.is_empty() {
        return false;
    }
    if field == r"\." {
        return true;
    }
    if field.contains(delim) || field.contains(['"', '\r', '\n']) {
        return true;
    }
    field.chars().next().is_some_and(char::is_whitespace)
}

/// One CSV record (includes the trailing newline). An invalid delimiter gives
/// empty output.
fn csv_line(f: &[String], delim: char) -> String {
    let mut sb = String::new();
    if !valid_delim(delim) {
        return sb;
    }
    for (n, field) in f.iter().enumerate() {
        if n > 0 {
            sb.push(delim);
        }
        if !csv_field_needs_quotes(field, delim) {
            sb.push_str(field);
            continue;
        }
        sb.push('"');
        for ch in field.chars() {
            if ch == '"' {
                sb.push_str("\"\"");
            } else {
                sb.push(ch);
            }
        }
        sb.push('"');
    }
    sb.push('\n');
    sb
}

fn escape_copy_text(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (i, c) in b.iter().enumerate() {
        let rep = match c {
            b'\\' => "\\\\",
            b'\t' => "\\t",
            b'\n' => "\\n",
            b'\r' => "\\r",
            _ => continue,
        };
        out.push_str(&s[last..i]);
        out.push_str(rep);
        last = i + 1;
    }
    out.push_str(&s[last..]);
    out
}

/// Splits received bytes into rows of values.
fn parse_copy_data(cs: &CopyStmt, num_cols: usize, data: &[u8]) -> Result<Vec<Vec<Value>>> {
    let decoded = text_from_bytes(data);
    let text = trim_end_chars(&decoded, "\n");
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if cs.csv {
        return parse_csv(cs, num_cols, text);
    }
    let delim = (cs.delimiter as char).to_string();
    let mut rows = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        // A lone \. ends the data. pg_dump writes one after every COPY block and
        // psql forwards it as ordinary CopyData, so without this the marker is read
        // as a one-field row and the whole load fails.
        if line == r"\." {
            break;
        }
        let f: Vec<&str> = line.split(delim.as_str()).collect();
        if f.len() != num_cols {
            return Err(errf!(
                SqlState::BAD_COPY_FILE_FORMAT,
                "COPY row {} has {} fields, expected {}",
                i + 1,
                f.len(),
                num_cols
            ));
        }
        let mut row = Vec::with_capacity(num_cols);
        for x in f {
            if x == cs.null {
                row.push(Value::Null);
            } else {
                row.push(Value::Text(unescape_copy_text(x)));
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// The CSV reader's line source: `line` is the text of the line without its newline,
/// `nl` whether it had one (\r\n normalized to \n, a final \r dropped).
#[derive(Debug)]
struct CsvLines<'a> {
    rest: &'a str,
    num_line: usize,
}

impl<'a> CsvLines<'a> {
    /// None at end of input.
    fn read_line(&mut self) -> Option<(&'a str, bool)> {
        self.num_line += 1;
        if self.rest.is_empty() {
            return None;
        }
        if let Some(i) = self.rest.find('\n') {
            let line = &self.rest[..i];
            self.rest = &self.rest[i + 1..];
            Some((line.strip_suffix('\r').unwrap_or(line), true))
        } else {
            let line = self.rest;
            self.rest = "";
            Some((line.strip_suffix('\r').unwrap_or(line), false))
        }
    }
}

fn csv_parse_error(start_line: usize, line: usize, column: usize, err: &str) -> Error {
    if start_line != line {
        return errf!(
            SqlState::BAD_COPY_FILE_FORMAT,
            "record on line {}; parse error on line {}, column {}: {}",
            start_line,
            line,
            column,
            err
        );
    }
    errf!(
        SqlState::BAD_COPY_FILE_FORMAT,
        "parse error on line {}, column {}: {}",
        line,
        column,
        err
    )
}

const CSV_ERR_BARE_QUOTE: &str = "bare \" in non-quoted field";
const CSV_ERR_QUOTE: &str = "extraneous or missing \" in quoted-field";

/// Reads one CSV record with this delimiter, no comment char, strict quotes.
/// Ok(None) is the end of input.
fn csv_read_record(r: &mut CsvLines<'_>, comma: char, fields_per_record: &mut usize) -> Result<Option<Vec<String>>> {
    if !valid_delim(comma) {
        return Err(errf!(
            SqlState::BAD_COPY_FILE_FORMAT,
            "csv: invalid field or comment delimiter"
        ));
    }
    // Read line (automatically skipping past empty lines).
    let (mut line, mut nl);
    loop {
        match r.read_line() {
            None => return Ok(None),
            Some((l, n)) => {
                if l.is_empty() {
                    continue; // Skip empty lines
                }
                line = l;
                nl = n;
                break;
            }
        }
    }

    let comma_len = comma.len_utf8();
    let rec_line = r.num_line; // Starting line for record
    let mut record: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut pos_line = r.num_line;
    let mut pos_col = 1usize;
    'parse_field: loop {
        if !line.starts_with('"') {
            // Non-quoted string field
            let i = line.find(comma);
            let field = match i {
                Some(i) => &line[..i],
                None => line,
            };
            // Check to make sure a quote does not appear in field.
            if let Some(j) = field.find('"') {
                return Err(csv_parse_error(rec_line, r.num_line, pos_col + j, CSV_ERR_BARE_QUOTE));
            }
            buf.push_str(field);
            record.push(std::mem::take(&mut buf));
            if let Some(i) = i {
                line = &line[i + comma_len..];
                pos_col += i + comma_len;
                continue 'parse_field;
            }
            break 'parse_field;
        }
        // Quoted string field
        line = &line[1..];
        pos_col += 1;
        loop {
            if let Some(i) = line.find('"') {
                // Hit next quote.
                buf.push_str(&line[..i]);
                line = &line[i + 1..];
                pos_col += i + 1;
                if line.starts_with('"') {
                    // `""` sequence (append quote).
                    buf.push('"');
                    line = &line[1..];
                    pos_col += 1;
                } else if line.starts_with(comma) {
                    // `",` sequence (end of field).
                    line = &line[comma_len..];
                    pos_col += comma_len;
                    record.push(std::mem::take(&mut buf));
                    continue 'parse_field;
                } else if line.is_empty() {
                    // `"\n` sequence (end of line).
                    record.push(std::mem::take(&mut buf));
                    break 'parse_field;
                } else {
                    // `"*` sequence (invalid non-escaped quote).
                    return Err(csv_parse_error(rec_line, r.num_line, pos_col - 1, CSV_ERR_QUOTE));
                }
            } else if !line.is_empty() || nl {
                // Hit end of line (copy all data so far).
                buf.push_str(line);
                pos_col += line.len();
                if nl {
                    buf.push('\n');
                    pos_col += 1;
                }
                if let Some((l, n)) = r.read_line() {
                    line = l;
                    nl = n;
                    if !line.is_empty() || nl {
                        pos_line += 1;
                        pos_col = 1;
                    }
                } else {
                    line = "";
                    nl = false;
                }
            } else {
                // Abrupt end of file.
                return Err(csv_parse_error(rec_line, pos_line, pos_col, CSV_ERR_QUOTE));
            }
        }
    }

    // Check or update the expected fields per record.
    if *fields_per_record > 0 {
        if record.len() != *fields_per_record {
            return Err(errf!(
                SqlState::BAD_COPY_FILE_FORMAT,
                "record on line {}: wrong number of fields",
                rec_line
            ));
        }
    } else {
        *fields_per_record = record.len();
    }
    Ok(Some(record))
}

fn parse_csv(cs: &CopyStmt, num_cols: usize, text: &str) -> Result<Vec<Vec<Value>>> {
    let comma = cs.delimiter as char;
    let mut r = CsvLines {
        rest: text,
        num_line: 0,
    };
    let mut fields_per_record = num_cols;
    let mut records: Vec<Vec<String>> = Vec::new();
    while let Some(rec) = csv_read_record(&mut r, comma, &mut fields_per_record)? {
        records.push(rec);
    }
    let mut rows = Vec::new();
    for (i, rec) in records.into_iter().enumerate() {
        if cs.header && i == 0 {
            continue;
        }
        let mut row = vec![Value::Null; num_cols];
        for (j, f) in rec.into_iter().enumerate() {
            if f == cs.null {
                row[j] = Value::Null;
            } else {
                row[j] = Value::Text(f);
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn unescape_copy_text(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b't' => out.push(b'\t'),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b'\\' => out.push(b'\\'),
                other => out.push(other),
            }
            i += 2;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    // Only ASCII bytes were dropped or replaced, so the text is still UTF-8.
    string_from_cut_utf8(out)
}

/// Renders a scalar for the copy text stream.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::Blob(b) => text_from_bytes(b).into_owned(),
        Value::Bool(b) => {
            if *b {
                "t".to_string()
            } else {
                "f".to_string()
            }
        }
        other => other.to_string(),
    }
}
