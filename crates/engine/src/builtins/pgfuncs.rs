//! Postgres' string, math, hashing, network and statistics functions that SQLite
//! lacks, each as Postgres' manual defines it: what it does with NULL, with a
//! negative count, with characters outside ASCII.

use crate::builtins::regex::compile_regex;
use crate::catalog::{AggregateFunction, aggregate_fn, scalar_fn};
use crate::error::{Result, errf};
use md5::{Digest, Md5};
use overlite_core::{SqlState, Value};
use rusqlite::Connection;
use sha2::{Sha224, Sha256, Sha384, Sha512};

/// The text of an argument: its string, or the bytes of a blob read as UTF-8.
fn text(v: &Value) -> String {
    match v {
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        v => v.to_string(),
    }
}

/// The bytes of an argument, as bytea or as the text a client sent.
fn bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.clone(),
        Value::Text(s) => match s.strip_prefix("\\x") {
            Some(h) => hex::decode(h).unwrap_or_else(|_| s.as_bytes().to_vec()),
            None => s.as_bytes().to_vec(),
        },
        v => v.to_string().into_bytes(),
    }
}

fn int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Float(f) => Ok(f.round() as i64),
        Value::Bool(b) => Ok(i64::from(*b)),
        v => v
            .to_string()
            .trim()
            .parse::<f64>()
            .map(|f| f.round() as i64)
            .map_err(|_| {
                errf!(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type integer: \"{v}\""
                )
            }),
    }
}

fn float(v: &Value) -> Result<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        v => v.to_string().trim().parse::<f64>().map_err(|_| {
            errf!(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type double precision: \"{v}\""
            )
        }),
    }
}

/// Registers f under name for n arguments (-1 any), NULL in any argument giving
/// NULL, as a STRICT function does.
fn strict<F>(conn: &Connection, name: &str, n: i32, f: F) -> Result<()>
where
    F: Fn(&[Value]) -> Result<Value> + Send + 'static,
{
    scalar_fn(conn, name, n, move |a| {
        if a.iter().any(Value::is_null) {
            return Ok(Value::Null);
        }
        f(a)
    })
}

/// The first n characters, or all but the last -n.
fn left(s: &str, n: i64) -> String {
    let len = s.chars().count() as i64;
    let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
    s.chars().take(take as usize).collect()
}

/// The last n characters, or all but the first -n.
fn right(s: &str, n: i64) -> String {
    let len = s.chars().count() as i64;
    let skip = if n >= 0 { (len - n).max(0) } else { (-n).min(len) };
    s.chars().skip(skip as usize).collect()
}

/// lpad and rpad: fill to len characters with fill, or cut to len.
fn pad(s: &str, len: i64, fill: &str, at_left: bool) -> String {
    let len = len.max(0) as usize;
    let n = s.chars().count();
    if n >= len {
        return s.chars().take(len).collect();
    }
    if fill.is_empty() {
        return s.to_string();
    }
    let filler: String = fill.chars().cycle().take(len - n).collect();
    if at_left { filler + s } else { s.to_string() + &filler }
}

/// initcap: the first letter of each run of letters and digits upper case, the rest
/// lower.
fn initcap(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_word = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if in_word {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            in_word = true;
        } else {
            out.push(c);
            in_word = false;
        }
    }
    out
}

/// btrim, ltrim and rtrim with a set of characters.
fn trim_set(s: &str, set: &str, left: bool, right: bool) -> String {
    let mut t = s;
    if left {
        t = t.trim_start_matches(|c| set.contains(c));
    }
    if right {
        t = t.trim_end_matches(|c| set.contains(c));
    }
    t.to_string()
}

/// translate: each character of from becomes the one at its place in to, or goes
/// when to is shorter.
fn translate(s: &str, from: &str, to: &str) -> String {
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    s.chars()
        .filter_map(|c| match from.iter().position(|&f| f == c) {
            Some(i) => to.get(i).copied(),
            None => Some(c),
        })
        .collect()
}

/// split_part: the nth field, counted from the end when n is negative.
fn split_part(s: &str, delim: &str, n: i64) -> Result<String> {
    if n == 0 {
        return Err(errf!(
            SqlState::INVALID_PARAMETER_VALUE,
            "field position must not be zero"
        ));
    }
    let parts: Vec<&str> = if delim.is_empty() {
        vec![s]
    } else {
        s.split(delim).collect()
    };
    let i = if n > 0 { n - 1 } else { parts.len() as i64 + n };
    Ok(usize::try_from(i)
        .ok()
        .and_then(|i| parts.get(i))
        .map_or_else(String::new, |p| (*p).to_string()))
}

/// A Postgres array literal of text elements, quoted where they have to be.
fn text_array(items: &[Option<String>]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|i| match i {
            None => "NULL".to_string(),
            Some(s)
                if s.is_empty()
                    || s.eq_ignore_ascii_case("null")
                    || s.chars()
                        .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace()) =>
            {
                ["\"", &s.replace('\\', "\\\\").replace('"', "\\\""), "\""].concat()
            }
            Some(s) => s.clone(),
        })
        .collect();
    ["{", &quoted.join(","), "}"].concat()
}

