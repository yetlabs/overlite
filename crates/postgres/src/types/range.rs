//! The range types: a range column (`int4range`, ..., `tstzrange`) and an array of
//! them go out under their own OIDs, as range_out writes them in text (a tstzrange's
//! bounds in the session's TimeZone) and as range_send lays them out in binary. A
//! range is stored as range_out's text with a tstzrange's bounds in UTC (`+00`).

use crate::types::{OID_DATE, OID_INT4, OID_INT8, OID_NUMERIC, OID_TIMESTAMP, OID_TIMESTAMPTZ, encode_binary};
use overlite_core::{Oid, Value};

pub(crate) const OID_INT4RANGE: Oid = Oid::new(3904);
pub(crate) const OID_NUMRANGE: Oid = Oid::new(3906);
pub(crate) const OID_TSRANGE: Oid = Oid::new(3908);
pub(crate) const OID_TSTZRANGE: Oid = Oid::new(3910);
pub(crate) const OID_DATERANGE: Oid = Oid::new(3912);
pub(crate) const OID_INT8RANGE: Oid = Oid::new(3926);

/// Each range type: its name, its OID, its array's OID and its element's OID.
const RANGES: [(&str, Oid, Oid, Oid); 6] = [
    ("int4range", OID_INT4RANGE, Oid::new(3905), OID_INT4),
    ("int8range", OID_INT8RANGE, Oid::new(3927), OID_INT8),
    ("numrange", OID_NUMRANGE, Oid::new(3907), OID_NUMERIC),
    ("tsrange", OID_TSRANGE, Oid::new(3909), OID_TIMESTAMP),
    ("tstzrange", OID_TSTZRANGE, Oid::new(3911), OID_TIMESTAMPTZ),
    ("daterange", OID_DATERANGE, Oid::new(3913), OID_DATE),
];

/// The OID of a range type declared so (`int4range`, any case), None for another.
pub(crate) fn range_oid_for_decl(decl: &str) -> Option<Oid> {
    let d = decl.trim().trim_matches('"');
    RANGES.iter().find(|r| d.eq_ignore_ascii_case(r.0)).map(|r| r.1)
}

/// The array OID of a range type declared so, None for another.
pub(crate) fn range_array_oid_for_decl(decl: &str) -> Option<Oid> {
    let d = decl.trim().trim_matches('"');
    RANGES.iter().find(|r| d.eq_ignore_ascii_case(r.0)).map(|r| r.2)
}

pub(crate) fn is_range_oid(oid: Oid) -> bool {
    RANGES.iter().any(|r| r.1 == oid)
}

/// The range type of an array of ranges, None for another OID.
pub(crate) fn range_of_array(oid: Oid) -> Option<Oid> {
    RANGES.iter().find(|r| r.2 == oid).map(|r| r.1)
}

/// range_out's text of a stored range: a tstzrange's bounds in the session's zone.
pub(crate) fn range_text(oid: Oid, stored: &str) -> String {
    if oid == OID_TSTZRANGE {
        return overlite_engine::tstzrange_session_text(stored).unwrap_or_else(|| stored.to_string());
    }
    stored.to_string()
}

/// range_send of a stored range: its flags, then each finite bound's length and its
/// element's binary form. None for what is no range or a bound its element type does
/// not read.
pub(crate) fn range_binary(oid: Oid, v: &Value) -> Option<Vec<u8>> {
    const EMPTY: u8 = 0x01;
    const LB_INC: u8 = 0x02;
    const UB_INC: u8 = 0x04;
    const LB_INF: u8 = 0x08;
    const UB_INF: u8 = 0x10;
    let elem = RANGES.iter().find(|r| r.1 == oid)?.3;
    let Value::Text(text) = v else { return None };
    let overlite_engine::Range {
        empty,
        lo,
        hi,
        lo_inc,
        hi_inc,
    } = overlite_engine::range_parts(text)?;
    if empty {
        return Some(vec![EMPTY]);
    }
    let mut flags = 0;
    if lo_inc {
        flags |= LB_INC;
    }
    if hi_inc {
        flags |= UB_INC;
    }
    if lo.is_none() {
        flags |= LB_INF;
    }
    if hi.is_none() {
        flags |= UB_INF;
    }
    let mut out = vec![flags];
    for b in [lo, hi].into_iter().flatten() {
        // A tstzrange's bound is stored in UTC with its `+00`.
        let b = b.strip_suffix("+00").map_or(b.clone(), str::to_string);
        let bytes = encode_binary(elem, &Value::Text(b))?;
        out.extend_from_slice(&i32::try_from(bytes.len()).ok()?.to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    Some(out)
}
