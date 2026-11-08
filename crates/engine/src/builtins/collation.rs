//! Postgres' language collations as SQLite collations: an ICU name (`sv-x-icu`,
//! `und-x-icu`), a libc locale (`sv_SE.utf8`), and what CREATE COLLATION makes.
//!
//! The order is the Unicode Collation Algorithm's in its broad lines, by levels: the
//! letters without their accents and case first, then the accents, then lower case
//! before upper; a language that sorts a letter apart (Swedish å, ä, ö after z) has it
//! so. A deterministic collation breaks what is left by code points, as ICU does
//! under Postgres; a nondeterministic one at level 1 or 2 (`ks-level2`) holds strings
//! equal that differ in what it ignores, which is what makes it case-insensitive.
//!
//! A collation is registered when a statement first needs it (sqlite3_collation_needed),
//! so a table keeps the name its column was declared with.

use overlite_core::sync::MutexExt;
use rusqlite::Connection;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// How a collation compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollationSpec {
    /// The language, lower case: "sv", "en", "und".
    pub language: String,
    /// 1 to 3: the levels compared before code points.
    pub strength: u8,
    pub deterministic: bool,
}

/// The collations CREATE COLLATION has made, by name.
fn registry() -> &'static Mutex<HashMap<String, CollationSpec>> {
    static R: OnceLock<Mutex<HashMap<String, CollationSpec>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records a created collation, for every connection's next need of it.
pub fn register_collation(name: &str, spec: CollationSpec) {
    registry().locked().insert(name.to_string(), spec);
}

/// Forgets a dropped collation.
pub fn forget_collation(name: &str) {
    registry().locked().remove(name);
}

/// The spec of a locale as CREATE COLLATION's locale gives it, or an ICU or libc
/// collation's name: `sv-x-icu`, `und-u-ks-level2`, `sv_SE.utf8`, `de-DE`.
pub fn spec_of_locale(locale: &str, deterministic: bool) -> CollationSpec {
    let lower = locale.to_ascii_lowercase();
    let language: String = lower.chars().take_while(char::is_ascii_alphabetic).collect();
    let strength = if lower.contains("ks-level1") {
        1
    } else if lower.contains("ks-level2") {
        2
    } else {
        3
    };
    CollationSpec {
        language: if language.is_empty() || language == "root" {
            "und".into()
        } else {
            language
        },
        strength,
        deterministic,
    }
}

/// Whether name is a collation this module provides: one created, an ICU name, or a
/// libc locale.
pub fn collation_known(name: &str) -> bool {
    lookup(name).is_some()
}

/// The spec of a collation by its name.
fn lookup(name: &str) -> Option<CollationSpec> {
    if let Some(s) = registry().locked().get(name) {
        return Some(s.clone());
    }
    let lower = name.to_ascii_lowercase();
    let icu = lower.ends_with("-x-icu") || lower == "unicode";
    let libc = {
        let b = lower.as_bytes();
        b.len() >= 5
            && b[..2].iter().all(u8::is_ascii_alphabetic)
            && b[2] == b'_'
            && b[3..5].iter().all(u8::is_ascii_alphabetic)
    };
    (icu || libc).then(|| spec_of_locale(&lower, true))
}

/// Registers the collation SQLite asks for, if it is one of these.
fn needed(conn: &Connection, name: &str) -> rusqlite::Result<()> {
    if let Some(spec) = lookup(name) {
        conn.create_collation(name, move |a, b| compare(&spec, a, b))?;
    }
    Ok(())
}

/// Lets each connection find these collations when a statement names one.
pub(crate) fn register(conn: &Connection) -> rusqlite::Result<()> {
    conn.collation_needed(needed)
}

/// A letter's place in the alphabet of a language: its base letter, and where among
/// the letters a language sorts it apart.
fn primary(language: &str, c: char) -> (u32, char) {
    let lower: char = c.to_lowercase().next().unwrap_or(c);
    // Letters a language sorts after z.
    let apart: &[char] = match language {
        "sv" | "fi" => &['å', 'ä', 'æ', 'ö', 'ø'],
        "da" | "nb" | "nn" | "no" => &['æ', 'ø', 'å'],
        "de" | "en" | "fr" | "es" | "it" | "pt" | "nl" | "und" => &[],
        _ => &[],
    };
    if let Some(i) = apart.iter().position(|&a| a == lower) {
        // Swedish sorts æ with ä and ø with ö.
        let i = match (language, lower) {
            ("sv" | "fi", 'æ') => 1,
            ("sv" | "fi", 'ö' | 'ø') => 3,
            _ => i as u32,
        };
        return (0x10_0000 + i, lower);
    }
    let base = crate::builtins::contrib::unaccent(&lower.to_string());
    let b = base.chars().next().unwrap_or(lower);
    let b = b.to_lowercase().next().unwrap_or(b);
    // Letters and digits before nothing else; punctuation and spaces first, as ICU's
    // variable characters sort ahead.
    let class = if b.is_alphanumeric() { 0x1000 } else { 0 };
    (class + b as u32, b)
}

