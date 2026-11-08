//! Postgres' default text search parser and the two configurations here, `simple` and
//! `english`: the tokens of a text, and the lexemes each configuration makes of them.
//!
//! The parser is a reading of Postgres' (src/backend/tsearch/wparser_def.c) by its
//! token types and the order it tries them in, held to its answers on a corpus: words
//! (ASCII, any letters, with digits), hyphenated words and their parts, emails, URLs,
//! hosts, file paths, numbers in their forms, and the XML tags and entities it skips.

use rust_stemmers::{Algorithm, Stemmer};
use std::collections::HashSet;
use std::sync::LazyLock;

/// A token type of the default parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    AsciiWord,
    Word,
    NumWord,
    AsciiHWord,
    HWord,
    NumHWord,
    HWordAsciiPart,
    HWordPart,
    HWordNumPart,
    Email,
    Url,
    Host,
    UrlPath,
    File,
    SFloat,
    Float,
    Int,
    UInt,
    Version,
}

/// A text search configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Config {
    Simple,
    English,
}

impl Config {
    /// The configuration a regconfig names, `pg_catalog.` or quotes and all.
    pub(crate) fn named(name: &str) -> Option<Config> {
        let n = name.trim().trim_matches('"');
        let n = n.strip_prefix("pg_catalog.").unwrap_or(n).trim_matches('"');
        match n.to_ascii_lowercase().as_str() {
            "simple" => Some(Config::Simple),
            "english" => Some(Config::English),
            _ => None,
        }
    }
}

/// The words english.stop holds, Postgres' English stop words.
const ENGLISH_STOP: &str = include_str!("english.stop");

static STOP_WORDS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ENGLISH_STOP.lines().map(str::trim).filter(|w| !w.is_empty()).collect());

static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));

/// The longest lexeme Postgres indexes, in bytes; a longer word is skipped.
const MAX_WORD: usize = 2047;

/// Lexemes of a text with the position each takes: a stop word takes one and gives no
/// lexeme, a skipped token (a tag, an entity, a protocol, a space) takes none.
pub(crate) fn lexemes(config: Config, text: &str) -> Vec<(u32, Option<String>)> {
    let mut out = Vec::new();
    let mut pos = 0u32;
    for (tok, word) in tokens(text) {
        if word.len() >= MAX_WORD {
            continue;
        }
        pos += 1;
        out.push((pos, lexize(config, tok, &word)));
    }
    out
}

/// What the configuration's dictionary for a token type makes of a word: None for a
/// stop word.
fn lexize(config: Config, tok: Tok, word: &str) -> Option<String> {
    let lower = word.to_lowercase();
    let stemmed = matches!(
        tok,
        Tok::AsciiWord | Tok::Word | Tok::AsciiHWord | Tok::HWord | Tok::HWordAsciiPart | Tok::HWordPart
    );
    if config == Config::English && stemmed {
        if lower.len() > 1000 || STOP_WORDS.contains(lower.as_str()) {
            return None;
        }
        return Some(STEMMER.stem(&lower).into_owned());
    }
    Some(lower)
}

fn is_letter(c: char) -> bool {
    c.is_alphabetic()
}

fn is_alnum(c: char) -> bool {
    c.is_alphabetic() || c.is_ascii_digit()
}

