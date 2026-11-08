//! The two on-disk HBA formats.

use crate::error::{Error, Result, RuleProblem};
use crate::policy::{Policy, Rule};
use overlite_core::text::{quoted, trim_chars};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::HashMap;
use std::io::Read;

/// Reads an HBA Policy from one on-disk format. ConfParser and YAMLParser are the
/// two implementations.
pub(crate) trait Parser {
    fn parse(&self, r: &mut dyn Read) -> Result<Policy>;
}

/// The longest line accepted. A longer one is an error, whose text is fixed.
const MAX_SCAN_TOKEN_SIZE: usize = 64 * 1024;

/// Parses the classic pg_hba.conf text format: one rule per line,
/// whitespace-separated columns, '#' comments. A `local` line omits the address
/// column.
#[derive(Debug)]
pub(crate) struct ConfParser;

impl Parser for ConfParser {
    fn parse(&self, r: &mut dyn Read) -> Result<Policy> {
        let mut data = Vec::new();
        r.read_to_end(&mut data)?;
        let mut p = Policy::default();
        for (i, raw) in data.split(|c| *c == b'\n').enumerate() {
            if raw.len() >= MAX_SCAN_TOKEN_SIZE {
                return Err(Error::LineTooLong);
            }
            let line = i + 1;
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            let text = String::from_utf8_lossy(raw);
            let text = match text.find('#') {
                Some(i) => &text[..i],
                None => &text[..],
            };
            let toks = tokenize(text);
            if toks.is_empty() {
                continue;
            }
            let mut rule = conf_rule(&toks).map_err(|problem| Error::Rule { line, problem })?;
            rule.compile();
            p.rules.push(rule);
        }
        Ok(p)
    }
}

fn conf_rule(toks: &[String]) -> std::result::Result<Rule, RuleProblem> {
    let mut r = Rule {
        typ: toks[0].to_lowercase(),
        ..Default::default()
    };

    let rest = if r.typ == "local" {
        // type db user method [options]
        if toks.len() < 4 {
            return Err(RuleProblem::ShortLocal);
        }
        r.database = toks[1].clone();
        r.user = toks[2].clone();
        r.method = toks[3].clone();
        &toks[4..]
    } else {
        // type db user address method [options]
        if toks.len() < 5 {
            return Err(RuleProblem::ShortHost(r.typ));
        }
        r.database = toks[1].clone();
        r.user = toks[2].clone();
        r.address = toks[3].clone();
        r.method = toks[4].clone();
        &toks[5..]
    };
    r.options = parse_options(rest);
    Ok(r)
}

/// Splits a line on whitespace, honoring "double quoted" tokens.
fn tokenize(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\r') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += 1;
            }
            toks.push(line[i + 1..j].to_string());
            i = j + 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j] != b' ' && b[j] != b'\t' && b[j] != b'\r' {
            j += 1;
        }
        toks.push(line[i..j].to_string());
        i = j;
    }
    toks
}

fn parse_options(toks: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::with_capacity(toks.len());
    for t in toks {
        if let Some(i) = t.find('=')
            && i > 0
        {
            m.insert(t[..i].to_string(), trim_chars(&t[i + 1..], "\"").to_string());
        }
    }
    m
}

/// Parses the YAML representation: a top-level `hba:` list of rules with
/// type/database/user/address/method fields and an optional options map.
#[derive(Debug)]
pub(crate) struct YAMLParser;

/// The fields of one `hba:` entry: a scalar keeps
/// its raw text whatever it would resolve to, and null leaves a field unset.
#[derive(Debug, Default)]
struct YamlRule {
    typ: Option<String>,
    database: Option<String>,
    user: Option<String>,
    address: Option<String>,
    method: Option<String>,
    options: Option<HashMap<String, String>>,
}

impl YamlRule {
    /// A `<<` merge: what the entry set itself wins.
    fn merge(&mut self, m: YamlRule) {
        self.typ = self.typ.take().or(m.typ);
        self.database = self.database.take().or(m.database);
        self.user = self.user.take().or(m.user);
        self.address = self.address.take().or(m.address);
        self.method = self.method.take().or(m.method);
        self.options = self.options.take().or(m.options);
    }
}

