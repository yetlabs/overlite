//! E'' strings, and the scan that skips string literals.

use crate::error::{Error, Result};
use crate::rewrite::blank::{skip_block_comment, skip_line_comment};
use crate::rewrite::index_from;
use overlite_core::SqlState;
use std::borrow::Cow;

/// What a statement holds that SQLite's scanner does not: E'' and U&'' strings, and a
/// string continued on another line. Without any of them nothing is copied, and the
/// check is one pass over the bytes, because every statement takes it.
fn has_postgres_literal(sql: &str) -> bool {
    let b = sql.as_bytes();
    b.windows(2).enumerate().any(|(i, w)| match w {
        [b'e' | b'E' | b'&', b'\''] => i == 0 || !is_word_byte(b[i - 1]) || w[0] == b'&',
        [b'\'', b'\n' | b'\r'] => true,
        _ => false,
    }) || sql.contains("' ") && b.contains(&b'\n')
}

/// Turns the strings Postgres' scanner reads into SQLite's one kind of literal, so that
/// nothing after this has to know them: every pass and the engine read `''` alone.
///
/// # Errors
///
/// What Postgres' scanner reports: 22025 for a `\\u` that is cut short, 42601 for one
/// that names no character, and 22021 for bytes that are not UTF-8.
pub(crate) fn normalize_literals(sql: &str) -> Result<Cow<'_, str>> {
    if !has_postgres_literal(sql) {
        return Ok(Cow::Borrowed(sql));
    }
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let (mut copied, mut i) = (0, 0);
    while i < b.len() {
        let c = b[i];
        let start = i;
        let kind = match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(sql, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(sql, i).0;
                continue;
            }
            b'"' => {
                i += 1;
                while i < b.len() {
                    i += 1;
                    if b[i - 1] == b'"' {
                        if b.get(i) != Some(&b'"') {
                            break;
                        }
                        i += 1;
                    }
                }
                continue;
            }
            b'$' if i == 0 || !is_word_byte(b[i - 1]) => {
                i = end_of_dollar_string(sql, i);
                continue;
            }
            b'\'' => StringKind::Plain,
            b'e' | b'E' if b.get(i + 1) == Some(&b'\'') => {
                i += 1;
                StringKind::Escape
            }
            b'u' | b'U' if b.get(i + 1) == Some(&b'&') && b.get(i + 2) == Some(&b'\'') => {
                i += 2;
                StringKind::Unicode
            }
            c if is_word_byte(c) => {
                while i < b.len() && is_word_byte(b[i]) {
                    i += 1;
                }
                // A prefix of another kind of string, whose text is left as it is.
                if b.get(i) == Some(&b'\'') {
                    i = end_of_string_literal(sql, i);
                }
                continue;
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let (value, end, segments) = read_string(sql, i, kind)?;
        if kind == StringKind::Plain && segments == 1 {
            i = end;
            continue;
        }
        out.push_str(&sql[copied..start]);
        out.push('\'');
        out.push_str(&value.replace('\'', "''"));
        out.push('\'');
        (copied, i) = (end, end);
    }
    if copied == 0 {
        return Ok(Cow::Borrowed(sql));
    }
    out.push_str(&sql[copied..]);
    Ok(Cow::Owned(out))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Plain,
    Escape,
    Unicode,
}

/// The index past the dollar-quoted string opening at i, or i + 1 for a `$` that opens
/// none, such as a parameter.
fn end_of_dollar_string(sql: &str, i: usize) -> usize {
    let b = sql.as_bytes();
    let mut j = i + 1;
    while j < b.len() && is_word_byte(b[j]) && !(j == i + 1 && b[j].is_ascii_digit()) {
        j += 1;
    }
    if b.get(j) != Some(&b'$') {
        return i + 1;
    }
    let tag = &sql[i..=j];
    index_from(sql, tag, j + 1).map_or(b.len(), |k| k + tag.len())
}