fn is_ascii_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// The tokens a text holds, in order, with the ones the configurations skip left out.
pub(crate) fn tokens(text: &str) -> Vec<(Tok, String)> {
    let c: Vec<char> = text.chars().collect();
    let n = c.len();
    let s = |a: usize, b: usize| c[a..b].iter().collect::<String>();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let ch = c[i];
        if ch == '<' {
            i = tag_end(&c, i).unwrap_or(i + 1);
            continue;
        }
        if ch == '&' {
            i = entity_end(&c, i).unwrap_or(i + 1);
            continue;
        }
        // `~/`, `./` and `../` open a path where a token may start, not after a space.
        let lead = if c[i..].starts_with(&['.', '.', '/']) {
            2
        } else {
            usize::from(c[i..].starts_with(&['.', '/']) || c[i..].starts_with(&['~', '/']))
        };
        if lead > 0
            && (i == 0 || is_alnum(c[i - 1]))
            && let Some(e) = path_end(&c, i + lead)
        {
            out.push((Tok::File, s(i, e)));
            i = e;
            continue;
        }
        if ch == '/' {
            match path_end(&c, i) {
                Some(e) => {
                    out.push((Tok::File, s(i, e)));
                    i = e;
                }
                None => i += 1,
            }
            continue;
        }
        if (ch == '-' || ch == '+') && c.get(i + 1).is_some_and(char::is_ascii_digit) {
            let (kind, e) = number(&c, i + 1, true);
            out.push((kind, s(i, e)));
            i = e;
            continue;
        }
        if !is_alnum(ch) {
            i += 1;
            continue;
        }
        // A float or a scientific number, which no host or email starts with.
        if starts_number(&c, i) {
            let (kind, e) = number(&c, i, false);
            out.push((kind, s(i, e)));
            i = e;
            continue;
        }
        if let Some(e) = email_end(&c, i) {
            out.push((Tok::Email, s(i, e)));
            i = e;
            continue;
        }
        if let Some(h) = host_end(&c, i) {
            if let Some(p) = url_path_end(&c, h) {
                out.push((Tok::Url, s(i, p)));
                out.push((Tok::Host, s(i, h)));
                out.push((Tok::UrlPath, s(h, p)));
                i = p;
            } else {
                out.push((Tok::Host, s(i, h)));
                i = h;
            }
            continue;
        }
        if ch.is_ascii_digit() && !c.get(i + 1).is_some_and(|x| is_letter(*x)) {
            let (kind, e) = number(&c, i, false);
            // Digits that run into letters are a word with digits, not a number.
            let run = alnum_end(&c, i);
            if kind == Tok::UInt && run > e {
                i = word(&c, i, &mut out);
                continue;
            }
            if c.get(e) == Some(&'/')
                && let Some(f) = file_end(&c, i)
            {
                out.push((Tok::File, s(i, f)));
                i = f;
                continue;
            }
            out.push((kind, s(i, e)));
            i = e;
            continue;
        }
        if let Some(f) = file_end(&c, i) {
            out.push((Tok::File, s(i, f)));
            i = f;
            continue;
        }
        // A protocol, `http://`, is skipped, and what follows read on its own.
        let run = alnum_end(&c, i);
        if c[i..run].iter().all(char::is_ascii_alphabetic) && c[run..].starts_with(&[':', '/', '/']) {
            i = run + 3;
            continue;
        }
        i = word(&c, i, &mut out);
    }
    out
}

/// Whether the digits at i start a float or a scientific number, which a letter may
/// follow: `1.2a`, `1e5x`.
fn starts_number(c: &[char], i: usize) -> bool {
    if !c[i].is_ascii_digit() {
        return false;
    }
    let mut j = i;
    while j < c.len() && c[j].is_ascii_digit() {
        j += 1;
    }
    match c.get(j) {
        Some('.') => c.get(j + 1).is_some_and(char::is_ascii_digit),
        Some('e' | 'E') => {
            let k = if matches!(c.get(j + 1), Some('+' | '-')) {
                j + 2
            } else {
                j + 1
            };
            c.get(k).is_some_and(char::is_ascii_digit)
        }
        _ => false,
    }
}