/// The decode targets, as the error text names them.
const YAML_RULE_TYPE: &str = "struct { Type string \"yaml:\\\"type\\\"\"; Database string \"yaml:\\\"database\\\"\"; User string \"yaml:\\\"user\\\"\"; Address string \"yaml:\\\"address\\\"\"; Method string \"yaml:\\\"method\\\"\"; Options map[string]string \"yaml:\\\"options\\\"\" }";
const YAML_OPTIONS_TYPE: &str = "map[string]string";

/// Marks a message of ours among serde_yaml's in the error it passes back.
const DECODE_ERROR: char = '\u{1}';

fn decode_error<E: de::Error>(msg: String) -> E {
    E::custom(format!("{DECODE_ERROR}{msg}"))
}

/// What a YAML node is decoded into.
#[derive(Debug, Clone, Copy)]
enum Target {
    Document,
    Rules,
    Rule,
    /// The value of `<<`: a mapping, or a sequence of them.
    Merge,
    Options,
}

impl Target {
    fn decode_target_name(self) -> String {
        match self {
            Target::Document => format!("struct {{ HBA []{YAML_RULE_TYPE} \"yaml:\\\"hba\\\"\" }}"),
            Target::Rules => format!("[]{YAML_RULE_TYPE}"),
            Target::Rule | Target::Merge => YAML_RULE_TYPE.to_string(),
            Target::Options => YAML_OPTIONS_TYPE.to_string(),
        }
    }
}

#[derive(Debug)]
enum Decoded {
    /// Null: the field stays as it was.
    Nothing,
    Rules(Vec<YamlRule>),
    Rule(YamlRule),
    Options(HashMap<String, String>),
}

impl<'de> DeserializeSeed<'de> for Target {
    type Value = Decoded;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Decoded, D::Error> {
        d.deserialize_any(self)
    }
}

impl Target {
    fn scalar<E: de::Error>(self, tag: &str, v: &dyn std::fmt::Display) -> std::result::Result<Decoded, E> {
        Err(decode_error(format!(
            "cannot unmarshal {tag} `{v}` into {}",
            self.decode_target_name()
        )))
    }

    fn collection<E: de::Error>(self, tag: &str) -> std::result::Result<Decoded, E> {
        Err(decode_error(format!(
            "cannot unmarshal {tag} into {}",
            self.decode_target_name()
        )))
    }
}

