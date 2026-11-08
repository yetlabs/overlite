//! Postgres' regular expressions anchor on words with escapes a linear-time
//! engine does not have: \m (start of a word), \M (end of one), \y (either) and
//! \Y (neither). A pattern that uses one runs on a backtracking engine with
//! lookarounds, fancy-regex, with each escape spelled as one. Every other
//! pattern stays on the `regex` crate.
//!
//! A word character is a letter, a digit or an underscore in any script, as in
//! Postgres with a UTF-8 database. \Z, the end of the string, is the crate's \z.

use crate::error::{Error, Result, errf};
use overlite_core::SqlState;
use overlite_core::sync::MutexExt;
use overlite_core::text::{ascii_pattern, has_raw_bytes, text_to_bytes};
use regex::Regex;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

const WORD_CLASS: &str = r"[\p{L}\p{N}_]";

/// Start of a word: `(?<!W)(?=W)`.
static WORD_START: LazyLock<String> = LazyLock::new(|| format!("(?<!{WORD_CLASS})(?={WORD_CLASS})"));
/// End of a word: `(?<=W)(?!W)`.
static WORD_END: LazyLock<String> = LazyLock::new(|| format!("(?<={WORD_CLASS})(?!{WORD_CLASS})"));
static WORD_BOUND: LazyLock<String> = LazyLock::new(|| format!("(?:{}|{})", *WORD_START, *WORD_END));
static WORD_INTERIOR: LazyLock<String> =
    LazyLock::new(|| format!("(?:(?<={WORD_CLASS})(?={WORD_CLASS})|(?<!{WORD_CLASS})(?!{WORD_CLASS}))"));

/// Bounds a backtracking match, which a linear engine never needs. fancy-regex
/// counts backtracks rather than time, so 5s is expressed as a backtrack budget.
const MATCH_TIMEOUT_MSG: &str = "5s";
/// What a match may spend on the caller's thread, about a tenth of a second.
/// Past it the match restarts on a thread of its own, which can be waited for.
const MATCH_BACKTRACK_LIMIT: usize = 1_000_000;
/// How long a backtracking match may take. The regex crates cannot be stopped
/// from outside, so the caller stops waiting at the deadline.
const MATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Ends a match nobody waits for any more; far past MATCH_TIMEOUT.
const ABANDONED_MATCH_LIMIT: usize = 1_000_000_000;

/// Rewrites a Postgres pattern for the regex engines. The flag reports that it uses a
/// word anchor, so it needs the backtracking engine.
fn translate_regex(pattern: &str) -> (String, bool) {
    let p = pattern.as_bytes();
    let mut b: Vec<u8> = Vec::with_capacity(p.len());
    let mut lookaround = false;
    let mut bracket = false;
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        if c == b'\\' && i + 1 < p.len() {
            let next = p[i + 1];
            i += 1;
            if bracket {
                b.push(c);
                b.push(next);
                i += 1;
                continue;
            }
            match next {
                b'm' => {
                    b.extend_from_slice(WORD_START.as_bytes());
                    lookaround = true;
                }
                b'M' => {
                    b.extend_from_slice(WORD_END.as_bytes());
                    lookaround = true;
                }
                b'y' => {
                    b.extend_from_slice(WORD_BOUND.as_bytes());
                    lookaround = true;
                }
                b'Y' => {
                    b.extend_from_slice(WORD_INTERIOR.as_bytes());
                    lookaround = true;
                }
                b'Z' => b.extend_from_slice(br"\z"),
                _ => {
                    b.push(c);
                    b.push(next);
                }
            }
        } else if c == b'[' && !bracket {
            bracket = true;
            b.push(c);
            // A ] right after [ or [^ is a member, not the end.
            if i + 1 < p.len() && p[i + 1] == b'^' {
                b.push(b'^');
                i += 1;
            }
            if i + 1 < p.len() && p[i + 1] == b']' {
                b.push(b']');
                i += 1;
            }
        } else if c == b']' && bracket {
            bracket = false;
            b.push(c);
        } else {
            b.push(c);
        }
        i += 1;
    }
    (String::from_utf8_lossy(&b).into_owned(), lookaround)
}

/// A pattern is bounded by check_pattern_syntax; the crate's smaller defaults
/// must not refuse what passes it.
const RE2_SIZE_LIMIT: usize = 1 << 30;
const RE2_NEST_LIMIT: u32 = 1000;

/// A compiled pattern on either engine.
#[derive(Debug)]
pub(crate) enum MatcherKind {
    Linear(Regex),
    Backtrack(BacktrackMatcher),
}

type Matcher = Arc<MatcherKind>;

impl MatcherKind {
    /// `m(s)`.
    pub(crate) fn is_match(&self, s: &str) -> Result<bool> {
        match self {
            MatcherKind::Linear(re) => Ok(re.is_match(s)),
            MatcherKind::Backtrack(re) => match re.quick.is_match(s) {
                Err(fancy_regex::Error::RuntimeError(_)) => re.match_with_deadline(s),
                other => other.map_err(|e| errf!(SqlState::INVALID_REGULAR_EXPRESSION, "{e}")),
            },
        }
    }
}

