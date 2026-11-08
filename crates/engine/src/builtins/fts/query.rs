//! tsquery: its text form both ways, what to_tsquery, plainto_tsquery,
//! phraseto_tsquery and websearch_to_tsquery make of a text with a configuration, and
//! whether a tsvector matches one (`@@`), phrases and weights included.

use super::parser::{Config, lexemes};
use super::vector::{Pos, TsVector, quote_lexeme};
use crate::error::{Error, Result};
use overlite_core::SqlState;

/// A node of a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    /// A lexeme, the weights it is held to (bit 3 A to bit 0 D, none for any), and
    /// whether it is a prefix.
    Val {
        lexeme: String,
        weight: u8,
        prefix: bool,
    },
    /// What a stop word left, gone once the query is built.
    Stop,
    Not(Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    /// Left, then right this far after it.
    Phrase(Box<Node>, Box<Node>, u16),
}

/// A query; None is the empty one, which matches nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TsQuery {
    pub(crate) root: Option<Node>,
}

fn syntax(text: &str) -> Error {
    Error::with_state(SqlState::SYNTAX_ERROR, format!("syntax error in tsquery: \"{text}\""))
}

#[derive(Debug, Clone, PartialEq)]
enum T {
    Val(String, u8, bool),
    Not,
    And,
    Or,
    Phrase(u16),
    Open,
    Close,
}

fn is_operator(c: char) -> bool {
    matches!(c, '!' | '&' | '|' | '(' | ')' | '<')
}

/// The tokens of a query in Postgres' syntax.
fn query_tokens(text: &str) -> Result<Vec<T>> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if super::is_space(ch) {
            i += 1;
            continue;
        }
        match ch {
            '!' => {
                out.push(T::Not);
                i += 1;
            }
            '&' => {
                out.push(T::And);
                i += 1;
            }
            '|' => {
                out.push(T::Or);
                i += 1;
            }
            '(' => {
                out.push(T::Open);
                i += 1;
            }
            ')' => {
                out.push(T::Close);
                i += 1;
            }
            '<' => {
                let end = c[i..].iter().position(|x| *x == '>').ok_or_else(|| syntax(text))? + i;
                let inner: String = c[i + 1..end].iter().collect();
                let distance = if inner == "-" {
                    1
                } else {
                    let n: i64 = inner.trim().parse().map_err(|_| syntax(text))?;
                    if !(0..=16384).contains(&n) {
                        return Err(Error::with_state(
                            SqlState::PROGRAM_LIMIT_EXCEEDED,
                            "distance in phrase operator must be an integer value between zero and 16384 inclusive",
                        ));
                    }
                    n as u16
                };
                out.push(T::Phrase(distance));
                i = end + 1;
            }
            _ => {
                let mut word = String::new();
                if ch == '\'' {
                    i += 1;
                    loop {
                        match c.get(i) {
                            None => return Err(syntax(text)),
                            Some('\'') if c.get(i + 1) == Some(&'\'') => {
                                word.push('\'');
                                i += 2;
                            }
                            Some('\'') => {
                                i += 1;
                                break;
                            }
                            Some('\\') => {
                                word.push(*c.get(i + 1).ok_or_else(|| syntax(text))?);
                                i += 2;
                            }
                            Some(x) => {
                                word.push(*x);
                                i += 1;
                            }
                        }
                    }
                } else {
                    while i < c.len() && !super::is_space(c[i]) && !is_operator(c[i]) && c[i] != ':' {
                        if c[i] == '\\' && i + 1 < c.len() {
                            i += 1;
                        }
                        word.push(c[i]);
                        i += 1;
                    }
                }
                let (mut weight, mut prefix) = (0u8, false);
                if c.get(i) == Some(&':') {
                    i += 1;
                    while let Some(x) = c.get(i) {
                        match x {
                            '*' => prefix = true,
                            'a' | 'A' => weight |= 1 << 3,
                            'b' | 'B' => weight |= 1 << 2,
                            'c' | 'C' => weight |= 1 << 1,
                            'd' | 'D' => weight |= 1,
                            _ => break,
                        }
                        i += 1;
                    }
                }
                out.push(T::Val(word, weight, prefix));
            }
        }
    }
    Ok(out)
}