/// string_to_array(s, delim [, null_string]).
fn string_to_array(a: &[Value]) -> Value {
    if a[0].is_null() {
        return Value::Null;
    }
    let s = text(&a[0]);
    if s.is_empty() {
        return Value::Text("{}".into());
    }
    let null_as = a.get(2).filter(|v| !v.is_null()).map(text);
    let items: Vec<String> = if a[1].is_null() {
        s.chars().map(String::from).collect()
    } else {
        let d = text(&a[1]);
        if d.is_empty() {
            vec![s]
        } else {
            s.split(d.as_str()).map(str::to_string).collect()
        }
    };
    let items: Vec<Option<String>> = items
        .into_iter()
        .map(|i| {
            if null_as.as_deref() == Some(i.as_str()) {
                None
            } else {
                Some(i)
            }
        })
        .collect();
    Value::Text(text_array(&items))
}

/// The bytes of a digest by Postgres' name for it.
fn digest(data: &[u8], alg: &str) -> Result<Vec<u8>> {
    Ok(match alg.to_ascii_lowercase().as_str() {
        "md5" => Md5::digest(data).to_vec(),
        "sha1" => sha1(data).to_vec(),
        "sha224" => Sha224::digest(data).to_vec(),
        "sha256" => Sha256::digest(data).to_vec(),
        "sha384" => Sha384::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        _ => {
            return Err(errf!(
                SqlState::EXTERNAL_ROUTINE_INVOCATION_EXCEPTION,
                "Cannot use \"{alg}\": No such hash algorithm"
            ));
        }
    })
}

/// SHA-1, FIPS 180-4.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// encode(bytea, format).
fn encode(data: &[u8], format: &str) -> Result<String> {
    use base64::Engine;
    Ok(match format.to_ascii_lowercase().as_str() {
        "hex" => hex::encode(data),
        "base64" => {
            let s = base64::engine::general_purpose::STANDARD.encode(data);
            // Postgres breaks base64 every 76 characters.
            s.as_bytes()
                .chunks(76)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        }
        "escape" => {
            let mut out = String::new();
            for &b in data {
                match b {
                    b'\\' => out.push_str("\\\\"),
                    0 | 0x80..=0xff => out.push_str(&format!("\\{b:03o}")),
                    _ => out.push(b as char),
                }
            }
            out
        }
        _ => {
            return Err(errf!(
                SqlState::INVALID_PARAMETER_VALUE,
                "unrecognized encoding: \"{format}\""
            ));
        }
    })
}

/// decode(text, format).
fn decode(s: &str, format: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    match format.to_ascii_lowercase().as_str() {
        "hex" => hex::decode(s.split_whitespace().collect::<String>())
            .map_err(|_| errf!(SqlState::INVALID_PARAMETER_VALUE, "invalid hexadecimal data")),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(s.split_whitespace().collect::<String>())
            .map_err(|_| errf!(SqlState::INVALID_PARAMETER_VALUE, "invalid base64 end sequence")),
        "escape" => {
            let b = s.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'\\' && b.get(i + 1) == Some(&b'\\') {
                    out.push(b'\\');
                    i += 2;
                } else if b[i] == b'\\' && i + 3 < b.len() + 1 && b[i + 1..].len() >= 3 {
                    let oct = std::str::from_utf8(&b[i + 1..i + 4])
                        .ok()
                        .and_then(|o| u8::from_str_radix(o, 8).ok());
                    match oct {
                        Some(v) => out.push(v),
                        None => {
                            return Err(errf!(
                                SqlState::INVALID_PARAMETER_VALUE,
                                "invalid input syntax for type bytea"
                            ));
                        }
                    }
                    i += 4;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            Ok(out)
        }
        _ => Err(errf!(
            SqlState::INVALID_PARAMETER_VALUE,
            "unrecognized encoding: \"{format}\""
        )),
    }
}

/// An inet or cidr value: its address bytes and prefix length.
struct Inet {
    addr: std::net::IpAddr,
    bits: u8,
    /// Whether the text gave the prefix.
    explicit: bool,
}

impl Inet {
    fn parse(s: &str) -> Result<Inet> {
        let s = s.trim();
        let bad = || {
            errf!(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type inet: \"{s}\""
            )
        };
        let (a, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u8>().map_err(|_| bad())?)),
            None => (s, None),
        };
        let addr: std::net::IpAddr = a.parse().map_err(|_| bad())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let b = bits.unwrap_or(max);
        if b > max {
            return Err(bad());
        }
        Ok(Inet {
            addr,
            bits: b,
            explicit: bits.is_some(),
        })
    }

    fn max(&self) -> u8 {
        if self.addr.is_ipv4() { 32 } else { 128 }
    }

    fn as_u128(&self) -> u128 {
        match self.addr {
            std::net::IpAddr::V4(a) => u128::from(u32::from(a)),
            std::net::IpAddr::V6(a) => u128::from(a),
        }
    }

    fn addr_of(&self, n: u128) -> std::net::IpAddr {
        match self.addr {
            std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::from(n as u32)),
            std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::from(n)),
        }
    }

    fn mask(&self) -> u128 {
        let max = u32::from(self.max());
        let host_bits = max - u32::from(self.bits);
        let all = if max == 32 { u128::from(u32::MAX) } else { u128::MAX };
        if host_bits >= 128 {
            0
        } else {
            all & !((1u128 << host_bits).wrapping_sub(1))
        }
    }
}

