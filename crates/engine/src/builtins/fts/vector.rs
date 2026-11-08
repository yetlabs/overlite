//! tsvector: lexemes in byte order, each with its positions and their weights, read
//! and written in Postgres' text form, which is also how one is stored.

use crate::error::{Error, Result};
use overlite_core::SqlState;

/// The highest position; a later one is taken as it.
pub(crate) const MAX_POS: u16 = 16383;
/// The most positions a lexeme keeps.
const MAX_NUM_POS: usize = 256;

/// A position and its weight, as Postgres packs them: the weight (D 0 to A 3) in the
/// top two bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos(pub(crate) u16);

impl Pos {
    pub(crate) fn new(pos: u16, weight: u8) -> Pos {
        Pos((u16::from(weight) << 14) | pos.min(MAX_POS))
    }
    pub(crate) fn pos(self) -> u16 {
        self.0 & 0x3fff
    }
    pub(crate) fn weight(self) -> u8 {
        (self.0 >> 14) as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) lexeme: String,
    /// Empty when the lexeme has no positions (a stripped vector).
    pub(crate) positions: Vec<Pos>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TsVector {
    /// Sorted by lexeme bytes, each lexeme once.
    pub(crate) entries: Vec<Entry>,
}

fn syntax(text: &str) -> Error {
    Error::with_state(SqlState::SYNTAX_ERROR, format!("syntax error in tsvector: \"{text}\""))
}

impl TsVector {
    /// Builds a vector from lexemes with positions, merging what repeats.
    pub(crate) fn from_parts(mut parts: Vec<(String, Vec<Pos>)>) -> TsVector {
        parts.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut entries: Vec<Entry> = Vec::with_capacity(parts.len());
        for (lexeme, positions) in parts {
            match entries.last_mut() {
                Some(e) if e.lexeme == lexeme => e.positions.extend(positions),
                _ => entries.push(Entry { lexeme, positions }),
            }
        }
        for e in &mut entries {
            unique_positions(&mut e.positions);
        }
        TsVector { entries }
    }

    /// tsvector_in. Text that is two vectors run together, as SQLite's `||` leaves two
    /// stored ones, `'a':1'b':1`, is read as their concatenation.
    pub(crate) fn parse(text: &str) -> Result<TsVector> {
        let mut segments: Vec<Vec<(String, Vec<Pos>)>> = vec![Vec::new()];
        let c: Vec<char> = text.chars().collect();
        let mut i = 0;
        let mut glued = false;
        loop {
            let ws_start = i;
            while i < c.len() && super::is_space(c[i]) {
                i += 1;
            }
            if i >= c.len() {
                break;
            }
            if glued && i == ws_start && c[i] == '\'' {
                segments.push(Vec::new());
            }
            // The lexeme, quoted or bare, backslash escaping a character.
            let mut lexeme = String::new();
            if c[i] == '\'' {
                i += 1;
                loop {
                    match c.get(i) {
                        None => return Err(syntax(text)),
                        Some('\'') if c.get(i + 1) == Some(&'\'') => {
                            lexeme.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some('\\') => {
                            let ch = *c.get(i + 1).ok_or_else(|| syntax(text))?;
                            lexeme.push(ch);
                            i += 2;
                        }
                        Some(ch) => {
                            lexeme.push(*ch);
                            i += 1;
                        }
                    }
                }
                if lexeme.is_empty() {
                    return Err(syntax(text));
                }
            } else {
                while i < c.len() && !super::is_space(c[i]) && c[i] != ':' {
                    if c[i] == '\\' {
                        i += 1;
                        lexeme.push(*c.get(i).ok_or_else(|| syntax(text))?);
                    } else if c[i] == '\'' {
                        return Err(syntax(text));
                    } else {
                        lexeme.push(c[i]);
                    }
                    i += 1;
                }
            }
            glued = false;
            let mut positions = Vec::new();
            if c.get(i) == Some(&':') {
                i += 1;
                loop {
                    let start = i;
                    while i < c.len() && c[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start {
                        return Err(syntax(text));
                    }
                    let n: u32 = c[start..i].iter().collect::<String>().parse().unwrap_or(u32::MAX);
                    if n == 0 {
                        return Err(Error::with_state(
                            SqlState::SYNTAX_ERROR,
                            format!("wrong position info in tsvector: \"{text}\""),
                        ));
                    }
                    let weight = match c.get(i) {
                        Some('a' | 'A') => 3,
                        Some('b' | 'B') => 2,
                        Some('c' | 'C') => 1,
                        _ => 0,
                    };
                    if matches!(c.get(i), Some('a' | 'A' | 'b' | 'B' | 'c' | 'C' | 'd' | 'D' | '*')) {
                        i += 1;
                    }
                    positions.push(Pos::new(n.min(u32::from(MAX_POS)) as u16, weight));
                    if c.get(i) == Some(&',') {
                        i += 1;
                        continue;
                    }
                    break;
                }
                glued = true;
            }
            if let Some(seg) = segments.last_mut() {
                seg.push((lexeme, positions));
            }
            if i < c.len() && !super::is_space(c[i]) && !(glued && c[i] == '\'') {
                return Err(syntax(text));
            }
        }
        let mut out = TsVector::default();
        for (k, seg) in segments.into_iter().enumerate() {
            let v = TsVector::from_parts(seg);
            out = if k == 0 { v } else { out.concat(&v) };
        }
        Ok(out)
    }

    /// tsvector_out.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for (k, e) in self.entries.iter().enumerate() {
            if k > 0 {
                out.push(' ');
            }
            quote_lexeme(&mut out, &e.lexeme);
            for (j, p) in e.positions.iter().enumerate() {
                out.push(if j == 0 { ':' } else { ',' });
                out.push_str(&p.pos().to_string());
                match p.weight() {
                    3 => out.push('A'),
                    2 => out.push('B'),
                    1 => out.push('C'),
                    _ => {}
                }
            }
        }
        out
    }