/// The collation keys of s at each level.
fn keys(language: &str, s: &str) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let (mut p, mut sec, mut ter) = (Vec::new(), Vec::new(), Vec::new());
    for c in s.chars() {
        let (key, base) = primary(language, c);
        let lower = c.to_lowercase().next().unwrap_or(c);
        // A ligature unaccent spells out (æ as ae) is two letters at level 1.
        let spelled = crate::builtins::contrib::unaccent(&lower.to_string());
        if key < 0x10_0000 && spelled.chars().count() > 1 {
            for b in spelled.chars() {
                p.push(0x1000 + b as u32);
            }
        } else {
            p.push(key);
        }
        sec.push(if lower == base || key >= 0x10_0000 {
            0
        } else {
            lower as u32
        });
        ter.push(u32::from(c != lower));
    }
    (p, sec, ter)
}

/// Compares two strings under a collation.
pub(crate) fn compare(spec: &CollationSpec, a: &str, b: &str) -> Ordering {
    let (pa, sa, ta) = keys(&spec.language, a);
    let (pb, sb, tb) = keys(&spec.language, b);
    let ord = pa.cmp(&pb);
    if ord != Ordering::Equal || spec.strength == 1 {
        return if ord == Ordering::Equal && spec.deterministic {
            a.cmp(b)
        } else {
            ord
        };
    }
    let ord = sa.cmp(&sb);
    if ord != Ordering::Equal || spec.strength == 2 {
        return if ord == Ordering::Equal && spec.deterministic {
            a.cmp(b)
        } else {
            ord
        };
    }
    let ord = ta.cmp(&tb);
    if ord != Ordering::Equal || !spec.deterministic {
        return ord;
    }
    a.cmp(b)
}

/// The collation a column of a CREATE TABLE is declared with, unquoted.
pub(crate) fn column_collation(ddl: &str, column: &str) -> Option<String> {
    let item = column_item(ddl, column)?;
    let at = item.to_ascii_lowercase().find(" collate ")?;
    let rest = item[at + 9..].trim_start();
    Some(if let Some(r) = rest.strip_prefix('"') {
        r.split('"').next().unwrap_or("").to_string()
    } else {
        rest.split(|c: char| c.is_whitespace() || c == ',' || c == ')')
            .next()
            .unwrap_or("")
            .to_string()
    })
}

/// The definition of a column in a CREATE TABLE's column list, as written.
pub(crate) fn column_item<'a>(ddl: &'a str, column: &str) -> Option<&'a str> {
    let open = ddl.find('(')?;
    let b = ddl.as_bytes();
    let (mut depth, mut start, mut i) = (0i32, open + 1, open);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = ddl[i + 2..].find("*/").map_or(b.len(), |e| i + e + 3);
            }
            b'(' => depth += 1,
            b')' | b',' if depth == 1 => {
                let item = ddl[start..i].trim();
                let name = item
                    .strip_prefix('"')
                    .and_then(|r| r.split('"').next())
                    .unwrap_or_else(|| item.split_whitespace().next().unwrap_or(""));
                if name.eq_ignore_ascii_case(column) {
                    return Some(item);
                }
                if b[i] == b')' {
                    return None;
                }
                start = i + 1;
            }
            b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swedish_sorts_its_letters_after_z() {
        let sv = spec_of_locale("sv-x-icu", true);
        let mut v = vec!["Ö", "P", "O", "a", "B", "å", "z", "Z"];
        v.sort_by(|a, b| compare(&sv, a, b));
        assert_eq!(v, ["a", "B", "O", "P", "z", "Z", "å", "Ö"]);
        let en = spec_of_locale("en-x-icu", true);
        let mut v = vec!["Ö", "P", "O", "é", "e", "E", "f"];
        v.sort_by(|a, b| compare(&en, a, b));
        assert_eq!(v, ["e", "E", "é", "f", "O", "Ö", "P"]);
    }

    #[test]
    fn nondeterministic_levels() {
        let ci = spec_of_locale("und-u-ks-level2", false);
        assert_eq!(compare(&ci, "ANDREW", "Andrew"), Ordering::Equal);
        assert_ne!(compare(&ci, "andré", "andre"), Ordering::Equal);
        let ai = spec_of_locale("und-u-ks-level1", false);
        assert_eq!(compare(&ai, "André", "andre"), Ordering::Equal);
        let cs = spec_of_locale("und-x-icu", true);
        assert_ne!(compare(&cs, "ANDREW", "Andrew"), Ordering::Equal);
    }

    #[test]
    fn names() {
        assert!(collation_known("sv-x-icu"));
        assert!(collation_known("sv_SE.utf8"));
        assert!(!collation_known("C"));
        assert_eq!(
            column_collation(r#"CREATE TABLE t ("a" text COLLATE "sv-x-icu" NOT NULL, b int)"#, "a").as_deref(),
            Some("sv-x-icu")
        );
        assert_eq!(column_collation("CREATE TABLE t (a text, b int)", "b"), None);
    }
}
