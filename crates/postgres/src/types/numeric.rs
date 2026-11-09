//! Exact numeric: a `numeric`/`decimal[(p,s)]` column is stored as its exact
//! decimal string in a TEXT cell that carries the DECIMAL collation, so storage
//! is lossless and comparison/ordering are numeric (see engine/decimal). We
//! re-type the column to `DECIMALTEXT COLLATE DECIMAL`: DECIMALTEXT gives it TEXT
//! affinity and lets overlite advertise OID 1700; the collation makes
//! `<`/`>`/`ORDER BY` compare as numbers.

const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;
/// The dscale numeric_send gives an infinity.
const NUMERIC_INF_DSCALE: u16 = 32;

/// numeric_send of a decimal text: ndigits, weight, sign and dscale, then the digits
/// in base 10000, aligned on the decimal point, without the zero groups at either
/// end. None for a text that is not a number, or too large for those fields.
pub(crate) fn numeric_binary(text: &str) -> Option<Vec<u8>> {
    let s = text.trim();
    match s.to_ascii_lowercase().as_str() {
        "nan" => return Some(numeric_fields(0, NUMERIC_NAN, 0, &[])),
        "infinity" | "+infinity" | "inf" | "+inf" => {
            return Some(numeric_fields(0, NUMERIC_PINF, NUMERIC_INF_DSCALE, &[]));
        }
        "-infinity" | "-inf" => return Some(numeric_fields(0, NUMERIC_NINF, NUMERIC_INF_DSCALE, &[])),
        _ => {}
    }
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.len() + frac.len() == 0 || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let dscale = u16::try_from((frac.len() as i64).checked_sub(exp)?.max(0)).ok()?;

    // Where the decimal point falls in the digits; move it to a group boundary.
    let point = (int.len() as i64).checked_add(exp)?;
    let lead = (4 - point.rem_euclid(4)) % 4;
    let point = point + lead;
    let weight = point / 4 - 1;
    i16::try_from(weight).ok()?;
    let mut digits = vec![0u8; lead as usize];
    digits.extend(int.bytes().chain(frac.bytes()).map(|c| c - b'0'));
    let len = (digits.len() as i64).max(point);
    digits.resize((len + (4 - len.rem_euclid(4)) % 4) as usize, 0);

    let mut groups: Vec<u16> = digits
        .chunks(4)
        .map(|g| g.iter().fold(0, |n, d| n * 10 + *d as u16))
        .collect();
    while groups.last() == Some(&0) {
        groups.pop();
    }
    let zeros = groups.iter().take_while(|g| **g == 0).count();
    groups.drain(..zeros);
    if groups.is_empty() {
        return Some(numeric_fields(0, 0, dscale, &[]));
    }
    i16::try_from(groups.len()).ok()?;
    let sign = if negative { NUMERIC_NEG } else { 0 };
    Some(numeric_fields(
        i16::try_from(weight - zeros as i64).ok()?,
        sign,
        dscale,
        &groups,
    ))
}

fn numeric_fields(weight: i16, sign: u16, dscale: u16, groups: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
    out.extend_from_slice(&weight.to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    out
}