/// inet functions by name.
fn inet_fn(name: &str, a: &[Value]) -> Result<Value> {
    let i = Inet::parse(&text(&a[0]))?;
    let max = i.max();
    let all = if max == 32 { u128::from(u32::MAX) } else { u128::MAX };
    Ok(match name {
        "host" => Value::Text(i.addr.to_string()),
        "masklen" => Value::Int(i64::from(i.bits)),
        "family" => Value::Int(if i.addr.is_ipv4() { 4 } else { 6 }),
        "netmask" => Value::Text(i.addr_of(i.mask()).to_string()),
        "hostmask" => Value::Text(i.addr_of(!i.mask() & all).to_string()),
        "network" => Value::Text(format!("{}/{}", i.addr_of(i.as_u128() & i.mask()), i.bits)),
        "broadcast" => Value::Text(format!("{}/{}", i.addr_of(i.as_u128() | (!i.mask() & all)), i.bits)),
        "abbrev" => Value::Text(if i.explicit && i.bits != max {
            format!("{}/{}", i.addr, i.bits)
        } else {
            i.addr.to_string()
        }),
        "text" => Value::Text(format!("{}/{}", i.addr, i.bits)),
        "set_masklen" => {
            let b = int(&a[1])?;
            if !(0..=i64::from(max)).contains(&b) {
                return Err(errf!(SqlState::INVALID_PARAMETER_VALUE, "invalid mask length: {b}"));
            }
            Value::Text(format!("{}/{b}", i.addr))
        }
        "inet_same_family" => Value::Bool(i.addr.is_ipv4() == Inet::parse(&text(&a[1]))?.addr.is_ipv4()),
        _ => Value::Null,
    })
}

fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a as i64
}

/// regexp_replace(s, pattern, replacement [, flags]).
fn regexp_replace(a: &[Value]) -> Result<Value> {
    let flags = a.get(3).map(text).unwrap_or_default();
    let pattern = text(&a[1]);
    let pattern = if flags.contains('i') {
        ["(?i)", &pattern].concat()
    } else {
        pattern
    };
    let m = compile_regex(&pattern)?;
    Ok(Value::Text(m.replace_pg(
        &text(&a[0]),
        &text(&a[2]),
        flags.contains('g'),
    )?))
}

/// regexp_match(s, pattern [, flags]): the first match's groups as a text array.
fn regexp_match(a: &[Value]) -> Result<Value> {
    let flags = a.get(2).map(text).unwrap_or_default();
    let pattern = text(&a[1]);
    let pattern = if flags.contains('i') {
        ["(?i)", &pattern].concat()
    } else {
        pattern
    };
    let m = compile_regex(&pattern)?;
    Ok(match m.captures_pg(&text(&a[0]))? {
        None => Value::Null,
        Some(groups) => Value::Text(text_array(&groups)),
    })
}

fn regex_with_flags(
    pattern: &Value,
    flags: Option<&Value>,
) -> Result<std::sync::Arc<crate::builtins::regex::MatcherKind>> {
    let flags = flags.map(text).unwrap_or_default();
    let pattern = text(pattern);
    compile_regex(&if flags.contains('i') {
        ["(?i)", &pattern].concat()
    } else {
        pattern
    })
}

/// A trigonometric function in degrees.
type DegreeFn = fn(f64) -> f64;