impl<'de> Visitor<'de> for Target {
    type Value = Decoded;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.decode_target_name())
    }

    fn visit_unit<E: de::Error>(self) -> std::result::Result<Decoded, E> {
        Ok(Decoded::Nothing)
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<Decoded, E> {
        Ok(Decoded::Nothing)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Decoded, E> {
        self.scalar("!!bool", &v)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Decoded, E> {
        self.scalar("!!int", &v)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Decoded, E> {
        self.scalar("!!int", &v)
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Decoded, E> {
        self.scalar("!!float", &v)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Decoded, E> {
        self.scalar("!!str", &v)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Decoded, A::Error> {
        match self {
            Target::Rules => {
                let mut rules = Vec::new();
                while let Some(e) = a.next_element_seed(Target::Rule)? {
                    // A null element of a list is dropped.
                    if let Decoded::Rule(r) = e {
                        rules.push(r);
                    }
                }
                Ok(Decoded::Rules(rules))
            }
            Target::Merge => {
                let mut out = YamlRule::default();
                while let Some(e) = a.next_element_seed(Target::Rule)? {
                    if let Decoded::Rule(r) = e {
                        out.merge(r);
                    }
                }
                Ok(Decoded::Rule(out))
            }
            _ => self.collection("!!seq"),
        }
    }

    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> std::result::Result<Decoded, A::Error> {
        let mut seen: Vec<String> = Vec::new();
        let mut key = move |a: &mut A| -> std::result::Result<Option<Option<String>>, A::Error> {
            a.next_key_seed(UniqueKey(&mut seen))
        };
        match self {
            Target::Document => {
                let mut rules = Vec::new();
                while let Some(k) = key(&mut a)? {
                    if k.as_deref() == Some("hba") {
                        if let Decoded::Rules(r) = a.next_value_seed(Target::Rules)? {
                            rules = r;
                        }
                    } else {
                        a.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Decoded::Rules(rules))
            }
            Target::Rule | Target::Merge => {
                let mut r = YamlRule::default();
                let mut merged = YamlRule::default();
                while let Some(k) = key(&mut a)? {
                    match k.as_deref() {
                        Some("type") => r.typ = a.next_value_seed(RawScalar)?,
                        Some("database") => r.database = a.next_value_seed(RawScalar)?,
                        Some("user") => r.user = a.next_value_seed(RawScalar)?,
                        Some("address") => r.address = a.next_value_seed(RawScalar)?,
                        Some("method") => r.method = a.next_value_seed(RawScalar)?,
                        Some("options") => {
                            if let Decoded::Options(o) = a.next_value_seed(Target::Options)? {
                                r.options = Some(o);
                            }
                        }
                        Some("<<") => {
                            if let Decoded::Rule(m) = a.next_value_seed(Target::Merge)? {
                                merged.merge(m);
                            }
                        }
                        _ => {
                            a.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                r.merge(merged);
                Ok(Decoded::Rule(r))
            }
            Target::Options => {
                let mut o = HashMap::new();
                while let Some(k) = key(&mut a)? {
                    let v = a.next_value_seed(RawScalar)?;
                    // A null key is no key; a null value is the empty string.
                    if let Some(k) = k {
                        o.insert(k, v.unwrap_or_default());
                    }
                }
                Ok(Decoded::Options(o))
            }
            Target::Rules => self.collection("!!map"),
        }
    }
}

/// A scalar into a string: its text as written, None for null.
#[derive(Debug)]
struct RawScalar;

impl<'de> DeserializeSeed<'de> for RawScalar {
    type Value = Option<String>;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Option<String>, D::Error> {
        d.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for RawScalar {
    type Value = Option<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("string")
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<Option<String>, E> {
        Ok(None)
    }

    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Option<String>, D::Error> {
        d.deserialize_str(self)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Option<String>, E> {
        Ok(Some(v.to_string()))
    }
}

/// A mapping key: raw text, and each only once.
#[derive(Debug)]
struct UniqueKey<'a>(&'a mut Vec<String>);

impl<'de> DeserializeSeed<'de> for UniqueKey<'_> {
    type Value = Option<String>;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Option<String>, D::Error> {
        d.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for UniqueKey<'_> {
    type Value = Option<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("string")
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<Option<String>, E> {
        Ok(None)
    }

    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Option<String>, D::Error> {
        d.deserialize_str(self)
    }

    fn visit_str<E: de::Error>(self, k: &str) -> std::result::Result<Option<String>, E> {
        if self.0.iter().any(|s| s == k) {
            return Err(E::custom(format!("{DECODE_ERROR}{DECODE_ERROR}{k}")));
        }
        self.0.push(k.to_string());
        Ok(Some(k.to_string()))
    }
}

/// The line, 1-based, of the nearest earlier key `key` at the same indent.
fn earlier_key_line(data: &[u8], key: &str, line: usize) -> usize {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.lines().collect();
    let indent = |l: &str| l.len() - l.trim_start_matches([' ', '-']).len();
    let want = lines.get(line.wrapping_sub(1)).map_or(0, |l| indent(l));
    let mine = format!("{key}:");
    for n in (0..line.saturating_sub(1).min(lines.len())).rev() {
        if indent(lines[n]) == want && lines[n].trim_start_matches([' ', '-']).starts_with(&mine) {
            return n + 1;
        }
    }
    line
}

/// A 1-based `line N` in serde_yaml's spelling of a mark.
fn mark_line(s: &str) -> Option<usize> {
    s.split_once(" at line ")?.1.split(' ').next()?.parse().ok()
}

/// The line an error is reported at: the 0-based line of the context, else of
/// the problem, one more for a scanner error.
fn yaml_syntax_error(full: &str) -> Error {
    let (problem_part, context) = match full.split_once(", while ") {
        Some((p, c)) => (p, Some(c)),
        None => (full, None),
    };
    let problem = problem_part.split(" at line ").next().unwrap_or(problem_part);
    let problem = problem.split(" at position ").next().unwrap_or(problem);
    let parser = context.is_some_and(|c| c.starts_with("parsing"))
        || problem.starts_with("did not find expected <")
        || problem.starts_with("found duplicate")
        || problem.starts_with("found incompatible")
        || problem.starts_with("found undefined");
    let zero_based = |l: Option<usize>| l.unwrap_or(1).saturating_sub(1);
    let (ctx, prob) = (
        zero_based(context.and_then(mark_line)),
        zero_based(mark_line(problem_part)),
    );
    let line = if ctx != 0 { ctx } else { prob };
    if line == 0 {
        return Error::Yaml(format!("yaml: {problem}"));
    }
    Error::Yaml(format!("yaml: line {}: {problem}", line + usize::from(!parser)))
}

/// serde_yaml's errors in the wording of the error text kept for operators.
fn yaml_error(e: serde_yaml::Error, data: &[u8]) -> Error {
    let full = e.to_string();
    let line = e.location().map_or(0, |l| l.line());
    let unmarshal = |msg: &str| Error::Yaml(format!("yaml: unmarshal errors:\n  line {line}: {msg}"));
    if let Some((_, msg)) = full.split_once(DECODE_ERROR) {
        let msg = msg.rsplit_once(" at line ").map_or(msg, |(m, _)| m);
        if let Some(key) = msg.strip_prefix(DECODE_ERROR) {
            let first = earlier_key_line(data, key, line);
            return unmarshal(&format!(
                "mapping key {:?} already defined at line {first}",
                quoted(key)
            ));
        }
        return unmarshal(msg);
    }
    if full.contains("invalid type: sequence") {
        return unmarshal("cannot unmarshal !!seq into string");
    }
    if full.contains("invalid type: map") {
        return unmarshal("cannot unmarshal !!map into string");
    }
    if full.starts_with("unknown anchor") {
        let at = e.location().map_or(0, |l| l.index());
        let rest = String::from_utf8_lossy(data.get(at..).unwrap_or_default()).into_owned();
        let name: String = rest
            .trim_start_matches('*')
            .chars()
            .take_while(|c| !c.is_whitespace() && !",[]{}".contains(*c))
            .collect();
        return Error::Yaml(format!("yaml: unknown anchor '{name}' referenced"));
    }
    if e.location().is_some() && !full.contains("at line") {
        return Error::Yaml(format!("yaml: {full}"));
    }
    yaml_syntax_error(&full)
}

impl Parser for YAMLParser {
    fn parse(&self, r: &mut dyn Read) -> Result<Policy> {
        let mut data = Vec::new();
        r.read_to_end(&mut data)?;
        let mut p = Policy::default();
        // The first document only, and an empty file is none.
        let Some(doc) = serde_yaml::Deserializer::from_slice(&data).next() else {
            return Ok(p);
        };
        let Decoded::Rules(rules) = Target::Document.deserialize(doc).map_err(|e| yaml_error(e, &data))? else {
            return Ok(p);
        };
        for y in rules {
            let mut rule = Rule {
                typ: y.typ.unwrap_or_default().to_lowercase(),
                database: or_default(y.database.unwrap_or_default(), "all"),
                user: or_default(y.user.unwrap_or_default(), "all"),
                address: y.address.unwrap_or_default(),
                method: y.method.unwrap_or_default(),
                options: y.options.unwrap_or_default(),
                cidr: None,
            };
            rule.compile();
            p.rules.push(rule);
        }
        Ok(p)
    }
}

fn or_default(v: String, def: &str) -> String {
    if v.is_empty() {
        return def.to_string();
    }
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn parse_conf() {
        let p = ConfParser
            .parse(
                &mut "
# a comment
local   all       all                        trust
host    all       all       127.0.0.1/32     scram-sha-256
hostssl shop      appuser   10.0.0.0/8       md5           clientcert=verify-full
host    all       all       0.0.0.0/0        reject
"
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(p.rules.len(), 4);

        assert_eq!(p.rules[0].typ, "local");
        assert_eq!(p.rules[0].method, "trust");
        assert_eq!(p.rules[1].method, "scram-sha-256");
        assert_eq!(p.rules[2].address, "10.0.0.0/8");
        assert_eq!(p.rules[2].options["clientcert"], "verify-full");
        assert_eq!(p.rules[3].method, "reject");
    }

    #[test]
    fn parse_yaml() {
        let p = YAMLParser
            .parse(
                &mut "
hba:
  - { type: local, database: all, user: all, method: trust }
  - type: host
    database: all
    user: all
    address: 127.0.0.1/32
    method: scram-sha-256
  - type: hostssl
    database: shop
    user: appuser
    address: 10.0.0.0/8
    method: md5
    options:
      clientcert: verify-full
  - type: host
    database: all
    user: all
    address: 0.0.0.0/0
    method: reject
"
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(p.rules.len(), 4);
        assert_eq!(p.rules[0].method, "trust");
        assert_eq!(p.rules[1].method, "scram-sha-256");
        assert_eq!(p.rules[2].options["clientcert"], "verify-full");
        assert_eq!(p.rules[3].method, "reject");
    }
}

#[cfg(test)]
mod yaml_tests {
    use super::*;

    pub(crate) fn parse(y: &str) -> Result<Policy> {
        YAMLParser.parse(&mut y.as_bytes())
    }

    #[test]
    fn only_the_first_document_counts() {
        let p = parse("hba:\n  - type: host\n    method: trust\n---\nnot: [valid\n").unwrap();
        assert_eq!(p.rules.len(), 1);
        assert!(parse("---\n---\nhba:\n  - type: host\n").unwrap().rules.is_empty());
    }

    #[test]
    fn a_scalar_keeps_its_text() {
        let p = parse("hba:\n  - database: 0x10\n    user: 1.0\n    address: 1e3\n    method: yes\n    options:\n      a: True\n      b: ~\n      5: x\n").unwrap();
        let r = &p.rules[0];
        assert_eq!(
            (
                r.database.as_str(),
                r.user.as_str(),
                r.address.as_str(),
                r.method.as_str()
            ),
            ("0x10", "1.0", "1e3", "yes")
        );
        assert_eq!(r.options["a"], "True");
        assert_eq!(r.options["b"], "");
        assert_eq!(r.options["5"], "x");
    }

    #[test]
    fn null_keeps_the_defaults_and_quoted_null_is_text() {
        let p = parse("hba:\n  - ~\n  - type: ~\n    user:\n  - database: \"null\"\n").unwrap();
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.rules[0].user, "all");
        assert_eq!(p.rules[1].database, "null");
    }

    #[test]
    fn aliases_and_merges_apply() {
        let p = parse("base: &b\n  type: host\nhba:\n  - <<: *b\n    method: md5\n  - *b\n").unwrap();
        assert_eq!((p.rules[0].typ.as_str(), p.rules[0].method.as_str()), ("host", "md5"));
        assert_eq!(p.rules[1].typ, "host");
    }

    #[test]
    fn errors_read_as_yaml_v3() {
        for (y, want) in [
            (
                "hba:\n  - type: host\n   method: x\n",
                "yaml: line 1: did not find expected '-' indicator",
            ),
            ("hba: [\n", "yaml: line 1: did not find expected node content"),
            ("a: b: c\n", "yaml: mapping values are not allowed in this context"),
            (
                "hba:\n\t- x\n",
                "yaml: line 2: found character that cannot start any token",
            ),
            ("\"unterminated\n", "yaml: line 2: found unexpected end of stream"),
            (
                "hba:\n  - type: host\n  type2\n",
                "yaml: line 3: could not find expected ':'",
            ),
            (
                "a: 1\na: 2\n",
                "yaml: unmarshal errors:\n  line 2: mapping key \"a\" already defined at line 1",
            ),
            ("hba:\n  - method: *nope\n", "yaml: unknown anchor 'nope' referenced"),
            (
                "hba:\n  - type: [a, b]\n",
                "yaml: unmarshal errors:\n  line 2: cannot unmarshal !!seq into string",
            ),
            (
                "hba:\n  - options: x\n",
                "yaml: unmarshal errors:\n  line 2: cannot unmarshal !!str `x` into map[string]string",
            ),
        ] {
            assert_eq!(parse(y).err().unwrap().to_string(), want);
        }
    }
}