impl MatcherKind {
    /// Where the matches in s stand, one after another.
    pub(crate) fn find_spans(&self, s: &str) -> Result<Vec<(usize, usize)>> {
        match self {
            MatcherKind::Linear(re) => Ok(re.find_iter(s).map(|m| (m.start(), m.end())).collect()),
            MatcherKind::Backtrack(re) => re
                .quick
                .find_iter(s)
                .map(|m| {
                    m.map(|m| (m.start(), m.end()))
                        .map_err(|e| errf!(SqlState::INVALID_REGULAR_EXPRESSION, "{e}"))
                })
                .collect(),
        }
    }
}

/// Where each group of one match stands, group 0 the whole match.
type GroupSpans = Vec<Option<(usize, usize)>>;

impl MatcherKind {
    /// regexp_replace: the first match, or every one, replaced by repl, where `\n`
    /// is the nth group and `\&` the whole match, as Postgres reads them.
    pub(crate) fn replace_pg(&self, s: &str, repl: &str, global: bool) -> Result<String> {
        let mut out = String::with_capacity(s.len());
        let mut at = 0;
        let groups = self.capture_spans(s, global)?;
        for g in groups {
            let Some((b, e)) = g[0] else { continue };
            out.push_str(&s[at..b]);
            let r = repl.as_bytes();
            let mut i = 0;
            while i < r.len() {
                if r[i] == b'\\' && i + 1 < r.len() {
                    let c = r[i + 1];
                    if c.is_ascii_digit() {
                        if let Some(Some((gb, ge))) = g.get(usize::from(c - b'0')) {
                            out.push_str(&s[*gb..*ge]);
                        }
                        i += 2;
                        continue;
                    }
                    if c == b'&' {
                        out.push_str(&s[b..e]);
                        i += 2;
                        continue;
                    }
                    if c == b'\\' {
                        out.push('\\');
                        i += 2;
                        continue;
                    }
                }
                let ch = repl[i..].chars().next().unwrap_or_default();
                out.push(ch);
                i += ch.len_utf8().max(1);
            }
            at = e;
        }
        out.push_str(&s[at..]);
        Ok(out)
    }

    /// regexp_match: the groups of the first match, or the match itself when the
    /// pattern has none.
    pub(crate) fn captures_pg(&self, s: &str) -> Result<Option<Vec<Option<String>>>> {
        let Some(g) = self.capture_spans(s, false)?.into_iter().next() else {
            return Ok(None);
        };
        let text = |sp: &Option<(usize, usize)>| sp.map(|(b, e)| s[b..e].to_string());
        if g.len() == 1 {
            return Ok(Some(vec![text(&g[0])]));
        }
        Ok(Some(g[1..].iter().map(text).collect()))
    }

