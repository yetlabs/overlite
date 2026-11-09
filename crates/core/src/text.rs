//! Text as the server sees it: bytes that need not be UTF-8, quoting for error
//! messages, and regular expressions with ASCII classes.

pub use regex::Regex;
use std::borrow::Cow;

// A client may send any bytes and a String holds only UTF-8, so bytes that are
// not UTF-8 travel inside a String as code points of plane 16: 0xNN is
// U+10FFNN, for NN from 0x80. Everything between the wire and SQLite treats
// them as ordinary text, and they reach SQLite as the bytes they were;
// `text_from_bytes` and `text_to_bytes` are the two doors. A client's own
// U+10FF80..U+10FFFF goes through as its four bytes, so nothing is ambiguous.

/// Whether s holds an escaped byte. A byte search, so it costs the hot path no
/// copy and no decoding: 0xF4 only leads code points past U+0FFFFF.
#[inline]
pub fn has_raw_bytes(s: &[u8]) -> bool {
    s.contains(&0xF4) && s.windows(3).any(|w| w[0] == 0xF4 && w[1] == 0x8F && w[2] >= 0xBE)
}

fn push_raw_byte(out: &mut String, b: u8) {
    out.extend(char::from_u32(0x10FF00 + b as u32));
}

/// Appends text from outside to an escaped String.
pub fn push_escaped(out: &mut String, s: &str) {
    if !has_raw_bytes(s.as_bytes()) {
        out.push_str(s);
        return;
    }
    for c in s.chars() {
        if ('\u{10FF80}'..='\u{10FFFF}').contains(&c) {
            c.encode_utf8(&mut [0; 4]).bytes().for_each(|b| push_raw_byte(out, b));
        } else {
            out.push(c);
        }
    }
}

/// The bytes as a String, unchanged when they are plain UTF-8. Anything else survives
/// the trip through a `String`, which [`text_to_bytes`] undoes.
///
/// ```
/// use overlite_core::text::{text_from_bytes, text_to_bytes};
///
/// assert_eq!(text_from_bytes(b"plain"), "plain");
/// let raw = b"caf\xe9"; // Latin-1, not UTF-8
/// assert_eq!(text_to_bytes(&text_from_bytes(raw)), &raw[..]);
/// ```
#[inline]
pub fn text_from_bytes(b: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(b) {
        Ok(s) if !has_raw_bytes(b) => Cow::Borrowed(s),
        _ => Cow::Owned(escape_bytes(b)),
    }
}

#[cold]
fn escape_bytes(mut b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() * 2);
    while !b.is_empty() {
        match std::str::from_utf8(b) {
            Ok(s) => {
                push_escaped(&mut out, s);
                break;
            }
            Err(e) => {
                let (good, bad) = b.split_at(e.valid_up_to());
                push_escaped(&mut out, std::str::from_utf8(good).unwrap_or_default());
                let n = e.error_len().unwrap_or(bad.len());
                bad[..n].iter().for_each(|c| push_raw_byte(&mut out, *c));
                b = &bad[n..];
            }
        }
    }
    out
}

/// The bytes a String stands for, as they go to a client or SQLite.
#[inline]
pub fn text_to_bytes(s: &str) -> Cow<'_, [u8]> {
    if has_raw_bytes(s.as_bytes()) {
        return Cow::Owned(unescape_bytes(s));
    }
    Cow::Borrowed(s.as_bytes())
}

#[cold]
fn unescape_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{10FF80}'..='\u{10FFFF}' => out.push((c as u32 - 0x10FF00) as u8),
            _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out
}

/// The same for text already in a buffer.
pub fn unescape_vec(b: Vec<u8>) -> Vec<u8> {
    if has_raw_bytes(&b)
        && let Ok(s) = String::from_utf8(b.clone())
    {
        return unescape_bytes(&s);
    }
    b
}

