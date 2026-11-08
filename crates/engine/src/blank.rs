//! The literal-blanking rule (ARCHITECTURE.md): copies of a statement with the text
//! of its literals, comments or quoted names turned to spaces, every byte where it
//! was, so that what looks for a keyword finds only code.

use std::borrow::Cow;

/// sql with the text of its string literals and comments turned to spaces, so that
/// what looks for a keyword finds only code. A literal keeps its quotes, and every
/// byte stays where it was: a position found here is a position in sql.
pub fn blank_literals(sql: &str) -> Cow<'_, str> {
    blank_text(sql, Blank::Literals)
}

/// blank_literals that blanks quoted names too, for looking for keywords. Not for
/// what reads names: a quoted one is gone from the result.
pub fn code_only(sql: &str) -> Cow<'_, str> {
    blank_text(sql, Blank::LiteralsAndNames)
}

/// sql with each comment turned to spaces and everything else as it was, which is
/// what a comment is to Postgres: a `--` to the end of the line, a `/* */` with the
/// ones nested in it. A quote, a `$1` or a keyword inside one is then nothing to
/// what reads the statement after. Comments inside literals, quoted names and
/// dollar-quoted bodies are their text and stay. An unclosed `/*` stays too, for
/// SQLite to refuse as Postgres does.
pub fn blank_comments(sql: &str) -> Cow<'_, str> {
    if !sql.contains("--") && !sql.contains("/*") {
        return Cow::Borrowed(sql);
    }
    blank_text(sql, Blank::Comments)
}

/// What blank_text turns to spaces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blank {
    /// The text of literals, and comments.
    Literals,
    /// That and the text of quoted names.
    LiteralsAndNames,
    /// Comments alone.
    Comments,
}

fn blank_text(sql: &str, what: Blank) -> Cow<'_, str> {
    let s = sql.as_bytes();
    let names = what == Blank::LiteralsAndNames;
    let literals = what != Blank::Comments;
    let mut b: Option<Vec<u8>> = None;
    let mut blank = |from: usize, to: usize| {
        if from < to {
            b.get_or_insert_with(|| s.to_vec())[from..to].fill(b' ');
        }
    };
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'\'' => {
                let escapes = i > 0 && (s[i - 1] | 0x20) == b'e' && (i < 2 || !is_word(s[i - 2]));
                let mut j = i + 1;
                while j < s.len() {
                    match s[j] {
                        b'\\' if escapes => j += 1,
                        b'\'' if s.get(j + 1) == Some(&b'\'') => j += 1,
                        b'\'' => break,
                        _ => {}
                    }
                    j += 1;
                }
                if literals {
                    blank(i + 1, j.min(s.len()));
                }
                i = j + 1;
            }
            b'"' => {
                let mut j = i + 1;
                while j < s.len() && (s[j] != b'"' || s.get(j + 1) == Some(&b'"')) {
                    j += if s[j] == b'"' { 2 } else { 1 };
                }
                if names {
                    blank(i + 1, j.min(s.len()));
                }
                i = j + 1;
            }
            b'-' if s.get(i + 1) == Some(&b'-') => {
                let j = s[i..].iter().position(|c| *c == b'\n').map_or(s.len(), |n| i + n);
                blank(i, j);
                i = j;
            }
            b'/' if s.get(i + 1) == Some(&b'*') => {
                let (mut j, mut depth) = (i + 2, 1);
                while j < s.len() && depth > 0 {
                    if s[j..].starts_with(b"/*") {
                        depth += 1;
                        j += 2;
                    } else if s[j..].starts_with(b"*/") {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                if depth == 0 || literals {
                    blank(i, j);
                }
                i = j;
            }
            b'$' if i == 0 || !is_word(s[i - 1]) => {
                let tag_end = (i + 1..s.len()).find(|&j| !is_word(s[j]));
                match tag_end.filter(|&j| s[j] == b'$' && !s[i + 1].is_ascii_digit()) {
                    Some(j) => {
                        let tag = &sql[i..=j];
                        let close = sql[j + 1..].find(tag).map_or(s.len(), |n| j + 1 + n);
                        if literals {
                            blank(j + 1, close);
                        }
                        i = close + tag.len();
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    match b {
        // Whole characters are blanked, between ASCII delimiters.
        Some(b) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
        None => Cow::Borrowed(sql),
    }
}

pub(crate) fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_literals_keep_positions() {
        for (sql, literals, names) in [
            ("SELECT 1", "SELECT 1", "SELECT 1"),
            ("a 'it''s' b", "a '     ' b", "a '     ' b"),
            (r#"a "x y" 'z'"#, r#"a "x y" ' '"#, r#"a "   " ' '"#),
            (r#"a "x""y" b"#, r#"a "x""y" b"#, r#"a "    " b"#),
            (r"E'a\'b' c", r"E'    ' c", r"E'    ' c"),
            (r"'a\' c", r"'  ' c", r"'  ' c"),
            ("a -- b 'c\nd", "a        \nd", "a        \nd"),
            (
                "a /* b /* c */ d */ e",
                "a                   e",
                "a                   e",
            ),
            ("a $$ b $$ c", "a $$   $$ c", "a $$   $$ c"),
            ("a $t$ $$ b $t$ c", "a $t$      $t$ c", "a $t$      $t$ c"),
            ("a = $1 AND b$c$d", "a = $1 AND b$c$d", "a = $1 AND b$c$d"),
            ("'é' x", "'  ' x", "'  ' x"),
            ("'open", "'    ", "'    "),
        ] {
            assert_eq!(blank_literals(sql), literals, "{sql}");
            assert_eq!(code_only(sql), names, "{sql}");
            assert_eq!(code_only(sql).len(), sql.len(), "{sql}");
        }
    }

    #[test]
    fn blank_comments_keeps_the_rest() {
        for (sql, want) in [
            ("SELECT 1", "SELECT 1"),
            ("a -- it's $1\nb", "a           \nb"),
            ("a /* it's /* $1 */ BEGIN */ b", "a                           b"),
            ("a 'x -- y' b", "a 'x -- y' b"),
            ("a '/* x */' b", "a '/* x */' b"),
            (r#"a "--" b"#, r#"a "--" b"#),
            (r"a E'\' -- ' b", r"a E'\' -- ' b"),
            ("a $$ -- x $$ b", "a $$ -- x $$ b"),
            ("a $t$ /* x */ $t$ -- y", "a $t$ /* x */ $t$     "),
            ("1--1", "1   "),
            ("a /* unclosed", "a /* unclosed"),
            ("a -- é\nb", "a      \nb"),
        ] {
            assert_eq!(blank_comments(sql), want, "{sql}");
            assert_eq!(blank_comments(sql).len(), sql.len(), "{sql}");
        }
    }
}