    /// The spans of each match's groups, group 0 the whole match: the first match,
    /// or all of them.
    fn capture_spans(&self, s: &str, all: bool) -> Result<Vec<GroupSpans>> {
        let mut out = Vec::new();
        match self {
            MatcherKind::Linear(re) => {
                for c in re.captures_iter(s) {
                    out.push(c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect());
                    if !all {
                        break;
                    }
                }
            }
            MatcherKind::Backtrack(re) => {
                for c in re.quick.captures_iter(s) {
                    let c = c.map_err(|e| errf!(SqlState::INVALID_REGULAR_EXPRESSION, "{e}"))?;
                    out.push(c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect());
                    if !all {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }
}

impl BacktrackMatcher {
    /// A match too long for the quick limit, given until MATCH_TIMEOUT.
    #[cold]
    fn match_with_deadline(&self, s: &str) -> Result<bool> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (re, text) = (self.slow.clone(), s.to_string());
        std::thread::Builder::new()
            .spawn(move || {
                let _ = tx.send(re.is_match(&text));
            })
            .map_err(|e| errf!(SqlState::INVALID_REGULAR_EXPRESSION, "{e}"))?;
        let timeout = || {
            errf!(
                SqlState::QUERY_CANCELED,
                "match timeout after {MATCH_TIMEOUT_MSG} on input `{s}`"
            )
        };
        match rx.recv_timeout(MATCH_TIMEOUT) {
            Ok(Ok(m)) => Ok(m),
            Ok(Err(fancy_regex::Error::RuntimeError(_))) | Err(_) => Err(timeout()),
            Ok(Err(e)) => Err(errf!(SqlState::INVALID_REGULAR_EXPRESSION, "{e}")),
        }
    }
}

/// A word-anchor pattern, built twice: with the limit for the common case, and
/// with one meant to outlast the deadline.
#[derive(Debug)]
pub(crate) struct BacktrackMatcher {
    quick: fancy_regex::Regex,
    slow: Arc<fancy_regex::Regex>,
}

/// A repeated lookaround matches nothing, and fancy-regex rejects it. For a match the outcome is the same without the repeat: at
/// least once is the assertion itself, zero times is no assertion at all.
fn unrepeat_lookarounds(p: &str) -> String {
    let b = p.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    // Where in `out` each open group starts.
    let mut open: Vec<usize> = Vec::new();
    let mut in_class = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            let start = out.len();
            out.extend_from_slice(&b[i..i + 2]);
            i += 2;
            if !in_class
                && matches!(b[i - 1], b'A' | b'z' | b'Z')
                && let Some((min, len)) = repeat_at(&b[i..])
            {
                if min == 0 {
                    out.truncate(start);
                }
                i += len;
            }
            continue;
        }
        if !in_class && (c == b'^' || c == b'$') {
            out.push(c);
            i += 1;
            if let Some((min, len)) = repeat_at(&b[i..]) {
                if min == 0 {
                    out.pop();
                }
                i += len;
            }
            continue;
        }
        if in_class {
            in_class = c != b']';
        } else if c == b'[' {
            in_class = true;
            out.push(c);
            i += 1;
            if b.get(i) == Some(&b'^') {
                out.push(b'^');
                i += 1;
            }
            if b.get(i) == Some(&b']') {
                out.push(b']');
                i += 1;
            }
            continue;
        } else if c == b'(' {
            open.push(out.len());
        } else if c == b')'
            && let Some(start) = open.pop()
        {
            out.push(c);
            i += 1;
            if is_zero_width(&out[start..])
                && let Some((min, len)) = repeat_at(&b[i..])
            {
                if min == 0 {
                    out.truncate(start);
                }
                i += len;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| p.to_string())
}

fn is_zero_width(group: &[u8]) -> bool {
    [&b"(?="[..], b"(?!", b"(?<=", b"(?<!"]
        .iter()
        .any(|h| group.starts_with(h))
        || group == WORD_BOUND.as_bytes()
        || group == WORD_INTERIOR.as_bytes()
}

/// A quantifier at the start of b, with its lazy suffix: (minimum, length).
fn repeat_at(b: &[u8]) -> Option<(u64, usize)> {
    let (min, mut n) = match *b.first()? {
        b'*' | b'?' => (0, 1),
        b'+' => (1, 1),
        b'{' if is_repeat(b) => {
            let end = b.iter().position(|c| *c == b'}')?;
            let digits = b[1..end].iter().take_while(|c| c.is_ascii_digit());
            (
                digits.fold(0u64, |m, c| m.saturating_mul(10).saturating_add((*c - b'0') as u64)),
                end + 1,
            )
        }
        _ => return None,
    };
    if b.get(n) == Some(&b'?') {
        n += 1;
    }
    Some((min, n))
}

static REGEX_CACHE: LazyLock<Mutex<HashMap<String, Matcher>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bounds the cache; a statement builds its patterns from data rarely, and then
/// the cache starts over rather than grow.
const REGEX_CACHE_MAX: usize = 512;

/// Normalizes what the regex crate would parse differently from what clients
/// expect: `\Q...\E`, a brace that does not form a repetition (a literal), and,
/// in a class, a `[` and the `&&`, `--`, `~~` set operators the crate would act
/// on (literals too).
fn re2_syntax_for_rust(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = String::with_capacity(p.len() + 8);
    let named_refs = p.contains(r"\k");
    let mut in_class = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            if !in_class && b[i + 1] == b'Q' {
                let rest = &p[i + 2..];
                let (lit, next) = match rest.find(r"\E") {
                    Some(e) => (&rest[..e], i + 2 + e + 2),
                    None => (rest, b.len()),
                };
                out.push_str(&::regex::escape(lit));
                i = next;
                continue;
            }
            out.push('\\');
            let Some(ch) = p[i + 1..].chars().next() else { break };
            out.push(ch);
            i += 1 + ch.len_utf8();
            // \p{Name} and \x{10FFFF} carry their braces with them.
            if matches!(ch, 'p' | 'P' | 'x')
                && b.get(i) == Some(&b'{')
                && let Some(e) = p[i..].find('}')
            {
                out.push_str(&p[i..=(i + e)]);
                i += e + 1;
            }
            continue;
        }
        if in_class {
            match c {
                b'[' => {
                    if b.get(i + 1) == Some(&b':')
                        && let Some(e) = p[i..].find(":]")
                    {
                        out.push_str(&p[i..i + e + 2]);
                        i += e + 2;
                        continue;
                    }
                    out.push_str(r"\[");
                    i += 1;
                    continue;
                }
                b']' => in_class = false,
                b'&' | b'-' | b'~' if b.get(i + 1) == Some(&c) => {
                    out.push(c as char);
                    out.push('\\');
                    out.push(c as char);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        } else if c == b'[' {
            in_class = true;
            out.push('[');
            i += 1;
            if i < b.len() && b[i] == b'^' {
                out.push('^');
                i += 1;
            }
            if i < b.len() && b[i] == b']' {
                out.push_str(r"\]");
                i += 1;
            }
            continue;
        } else if c == b'{' && !is_repeat(&b[i..]) {
            out.push_str(r"\{");
            i += 1;
            continue;
        } else if c == b'(' && !named_refs {
            // Only a match is asked for, and the crates refuse a name twice or
            // a number next to a name, which are accepted here.
            let name = p[i..].strip_prefix("(?P<").or_else(|| p[i..].strip_prefix("(?<"));
            if let Some(end) = name
                .filter(|n| !n.starts_with(['=', '!']))
                .and_then(|n| n.find('>').map(|e| p.len() - n.len() + e))
            {
                out.push('(');
                i = end + 1;
                continue;
            }
        }
        let Some(ch) = p[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Whether b starts with `{n}`, `{n,}` or `{n,m}`.
fn is_repeat(b: &[u8]) -> bool {
    let mut i = 1;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    if !digits(&mut i) {
        return false;
    }
    if i < b.len() && b[i] == b',' {
        i += 1;
        digits(&mut i);
    }
    i < b.len() && b[i] == b'}'
}

/// An escape the engine doesn't know reads as the character itself.
fn unknown_escapes_literal(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = String::with_capacity(p.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            if !b[i + 1].is_ascii_alphanumeric() || b"AbBdDsSwWzpPaftnrvxGkKcueZ0123456789".contains(&b[i + 1]) {
                out.push('\\');
            }
            let Some(ch) = p[i + 1..].chars().next() else { break };
            out.push(ch);
            i += 1 + ch.len_utf8();
            continue;
        }
        let Some(ch) = p[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// A fixed wording for the syntax errors a pattern commonly has; the crates' own
/// text for the rest. `pattern` is the translated pattern.
fn regex_syntax_error(pattern: &str, detail: &str, lookaround: bool) -> Error {
    if lookaround {
        // A lookaround pattern's error names the whole pattern.
        let code = if detail.contains("without closing parenthesis") || detail.contains("unclosed group") {
            "missing closing )"
        } else if detail.contains("unclosed character class") || detail.contains("Invalid character class") {
            "unterminated [] set"
        } else if detail.contains("unopened group") || detail.contains("end of string not reached") {
            "unexpected )"
        } else if detail.contains("without following character") {
            "illegal \\ at end of pattern"
        } else if detail.contains("Error compiling regex") {
            return errf!(SqlState::INVALID_REGULAR_EXPRESSION, "error parsing regexp: {detail}");
        } else if detail.contains("Target of repeat") || detail.contains("missing expression") {
            "missing argument to repetition operator"
        } else {
            return errf!(SqlState::INVALID_REGULAR_EXPRESSION, "error parsing regexp: {detail}");
        };
        return errf!(
            SqlState::INVALID_REGULAR_EXPRESSION,
            "error parsing regexp: {code} in `{pattern}`"
        );
    }
    errf!(SqlState::INVALID_REGULAR_EXPRESSION, "error parsing regexp: {detail}")
}

/// What the checker keeps on its stack, as far as its errors depend on it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Parsed {
    /// A finished expression, with the largest product of repeat counts in it.
    Expr(u64),
    LeftParen,
    VerticalBar,
}

/// A syntax check of Perl-style patterns that stops at the first error, so the
/// crate's different grammar and order of checks never reach a client.
#[derive(Debug)]
struct SyntaxCheck<'a> {
    whole: &'a str,
    /// Bytes outside UTF-8 are an error of their own here.
    raw: bool,
    stack: Vec<Parsed>,
}

fn re2_error(code: &str, expr: &str) -> Error {
    errf!(
        SqlState::INVALID_REGULAR_EXPRESSION,
        "error parsing regexp: {code}: `{expr}`"
    )
}

fn past<'a>(s: &'a str, rest: &str) -> &'a str {
    &s[..s.len() - rest.len()]
}

fn is_perl_class(t: &str) -> bool {
    t.get(..2)
        .is_some_and(|c| matches!(c, r"\d" | r"\D" | r"\s" | r"\S" | r"\w" | r"\W"))
}

impl<'a> SyntaxCheck<'a> {
    fn next_rune(&self, s: &'a str) -> Result<(char, &'a str)> {
        let c = s.chars().next().unwrap_or('\0');
        if self.raw && ('\u{10FF80}'..='\u{10FFFF}').contains(&c) {
            return Err(re2_error("invalid UTF-8", s));
        }
        Ok((c, &s[c.len_utf8()..]))
    }

    fn check_utf8(&self, mut s: &'a str) -> Result<()> {
        while !s.is_empty() {
            s = self.next_rune(s)?.1;
        }
        Ok(())
    }

    /// p.concat: what is above the last pseudo-operator becomes one expression.
    fn concat(&mut self) {
        let mut weight = 1;
        while let Some(Parsed::Expr(w)) = self.stack.last() {
            weight = weight.max(*w);
            self.stack.pop();
        }
        self.stack.push(Parsed::Expr(weight));
    }

    /// p.alternate: the same up to the last left paren.
    fn alternate(&mut self) {
        let mut weight = 1;
        while let Some(top) = self.stack.last() {
            match top {
                Parsed::LeftParen => break,
                Parsed::Expr(w) => weight = weight.max(*w),
                Parsed::VerticalBar => {}
            }
            self.stack.pop();
        }
        self.stack.push(Parsed::Expr(weight));
    }

    /// True when a bar sits under the top expression.
    fn swap_vertical_bar(&mut self) -> bool {
        let n = self.stack.len();
        if n >= 2 && self.stack[n - 2] == Parsed::VerticalBar {
            self.stack.swap(n - 1, n - 2);
            return true;
        }
        false
    }

    fn close_alternation(&mut self) {
        self.concat();
        if self.swap_vertical_bar() {
            self.stack.pop();
        }
        self.alternate();
    }

    fn right_paren(&mut self) -> Result<()> {
        self.close_alternation();
        let n = self.stack.len();
        if n < 2 || self.stack[n - 2] != Parsed::LeftParen {
            return Err(re2_error("unexpected )", self.whole));
        }
        let top = self.stack[n - 1];
        self.stack.truncate(n - 2);
        self.stack.push(top);
        Ok(())
    }

    fn repeat(
        &mut self,
        count: Option<(i64, i64)>,
        before: &'a str,
        after: &'a str,
        last: Option<&'a str>,
    ) -> Result<&'a str> {
        let after = after.strip_prefix('?').unwrap_or(after);
        if let Some(l) = last {
            return Err(re2_error("invalid nested repetition operator", past(l, after)));
        }
        let weight = match self.stack.last() {
            Some(Parsed::Expr(w)) => *w,
            _ => {
                return Err(re2_error(
                    "missing argument to repetition operator",
                    past(before, after),
                ));
            }
        };
        let mut new = weight;
        if let Some((min, max)) = count {
            let m = if max < 0 { min } else { max };
            new = if m == 0 { 1 } else { weight.saturating_mul(m as u64) };
            if (min >= 2 || max >= 2) && new > 1000 {
                return Err(re2_error("invalid repeat count", past(before, after)));
            }
        }
        if let Some(top) = self.stack.last_mut() {
            *top = Parsed::Expr(new);
        }
        Ok(after)
    }

    /// `{n}`, `{n,}` or `{n,m}`; -1 is a number too large.
    fn parse_repeat(s: &str) -> Option<(i64, i64, &str)> {
        fn int(s: &str) -> Option<(i64, &str)> {
            let n = s.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 || (n >= 2 && s.as_bytes()[0] == b'0') {
                return None;
            }
            let v = if n >= 8 { -1 } else { s[..n].parse().ok()? };
            Some((v, &s[n..]))
        }
        let (min, t) = int(s.strip_prefix('{')?)?;
        let (max, t) = match t.strip_prefix(',') {
            None => (min, t),
            Some(t) if t.starts_with('}') => (-1, t),
            Some(t) => {
                let (m, t) = int(t)?;
                if m < 0 {
                    (-2, t) // too big, and never a valid range
                } else {
                    (m, t)
                }
            }
        };
        Some((min, max, t.strip_prefix('}')?))
    }

    fn perl_flags(&mut self, s: &'a str) -> Result<&'a str> {
        let b = s.as_bytes();
        let with_p = b.len() > 4 && b[2] == b'P' && b[3] == b'<';
        let with_name = b.len() > 3 && b[2] == b'<';
        if with_p || with_name {
            let start = if with_name { 3 } else { 4 };
            let Some(end) = s.find('>') else {
                self.check_utf8(s)?;
                return Err(re2_error("invalid named capture", s));
            };
            let name = s.get(start..end).unwrap_or("");
            self.check_utf8(name)?;
            if name.is_empty() || !name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()) {
                return Err(re2_error("invalid named capture", &s[..=end]));
            }
            self.stack.push(Parsed::LeftParen);
            return Ok(&s[end + 1..]);
        }
        let mut t = &s[2..];
        let (mut negated, mut saw_flag) = (false, false);
        while !t.is_empty() {
            let (c, rest) = self.next_rune(t)?;
            t = rest;
            match c {
                'i' | 'm' | 's' | 'U' => saw_flag = true,
                '-' if !negated => {
                    negated = true;
                    saw_flag = false;
                }
                ':' | ')' if !negated || saw_flag => {
                    if c == ':' {
                        self.stack.push(Parsed::LeftParen);
                    }
                    return Ok(t);
                }
                _ => break,
            }
        }
        Err(re2_error("invalid or unsupported Perl syntax", past(s, t)))
    }

    /// The escaped code point and what follows.
    fn escape(&self, s: &'a str) -> Result<(u32, &'a str)> {
        let bad = |t: &str| re2_error("invalid escape sequence", past(s, t));
        let t = &s[1..];
        if t.is_empty() {
            return Err(re2_error("trailing backslash at end of expression", ""));
        }
        let (c, mut t) = self.next_rune(t)?;
        let octal = |t: &str| t.as_bytes().first().is_some_and(|d| (b'0'..=b'7').contains(d));
        match c {
            '1'..='7' | '0' if c == '0' || octal(t) => {
                let mut r = c as u32 - '0' as u32;
                for _ in 1..3 {
                    if !octal(t) {
                        break;
                    }
                    r = r * 8 + t.as_bytes()[0] as u32 - '0' as u32;
                    t = &t[1..];
                }
                Ok((r, t))
            }
            'x' => {
                if t.is_empty() {
                    return Err(bad(t));
                }
                let (c, mut t) = self.next_rune(t)?;
                if c == '{' {
                    let (mut r, mut n) = (0u32, 0);
                    loop {
                        if t.is_empty() {
                            return Err(bad(t));
                        }
                        let (c, rest) = self.next_rune(t)?;
                        t = rest;
                        if c == '}' {
                            break;
                        }
                        let Some(v) = c.to_digit(16) else { return Err(bad(t)) };
                        r = r * 16 + v;
                        if r > 0x10FFFF {
                            return Err(bad(t));
                        }
                        n += 1;
                    }
                    if n == 0 {
                        return Err(bad(t));
                    }
                    return Ok((r, t));
                }
                if t.is_empty() {
                    // The end of input reads as U+FFFD, not an error.
                    return Err(re2_error("invalid escape sequence", s));
                }
                let (d, t) = self.next_rune(t)?;
                match (c.to_digit(16), d.to_digit(16)) {
                    (Some(x), Some(y)) => Ok((x * 16 + y, t)),
                    _ => Err(bad(t)),
                }
            }
            'a' => Ok((7, t)),
            'f' => Ok((12, t)),
            'n' => Ok((10, t)),
            'r' => Ok((13, t)),
            't' => Ok((9, t)),
            'v' => Ok((11, t)),
            c if c.is_ascii() && !c.is_ascii_alphanumeric() => Ok((c as u32, t)),
            _ => Err(bad(t)),
        }
    }

    /// Some(rest) when s starts with a Unicode class.
    fn unicode_class(&self, s: &'a str) -> Result<Option<&'a str>> {
        if !(s.starts_with("\\p") || s.starts_with("\\P")) {
            return Ok(None);
        }
        let t = &s[2..];
        if t.is_empty() {
            return Err(re2_error("invalid character class range", s));
        }
        let (c, t) = self.next_rune(t)?;
        let (seq, name, t) = if c == '{' {
            let Some(end) = s.find('}') else {
                self.check_utf8(s)?;
                return Err(re2_error("invalid character class range", s));
            };
            self.check_utf8(s.get(3..end).unwrap_or(""))?;
            (&s[..=end], s.get(3..end).unwrap_or(""), &s[end + 1..])
        } else {
            (past(s, t), &past(s, t)[2..], t)
        };
        let name = name.strip_prefix('^').unwrap_or(name);
        let known = !name.is_empty()
            && (name == "Any" || name == "any" || name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()))
            && Regex::new(&format!(r"\p{{{name}}}")).is_ok();
        if !known {
            return Err(re2_error("invalid character class range", seq));
        }
        Ok(Some(t))
    }

    fn class_char(&self, s: &'a str, whole: &'a str) -> Result<(u32, &'a str)> {
        if s.is_empty() {
            return Err(re2_error("missing closing ]", whole));
        }
        if s.starts_with('\\') {
            return self.escape(s);
        }
        let (c, t) = self.next_rune(s)?;
        Ok((c as u32, t))
    }

    fn class(&mut self, s: &'a str) -> Result<&'a str> {
        let mut t = &s[1..];
        if let Some(r) = t.strip_prefix('^') {
            t = r;
        }
        let mut first = true;
        while t.is_empty() || !t.starts_with(']') || first {
            first = false;
            if t.len() > 2
                && t.starts_with("[:")
                && let Some(i) = t[2..].find(":]")
            {
                let name = &t[..i + 4];
                const POSIX: [&str; 14] = [
                    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
                    "upper", "word", "xdigit",
                ];
                if !POSIX.contains(&name[2..i + 2].trim_start_matches('^')) {
                    return Err(re2_error("invalid character class range", name));
                }
                t = &t[i + 4..];
                continue;
            }
            if let Some(rest) = self.unicode_class(t)? {
                t = rest;
                continue;
            }
            if is_perl_class(t) {
                t = &t[2..];
                continue;
            }
            let range = t;
            let (lo, rest) = self.class_char(t, s)?;
            t = rest;
            if t.len() >= 2 && t.starts_with('-') && !t[1..].starts_with(']') {
                let (hi, rest) = self.class_char(&t[1..], s)?;
                t = rest;
                if hi < lo {
                    return Err(re2_error("invalid character class range", past(range, t)));
                }
            }
        }
        self.stack.push(Parsed::Expr(1));
        Ok(&t[1..])
    }

    fn run(&mut self) -> Result<()> {
        let mut t = self.whole;
        let mut last: Option<&'a str> = None;
        while !t.is_empty() {
            let mut repeat = None;
            match t.as_bytes()[0] {
                b'(' if t.as_bytes().get(1) == Some(&b'?') => t = self.perl_flags(t)?,
                b'(' => {
                    self.stack.push(Parsed::LeftParen);
                    t = &t[1..];
                }
                b'|' => {
                    self.concat();
                    if !self.swap_vertical_bar() {
                        self.stack.push(Parsed::VerticalBar);
                    }
                    t = &t[1..];
                }
                b')' => {
                    self.right_paren()?;
                    t = &t[1..];
                }
                b'^' | b'$' | b'.' => {
                    self.stack.push(Parsed::Expr(1));
                    t = &t[1..];
                }
                b'[' => t = self.class(t)?,
                b'*' | b'+' | b'?' => {
                    let before = t;
                    t = self.repeat(None, before, &t[1..], last)?;
                    repeat = Some(before);
                }
                b'{' => match Self::parse_repeat(t) {
                    None => {
                        self.stack.push(Parsed::Expr(1));
                        t = &t[1..];
                    }
                    Some((min, max, after)) => {
                        if !(0..=1000).contains(&min) || max > 1000 || max == -2 || (max >= 0 && min > max) {
                            return Err(re2_error("invalid repeat count", past(t, after)));
                        }
                        let before = t;
                        t = self.repeat(Some((min, max)), before, after, last)?;
                        repeat = Some(before);
                    }
                },
                b'\\' => {
                    match t.as_bytes().get(1) {
                        Some(b'A' | b'b' | b'B' | b'z') => {
                            self.stack.push(Parsed::Expr(1));
                            t = &t[2..];
                            last = repeat;
                            continue;
                        }
                        Some(b'C') => return Err(re2_error("invalid escape sequence", &t[..2])),
                        Some(b'Q') => {
                            let body = &t[2..];
                            let (lit, rest) = body.split_once(r"\E").unwrap_or((body, ""));
                            self.check_utf8(lit)?;
                            if !lit.is_empty() {
                                self.stack.push(Parsed::Expr(1));
                            }
                            t = rest;
                            last = repeat;
                            continue;
                        }
                        _ => {}
                    }
                    if let Some(rest) = self.unicode_class(t)? {
                        t = rest;
                    } else if is_perl_class(t) {
                        t = &t[2..];
                    } else {
                        t = self.escape(t)?.1;
                    }
                    self.stack.push(Parsed::Expr(1));
                }
                _ => {
                    t = self.next_rune(t)?.1;
                    self.stack.push(Parsed::Expr(1));
                }
            }
            last = repeat;
        }
        self.close_alternation();
        if self.stack.len() != 1 {
            return Err(re2_error("missing closing )", self.whole));
        }
        Ok(())
    }
}

/// The verdict on a pattern, in the wording clients get.
fn check_pattern_syntax(p: &str) -> Result<()> {
    let raw = has_raw_bytes(p.as_bytes()) && std::str::from_utf8(&text_to_bytes(p)).is_err();
    SyntaxCheck {
        whole: p,
        raw,
        stack: Vec::new(),
    }
    .run()
}

/// The errors a word-anchor pattern commonly has, in scanning order, in the
/// wording clients get.
fn regexp2_check(p: &str) -> Result<()> {
    const POSIX: [&str; 14] = [
        "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space", "upper",
        "word", "xdigit",
    ];
    let fail = |code: String| {
        Err(errf!(
            SqlState::INVALID_REGULAR_EXPRESSION,
            "error parsing regexp: {code} in `{p}`"
        ))
    };
    let chars: Vec<char> = p.chars().collect();
    let groups = {
        let (mut n, mut i, mut class) = (0, 0, false);
        while i < chars.len() {
            match chars[i] {
                '\\' => i += 1,
                '[' => class = true,
                ']' => class = false,
                '(' if !class => {
                    let named = chars.get(i + 1) == Some(&'?')
                        && ((chars.get(i + 2) == Some(&'P') && chars.get(i + 3) == Some(&'<'))
                            || (chars.get(i + 2) == Some(&'<') && !matches!(chars.get(i + 3), Some('=' | '!'))));
                    if chars.get(i + 1) != Some(&'?') || named {
                        n += 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        n
    };
    let (mut i, mut unit, mut after_repeat, mut depth) = (0, false, false, 0);
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i > start && chars[start] != '0' {
                    let n: String = chars[start..i].iter().collect();
                    if n.parse::<u64>().map_or(true, |n| n > groups) {
                        return fail(format!("reference to undefined group number {n}"));
                    }
                } else if i == start {
                    i += 1;
                }
                (unit, after_repeat) = (true, false);
                continue;
            }
            '[' => {
                i += 1;
                if chars.get(i) == Some(&'^') {
                    i += 1;
                }
                let mut first = true;
                while i < chars.len() && (chars[i] != ']' || first) {
                    first = false;
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '[' && chars.get(i + 1) == Some(&':') {
                        let rest: String = chars[i + 2..].iter().collect();
                        if let Some((name, _)) = rest.split_once(":]") {
                            if !POSIX.contains(&name.trim_start_matches('^')) {
                                return fail("invalid character class range".to_string());
                            }
                            i += name.chars().count() + 4;
                            continue;
                        }
                    }
                    if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|h| *h != ']' && *h != '\\') {
                        if chars[i] > chars[i + 2] {
                            return fail(format!("[{}-{}] range in reverse order", chars[i], chars[i + 2]));
                        }
                        i += 3;
                        continue;
                    }
                    i += 1;
                }
                (unit, after_repeat) = (true, false);
            }
            '(' => {
                (unit, after_repeat) = (false, false);
                depth += 1;
                if chars.get(i + 1) == Some(&'?') {
                    i += 2;
                    match chars.get(i) {
                        Some('=' | '!') => {}
                        Some('<') if matches!(chars.get(i + 1), Some('=' | '!')) => i += 1,
                        Some('P' | '<') => {
                            while i < chars.len() && chars[i] != '>' {
                                i += 1;
                            }
                        }
                        _ => {
                            while i < chars.len() && chars[i] != ':' && chars[i] != ')' {
                                i += 1;
                            }
                        }
                    }
                }
            }
            '|' => (unit, after_repeat) = (false, false),
            ')' => {
                if depth == 0 {
                    return fail("unexpected )".to_string());
                }
                depth -= 1;
                (unit, after_repeat) = (true, false);
            }
            '*' | '+' | '?' | '{'
                if c != '{'
                    || is_repeat(
                        p.get(chars[..i].iter().map(|c| c.len_utf8()).sum::<usize>()..)
                            .unwrap_or("")
                            .as_bytes(),
                    ) =>
            {
                if !unit {
                    let code = if after_repeat {
                        "invalid nested repetition operator"
                    } else {
                        "missing argument to repetition operator"
                    };
                    return fail(code.to_string());
                }
                if c == '{' {
                    let end = chars[i..].iter().position(|c| *c == '}').map_or(i, |e| i + e);
                    let body: String = chars[i + 1..end].iter().collect();
                    if let Some((n, m)) = body.split_once(',')
                        && let (Ok(n), Ok(m)) = (n.parse::<u64>(), m.parse::<u64>())
                        && n > m
                    {
                        return fail("invalid repeat count".to_string());
                    }
                    i = end;
                }
                if chars.get(i + 1) == Some(&'?') {
                    i += 1;
                }
                (unit, after_repeat) = (false, true);
            }
            _ => (unit, after_repeat) = (true, false),
        }
        i += 1;
    }
    Ok(())
}

/// Compiles a Postgres pattern once and reuses it: a WHERE runs the regexp
/// function once per row with the same pattern.
pub(crate) fn compile_regex(pattern: &str) -> Result<Matcher> {
    if let Some(m) = REGEX_CACHE.locked().get(pattern) {
        return Ok(m.clone());
    }

    let (translated, lookaround) = translate_regex(pattern);
    let m: Matcher = if lookaround {
        regexp2_check(&translated)?;
        // fancy-regex takes no `(?-u:`, the way ascii_pattern spells \\b.
        let fixed = ascii_pattern(&re2_syntax_for_rust(&unknown_escapes_literal(&translated)))
            .replace(r"(?-u:\b)", &WORD_BOUND)
            .replace(r"(?-u:\B)", &WORD_INTERIOR);
        let source = unrepeat_lookarounds(&fixed);
        let build = |limit| {
            fancy_regex::RegexBuilder::new(&source)
                .backtrack_limit(limit)
                .build()
                .map_err(|e| regex_syntax_error(&translated, &e.to_string(), true))
        };
        let (quick, slow) = (build(MATCH_BACKTRACK_LIMIT)?, Arc::new(build(ABANDONED_MATCH_LIMIT)?));
        Arc::new(MatcherKind::Backtrack(BacktrackMatcher { quick, slow }))
    } else {
        check_pattern_syntax(&translated)?;
        let fixed = ascii_pattern(&re2_syntax_for_rust(&translated));
        let re = ::regex::RegexBuilder::new(&fixed)
            .size_limit(RE2_SIZE_LIMIT)
            .nest_limit(RE2_NEST_LIMIT)
            .build()
            .map_err(|e| regex_syntax_error(&translated, &e.to_string(), false))?;
        Arc::new(MatcherKind::Linear(re))
    };

    let mut cache = REGEX_CACHE.locked();
    if cache.len() >= REGEX_CACHE_MAX {
        *cache = HashMap::new();
    }
    cache.insert(pattern.to_string(), m.clone());
    drop(cache);
    Ok(m)
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        compile_regex(p).unwrap().is_match(s).unwrap()
    }

    #[test]
    fn a_repeated_word_anchor_matches_as_regexp2_does() {
        assert!(m(r"\m*foo", "foo"));
        assert!(!m(r"\m*foo", "afoo")); // only the lookahead half repeats
        assert!(m(r"\m+foo", "a foo"));
        assert!(!m(r"\m+oo", "foo"));
        assert!(m(r"\y*foo", "afoo")); // the whole group repeats
        assert!(m(r"\Y*oo", "foo"));
        assert!(m(r"\m{0}foo", "foo"));
        assert!(m(r"foo\M*", "foo"));
        assert!(m(r"\m*?foo", "foo"));
    }

    #[test]
    fn a_nested_repetition_is_regexp2_error() {
        let e = compile_regex(r"\m**").err().unwrap();
        assert!(
            e.message()
                .starts_with("error parsing regexp: invalid nested repetition operator in `"),
            "{}",
            e.message()
        );
    }

    #[test]
    fn a_runaway_match_times_out_at_five_seconds() {
        let re = compile_regex(r"^(?:\Ya|a)*b").unwrap();
        let input = "a".repeat(64);
        let t0 = std::time::Instant::now();
        let e = re.is_match(&input).unwrap_err();
        let took = t0.elapsed();
        assert_eq!(e.message(), format!("match timeout after 5s on input `{input}`"));
        assert!(took >= std::time::Duration::from_millis(4900), "{took:?}");
        assert!(took < std::time::Duration::from_secs(8), "{took:?}");
    }
}