/// Replaces bad bytes as decoding by code point does: U+FFFD, one per byte. The
/// result is still an escaped String.
pub fn valid_utf8(s: &str) -> Cow<'_, str> {
    if !has_raw_bytes(s.as_bytes()) {
        return Cow::Borrowed(s);
    }
    let raw = unescape_bytes(s);
    let mut b = raw.as_slice();
    let mut out = String::with_capacity(b.len());
    while !b.is_empty() {
        match std::str::from_utf8(b) {
            Ok(g) => {
                push_escaped(&mut out, g);
                break;
            }
            Err(e) => {
                let (good, bad) = b.split_at(e.valid_up_to());
                push_escaped(&mut out, std::str::from_utf8(good).unwrap_or_default());
                let n = e.error_len().unwrap_or(bad.len());
                out.extend(std::iter::repeat_n('\u{FFFD}', n));
                b = &bad[n..];
            }
        }
    }
    Cow::Owned(out)
}

/// A double-quoted string, for `{:?}`, in the form error messages use. Rust's
/// own Debug spells control
/// characters and unprintable code points differently, and those reach clients
/// in error messages and SQLite in generated identifiers.
pub struct Quoted<S>(S);

pub fn quoted<S: AsRef<str>>(s: S) -> Quoted<S> {
    Quoted(s)
}

impl<S: AsRef<str>> std::fmt::Debug for Quoted<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&quote_with(self.0.as_ref(), '"'))
    }
}

/// A single-quoted character, for `{:?}`.
pub fn quoted_char(c: char) -> QuotedChar {
    QuotedChar(c)
}

pub struct QuotedChar(char);

impl std::fmt::Debug for QuotedChar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&quote_with(self.0.encode_utf8(&mut [0; 4]), '\''))
    }
}

/// Whether a code point past ASCII prints as itself when quoted.
fn is_printable(c: char) -> bool {
    #[allow(clippy::unwrap_used)] // a literal, run by every quoted name
    static PRINT: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^[\p{L}\p{M}\p{N}\p{P}\p{S}]$").unwrap());
    PRINT.is_match(c.encode_utf8(&mut [0; 4]))
}

fn quote_with(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    let raw = text_to_bytes(s);
    let mut b: &[u8] = &raw;
    while !b.is_empty() {
        match std::str::from_utf8(b) {
            Ok(good) => {
                quote_chars(good, quote, &mut out);
                break;
            }
            Err(e) => {
                let (good, bad) = b.split_at(e.valid_up_to());
                quote_chars(std::str::from_utf8(good).unwrap_or_default(), quote, &mut out);
                let n = e.error_len().unwrap_or(bad.len());
                bad[..n].iter().for_each(|c| out.push_str(&format!("\\x{c:02x}")));
                b = &bad[n..];
            }
        }
    }
    out.push(quote);
    out
}

fn quote_chars(s: &str, quote: char, out: &mut String) {
    use std::fmt::Write;
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            '\0'..='\u{1f}' | '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if is_printable(c) => out.push(c),
            c if (c as u32) < 0x10000 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let _ = write!(out, "\\U{:08x}", c as u32);
            }
        }
    }
}