/// A recursive-descent reading of the tokens: NOT binds tightest, then a phrase, AND,
/// OR, each to the left.
struct Reader<'a> {
    toks: &'a [T],
    i: usize,
    text: &'a str,
    operand: &'a mut dyn FnMut(&str, u8, bool) -> Node,
}

impl Reader<'_> {
    fn or(&mut self) -> Result<Node> {
        let mut l = self.and()?;
        while self.toks.get(self.i) == Some(&T::Or) {
            self.i += 1;
            let r = self.and()?;
            l = Node::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Node> {
        let mut l = self.phrase()?;
        while self.toks.get(self.i) == Some(&T::And) {
            self.i += 1;
            let r = self.phrase()?;
            l = Node::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn phrase(&mut self) -> Result<Node> {
        let mut l = self.unary()?;
        while let Some(T::Phrase(d)) = self.toks.get(self.i) {
            let d = *d;
            self.i += 1;
            let r = self.unary()?;
            l = Node::Phrase(Box::new(l), Box::new(r), d);
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Node> {
        match self.toks.get(self.i).cloned() {
            Some(T::Not) => {
                self.i += 1;
                Ok(Node::Not(Box::new(self.unary()?)))
            }
            Some(T::Open) => {
                self.i += 1;
                let e = self.or()?;
                if self.toks.get(self.i) != Some(&T::Close) {
                    return Err(syntax(self.text));
                }
                self.i += 1;
                Ok(e)
            }
            Some(T::Val(w, weight, prefix)) => {
                self.i += 1;
                Ok((self.operand)(&w, weight, prefix))
            }
            None => Err(Error::with_state(
                SqlState::SYNTAX_ERROR,
                format!("no operand in tsquery: \"{}\"", self.text),
            )),
            _ => Err(syntax(self.text)),
        }
    }
}

fn read(text: &str, operand: &mut dyn FnMut(&str, u8, bool) -> Node) -> Result<Option<Node>> {
    let toks = query_tokens(text)?;
    if toks.is_empty() {
        return Ok(None);
    }
    let mut r = Reader {
        toks: &toks,
        i: 0,
        text,
        operand,
    };
    let node = r.or()?;
    if r.i != toks.len() {
        return Err(syntax(text));
    }
    Ok(Some(node))
}

/// The operator a morphed operand joins its lexemes with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Join {
    Phrase,
    And,
}

/// What a configuration makes of an operand: its lexemes joined, a stop word for each
/// position one took (pushval_morph).
fn morph(config: Config, text: &str, weight: u8, prefix: bool, join: Join) -> Node {
    let words: Vec<(u32, String)> = lexemes(config, text)
        .into_iter()
        .filter_map(|(p, w)| w.map(|w| (p, w)))
        .collect();
    if words.is_empty() {
        return Node::Stop;
    }
    let combine = |l: Node, r: Node| match join {
        Join::Phrase => Node::Phrase(Box::new(l), Box::new(r), 1),
        Join::And => Node::And(Box::new(l), Box::new(r)),
    };
    let mut acc: Option<Node> = None;
    let mut pos = 0u32;
    for (p, w) in words {
        if pos > 0 {
            while pos + 1 < p {
                acc = acc.map(|a| combine(a, Node::Stop));
                pos += 1;
            }
        }
        pos = p;
        let val = Node::Val {
            lexeme: w,
            weight,
            prefix,
        };
        acc = Some(match acc {
            Some(a) => combine(a, val),
            None => val,
        });
    }
    acc.unwrap_or(Node::Stop)
}

/// Takes out what stop words left, carrying a phrase's distance over them
/// (clean_stopword_intree). The distances it could not place: left and right.
fn clean(node: Node) -> (Option<Node>, i32, i32) {
    let (l, r, op, distance) = match node {
        Node::Val { .. } => return (Some(node), 0, 0),
        Node::Stop => return (None, 0, 0),
        Node::Not(child) => {
            let (c, l, r) = clean(*child);
            return (c.map(|c| Node::Not(Box::new(c))), l, r);
        }
        Node::And(l, r) => (*l, *r, 0, 0),
        Node::Or(l, r) => (*l, *r, 1, 0),
        Node::Phrase(l, r, d) => (*l, *r, 2, i32::from(d)),
    };
    let isphrase = op == 2;
    let (left, lladd, lradd) = clean(l);
    let (right, rladd, rradd) = clean(r);
    match (left, right) {
        (None, None) => {
            let add = if isphrase {
                lladd + distance + rladd
            } else {
                lladd.max(rladd)
            };
            (None, add, add)
        }
        (None, Some(right)) => {
            if isphrase {
                (Some(right), lladd + distance + rladd, rradd)
            } else {
                (Some(right), rladd, rradd)
            }
        }
        (Some(left), None) => {
            if isphrase {
                (Some(left), lladd, lradd + distance + rradd)
            } else {
                (Some(left), lladd, lradd)
            }
        }
        (Some(left), Some(right)) => {
            let node = match op {
                0 => Node::And(Box::new(left), Box::new(right)),
                1 => Node::Or(Box::new(left), Box::new(right)),
                _ => {
                    let d = (distance + lradd + rladd).clamp(0, 16384) as u16;
                    return (Some(Node::Phrase(Box::new(left), Box::new(right), d)), lladd, rradd);
                }
            };
            (Some(node), 0, 0)
        }
    }
}

fn cleaned(node: Option<Node>) -> TsQuery {
    TsQuery {
        root: node.and_then(|n| clean(n).0),
    }
}

impl TsQuery {
    /// tsquery_in: the operands as written.
    pub(crate) fn parse(text: &str) -> Result<TsQuery> {
        let mut operand = |w: &str, weight: u8, prefix: bool| {
            if w.is_empty() {
                return Node::Stop;
            }
            Node::Val {
                lexeme: w.to_string(),
                weight,
                prefix,
            }
        };
        // An empty operand is a syntax error to the cast.
        let toks = query_tokens(text)?;
        if toks.iter().any(|t| matches!(t, T::Val(w, ..) if w.is_empty())) {
            return Err(syntax(text));
        }
        Ok(cleaned(read(text, &mut operand)?))
    }

    /// to_tsquery: each operand through the configuration.
    pub(crate) fn to_tsquery(config: Config, text: &str) -> Result<TsQuery> {
        let mut operand = |w: &str, weight: u8, prefix: bool| morph(config, w, weight, prefix, Join::Phrase);
        Ok(cleaned(read(text, &mut operand)?))
    }

    /// plainto_tsquery: the whole text one operand, its lexemes ANDed.
    pub(crate) fn plainto(config: Config, text: &str) -> TsQuery {
        cleaned(Some(morph(config, text, 0, false, Join::And)))
    }

    /// phraseto_tsquery: the whole text one phrase.
    pub(crate) fn phraseto(config: Config, text: &str) -> TsQuery {
        cleaned(Some(morph(config, text, 0, false, Join::Phrase)))
    }

    /// websearch_to_tsquery: words ANDed, "quoted text" a phrase, OR, and -word.
    pub(crate) fn websearch(config: Config, text: &str) -> TsQuery {
        let c: Vec<char> = text.chars().collect();
        // Operands with whether each is negated, in groups joined by OR.
        let mut groups: Vec<Vec<Node>> = vec![Vec::new()];
        let mut i = 0;
        let mut negate = 0usize;
        while i < c.len() {
            let ch = c[i];
            // Spaces and the operators of a query's syntax separate operands.
            if super::is_space(ch) || matches!(ch, '!' | '&' | '|' | '(' | ')' | '<' | ':') {
                i += 1;
                continue;
            }
            let operand: Node;
            if ch == '"' {
                let end = c[i + 1..].iter().position(|x| *x == '"').map_or(c.len(), |p| i + 1 + p);
                let inner: String = c[i + 1..end].iter().collect();
                operand = morph(config, &inner, 0, false, Join::Phrase);
                i = (end + 1).min(c.len());
            } else if ch == '-' {
                negate += 1;
                i += 1;
                continue;
            } else {
                let start = i;
                while i < c.len()
                    && !super::is_space(c[i])
                    && !matches!(c[i], '"' | '!' | '&' | '|' | '(' | ')' | '<' | ':')
                {
                    i += 1;
                }
                let word: String = c[start..i].iter().collect();
                if word.eq_ignore_ascii_case("or") && negate == 0 {
                    let last_has = groups.last().is_some_and(|g| !g.is_empty());
                    let more = c[i..].iter().any(|x| !super::is_space(*x));
                    if last_has && more {
                        groups.push(Vec::new());
                        continue;
                    }
                }
                operand = morph(config, &word, 0, false, Join::Phrase);
            }
            let mut operand = operand;
            for _ in 0..negate {
                operand = Node::Not(Box::new(operand));
            }
            negate = 0;
            if let Some(g) = groups.last_mut() {
                g.push(operand);
            }
        }
        let mut root: Option<Node> = None;
        for g in groups {
            let mut acc: Option<Node> = None;
            for n in g {
                acc = Some(match acc {
                    Some(a) => Node::And(Box::new(a), Box::new(n)),
                    None => n,
                });
            }
            if let Some(a) = acc {
                root = Some(match root {
                    Some(r) => Node::Or(Box::new(r), Box::new(a)),
                    None => a,
                });
            }
        }
        cleaned(root)
    }

    /// tsqueryout.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        if let Some(root) = &self.root {
            infix(root, -1, false, &mut out);
        }
        out
    }

    /// The operands, in the order of the tree.
    pub(crate) fn operands(&self) -> Vec<&Node> {
        fn walk<'a>(n: &'a Node, out: &mut Vec<&'a Node>) {
            match n {
                Node::Val { .. } => out.push(n),
                Node::Stop => {}
                Node::Not(c) => walk(c, out),
                Node::And(l, r) | Node::Or(l, r) | Node::Phrase(l, r, _) => {
                    walk(l, out);
                    walk(r, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(r) = &self.root {
            walk(r, &mut out);
        }
        out
    }
}

fn priority(n: &Node) -> i32 {
    match n {
        Node::Not(_) => 4,
        Node::Phrase(..) => 3,
        Node::And(..) => 2,
        Node::Or(..) => 1,
        _ => 5,
    }
}

/// Postgres' infix(): parentheses where a child binds looser than its parent, and
/// around a phrase on the right of a phrase.
fn infix(n: &Node, parent: i32, right_phrase: bool, out: &mut String) {
    match n {
        Node::Val { lexeme, weight, prefix } => {
            quote_lexeme(out, lexeme);
            if *weight != 0 || *prefix {
                out.push(':');
                if *prefix {
                    out.push('*');
                }
                for (bit, letter) in [(3, 'A'), (2, 'B'), (1, 'C'), (0, 'D')] {
                    if weight & (1 << bit) != 0 {
                        out.push(letter);
                    }
                }
            }
        }
        Node::Stop => {}
        Node::Not(child) => {
            let p = priority(n);
            if p < parent {
                out.push_str("( ");
            }
            out.push('!');
            infix(child, p, false, out);
            if p < parent {
                out.push_str(" )");
            }
        }
        Node::And(l, r) | Node::Or(l, r) | Node::Phrase(l, r, _) => {
            let p = priority(n);
            let is_phrase = matches!(n, Node::Phrase(..));
            let paren = p < parent || (is_phrase && right_phrase);
            if paren {
                out.push_str("( ");
            }
            infix(l, p, false, out);
            match n {
                Node::Or(..) => out.push_str(" | "),
                Node::And(..) => out.push_str(" & "),
                Node::Phrase(_, _, 1) => out.push_str(" <-> "),
                Node::Phrase(_, _, d) => out.push_str(&format!(" <{d}> ")),
                _ => {}
            }
            infix(r, p, is_phrase, out);
            if paren {
                out.push_str(" )");
            }
        }
    }
}

/// What an operand found: its positions, None for a lexeme without any (stripped).
pub(crate) type Found = Option<Vec<u16>>;

/// Positions a phrase evaluation carries: where the match ends, and its width.
#[derive(Debug, Clone)]
struct PhraseData {
    pos: Vec<u16>,
    negate: bool,
    width: u16,
    /// Some operand had no positions: the phrase may match.
    maybe: bool,
}

/// Evaluates a query with what each operand finds, by its index in `operands()`.
pub(crate) fn execute(root: &Node, check: &mut dyn FnMut(usize, &Node) -> Option<Found>) -> bool {
    let mut idx = 0;
    exec(root, check, &mut idx)
}

fn skip(n: &Node, idx: &mut usize) {
    match n {
        Node::Val { .. } => *idx += 1,
        Node::Stop => {}
        Node::Not(c) => skip(c, idx),
        Node::And(l, r) | Node::Or(l, r) | Node::Phrase(l, r, _) => {
            skip(l, idx);
            skip(r, idx);
        }
    }
}

fn exec(n: &Node, check: &mut dyn FnMut(usize, &Node) -> Option<Found>, idx: &mut usize) -> bool {
    match n {
        Node::Val { .. } => {
            let i = *idx;
            *idx += 1;
            check(i, n).is_some()
        }
        Node::Stop => false,
        Node::Not(c) => !exec(c, check, idx),
        Node::And(l, r) => {
            let a = exec(l, check, idx);
            if !a {
                skip(r, idx);
                return false;
            }
            exec(r, check, idx)
        }
        Node::Or(l, r) => {
            let a = exec(l, check, idx);
            if a {
                skip(r, idx);
                return true;
            }
            exec(r, check, idx)
        }
        Node::Phrase(..) => match phrase(n, check, idx) {
            Some(d) => d.maybe || d.negate || !d.pos.is_empty(),
            None => false,
        },
    }
}

/// TS_phrase_execute: the end positions where a phrase subtree matches.
fn phrase(n: &Node, check: &mut dyn FnMut(usize, &Node) -> Option<Found>, idx: &mut usize) -> Option<PhraseData> {
    match n {
        Node::Val { .. } => {
            let i = *idx;
            *idx += 1;
            match check(i, n)? {
                Some(pos) => Some(PhraseData {
                    pos,
                    negate: false,
                    width: 0,
                    maybe: false,
                }),
                None => Some(PhraseData {
                    pos: Vec::new(),
                    negate: false,
                    width: 0,
                    maybe: true,
                }),
            }
        }
        Node::Stop => None,
        Node::Not(c) => match phrase(c, check, idx) {
            None => Some(PhraseData {
                pos: Vec::new(),
                negate: true,
                width: 0,
                maybe: false,
            }),
            Some(d) if d.maybe => Some(d),
            Some(d) => Some(PhraseData {
                pos: d.pos,
                negate: !d.negate,
                width: d.width,
                maybe: false,
            }),
        },
        Node::Phrase(l, r, distance) => {
            let ld = phrase(l, check, idx);
            let rd = phrase(r, check, idx);
            let (ld, rd) = (ld?, rd?);
            if ld.maybe || rd.maybe {
                return Some(PhraseData {
                    pos: Vec::new(),
                    negate: false,
                    width: 0,
                    maybe: true,
                });
            }
            let width = distance + ld.width + rd.width;
            let loff = i32::from(*distance) + i32::from(rd.width);
            let mut out = Vec::new();
            match (ld.negate, rd.negate) {
                (false, false) => {
                    for &p in &rd.pos {
                        let l = i32::from(p) - loff;
                        if l >= 0 && ld.pos.contains(&(l as u16)) {
                            out.push(p);
                        }
                    }
                }
                (true, false) => {
                    for &p in &rd.pos {
                        let l = i32::from(p) - loff;
                        if l < 0 || !ld.pos.contains(&(l as u16)) {
                            out.push(p);
                        }
                    }
                }
                (false, true) => {
                    for &p in &ld.pos {
                        let e = (i32::from(p) + loff) as u16;
                        if !rd.pos.contains(&e) {
                            out.push(e);
                        }
                    }
                }
                (true, true) => {
                    return Some(PhraseData {
                        pos: Vec::new(),
                        negate: true,
                        width,
                        maybe: false,
                    });
                }
            }
            out.sort_unstable();
            out.dedup();
            Some(PhraseData {
                pos: out,
                negate: false,
                width,
                maybe: false,
            })
        }
        Node::And(l, r) | Node::Or(l, r) => {
            let is_and = matches!(n, Node::And(..));
            let ld = phrase(l, check, idx);
            let rd = phrase(r, check, idx);
            match (ld, rd) {
                (Some(a), Some(b)) => {
                    if a.maybe || b.maybe {
                        return Some(PhraseData { maybe: true, ..a });
                    }
                    let width = a.width.max(b.width);
                    let pos: Vec<u16> = if is_and {
                        a.pos.iter().copied().filter(|p| b.pos.contains(p)).collect()
                    } else {
                        let mut u = a.pos.clone();
                        u.extend(b.pos.iter().copied());
                        u.sort_unstable();
                        u.dedup();
                        u
                    };
                    Some(PhraseData {
                        pos,
                        negate: false,
                        width,
                        maybe: false,
                    })
                }
                (Some(a), None) | (None, Some(a)) if !is_and => Some(a),
                _ => None,
            }
        }
    }
}

/// The positions a vector holds for an operand, by its weights: None when there are
/// none to give, Some(None) for lexemes without positions.
pub(crate) fn find_in(v: &TsVector, n: &Node) -> Option<Found> {
    let Node::Val { lexeme, weight, prefix } = n else {
        return None;
    };
    let entries = v.find(lexeme, *prefix);
    if entries.is_empty() {
        return None;
    }
    let mut pos: Vec<u16> = Vec::new();
    let mut without = false;
    for e in entries {
        if e.positions.is_empty() {
            without = true;
            continue;
        }
        pos.extend(
            e.positions
                .iter()
                .filter(|p: &&Pos| *weight == 0 || weight & (1 << p.weight()) != 0)
                .map(|p| p.pos()),
        );
    }
    if pos.is_empty() {
        return if without && *weight == 0 { Some(None) } else { None };
    }
    pos.sort_unstable();
    pos.dedup();
    Some(Some(pos))
}

/// tsvector @@ tsquery.
pub(crate) fn matches(v: &TsVector, q: &TsQuery) -> bool {
    let Some(root) = &q.root else {
        return false;
    };
    execute(root, &mut |_, n| find_in(v, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_text() {
        let q = |s: &str| TsQuery::parse(s).unwrap().render();
        assert_eq!(q("a & b | c"), "'a' & 'b' | 'c'");
        assert_eq!(q("a & (b | c)"), "'a' & ( 'b' | 'c' )");
        assert_eq!(q("!a <-> b"), "!'a' <-> 'b'");
        assert_eq!(q("a <-> (b <-> c)"), "'a' <-> ( 'b' <-> 'c' )");
        assert_eq!(q("'bread':*"), "'bread':*");
        assert_eq!(q("a:AB <2> b"), "'a':AB <2> 'b'");
        assert!(TsQuery::parse("a &").is_err());
        let e = |s: &str| TsQuery::to_tsquery(Config::English, s).unwrap().render();
        assert_eq!(e("The & Fat & Rats"), "'fat' & 'rat'");
        assert_eq!(e("supernovae:*"), "'supernova':*");
        assert_eq!(
            TsQuery::phraseto(Config::English, "The cat in the hat").render(),
            "'cat' <3> 'hat'"
        );
        assert_eq!(
            TsQuery::plainto(Config::English, "The Fat Rats").render(),
            "'fat' & 'rat'"
        );
    }
}