    /// The highest position the vector holds.
    fn max_pos(&self) -> u16 {
        self.entries
            .iter()
            .flat_map(|e| e.positions.iter().map(|p| p.pos()))
            .max()
            .unwrap_or(0)
    }

    /// tsvector || tsvector: the right's positions follow the left's last.
    pub(crate) fn concat(&self, other: &TsVector) -> TsVector {
        let shift = self.max_pos();
        let mut parts: Vec<(String, Vec<Pos>)> = self
            .entries
            .iter()
            .map(|e| (e.lexeme.clone(), e.positions.clone()))
            .collect();
        for e in &other.entries {
            let shifted = e
                .positions
                .iter()
                .map(|p| {
                    Pos::new(
                        (u32::from(p.pos()) + u32::from(shift)).min(u32::from(MAX_POS)) as u16,
                        p.weight(),
                    )
                })
                .collect();
            parts.push((e.lexeme.clone(), shifted));
        }
        TsVector::from_parts(parts)
    }

    pub(crate) fn setweight(&self, weight: u8, only: Option<&[String]>) -> TsVector {
        let mut v = self.clone();
        for e in &mut v.entries {
            if only.is_some_and(|l| !l.contains(&e.lexeme)) {
                continue;
            }
            for p in &mut e.positions {
                *p = Pos::new(p.pos(), weight);
            }
        }
        v
    }

    pub(crate) fn strip(&self) -> TsVector {
        TsVector {
            entries: self
                .entries
                .iter()
                .map(|e| Entry {
                    lexeme: e.lexeme.clone(),
                    positions: Vec::new(),
                })
                .collect(),
        }
    }

    /// The entries a query operand finds: the lexeme, or each it is a prefix of.
    pub(crate) fn find(&self, lexeme: &str, prefix: bool) -> &[Entry] {
        &self.entries[self.find_range(lexeme, prefix)]
    }

    /// Where find's entries are.
    pub(crate) fn find_range(&self, lexeme: &str, prefix: bool) -> std::ops::Range<usize> {
        let start = self
            .entries
            .partition_point(|e| e.lexeme.as_bytes() < lexeme.as_bytes());
        let mut end = start;
        while end < self.entries.len()
            && (if prefix {
                self.entries[end].lexeme.as_bytes().starts_with(lexeme.as_bytes())
            } else {
                self.entries[end].lexeme == lexeme
            })
        {
            end += 1;
        }
        start..end
    }

    /// The number of positions, a lexeme without any counting one: cnt_length.
    pub(crate) fn count_length(&self) -> usize {
        self.entries.iter().map(|e| e.positions.len().max(1)).sum()
    }
}

/// Sorts positions, keeps one of each with the highest weight given it, and at most
/// MAX_NUM_POS of them: uniquePos.
fn unique_positions(p: &mut Vec<Pos>) {
    p.sort_by_key(|x| (x.pos(), x.weight()));
    let mut out: Vec<Pos> = Vec::with_capacity(p.len());
    for x in p.iter() {
        match out.last_mut() {
            Some(last) if last.pos() == x.pos() => {
                if x.weight() > last.weight() {
                    *last = *x;
                }
            }
            _ => out.push(*x),
        }
    }
    out.truncate(MAX_NUM_POS);
    *p = out;
}

/// A lexeme in quotes, a quote and a backslash doubled.
pub(crate) fn quote_lexeme(out: &mut String, lexeme: &str) {
    out.push('\'');
    for ch in lexeme.chars() {
        if ch == '\'' || ch == '\\' {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form() {
        let v = TsVector::parse("b:2 a:1,3A 'it''s':4 c").unwrap();
        assert_eq!(v.render(), "'a':1,3A 'b':2 'c' 'it''s':4");
        let glued = TsVector::parse("'a':1 'b':2'c':1,2 'a':3").unwrap();
        assert_eq!(glued.render(), "'a':1,5 'b':2 'c':3,4");
        assert!(TsVector::parse("'a").is_err());
        assert_eq!(TsVector::parse("").unwrap().render(), "");
        assert_eq!(TsVector::parse("a:1 a:1B").unwrap().render(), "'a':1B");
    }
}