/// Makes `\b \w \d \s` ASCII-only for the `regex` crate, whose classes are
/// Unicode, by spelling them out.
pub fn ascii_pattern(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = String::with_capacity(p.len() + 16);
    let mut i = 0;
    let mut in_class = false;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            let rep = match (n, in_class) {
                (b'b', false) => Some("(?-u:\\b)"),
                (b'B', false) => Some("(?-u:\\B)"),
                (b'w', false) => Some("[0-9A-Za-z_]"),
                (b'W', false) => Some("[^0-9A-Za-z_]"),
                (b'd', false) => Some("[0-9]"),
                (b'D', false) => Some("[^0-9]"),
                (b's', false) => Some("[\\t\\n\\f\\r ]"),
                (b'S', false) => Some("[^\\t\\n\\f\\r ]"),
                (b'w', true) => Some("0-9A-Za-z_"),
                (b'd', true) => Some("0-9"),
                (b's', true) => Some("\\t\\n\\f\\r "),
                _ => None,
            };
            if let Some(r) = rep {
                out.push_str(r);
            } else {
                out.push('\\');
                // Copy the whole next char (it may be multi-byte).
                let Some(ch) = p[i + 1..].chars().next() else { break };
                out.push(ch);
                i += ch.len_utf8() - 1;
            }
            i += 2;
            continue;
        }
        if c == b'[' && !in_class {
            in_class = true;
            out.push('[');
            i += 1;
            // A leading ^ and a leading ] are literal parts of the class head.
            if i < b.len() && b[i] == b'^' {
                out.push('^');
                i += 1;
            }
            if i < b.len() && b[i] == b']' {
                out.push_str("\\]");
                i += 1;
            }
            continue;
        }
        if c == b']' && in_class {
            in_class = false;
        }
        let Some(ch) = p[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Compiles a pattern known at build time.
///
/// # Panics
///
/// On a bad pattern, which is a bug in this source: none is built from client text
/// without escaping.
#[allow(clippy::panic)]
pub fn compile_or_panic(p: &str) -> Regex {
    Regex::new(&ascii_pattern(p)).unwrap_or_else(|e| panic!("bad regex {p:?}: {e}"))
}

/// Declares a static regex, compiled on first use: `lazy_regex!(RE_X, r"...");`
#[macro_export]
macro_rules! lazy_regex {
    ($vis:vis $name:ident, $pat:expr) => {
        $vis static $name: std::sync::LazyLock<$crate::text::Regex> =
            std::sync::LazyLock::new(|| $crate::text::compile_or_panic($pat));
    };
}

/// A String from bytes built out of valid text, cut only at ASCII delimiters.
pub fn string_from_cut_utf8(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Replaces every match, expanding `$1`/`${1}` in rep.
pub fn replace_all(re: &Regex, s: &str, rep: &str) -> String {
    re.replace_all(s, rep).into_owned()
}

/// Replaces every match with what f returns; f sees the whole match.
pub fn replace_all_func(re: &Regex, s: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in re.find_iter(s) {
        out.push_str(&s[last..m.start()]);
        out.push_str(&f(m.as_str()));
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

/// `s` as an SQL string literal, in single quotes with each quote doubled:
/// `it's` is `'it''s'`.
pub fn quote_literal(s: &str) -> String {
    ["'", &s.replace('\'', "''"), "'"].concat()
}

/// The index just past the single-quoted string literal opening at i, its doubled
/// quotes inside it; the end of sql for one never closed.
pub fn end_of_string_literal(sql: &str, i: usize) -> usize {
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

/// `s` as an SQL identifier, always in double quotes with each quote doubled:
/// `a"b` is `"a""b"`. Postgres' quote_ident(), which leaves a plain lower-case
/// name bare, is `quote_ident_if_needed` in the engine's catalog.
pub fn quote_ident(s: &str) -> String {
    ["\"", &s.replace('"', "\"\""), "\""].concat()
}

/// Splits around runs of Unicode whitespace.
pub fn split_words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Case-insensitive equality, for the ASCII keywords this code base compares.
pub fn eq_fold(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || (!a.is_ascii() || !b.is_ascii()) && a.to_lowercase() == b.to_lowercase()
}

/// Trims the characters of cutset from one or both ends of s.
pub fn trim_end_chars<'a>(s: &'a str, cutset: &str) -> &'a str {
    s.trim_end_matches(|c| cutset.contains(c))
}
pub fn trim_start_chars<'a>(s: &'a str, cutset: &str) -> &'a str {
    s.trim_start_matches(|c| cutset.contains(c))
}
pub fn trim_chars<'a>(s: &'a str, cutset: &str) -> &'a str {
    s.trim_matches(|c| cutset.contains(c))
}

/// A byte search from an offset: the absolute index of `needle` at
/// or after `from`, usable with `str::find` semantics on a byte buffer.
pub fn find_from(s: &str, needle: &str, from: usize) -> Option<usize> {
    if from > s.len() {
        return None;
    }
    let hay = &s.as_bytes()[from..];
    let n = needle.as_bytes();
    if n.is_empty() {
        return Some(from);
    }
    hay.windows(n.len()).position(|w| w == n).map(|p| p + from)
}