/// A number from the digits at i: a version, a float, a scientific one, an integer.
fn number(c: &[char], i: usize, signed: bool) -> (Tok, usize) {
    let digits = |mut j: usize| {
        while j < c.len() && c[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let mut j = digits(i);
    let mut kind = if signed { Tok::Int } else { Tok::UInt };
    if c.get(j) == Some(&'.') && c.get(j + 1).is_some_and(char::is_ascii_digit) {
        j = digits(j + 1);
        kind = Tok::Float;
        if !signed && c.get(j) == Some(&'.') && c.get(j + 1).is_some_and(char::is_ascii_digit) {
            while c.get(j) == Some(&'.') && c.get(j + 1).is_some_and(char::is_ascii_digit) {
                j = digits(j + 1);
            }
            return (Tok::Version, j);
        }
    }
    if matches!(c.get(j), Some('e' | 'E')) {
        let k = if matches!(c.get(j + 1), Some('+' | '-')) {
            j + 2
        } else {
            j + 1
        };
        if c.get(k).is_some_and(char::is_ascii_digit) {
            return (Tok::SFloat, digits(k));
        }
    }
    (kind, j)
}

fn alnum_end(c: &[char], mut i: usize) -> usize {
    while i < c.len() && is_alnum(c[i]) {
        i += 1;
    }
    i
}

/// A word at i, hyphenated or not, pushed with its parts; where it ends.
fn word(c: &[char], i: usize, out: &mut Vec<(Tok, String)>) -> usize {
    let mut parts: Vec<(usize, usize)> = vec![(i, alnum_end(c, i))];
    let mut end = parts[0].1;
    loop {
        if c.get(end) != Some(&'-') || !c.get(end + 1).is_some_and(|x| is_alnum(*x)) {
            break;
        }
        let e = alnum_end(c, end + 1);
        if !c[end + 1..e].iter().any(|x| is_letter(*x)) {
            // Digits alone end the hyphenated word; after one, the hyphen goes with it.
            if parts.len() > 1 {
                end += 1;
            }
            break;
        }
        parts.push((end + 1, e));
        end = e;
    }
    let text = |a: usize, b: usize| c[a..b].iter().collect::<String>();
    let kind = |a: usize, b: usize| {
        let w = &c[a..b];
        if w.iter().any(char::is_ascii_digit) {
            2
        } else {
            usize::from(!w.iter().all(char::is_ascii_alphabetic))
        }
    };
    if parts.len() == 1 {
        let (a, b) = parts[0];
        let tok = [Tok::AsciiWord, Tok::Word, Tok::NumWord][kind(a, b)];
        out.push((tok, text(a, b)));
        return b;
    }
    let whole = parts.iter().map(|(a, b)| kind(*a, *b)).max().unwrap_or(0);
    let last = parts.last().map_or(i, |p| p.1);
    out.push(([Tok::AsciiHWord, Tok::HWord, Tok::NumHWord][whole], text(i, last)));
    for (a, b) in &parts {
        let tok = [Tok::HWordAsciiPart, Tok::HWordPart, Tok::HWordNumPart][kind(*a, *b)];
        out.push((tok, text(*a, *b)));
    }
    end
}

/// The end of an XML tag at i: `<p>`, `</a>`, `<br/>`, `<a href="x">`, `<!-- -->`.
fn tag_end(c: &[char], i: usize) -> Option<usize> {
    let rest: String = c[i..].iter().take(4).collect();
    if rest.starts_with("<!--") {
        let mut j = i + 4;
        while j + 2 < c.len() {
            if c[j] == '-' && c[j + 1] == '-' && c[j + 2] == '>' {
                return Some(j + 3);
            }
            j += 1;
        }
        return None;
    }
    let mut j = i + 1;
    if matches!(c.get(j), Some('/' | '?')) {
        j += 1;
    }
    if !c
        .get(j)
        .is_some_and(|x| x.is_ascii_alphabetic() || *x == '_' || *x == ':')
    {
        return None;
    }
    while j < c.len() {
        match c[j] {
            '>' => return Some(j + 1),
            '<' => return None,
            '"' | '\'' => {
                let q = c[j];
                j += 1;
                while j < c.len() && c[j] != q {
                    j += 1;
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    None
}

/// The end of an XML entity at i: `&amp;`, `&#169;`, `&#x41;`.
fn entity_end(c: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if c.get(j) == Some(&'#') {
        j += 1;
        let hex = matches!(c.get(j), Some('x' | 'X'));
        if hex {
            j += 1;
        }
        let start = j;
        while j < c.len()
            && (if hex {
                c[j].is_ascii_hexdigit()
            } else {
                c[j].is_ascii_digit()
            })
        {
            j += 1;
        }
        return (j > start && c.get(j) == Some(&';')).then_some(j + 1);
    }
    let start = j;
    while j < c.len() && c[j].is_ascii_alphanumeric() {
        j += 1;
    }
    (j > start && c.get(j) == Some(&';')).then_some(j + 1)
}

/// Dotted ASCII labels from i: where each ends.
fn labels(c: &[char], i: usize, extra: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut j = i;
    loop {
        let start = j;
        while j < c.len() && (is_ascii_alnum(c[j]) || extra.contains(&c[j])) {
            j += 1;
        }
        if j == start || !is_ascii_alnum(c[start]) {
            break;
        }
        out.push((start, j));
        if c.get(j) == Some(&'.') && c.get(j + 1).is_some_and(|x| is_ascii_alnum(*x)) {
            j += 1;
        } else {
            break;
        }
    }
    out
}

/// A host at i: two labels or more, the last of two characters or more with a letter,
/// and a port.
fn host_end(c: &[char], i: usize) -> Option<usize> {
    let ls = labels(c, i, &['-', '_']);
    if ls.len() < 2 {
        return None;
    }
    let (a, b) = *ls.last()?;
    if b - a < 2 || !c[a..b].iter().any(char::is_ascii_alphabetic) {
        return None;
    }
    // A label of a host is not a word running on in letters a host takes no part of.
    if c.get(b).is_some_and(|x| is_letter(*x) || *x == '@') {
        return None;
    }
    let mut end = b;
    if c.get(end) == Some(&':') && c.get(end + 1).is_some_and(char::is_ascii_digit) {
        end += 1;
        while end < c.len() && c[end].is_ascii_digit() {
            end += 1;
        }
    }
    Some(end)
}

/// An email at i: a local part, an `@`, and a host.
fn email_end(c: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    while j < c.len() && (is_ascii_alnum(c[j]) || matches!(c[j], '.' | '_' | '-')) {
        j += 1;
    }
    if c.get(j) != Some(&'@') || j == i {
        return None;
    }
    let ls = labels(c, j + 1, &['-']);
    if ls.len() < 2 {
        return None;
    }
    let (a, b) = *ls.last()?;
    (b - a >= 2 && c[a..b].iter().any(char::is_ascii_alphabetic)).then_some(b)
}

fn is_path_char(x: char) -> bool {
    is_ascii_alnum(x)
        || matches!(
            x,
            '/' | '.'
                | '_'
                | '-'
                | '~'
                | '?'
                | '='
                | '&'
                | '%'
                | '+'
                | ':'
                | '#'
                | ','
                | ';'
                | '!'
                | '*'
                | '@'
                | '$'
                | '('
                | ')'
        )
}

/// The path of a URL after its host at h.
fn url_path_end(c: &[char], h: usize) -> Option<usize> {
    if c.get(h) != Some(&'/') {
        return None;
    }
    let mut j = h + 1;
    while j < c.len() && is_path_char(c[j]) {
        j += 1;
    }
    // What closes a sentence is not the path's.
    while j > h + 1 && matches!(c[j - 1], '.' | ',' | ';' | '!' | '?' | ':') {
        j -= 1;
    }
    (j > h + 1).then_some(j)
}

fn is_file_char(x: char) -> bool {
    is_ascii_alnum(x) || matches!(x, '.' | '_' | '-')
}

/// A path at a `/`: `/usr/local/bin`, `/notes.md`.
fn path_end(c: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    let mut any = false;
    while c.get(j) == Some(&'/') {
        let start = j + 1;
        let mut k = start;
        while k < c.len() && is_file_char(c[k]) {
            k += 1;
        }
        if k == start {
            break;
        }
        any = true;
        j = k;
    }
    let j = trim_dots(c, i, j);
    any.then_some(j)
}

fn trim_dots(c: &[char], start: usize, mut j: usize) -> usize {
    while j > start + 1 && matches!(c[j - 1], '.' | '-') {
        j -= 1;
    }
    j
}

/// A file name at i that is no host: dotted with a short last part (`a.b.c`,
/// `U.S.A`, `v1.2.3`), or with a slash (`foo/bar.txt`, `8080/p`).
fn file_end(c: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    while j < c.len() && is_file_char(c[j]) {
        j += 1;
    }
    let first = trim_dots(c, i, j);
    let dotted = c[i..first].contains(&'.');
    if c.get(j) == Some(&'/')
        && let Some(e) = path_end(c, j)
    {
        return Some(e);
    }
    if !dotted {
        return None;
    }
    // Dots between ASCII letters and digits only, with no empty part.
    let name = &c[i..first];
    if name.iter().any(|x| !x.is_ascii()) || name.windows(2).any(|w| w[0] == '.' && w[1] == '.') {
        return None;
    }
    if name.iter().any(|x| *x == '-' || *x == '_') {
        return None;
    }
    // Followed by more letters, it is a word that runs on.
    if c.get(first).is_some_and(|x| is_letter(*x)) {
        return None;
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        tokens(text).into_iter().map(|(k, w)| format!("{k:?}:{w}")).collect()
    }

    #[test]
    fn tokens_as_postgres_reads_them() {
        assert_eq!(
            kinds("Pies (Meat-Free) 14th-century 6th [6] 1361."),
            [
                "AsciiWord:Pies",
                "AsciiHWord:Meat-Free",
                "HWordAsciiPart:Meat",
                "HWordAsciiPart:Free",
                "NumHWord:14th-century",
                "HWordNumPart:14th",
                "HWordAsciiPart:century",
                "NumWord:6th",
                "UInt:6",
                "UInt:1361"
            ]
        );
        assert_eq!(
            kinds("foo.bar@example.com http://www.example.com/path/to?x=1 now"),
            [
                "Email:foo.bar@example.com",
                "Url:www.example.com/path/to?x=1",
                "Host:www.example.com",
                "UrlPath:/path/to?x=1",
                "AsciiWord:now"
            ]
        );
        assert_eq!(
            kinds("1.2.3 3.14 -5 +7 1e10 2.5E-3 10,000"),
            [
                "Version:1.2.3",
                "Float:3.14",
                "Int:-5",
                "Int:+7",
                "SFloat:1e10",
                "SFloat:2.5E-3",
                "UInt:10",
                "UInt:000"
            ]
        );
        assert_eq!(kinds("<p>Hello &amp; <b>x</b></p>"), ["AsciiWord:Hello", "AsciiWord:x"]);
        assert_eq!(
            kinds("abc-123 foo-bar-123"),
            [
                "AsciiWord:abc",
                "Int:-123",
                "AsciiHWord:foo-bar",
                "HWordAsciiPart:foo",
                "HWordAsciiPart:bar",
                "UInt:123"
            ]
        );
        assert_eq!(
            kinds("ab.cd ab.c U.S.A. readme.txt"),
            ["Host:ab.cd", "File:ab.c", "File:U.S.A", "Host:readme.txt"]
        );
    }
}