/// The value of the string whose opening quote is at i with its continuations, the
/// index past it all, and how many segments that was.
fn read_string(sql: &str, i: usize, kind: StringKind) -> Result<(String, usize, usize)> {
    let b = sql.as_bytes();
    let mut raw = Vec::new();
    let mut at = i;
    let mut segments = 0;
    loop {
        at = read_segment(sql, at, kind == StringKind::Escape, &mut raw)?;
        segments += 1;
        // Whitespace holding a newline joins the next string to this one.
        let mut j = at;
        let mut newline = false;
        while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') {
            newline |= matches!(b[j], b'\n' | b'\r');
            j += 1;
        }
        if !newline || b.get(j) != Some(&b'\'') {
            break;
        }
        at = j;
    }
    let mut value = String::from_utf8(raw).map_err(|e| {
        let byte = e.as_bytes().get(e.utf8_error().valid_up_to()).copied().unwrap_or(0);
        invalid_bytes(byte)
    })?;
    if kind == StringKind::Unicode {
        let (escape, past) = uescape_char(sql, at)?;
        value = decode_unicode_string(&value, escape)?;
        at = past;
    }
    Ok((value, at, segments))
}

fn invalid_bytes(byte: u8) -> Error {
    Error::with_state(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"UTF8\": 0x{byte:02x}"),
    )
}

/// A Unicode escape that does not name a character, which Postgres' scanner refuses
/// as a syntax error.
fn invalid_unicode(message: &str) -> Error {
    Error::with_state(SqlState::SYNTAX_ERROR, message)
}

/// Adds the text of the segment at i to out, reading backslashes if `escapes`, and
/// returns the index past it.
fn read_segment(sql: &str, i: usize, escapes: bool, out: &mut Vec<u8>) -> Result<usize> {
    let b = sql.as_bytes();
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\'' if b.get(j + 1) == Some(&b'\'') => {
                out.push(b'\'');
                j += 2;
            }
            b'\'' => return Ok(j + 1),
            b'\\' if escapes => j = read_escape(sql, j + 1, out)?,
            c => {
                out.push(c);
                j += 1;
            }
        }
    }
    Err(Error::with_state(SqlState::SYNTAX_ERROR, "unterminated quoted string"))
}

fn digit_run(b: &[u8], from: usize, max: usize, radix: u32) -> usize {
    b[from..]
        .iter()
        .take(max)
        .take_while(|c| (**c as char).is_digit(radix))
        .count()
}

/// Reads the escape whose letter is at i, as Postgres' scanner does.
fn read_escape(sql: &str, i: usize, out: &mut Vec<u8>) -> Result<usize> {
    let b = sql.as_bytes();
    let Some(&c) = b.get(i) else {
        return Err(Error::with_state(SqlState::SYNTAX_ERROR, "unterminated quoted string"));
    };
    let number = |from: usize, n: usize, radix: u32| u32::from_str_radix(&sql[from..from + n], radix).unwrap_or(0);
    let mut push_byte = |v: u32| {
        // A byte is a byte, and what is built of them is checked once it is whole.
        match v as u8 {
            0 => Err(invalid_bytes(0)),
            byte => {
                out.push(byte);
                Ok(())
            }
        }
    };
    match c {
        b'b' => push_byte(0x08).map(|()| i + 1),
        b'f' => push_byte(0x0c).map(|()| i + 1),
        b'n' => push_byte(0x0a).map(|()| i + 1),
        b'r' => push_byte(0x0d).map(|()| i + 1),
        b't' => push_byte(0x09).map(|()| i + 1),
        b'v' => push_byte(0x0b).map(|()| i + 1),
        b'0'..=b'7' => {
            let n = digit_run(b, i, 3, 8);
            push_byte(number(i, n, 8)).map(|()| i + n)
        }
        b'x' if digit_run(b, i + 1, 2, 16) > 0 => {
            let n = digit_run(b, i + 1, 2, 16);
            push_byte(number(i + 1, n, 16)).map(|()| i + 1 + n)
        }
        b'u' | b'U' => {
            let want = if c == b'u' { 4 } else { 8 };
            if digit_run(b, i + 1, want, 16) < want {
                return Err(Error::with_state(
                    SqlState::INVALID_ESCAPE_SEQUENCE,
                    "invalid Unicode escape",
                ));
            }
            let first = number(i + 1, want, 16);
            let mut end = i + 1 + want;
            let code = if (0xD800..0xDC00).contains(&first) {
                let low = low_surrogate(sql, end)?;
                end += 2 + if b[end + 1] == b'u' { 4 } else { 8 };
                0x10000 + ((first - 0xD800) << 10) + (low - 0xDC00)
            } else {
                first
            };
            let ch = unicode_char(code)?;
            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            Ok(end)
        }
        _ => {
            // Any other character stands for itself, whole.
            let ch = sql[i..].chars().next().unwrap_or(' ');
            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            Ok(i + ch.len_utf8())
        }
    }
}