/// Registers every function of this module on conn.
pub(crate) fn register(conn: &Connection) -> Result<()> {
    strict(conn, "ascii", 1, |a| {
        Ok(Value::Int(
            text(&a[0]).chars().next().map_or(0, |c| i64::from(u32::from(c))),
        ))
    })?;
    strict(conn, "chr", 1, |a| {
        let n = int(&a[0])?;
        if n == 0 {
            return Err(errf!(SqlState::PROGRAM_LIMIT_EXCEEDED, "null character not permitted"));
        }
        u32::try_from(n)
            .ok()
            .and_then(char::from_u32)
            .map(|c| Value::Text(c.to_string()))
            .ok_or_else(|| {
                errf!(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    "requested character too large for encoding: {n}"
                )
            })
    })?;
    strict(conn, "bit_length", 1, |a| Ok(Value::Int(bytes(&a[0]).len() as i64 * 8)))?;
    for name in ["char_length", "character_length"] {
        strict(conn, name, 1, |a| Ok(Value::Int(text(&a[0]).chars().count() as i64)))?;
    }
    strict(conn, "initcap", 1, |a| Ok(Value::Text(initcap(&text(&a[0])))))?;
    strict(conn, "left", 2, |a| Ok(Value::Text(left(&text(&a[0]), int(&a[1])?))))?;
    strict(conn, "right", 2, |a| Ok(Value::Text(right(&text(&a[0]), int(&a[1])?))))?;
    strict(conn, "lpad", 2, |a| {
        Ok(Value::Text(pad(&text(&a[0]), int(&a[1])?, " ", true)))
    })?;
    strict(conn, "lpad", 3, |a| {
        Ok(Value::Text(pad(&text(&a[0]), int(&a[1])?, &text(&a[2]), true)))
    })?;
    strict(conn, "rpad", 2, |a| {
        Ok(Value::Text(pad(&text(&a[0]), int(&a[1])?, " ", false)))
    })?;
    strict(conn, "rpad", 3, |a| {
        Ok(Value::Text(pad(&text(&a[0]), int(&a[1])?, &text(&a[2]), false)))
    })?;
    strict(conn, "btrim", 1, |a| {
        Ok(Value::Text(trim_set(&text(&a[0]), " ", true, true)))
    })?;
    strict(conn, "btrim", 2, |a| {
        Ok(Value::Text(trim_set(&text(&a[0]), &text(&a[1]), true, true)))
    })?;
    strict(conn, "repeat", 2, |a| {
        let n = int(&a[1])?.max(0) as usize;
        let s = text(&a[0]);
        if s.len().saturating_mul(n) > 1 << 30 {
            return Err(errf!(SqlState::PROGRAM_LIMIT_EXCEEDED, "requested length too large"));
        }
        Ok(Value::Text(s.repeat(n)))
    })?;
    strict(conn, "reverse", 1, |a| {
        Ok(Value::Text(text(&a[0]).chars().rev().collect()))
    })?;
    strict(conn, "strpos", 2, |a| {
        let (s, sub) = (text(&a[0]), text(&a[1]));
        Ok(Value::Int(
            s.find(&sub).map_or(0, |i| s[..i].chars().count() as i64 + 1),
        ))
    })?;
    strict(conn, "starts_with", 2, |a| {
        Ok(Value::Bool(text(&a[0]).starts_with(&text(&a[1]))))
    })?;
    strict(conn, "split_part", 3, |a| {
        Ok(Value::Text(split_part(&text(&a[0]), &text(&a[1]), int(&a[2])?)?))
    })?;
    strict(conn, "translate", 3, |a| {
        Ok(Value::Text(translate(&text(&a[0]), &text(&a[1]), &text(&a[2]))))
    })?;
    strict(conn, "to_hex", 1, |a| Ok(Value::Text(format!("{:x}", int(&a[0])?))))?;
    scalar_fn(conn, "quote_nullable", 1, |a| {
        Ok(Value::Text(if a[0].is_null() {
            "NULL".to_string()
        } else {
            let s = text(&a[0]);
            let q = ["'", &s.replace('\'', "''"), "'"].concat();
            if s.contains('\\') {
                ["E", &q.replace('\\', "\\\\")].concat()
            } else {
                q
            }
        }))
    })?;
    scalar_fn(conn, "string_to_array", 2, |a| Ok(string_to_array(a)))?;
    scalar_fn(conn, "string_to_array", 3, |a| Ok(string_to_array(a)))?;
    scalar_fn(conn, "num_nulls", -1, |a| {
        Ok(Value::Int(a.iter().filter(|v| v.is_null()).count() as i64))
    })?;
    scalar_fn(conn, "num_nonnulls", -1, |a| {
        Ok(Value::Int(a.iter().filter(|v| !v.is_null()).count() as i64))
    })?;
    scalar_fn(conn, "text", 1, |a| {
        Ok(if a[0].is_null() {
            Value::Null
        } else {
            Value::Text(text(&a[0]))
        })
    })?;

    // Hashes and bytea.
    strict(conn, "md5", 1, |a| {
        Ok(Value::Text(hex::encode(Md5::digest(bytes(&a[0])))))
    })?;
    for alg in ["sha224", "sha256", "sha384", "sha512"] {
        strict(conn, alg, 1, move |a| Ok(Value::Blob(digest(&bytes(&a[0]), alg)?)))?;
    }
    strict(conn, "digest", 2, |a| {
        Ok(Value::Blob(digest(&bytes(&a[0]), &text(&a[1]))?))
    })?;
    strict(conn, "encode", 2, |a| {
        Ok(Value::Text(encode(&bytes(&a[0]), &text(&a[1]))?))
    })?;
    strict(conn, "decode", 2, |a| {
        Ok(Value::Blob(decode(&text(&a[0]), &text(&a[1]))?))
    })?;
    strict(conn, "convert_to", 2, |a| Ok(Value::Blob(text(&a[0]).into_bytes())))?;
    strict(conn, "convert_from", 2, |a| Ok(Value::Text(text(&a[0]))))?;

    // Regular expressions.
    for n in [3, 4] {
        strict(conn, "regexp_replace", n, regexp_replace)?;
    }
    for n in [2, 3] {
        strict(conn, "regexp_match", n, regexp_match)?;
        strict(conn, "regexp_like", n, |a| {
            Ok(Value::Bool(regex_with_flags(&a[1], a.get(2))?.is_match(&text(&a[0]))?))
        })?;
    }
    strict(conn, "regexp_count", 2, |a| {
        Ok(Value::Int(
            regex_with_flags(&a[1], None)?.find_spans(&text(&a[0]))?.len() as i64,
        ))
    })?;
    strict(conn, "regexp_substr", 2, |a| {
        let s = text(&a[0]);
        Ok(regex_with_flags(&a[1], None)?
            .find_spans(&s)?
            .first()
            .map_or(Value::Null, |&(b, e)| Value::Text(s[b..e].to_string())))
    })?;
    strict(conn, "regexp_instr", 2, |a| {
        let s = text(&a[0]);
        Ok(Value::Int(
            regex_with_flags(&a[1], None)?
                .find_spans(&s)?
                .first()
                .map_or(0, |&(b, _)| s[..b].chars().count() as i64 + 1),
        ))
    })?;

    // Math.
    strict(conn, "cbrt", 1, |a| Ok(Value::Float(float(&a[0])?.cbrt())))?;
    strict(conn, "gamma", 1, |a| gamma(float(&a[0])?).map(Value::Float))?;
    strict(conn, "lgamma", 1, |a| lgamma(float(&a[0])?).map(Value::Float))?;
    strict(conn, "cot", 1, |a| Ok(Value::Float(1.0 / float(&a[0])?.tan())))?;
    let degree_fns: [(&str, DegreeFn); 7] = [
        ("sind", |x| x.to_radians().sin()),
        ("cosd", |x| x.to_radians().cos()),
        ("tand", |x| x.to_radians().tan()),
        ("cotd", |x| 1.0 / x.to_radians().tan()),
        ("asind", |x| x.asin().to_degrees()),
        ("acosd", |x| x.acos().to_degrees()),
        ("atand", |x| x.atan().to_degrees()),
    ];
    for (name, f) in degree_fns {
        strict(conn, name, 1, move |a| {
            let r = f(float(&a[0])?);
            // Exact at the angles Postgres makes exact (sind(30) is 0.5).
            let exact = [r.round(), (r * 2.0).round() / 2.0]
                .into_iter()
                .find(|e| (r - e).abs() < 1e-12);
            Ok(Value::Float(exact.unwrap_or(r)))
        })?;
    }
    strict(conn, "atan2d", 2, |a| {
        Ok(Value::Float(float(&a[0])?.atan2(float(&a[1])?).to_degrees()))
    })?;
    strict(conn, "div", 2, |a| {
        let (x, y) = (float(&a[0])?, float(&a[1])?);
        if y == 0.0 {
            return Err(errf!(SqlState::DIVISION_BY_ZERO, "division by zero"));
        }
        Ok(Value::Int((x / y).trunc() as i64))
    })?;
    strict(conn, "factorial", 1, |a| {
        let n = int(&a[0])?;
        if n < 0 {
            return Err(errf!(
                SqlState::INVALID_PARAMETER_VALUE,
                "factorial of a negative number is undefined"
            ));
        }
        let mut r: i64 = 1;
        for k in 2..=n {
            r = r
                .checked_mul(k)
                .ok_or_else(|| errf!(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format"))?;
        }
        Ok(Value::Int(r))
    })?;
    // JSON_ARRAY's and JSON_OBJECT's ABSENT ON NULL: the array or object without its
    // nulls, elements and members in the order written.
    strict(conn, "overlite_json_absent", 1, |a| {
        let s = text(&a[0]);
        if let Some(items) = crate::builtins::json::json_unmarshal_raw_array(&s) {
            let kept: Vec<&str> = items.into_iter().filter(|i| i.trim() != "null").collect();
            return Ok(Value::Text(["[", &kept.join(","), "]"].concat()));
        }
        if let Some(members) = crate::builtins::json::json_raw_object(&s) {
            let kept: Vec<String> = members
                .into_iter()
                .filter(|(_, v)| v.trim() != "null")
                .map(|(k, v)| [crate::builtins::json::json_quote(&k).as_str(), ":", v].concat())
                .collect();
            return Ok(Value::Text(["{", &kept.join(","), "}"].concat()));
        }
        Ok(Value::Text(s))
    })?;
    strict(conn, "overlite_xor", 2, |a| Ok(Value::Int(int(&a[0])? ^ int(&a[1])?)))?;
    strict(conn, "gcd", 2, |a| Ok(Value::Int(gcd(int(&a[0])?, int(&a[1])?))))?;
    strict(conn, "lcm", 2, |a| {
        let (x, y) = (int(&a[0])?, int(&a[1])?);
        if x == 0 || y == 0 {
            return Ok(Value::Int(0));
        }
        Ok(Value::Int((x / gcd(x, y) * y).abs()))
    })?;
    strict(conn, "trunc", 2, |a| {
        let (x, d) = (float(&a[0])?, int(&a[1])?);
        let p = 10f64.powi(d as i32);
        Ok(Value::Float((x * p).trunc() / p))
    })?;
    strict(conn, "width_bucket", 4, |a| {
        let (x, lo, hi, n) = (float(&a[0])?, float(&a[1])?, float(&a[2])?, int(&a[3])?);
        if n <= 0 {
            return Err(errf!(
                SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION,
                "count must be greater than zero"
            ));
        }
        if (lo - hi).abs() < f64::EPSILON {
            return Err(errf!(
                SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION,
                "lower bound cannot equal upper bound"
            ));
        }
        let b = if lo < hi {
            if x < lo {
                0
            } else if x >= hi {
                n + 1
            } else {
                ((x - lo) / (hi - lo) * n as f64) as i64 + 1
            }
        } else if x > lo {
            0
        } else if x <= hi {
            n + 1
        } else {
            ((lo - x) / (lo - hi) * n as f64) as i64 + 1
        };
        Ok(Value::Int(b))
    })?;

    // Network addresses.
    for name in [
        "host",
        "masklen",
        "family",
        "netmask",
        "hostmask",
        "network",
        "broadcast",
        "abbrev",
    ] {
        strict(conn, name, 1, move |a| inet_fn(name, a))?;
    }
    for name in ["set_masklen", "inet_same_family"] {
        strict(conn, name, 2, move |a| inet_fn(name, a))?;
    }

    // Aggregates.
    for (name, kind) in [
        ("stddev", Stat::StddevSamp),
        ("stddev_samp", Stat::StddevSamp),
        ("stddev_pop", Stat::StddevPop),
        ("variance", Stat::VarSamp),
        ("var_samp", Stat::VarSamp),
        ("var_pop", Stat::VarPop),
    ] {
        aggregate_fn(conn, name, 1, stat_factory(kind))?;
    }
    for (name, kind) in [
        ("corr", Stat::Corr),
        ("covar_pop", Stat::CovarPop),
        ("covar_samp", Stat::CovarSamp),
        ("regr_avgx", Stat::RegrAvgX),
        ("regr_avgy", Stat::RegrAvgY),
        ("regr_count", Stat::RegrCount),
        ("regr_intercept", Stat::RegrIntercept),
        ("regr_r2", Stat::RegrR2),
        ("regr_slope", Stat::RegrSlope),
        ("regr_sxx", Stat::RegrSxx),
        ("regr_sxy", Stat::RegrSxy),
        ("regr_syy", Stat::RegrSyy),
    ] {
        aggregate_fn(conn, name, 2, stat_factory(kind))?;
    }
    aggregate_fn(conn, "bit_and", 1, || -> Box<dyn AggregateFunction> {
        Box::new(Bits {
            op: BitOp::And,
            ..Bits::default()
        })
    })?;
    aggregate_fn(conn, "bit_or", 1, || -> Box<dyn AggregateFunction> {
        Box::new(Bits {
            op: BitOp::Or,
            ..Bits::default()
        })
    })?;
    aggregate_fn(conn, "bit_xor", 1, || -> Box<dyn AggregateFunction> {
        Box::new(Bits {
            op: BitOp::Xor,
            ..Bits::default()
        })
    })?;
    aggregate_fn(conn, "any_value", 1, || Box::<AnyValue>::default())?;
    aggregate_fn(conn, "overlite_string_agg_distinct", 2, || {
        Box::<StringAggDistinct>::default()
    })?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Stat {
    StddevSamp,
    StddevPop,
    VarSamp,
    VarPop,
    Corr,
    CovarPop,
    CovarSamp,
    RegrAvgX,
    RegrAvgY,
    RegrCount,
    RegrIntercept,
    RegrR2,
    RegrSlope,
    RegrSxx,
    RegrSxy,
    RegrSyy,
}

fn stat_factory(kind: Stat) -> fn() -> Box<dyn AggregateFunction> {
    macro_rules! f {
        ($k:ident) => {
            || -> Box<dyn AggregateFunction> {
                Box::new(Stats {
                    kind: Stat::$k,
                    rows: Vec::new(),
                    head: 0,
                })
            }
        };
    }
    match kind {
        Stat::StddevSamp => f!(StddevSamp),
        Stat::StddevPop => f!(StddevPop),
        Stat::VarSamp => f!(VarSamp),
        Stat::VarPop => f!(VarPop),
        Stat::Corr => f!(Corr),
        Stat::CovarPop => f!(CovarPop),
        Stat::CovarSamp => f!(CovarSamp),
        Stat::RegrAvgX => f!(RegrAvgX),
        Stat::RegrAvgY => f!(RegrAvgY),
        Stat::RegrCount => f!(RegrCount),
        Stat::RegrIntercept => f!(RegrIntercept),
        Stat::RegrR2 => f!(RegrR2),
        Stat::RegrSlope => f!(RegrSlope),
        Stat::RegrSxx => f!(RegrSxx),
        Stat::RegrSxy => f!(RegrSxy),
        Stat::RegrSyy => f!(RegrSyy),
    }
}

/// The statistics aggregates over the rows whose arguments are all non-NULL: (y, x)
/// for the two-argument ones, as Postgres orders them.
struct Stats {
    kind: Stat,
    rows: Vec<(f64, f64)>,
    /// How many leading rows the window dropped.
    head: usize,
}

impl AggregateFunction for Stats {
    fn step(&mut self, args: &[Value]) {
        if args.iter().any(Value::is_null) {
            return;
        }
        let y = float(&args[0]).unwrap_or(f64::NAN);
        let x = args.get(1).map_or(0.0, |v| float(v).unwrap_or(f64::NAN));
        self.rows.push((y, x));
    }

    fn window_inverse(&mut self, args: &[Value]) {
        if !args.iter().any(Value::is_null) && self.head < self.rows.len() {
            self.head += 1;
        }
    }

    fn window_value(&mut self) -> Value {
        let rows = &self.rows[self.head..];
        let n = rows.len() as f64;
        if matches!(self.kind, Stat::RegrCount) {
            return Value::Int(rows.len() as i64);
        }
        if rows.is_empty() {
            return Value::Null;
        }
        let (sy, sx) = rows.iter().fold((0.0, 0.0), |(a, b), (y, x)| (a + y, b + x));
        let (my, mx) = (sy / n, sx / n);
        let syy: f64 = rows.iter().map(|(y, _)| (y - my) * (y - my)).sum();
        let sxx: f64 = rows.iter().map(|(_, x)| (x - mx) * (x - mx)).sum();
        let sxy: f64 = rows.iter().map(|(y, x)| (y - my) * (x - mx)).sum();
        let v = match self.kind {
            Stat::VarPop => Some(syy / n),
            Stat::VarSamp => (n > 1.0).then(|| syy / (n - 1.0)),
            Stat::StddevPop => Some((syy / n).sqrt()),
            Stat::StddevSamp => (n > 1.0).then(|| (syy / (n - 1.0)).sqrt()),
            Stat::CovarPop => Some(sxy / n),
            Stat::CovarSamp => (n > 1.0).then(|| sxy / (n - 1.0)),
            Stat::Corr => (sxx != 0.0 && syy != 0.0).then(|| sxy / (sxx * syy).sqrt()),
            Stat::RegrAvgX => Some(mx),
            Stat::RegrAvgY => Some(my),
            Stat::RegrSxx => Some(sxx),
            Stat::RegrSyy => Some(syy),
            Stat::RegrSxy => Some(sxy),
            Stat::RegrSlope => (sxx != 0.0).then(|| sxy / sxx),
            Stat::RegrIntercept => (sxx != 0.0).then(|| my - sxy / sxx * mx),
            Stat::RegrR2 => {
                if sxx == 0.0 {
                    None
                } else if syy == 0.0 {
                    Some(1.0)
                } else {
                    Some(sxy * sxy / (sxx * syy))
                }
            }
            Stat::RegrCount => None,
        };
        v.map_or(Value::Null, Value::Float)
    }
}

#[derive(Default, Clone, Copy)]
enum BitOp {
    #[default]
    And,
    Or,
    Xor,
}

/// bit_and, bit_or and bit_xor over the non-NULL integers.
#[derive(Default)]
struct Bits {
    op: BitOp,
    vals: Vec<i64>,
    head: usize,
}

impl AggregateFunction for Bits {
    fn step(&mut self, args: &[Value]) {
        if args[0].is_null() {
            return;
        }
        if let Ok(v) = int(&args[0]) {
            self.vals.push(v);
        }
    }

    fn window_inverse(&mut self, args: &[Value]) {
        if !args[0].is_null() && self.head < self.vals.len() {
            self.head += 1;
        }
    }

    fn window_value(&mut self) -> Value {
        let vals = &self.vals[self.head..];
        let Some(first) = vals.first() else { return Value::Null };
        Value::Int(vals[1..].iter().fold(*first, |acc, v| match self.op {
            BitOp::And => acc & v,
            BitOp::Or => acc | v,
            BitOp::Xor => acc ^ v,
        }))
    }
}

/// any_value: a non-NULL value of the group, the first one met.
#[derive(Default)]
struct AnyValue {
    vals: Vec<Value>,
    head: usize,
}

impl AggregateFunction for AnyValue {
    fn step(&mut self, args: &[Value]) {
        if !args[0].is_null() {
            self.vals.push(args[0].clone());
        }
    }

    fn window_inverse(&mut self, args: &[Value]) {
        if !args[0].is_null() && self.head < self.vals.len() {
            self.head += 1;
        }
    }

    fn window_value(&mut self) -> Value {
        self.vals.get(self.head).cloned().unwrap_or(Value::Null)
    }
}

/// string_agg(DISTINCT x, sep): SQLite takes DISTINCT on an aggregate of one
/// argument only, so the distinct values are kept here, in the order they come,
/// which an ORDER BY in the call decides.
#[derive(Default)]
struct StringAggDistinct {
    vals: Vec<String>,
    sep: Option<String>,
}

impl AggregateFunction for StringAggDistinct {
    fn step(&mut self, args: &[Value]) {
        if args[0].is_null() {
            return;
        }
        let v = text(&args[0]);
        if self.sep.is_none() {
            self.sep = Some(if args[1].is_null() {
                String::new()
            } else {
                text(&args[1])
            });
        }
        if !self.vals.contains(&v) {
            self.vals.push(v);
        }
    }

    fn window_inverse(&mut self, _args: &[Value]) {}

    fn window_value(&mut self) -> Value {
        if self.vals.is_empty() {
            return Value::Null;
        }
        Value::Text(self.vals.join(self.sep.as_deref().unwrap_or("")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        assert_eq!(left("abc", 2), "ab");
        assert_eq!(left("abc", -1), "ab");
        assert_eq!(right("abc", 2), "bc");
        assert_eq!(right("abc", -2), "c");
        assert_eq!(right("abc", 0), "");
        assert_eq!(pad("a", 3, "xy", true), "xya");
        assert_eq!(pad("abcd", 2, "x", false), "ab");
        assert_eq!(initcap("hello wORLD-x1y"), "Hello World-X1y");
        assert_eq!(translate("abcb", "ab", "x"), "xc");
        assert_eq!(split_part("a,b,c", ",", -1).unwrap(), "c");
        assert_eq!(
            text_array(&[Some("a b".into()), None, Some("c".into())]),
            r#"{"a b",NULL,c}"#
        );
    }

    #[test]
    fn hashes() {
        assert_eq!(hex::encode(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex::encode(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(encode(b"abc", "base64").unwrap(), "YWJj");
        assert_eq!(decode("YWJj", "base64").unwrap(), b"abc");
        assert_eq!(encode(b"a\\\x00", "escape").unwrap(), "a\\\\\\000");
    }

    #[test]
    fn inet() {
        let t = |f: &str, s: &str| inet_fn(f, &[Value::Text(s.into())]).unwrap().to_string();
        assert_eq!(t("host", "192.168.1.5/24"), "192.168.1.5");
        assert_eq!(t("netmask", "192.168.1.5/24"), "255.255.255.0");
        assert_eq!(t("hostmask", "192.168.23.20/30"), "0.0.0.3");
        assert_eq!(t("network", "192.168.1.5/24"), "192.168.1.0/24");
        assert_eq!(t("broadcast", "192.168.1.5/24"), "192.168.1.255/24");
        assert_eq!(t("masklen", "::1"), "128");
        assert_eq!(t("abbrev", "10.1.0.0/16"), "10.1.0.0/16");
    }
}

unsafe extern "C" {
    /// The C library's gamma function, which Postgres calls too.
    safe fn tgamma(x: f64) -> f64;
    /// The C library's natural logarithm of the absolute value of the gamma function.
    #[link_name = "lgamma"]
    safe fn c_lgamma(x: f64) -> f64;
}

fn float_overflow() -> crate::error::Error {
    errf!(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

/// gamma(x) as Postgres 18's dgamma: NaN and +Infinity are themselves, and a pole, an
/// overflow or -Infinity is 22003, as is a result that underflows to 0.
fn gamma(x: f64) -> Result<f64> {
    if x.is_nan() || x == f64::INFINITY {
        return Ok(x);
    }
    if x == f64::NEG_INFINITY {
        return Err(float_overflow());
    }
    let r = tgamma(x);
    if r.is_infinite() || r.is_nan() {
        return Err(float_overflow());
    }
    if r == 0.0 {
        return Err(errf!(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow"
        ));
    }
    Ok(r)
}

/// lgamma(x) as Postgres 18's dlgamma: an infinite result of a finite argument (a pole
/// at zero and the negative integers, or an overflow) is 22003.
fn lgamma(x: f64) -> Result<f64> {
    let r = c_lgamma(x);
    if r.is_infinite() && x.is_finite() {
        return Err(float_overflow());
    }
    Ok(r)
}