/// `&s[a..b]` that never panics: when a computed byte offset lands inside a multi-byte
/// char the slice is widened to the char boundary, and a range that ends before it
/// starts or past the text is cut to what there is.
pub fn slice(s: &str, a: usize, b: usize) -> &str {
    let mut b = b.min(s.len());
    let mut a = a.min(b);
    while a > 0 && !s.is_char_boundary(a) {
        a -= 1;
    }
    while b < s.len() && !s.is_char_boundary(b) {
        b += 1;
    }
    &s[a..b]
}

#[cfg(test)]
mod raw_bytes_tests {
    use super::*;

    #[test]
    fn bytes_survive_the_trip_through_a_string() {
        let cases: [&[u8]; 8] = [
            b"plain",
            "a\u{e7}\u{e3}o".as_bytes(),
            b"a\xffb\xc3c\x80\xc0\xafd\xe2\x82",
            b"\xf4",
            b"\xf4\x8f",
            b"x\xf4\x8f\xbe\x80y\xf4\x8f\xbf\xbfz", // U+10FF80, U+10FFFF
            b"\xf4\x8f\xbf\xbf\xff",
            b"\xf4\x90\x80\x80", // past U+10FFFF
        ];
        for b in cases {
            assert_eq!(&*text_to_bytes(&text_from_bytes(b)), b, "{b:?}");
        }
    }

    #[test]
    fn plain_text_is_not_copied() {
        for b in [&b"plain"[..], "\u{10000}\u{10ff7f}".as_bytes()] {
            assert!(matches!(text_from_bytes(b), Cow::Borrowed(_)));
            assert!(matches!(
                text_to_bytes(std::str::from_utf8(b).unwrap()),
                Cow::Borrowed(_)
            ));
        }
    }

    #[test]
    fn utf8_a_splitter_sees_stays_intact() {
        // An ASCII delimiter keeps its meaning next to a bad byte.
        let s = text_from_bytes(b"\xff,\xfe");
        let parts: Vec<&str> = s.split(',').collect();
        assert_eq!(&*text_to_bytes(parts[0]), b"\xff");
        assert_eq!(&*text_to_bytes(parts[1]), b"\xfe");
    }

    #[test]
    fn bad_bytes_become_one_replacement_each() {
        let s = text_from_bytes(b"a\xe2\x82b\xff");
        assert_eq!(valid_utf8(&s), "a\u{fffd}\u{fffd}b\u{fffd}");
        let genuine = text_from_bytes("\u{10ff80}".as_bytes());
        assert_eq!(&*text_to_bytes(&valid_utf8(&genuine)), "\u{10ff80}".as_bytes());
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;

    #[test]
    fn quoted_escapes_control_and_bad_bytes() {
        for (s, want) in [
            ("plain", r#""plain""#),
            ("a\"b\\c'", r#""a\"b\\c'""#),
            ("\0\u{1}\u{7f}\u{1b}", r#""\x00\x01\x7f\x1b""#),
            ("\u{7}\u{8}\u{c}\n\r\t\u{b}", r#""\a\b\f\n\r\t\v""#),
            ("a\u{e7}\u{e3}o \u{1f600}", "\"a\u{e7}\u{e3}o \u{1f600}\""),
            (
                "\u{80}\u{ad}\u{200b}\u{feff}\u{a0}",
                concat!("\"", "\\u0080", "\\u00ad", "\\u200b", "\\ufeff", "\\u00a0", "\""),
            ),
            ("\u{e0001}", r#""\U000e0001""#),
            ("\u{301}", "\"\u{301}\""),
        ] {
            assert_eq!(format!("{:?}", quoted(s)), want);
        }
        assert_eq!(format!("{:?}", quoted(text_from_bytes(b"a\xffb"))), r#""a\xffb""#);
        assert_eq!(format!("{:?}", quoted_char('\0')), r"'\x00'");
        assert_eq!(format!("{:?}", quoted_char('\'')), r"'\''");
    }
}