/// The low half of a surrogate pair, whose backslash is at i.
fn low_surrogate(sql: &str, i: usize) -> Result<u32> {
    let b = sql.as_bytes();
    let want = match b.get(i..i + 2) {
        Some(b"\\u") => 4,
        Some(b"\\U") => 8,
        _ => return Err(invalid_unicode("invalid Unicode surrogate pair")),
    };
    if digit_run(b, i + 2, want, 16) < want {
        return Err(invalid_unicode("invalid Unicode surrogate pair"));
    }
    let low = u32::from_str_radix(&sql[i + 2..i + 2 + want], 16).unwrap_or(0);
    if !(0xDC00..0xE000).contains(&low) {
        return Err(invalid_unicode("invalid Unicode surrogate pair"));
    }
    Ok(low)
}

fn unicode_char(code: u32) -> Result<char> {
    match code {
        0 => Err(invalid_unicode("invalid Unicode escape value")),
        0xD800..=0xDFFF => Err(invalid_unicode("invalid Unicode surrogate pair")),
        _ => char::from_u32(code).ok_or_else(|| invalid_unicode("invalid Unicode escape value")),
    }
}

/// The escape character of a U&'' string, from the UESCAPE that may follow it at i, and
/// the index past it.
fn uescape_char(sql: &str, i: usize) -> Result<(char, usize)> {
    let b = sql.as_bytes();
    let mut j = i;
    while j < b.len() && b[j].is_ascii_whitespace() {
        j += 1;
    }
    if !sql.get(j..j + 7).is_some_and(|w| w.eq_ignore_ascii_case("uescape")) {
        return Ok(('\\', i));
    }
    j += 7;
    while j < b.len() && b[j].is_ascii_whitespace() {
        j += 1;
    }
    if b.get(j) == Some(&b'\'') {
        let mut text = Vec::new();
        let end = read_segment(sql, j, false, &mut text)?;
        if let [c] = text[..]
            && !c.is_ascii_hexdigit()
            && !matches!(c, b'+' | b'\'' | b'"' | b' ' | b'\t' | b'\n' | b'\r')
        {
            return Ok((c as char, end));
        }
    }
    Err(Error::with_state(
        SqlState::SYNTAX_ERROR,
        "invalid Unicode escape character",
    ))
}

/// The value of a U&'' string, whose text is value.
fn decode_unicode_string(value: &str, escape: char) -> Result<String> {
    let chars: Vec<char> = value.chars().collect();
    let hex = |from: usize, n: usize| -> Option<u32> {
        let digits = chars.get(from..from + n)?;
        digits.iter().try_fold(0, |v, c| Some(v * 16 + c.to_digit(16)?))
    };
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != escape {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&escape) {
            out.push(escape);
            i += 2;
            continue;
        }
        let (code, len) = if chars.get(i + 1) == Some(&'+') {
            (hex(i + 2, 6), 8)
        } else {
            (hex(i + 1, 4), 5)
        };
        let Some(code) = code else {
            return Err(invalid_unicode("invalid Unicode escape"));
        };
        i += len;
        let code = if (0xD800..0xDC00).contains(&code) {
            let low = (chars.get(i) == Some(&escape))
                .then(|| hex(i + 1, 4))
                .flatten()
                .filter(|l| (0xDC00..0xE000).contains(l));
            let Some(low) = low else {
                return Err(invalid_unicode("invalid Unicode surrogate pair"));
            };
            i += 5;
            0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
        } else {
            code
        };
        out.push(unicode_char(code)?);
    }
    Ok(out)
}

use overlite_core::text::end_of_string_literal;

fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}
