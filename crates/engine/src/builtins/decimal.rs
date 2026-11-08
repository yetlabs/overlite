//! Exact numeric over SQLite. A `numeric`/`decimal`
//! column is stored as its exact decimal string in a TEXT cell (plain-SQLite
//! readable). SQLite's operators are float, so overlite routes numeric
//! arithmetic/aggregation through these big-rational functions, and
//! compares/orders via the DECIMAL collation.
//!
//! Arbitrary precision is hand-written here, since no dependency provides it:
//! `Nat`, `Rat`, and the 200-bit float round-trip of dec_round.

use crate::catalog::AggregateFunction;
use overlite_core::Value;
use std::cmp::Ordering;

// ---------------------------------------------------------------------------
// Natural numbers (big.nat): little-endian 64-bit limbs without leading zeros.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Nat(pub Vec<u64>);

impl Nat {
    fn zero() -> Nat {
        Nat(Vec::new())
    }

    fn from_u64(x: u64) -> Nat {
        if x == 0 { Nat(Vec::new()) } else { Nat(vec![x]) }
    }

    fn is_zero(&self) -> bool {
        self.0.is_empty()
    }

    fn norm(mut self) -> Nat {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
        self
    }

    fn bit_len(&self) -> i64 {
        match self.0.last() {
            None => 0,
            Some(top) => (self.0.len() as i64 - 1) * 64 + (64 - top.leading_zeros() as i64),
        }
    }

    fn cmp(&self, y: &Nat) -> Ordering {
        if self.0.len() != y.0.len() {
            return self.0.len().cmp(&y.0.len());
        }
        for i in (0..self.0.len()).rev() {
            if self.0[i] != y.0[i] {
                return self.0[i].cmp(&y.0[i]);
            }
        }
        Ordering::Equal
    }

    fn add(&self, y: &Nat) -> Nat {
        let (long, short) = if self.0.len() >= y.0.len() {
            (self, y)
        } else {
            (y, self)
        };
        let mut out = Vec::with_capacity(long.0.len() + 1);
        let mut carry = false;
        for i in 0..long.0.len() {
            let b = if i < short.0.len() { short.0[i] } else { 0 };
            let (s1, c1) = long.0[i].overflowing_add(b);
            let (s2, c2) = s1.overflowing_add(carry as u64);
            out.push(s2);
            carry = c1 || c2;
        }
        if carry {
            out.push(1);
        }
        Nat(out)
    }

    /// self - y, for self >= y.
    fn sub(&self, y: &Nat) -> Nat {
        let mut out = Vec::with_capacity(self.0.len());
        let mut borrow = false;
        for i in 0..self.0.len() {
            let b = if i < y.0.len() { y.0[i] } else { 0 };
            let (s1, c1) = self.0[i].overflowing_sub(b);
            let (s2, c2) = s1.overflowing_sub(borrow as u64);
            out.push(s2);
            borrow = c1 || c2;
        }
        Nat(out).norm()
    }

    fn mul(&self, y: &Nat) -> Nat {
        if self.is_zero() || y.is_zero() {
            return Nat::zero();
        }
        let mut out = vec![0u64; self.0.len() + y.0.len()];
        for (i, &xi) in self.0.iter().enumerate() {
            let mut carry = 0u128;
            for (j, &yj) in y.0.iter().enumerate() {
                let t = xi as u128 * yj as u128 + out[i + j] as u128 + carry;
                out[i + j] = t as u64;
                carry = t >> 64;
            }
            out[i + y.0.len()] = carry as u64;
        }
        Nat(out).norm()
    }

    /// self*m + a, in place.
    fn mul_add_word(&mut self, m: u64, a: u64) {
        let mut carry = a as u128;
        for limb in &mut self.0 {
            let t = *limb as u128 * m as u128 + carry;
            *limb = t as u64;
            carry = t >> 64;
        }
        if carry != 0 {
            self.0.push(carry as u64);
        }
    }

    /// Divides by a single limb; returns (quotient, remainder).
    fn div_word(&self, d: u64) -> (Nat, u64) {
        let mut q = vec![0u64; self.0.len()];
        let mut r = 0u128;
        for i in (0..self.0.len()).rev() {
            let cur = (r << 64) | self.0[i] as u128;
            q[i] = (cur / d as u128) as u64;
            r = cur % d as u128;
        }
        (Nat(q).norm(), r as u64)
    }

    /// Knuth algorithm D; returns (quotient, remainder).
    fn div_rem(&self, v: &Nat) -> (Nat, Nat) {
        if self.cmp(v) == Ordering::Less {
            return (Nat::zero(), self.clone());
        }
        if v.0.len() == 1 {
            let (q, r) = self.div_word(v.0[0]);
            return (q, Nat::from_u64(r));
        }
        let s = v.0.last().map_or(0, |w| w.leading_zeros()) as u64;
        let vn = v.shl(s).0;
        let mut un = self.shl(s).0;
        let n = vn.len();
        un.resize(self.0.len() + 1, 0);
        let m = un.len() - n - 1;
        let mut q = vec![0u64; m + 1];
        let b: u128 = 1 << 64;
        let v1 = vn[n - 1] as u128;
        let v2 = vn[n - 2] as u128;
        for j in (0..=m).rev() {
            let num = ((un[j + n] as u128) << 64) | un[j + n - 1] as u128;
            let mut qhat = num / v1;
            let mut rhat = num % v1;
            while qhat >= b || qhat * v2 > ((rhat << 64) | un[j + n - 2] as u128) {
                qhat -= 1;
                rhat += v1;
                if rhat >= b {
                    break;
                }
            }
            // Multiply and subtract.
            let mut borrow: i128 = 0;
            let mut carry: u128 = 0;
            for i in 0..n {
                let p = qhat * vn[i] as u128 + carry;
                carry = p >> 64;
                let t = un[i + j] as i128 - borrow - (p as u64) as i128;
                un[i + j] = t as u64;
                borrow = i128::from(t < 0);
            }
            let t = un[j + n] as i128 - borrow - carry as i128;
            un[j + n] = t as u64;
            if t < 0 {
                // Add back.
                qhat -= 1;
                let mut c = 0u128;
                for i in 0..n {
                    let t = un[i + j] as u128 + vn[i] as u128 + c;
                    un[i + j] = t as u64;
                    c = t >> 64;
                }
                un[j + n] = (un[j + n] as u128 + c) as u64;
            }
            q[j] = qhat as u64;
        }
        un.truncate(n);
        (Nat(q).norm(), Nat(un).norm().shr(s))
    }

    fn shl(&self, s: u64) -> Nat {
        if self.is_zero() {
            return Nat::zero();
        }
        let limbs = (s / 64) as usize;
        let bits = (s % 64) as u32;
        let mut out = vec![0u64; limbs];
        out.reserve(self.0.len() + 1);
        if bits == 0 {
            out.extend_from_slice(&self.0);
        } else {
            let mut carry = 0u64;
            for &l in &self.0 {
                out.push((l << bits) | carry);
                carry = l >> (64 - bits);
            }
            if carry != 0 {
                out.push(carry);
            }
        }
        Nat(out)
    }

    fn shr(&self, s: u64) -> Nat {
        let limbs = (s / 64) as usize;
        if limbs >= self.0.len() {
            return Nat::zero();
        }
        let bits = (s % 64) as u32;
        let src = &self.0[limbs..];
        let mut out = Vec::with_capacity(src.len());
        if bits == 0 {
            out.extend_from_slice(src);
        } else {
            for i in 0..src.len() {
                let hi = if i + 1 < src.len() {
                    src[i + 1] << (64 - bits)
                } else {
                    0
                };
                out.push((src[i] >> bits) | hi);
            }
        }
        Nat(out).norm()
    }

    /// Bit i of the number.
    fn bit(&self, i: u64) -> bool {
        let limb = (i / 64) as usize;
        limb < self.0.len() && (self.0[limb] >> (i % 64)) & 1 == 1
    }

    /// Whether any of the low n bits is set.
    fn low_bits_set(&self, n: u64) -> bool {
        let limbs = (n / 64) as usize;
        for i in 0..limbs.min(self.0.len()) {
            if self.0[i] != 0 {
                return true;
            }
        }
        limbs < self.0.len() && !n.is_multiple_of(64) && self.0[limbs] << (64 - n % 64) != 0
    }

    fn pow(base: u64, mut n: u64) -> Nat {
        let mut result = Nat::from_u64(1);
        let mut b = Nat::from_u64(base);
        while n > 0 {
            if n & 1 == 1 {
                result = result.mul(&b);
            }
            n >>= 1;
            if n > 0 {
                b = b.mul(&b);
            }
        }
        result
    }

    fn gcd(&self, y: &Nat) -> Nat {
        let (mut a, mut b) = (self.clone(), y.clone());
        while !b.is_zero() {
            if a.0.len() <= 1 && b.0.len() <= 1 {
                let (mut x, mut z) = (a.0.first().copied().unwrap_or(0), b.0[0]);
                while z != 0 {
                    (x, z) = (z, x % z);
                }
                return Nat::from_u64(x);
            }
            let r = a.div_rem(&b).1;
            a = b;
            b = r;
        }
        a
    }

    /// The number in decimal.
    fn to_decimal(&self) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        const CHUNK: u64 = 10_000_000_000_000_000_000; // 10^19
        let mut chunks = Vec::new();
        let mut cur = self.clone();
        while !cur.is_zero() {
            let (q, r) = cur.div_word(CHUNK);
            chunks.push(r);
            cur = q;
        }
        let mut s = chunks.pop().unwrap_or(0).to_string();
        for c in chunks.iter().rev() {
            s.push_str(&format!("{c:019}"));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Scanning over a byte reader.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ByteReader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl ByteReader<'_> {
    /// The next byte; None at the end.
    fn read(&mut self) -> Option<u8> {
        let c = self.b.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// UnreadByte of the byte just read.
    fn unread(&mut self) {
        self.pos -= 1;
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.b.len()
    }
}

fn scan_sign(r: &mut ByteReader<'_>) -> Option<bool> {
    let ch = r.read()?;
    match ch {
        b'-' => Some(true),
        b'+' => Some(false),
        _ => {
            r.unread();
            Some(false)
        }
    }
}

/// nat.scan with base 0: returns (value, base, count), None on error.
fn nat_scan(r: &mut ByteReader<'_>, mut frac_ok: bool) -> Option<(Nat, u64, i64)> {
    let mut prev = b'.';
    let mut inval_sep = false;
    let mut count: i64 = 0;

    // Actual base is 10 unless there's a base prefix.
    let mut ch = r.read();
    let mut b: u64 = 10;
    let mut prefix = 0u8;
    if ch == Some(b'0') {
        prev = b'0';
        count = 1;
        ch = r.read();
        if let Some(c) = ch {
            match c {
                b'b' | b'B' => (b, prefix) = (2, b'b'),
                b'o' | b'O' => (b, prefix) = (8, b'o'),
                b'x' | b'X' => (b, prefix) = (16, b'x'),
                _ => {
                    if !frac_ok {
                        (b, prefix) = (8, b'0');
                    }
                }
            }
            if prefix != 0 {
                count = 0; // prefix is not counted
                if prefix != b'0' {
                    ch = r.read();
                }
            }
        }
    }

    // Digits are collected in groups that fit a word.
    let (bn, n): (u64, u32) = match b {
        2 => (1 << 63, 63),
        8 => (1 << 63, 21),
        16 => (1 << 60, 15),
        _ => (10_000_000_000_000_000_000, 19),
    };
    let mut z = Nat::zero();
    let mut di = 0u64;
    let mut i = 0u32;
    let mut dp: i64 = -1;
    while let Some(c) = ch {
        if c == b'.' && frac_ok {
            frac_ok = false;
            if prev == b'_' {
                inval_sep = true;
            }
            prev = b'.';
            dp = count;
        } else if c == b'_' {
            if prev != b'0' {
                inval_sep = true;
            }
            prev = b'_';
        } else {
            let d1 = match c {
                b'0'..=b'9' => (c - b'0') as u64,
                b'a'..=b'z' => (c - b'a' + 10) as u64,
                b'A'..=b'Z' => (c - b'A' + 10) as u64,
                _ => u64::MAX,
            };
            if d1 >= b {
                r.unread(); // ch does not belong to number anymore
                break;
            }
            prev = b'0';
            count += 1;
            di = di * b + d1;
            i += 1;
            if i == n {
                z.mul_add_word(bn, di);
                di = 0;
                i = 0;
            }
        }
        ch = r.read();
    }

    let bad = inval_sep || prev == b'_';
    if count == 0 {
        if prefix == b'0' {
            // Only the octal prefix 0: decimal 0.
            return if bad { None } else { Some((Nat::zero(), 10, 1)) };
        }
        return None; // no digits found
    }
    if bad {
        return None;
    }
    if i > 0 {
        z.mul_add_word(b.pow(i), di);
    }
    let z = z.norm();
    if dp >= 0 {
        count = dp - count;
    }
    Some((z, b, count))
}

/// Scans an exponent: (exp, base), None on error.
fn scan_exponent(r: &mut ByteReader<'_>) -> Option<(i64, u32)> {
    let Some(ch) = r.read() else {
        return Some((0, 10));
    };
    let base = match ch {
        b'e' | b'E' => 10,
        b'p' | b'P' => 2,
        _ => {
            r.unread(); // ch does not belong to exponent anymore
            return Some((0, 10));
        }
    };

    let mut digits = String::new();
    let mut ch = r.read();
    if ch == Some(b'+') || ch == Some(b'-') {
        if ch == Some(b'-') {
            digits.push('-');
        }
        ch = r.read();
    }

    let mut prev = b'.';
    let mut inval_sep = false;
    let mut has_digits = false;
    while let Some(c) = ch {
        if c.is_ascii_digit() {
            digits.push(c as char);
            prev = b'0';
            has_digits = true;
        } else if c == b'_' {
            if prev != b'0' {
                inval_sep = true;
            }
            prev = b'_';
        } else {
            r.unread(); // ch does not belong to number anymore
            break;
        }
        ch = r.read();
    }

    if !has_digits {
        return None;
    }
    let exp = digits.parse::<i64>().ok()?;
    if inval_sep || prev == b'_' {
        return None;
    }
    Some((exp, base))
}

// ---------------------------------------------------------------------------
// Rationals: sign-magnitude numerator, denominator > 0, in lowest
// terms; zero is 0/1.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rat {
    neg: bool,
    a: Nat,
    b: Nat,
}

/// Signed sum of two sign-magnitude values.
fn signed_add(xn: bool, x: Nat, yn: bool, y: Nat) -> (bool, Nat) {
    if xn == yn {
        return (xn, x.add(&y));
    }
    match x.cmp(&y) {
        Ordering::Less => (yn, y.sub(&x)),
        _ => (xn, x.sub(&y)),
    }
}

impl Rat {
    fn new() -> Rat {
        Rat {
            neg: false,
            a: Nat::zero(),
            b: Nat::from_u64(1),
        }
    }

    fn from_int64(x: i64) -> Rat {
        Rat {
            neg: x < 0,
            a: Nat::from_u64(x.unsigned_abs()),
            b: Nat::from_u64(1),
        }
    }

    fn norm(mut self) -> Rat {
        if self.a.is_zero() {
            self.neg = false;
            self.b = Nat::from_u64(1);
            return self;
        }
        let f = self.a.gcd(&self.b);
        if f.0.len() != 1 || f.0[0] != 1 {
            self.a = self.a.div_rem(&f).0;
            self.b = self.b.div_rem(&f).0;
        }
        self
    }

    /// Parses a rational from text.
    fn set_string(s: &str) -> Option<Rat> {
        if s.is_empty() {
            return None;
        }
        let bytes = s.as_bytes();

        // Parse fraction a/b, if any.
        if let Some(sep) = bytes.iter().position(|&c| c == b'/') {
            let mut r = ByteReader {
                b: &bytes[..sep],
                pos: 0,
            };
            let neg = scan_sign(&mut r)?;
            let (a, _, _) = nat_scan(&mut r, false)?;
            if !r.at_eof() {
                return None;
            }
            let mut r = ByteReader {
                b: &bytes[sep + 1..],
                pos: 0,
            };
            let (b, _, _) = nat_scan(&mut r, false)?;
            // Entire string must have been consumed.
            if !r.at_eof() || b.is_zero() {
                return None;
            }
            return Some(Rat { neg, a, b }.norm());
        }

        // Parse floating-point number.
        let mut r = ByteReader { b: bytes, pos: 0 };
        let neg = scan_sign(&mut r)?;
        let (mut a, base, fcount) = nat_scan(&mut r, true)?;
        let (exp, ebase) = scan_exponent(&mut r)?;
        // There should be no unread characters left.
        if !r.at_eof() {
            return None;
        }
        // Special-case 0.
        if a.is_zero() {
            return Some(Rat::new());
        }

        // Radix-point contribution to the binary or decimal exponent.
        let (mut exp2, mut exp5) = (0i64, 0i64);
        if fcount < 0 {
            let d = fcount;
            match base {
                10 => {
                    exp5 = d;
                    exp2 = d;
                }
                2 => exp2 = d,
                8 => exp2 = d * 3,
                16 => exp2 = d * 4,
                _ => unreachable!("unexpected mantissa base"),
            }
        }

        // Take actual exponent into account, wrapping.
        if ebase == 10 {
            exp5 = exp5.wrapping_add(exp);
        }
        exp2 = exp2.wrapping_add(exp);

        // Apply exp5 contributions.
        let mut b = Nat::from_u64(1);
        if exp5 != 0 {
            let mut n = exp5;
            if n < 0 {
                n = n.wrapping_neg();
                if n < 0 {
                    return None;
                }
            }
            if n > 1_000_000 {
                return None; // avoid excessively large exponents
            }
            let pow5 = Nat::pow(5, n as u64);
            if exp5 > 0 {
                a = a.mul(&pow5);
            } else {
                b = pow5;
            }
        }

        // Apply exp2 contributions.
        if !(-10_000_000..=10_000_000).contains(&exp2) {
            return None; // avoid excessively large exponents
        }
        if exp2 > 0 {
            a = a.shl(exp2 as u64);
        } else if exp2 < 0 {
            b = b.shl((-exp2) as u64);
        }

        Some(
            Rat {
                neg: neg && !a.is_zero(),
                a,
                b,
            }
            .norm(),
        )
    }

    pub(crate) fn sign(&self) -> i32 {
        if self.a.is_zero() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    fn is_int(&self) -> bool {
        self.b.0.len() == 1 && self.b.0[0] == 1
    }

    pub(crate) fn cmp(&self, y: &Rat) -> i32 {
        let a = self.a.mul(&y.b);
        let b = y.a.mul(&self.b);
        // Compare as signed values; zero has no sign.
        let (xs, ys) = (self.sign(), y.sign());
        let ord = if xs != ys {
            xs.cmp(&ys)
        } else if xs < 0 {
            b.cmp(&a)
        } else {
            a.cmp(&b)
        };
        match ord {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    pub(crate) fn add(&self, y: &Rat) -> Rat {
        let a1 = self.a.mul(&y.b);
        let a2 = y.a.mul(&self.b);
        let (neg, a) = signed_add(self.neg, a1, y.neg, a2);
        Rat {
            neg,
            a,
            b: self.b.mul(&y.b),
        }
        .norm()
    }

    pub(crate) fn sub(&self, y: &Rat) -> Rat {
        let a1 = self.a.mul(&y.b);
        let a2 = y.a.mul(&self.b);
        let (neg, a) = signed_add(self.neg, a1, !y.neg, a2);
        Rat {
            neg,
            a,
            b: self.b.mul(&y.b),
        }
        .norm()
    }

    pub(crate) fn mul(&self, y: &Rat) -> Rat {
        Rat {
            neg: self.neg != y.neg,
            a: self.a.mul(&y.a),
            b: self.b.mul(&y.b),
        }
        .norm()
    }

    pub(crate) fn quo(&self, y: &Rat) -> Rat {
        assert!(!y.a.is_zero(), "division by zero");
        let a = self.a.mul(&y.b);
        let b = y.a.mul(&self.b);
        Rat {
            neg: self.neg != y.neg,
            a,
            b,
        }
        .norm()
    }

    /// The value in decimal, with prec fractional digits.
    fn float_string(&self, prec: usize) -> String {
        let mut buf = String::new();
        if self.is_int() {
            if self.neg {
                buf.push('-');
            }
            buf.push_str(&self.a.to_decimal());
            if prec > 0 {
                buf.push('.');
                buf.push_str(&"0".repeat(prec));
            }
            return buf;
        }
        let (mut q, r) = self.a.div_rem(&self.b);
        let p = if prec > 0 {
            Nat::pow(10, prec as u64)
        } else {
            Nat::from_u64(1)
        };
        let r = r.mul(&p);
        let (mut r, r2) = r.div_rem(&self.b);
        // See if we need to round up.
        let r2 = r2.add(&r2);
        if self.b.cmp(&r2) != Ordering::Greater {
            r = r.add(&Nat::from_u64(1));
            if r.cmp(&p) != Ordering::Less {
                q = q.add(&Nat::from_u64(1));
                r = r.sub(&p);
            }
        }
        if self.neg {
            buf.push('-');
        }
        buf.push_str(&q.to_decimal());
        if prec > 0 {
            buf.push('.');
            let rs = r.to_decimal();
            buf.push_str(&"0".repeat(prec.saturating_sub(rs.len())));
            buf.push_str(&rs);
        }
        buf
    }

    /// Rat.Float64: the nearest float64 (quotToFloat64).
    fn float64(&self) -> f64 {
        const MSIZE: i64 = 52;
        const MSIZE1: i64 = MSIZE + 1;
        const MSIZE2: i64 = MSIZE1 + 1;
        const EBIAS: i64 = 1023;
        const EMIN: i64 = 1 - EBIAS;

        let alen = self.a.bit_len();
        if alen == 0 {
            return 0.0;
        }
        let blen = self.b.bit_len();
        let mut exp = alen - blen;
        let shift = MSIZE2 - exp;
        let (q, r) = match shift.cmp(&0) {
            Ordering::Greater => self.a.shl(shift as u64).div_rem(&self.b),
            Ordering::Less => self.a.div_rem(&self.b.shl((-shift) as u64)),
            Ordering::Equal => self.a.div_rem(&self.b),
        };
        let mut mantissa = q.0.first().copied().unwrap_or(0);
        let mut have_rem = !r.is_zero();

        if mantissa >> MSIZE2 == 1 {
            if mantissa & 1 == 1 {
                have_rem = true;
            }
            mantissa >>= 1;
            exp += 1;
        }

        // Denormal case: lose `shift` bits of precision.
        if (EMIN - MSIZE..=EMIN).contains(&exp) {
            let shift = (EMIN - (exp - 1)) as u32;
            let lostbits = mantissa & ((1u64 << shift) - 1);
            have_rem = have_rem || lostbits != 0;
            mantissa >>= shift;
            exp = 2 - EBIAS;
        }
        // Round using round-half-to-even.
        if mantissa & 1 != 0 && (have_rem || mantissa & 2 != 0) {
            mantissa += 1;
            if mantissa >= 1 << MSIZE2 {
                // Complete rollover 11...1 => 100...0, so shift is safe.
                mantissa >>= 1;
                exp += 1;
            }
        }
        mantissa >>= 1; // discard rounding bit

        let f = ldexp(mantissa as f64, exp - MSIZE1);
        if self.neg { -f } else { f }
    }
}

impl Default for Rat {
    fn default() -> Self {
        Rat::new()
    }
}

/// f * 2^e for an exactly representable result (or an overflow to Inf).
fn ldexp(mut f: f64, mut e: i64) -> f64 {
    while e > 1000 {
        f *= f64::from_bits(((1000 + 1023) as u64) << 52);
        e -= 1000;
        if f.is_infinite() {
            return f;
        }
    }
    while e < -1000 {
        f *= f64::from_bits(((-1000 + 1023) as u64) << 52);
        e += 1000;
    }
    f * f64::from_bits(((e + 1023) as u64) << 52)
}

// ---------------------------------------------------------------------------
// Aggregates.
// ---------------------------------------------------------------------------

/// DecSum is an exact SUM aggregate over decimal text (dec_sum).
#[derive(Debug, Default)]
pub(crate) struct DecSum {
    acc: Option<Rat>,
    any: bool,
}

impl AggregateFunction for DecSum {
    fn step(&mut self, args: &[Value]) {
        if args.is_empty() || args[0].is_null() {
            return;
        }
        if let Some(r) = parse_dec(&args[0]) {
            let acc = self.acc.get_or_insert_with(Rat::new);
            *acc = acc.add(&r);
            self.any = true;
        }
    }

    fn window_inverse(&mut self, args: &[Value]) {
        if !args.is_empty()
            && !args[0].is_null()
            && let Some(r) = parse_dec(&args[0])
            && let Some(acc) = &mut self.acc
        {
            *acc = acc.sub(&r);
        }
    }

    fn window_value(&mut self) -> Value {
        self.final_value()
    }
}

impl DecSum {
    fn final_value(&self) -> Value {
        match &self.acc {
            Some(acc) if self.any => Value::Text(dec_string(acc)),
            _ => Value::Null,
        }
    }
}

/// DecAgg overrides the built-in sum/avg so they are exact over decimal-text
/// (numeric) columns, while preserving the native return type for int/real
/// columns (int64 / float64) so behavior elsewhere is unchanged.
#[derive(Debug, Default)]
pub(crate) struct DecAgg {
    pub(crate) acc: Option<Rat>,
    pub(crate) n: i64,
    pub(crate) avg: bool,
    pub(crate) saw_str: bool,
    pub(crate) saw_float: bool,
    /// The intervals summed: an interval column's text, which is no decimal.
    pub(crate) intervals: Vec<String>,
    /// The largest scale of a numeric summed, which a numeric SUM keeps.
    pub(crate) scale: i64,
}

impl AggregateFunction for DecAgg {
    fn step(&mut self, args: &[Value]) {
        if args.is_empty() || args[0].is_null() {
            return;
        }
        match &args[0] {
            Value::Text(_) | Value::Blob(_) => {
                self.saw_str = true;
                self.scale = self.scale.max(scale_of(&args[0]));
            }
            Value::Float(_) => self.saw_float = true,
            _ => {}
        }
        if let Some(r) = parse_dec(&args[0]) {
            let acc = self.acc.get_or_insert_with(Rat::new);
            *acc = acc.add(&r);
            self.n += 1;
        } else if let Value::Text(t) = &args[0]
            && crate::builtins::temporal::looks_like_interval(t)
        {
            self.intervals.push(t.clone());
        }
    }

    fn window_inverse(&mut self, args: &[Value]) {
        if !args.is_empty()
            && !args[0].is_null()
            && let Some(r) = parse_dec(&args[0])
            && let Some(acc) = &mut self.acc
        {
            *acc = acc.sub(&r);
            self.n -= 1;
        } else if let Some(Value::Text(t)) = args.first()
            && let Some(at) = self.intervals.iter().position(|i| i == t)
        {
            self.intervals.remove(at);
        }
    }

    fn window_value(&mut self) -> Value {
        self.value()
    }
}

impl DecAgg {
    fn value(&self) -> Value {
        if self.acc.is_none() && !self.intervals.is_empty() {
            return crate::builtins::temporal::interval_sum(&self.intervals, self.avg).map_or(Value::Null, Value::Text);
        }
        let acc = match &self.acc {
            Some(acc) if self.n != 0 => acc,
            _ => return Value::Null, // empty / all-NULL -> NULL (matches sum/avg)
        };
        if self.saw_str {
            // A numeric column -> exact decimal, at the scale Postgres gives a sum and an
            // average.
            if self.avg {
                let n = Rat::from_int64(self.n);
                let scale = div_scale(acc, self.scale, &n, 0);
                return Value::Text(dec_with_scale(&acc.quo(&n), scale));
            }
            return Value::Text(dec_with_scale(acc, self.scale));
        }
        let mut res = acc.clone();
        if self.avg {
            res = res.quo(&Rat::from_int64(self.n));
        }
        if self.avg || self.saw_float {
            // Real column -> float, as SQLite does.
            return Value::Float(res.float64());
        }
        if res.is_int() {
            // Sum of ints -> int.
            if let Some(i) = nat_int64(res.neg, &res.a) {
                return Value::Int(i);
            }
        }
        Value::Text(dec_string(&res))
    }
}

/// The value as an i64, when it fits.
fn nat_int64(neg: bool, a: &Nat) -> Option<i64> {
    match a.0.len() {
        0 => Some(0),
        1 => {
            let w = a.0[0];
            if i64::try_from(w).is_ok() {
                Some(if neg { -(w as i64) } else { w as i64 })
            } else if neg && w == 1 << 63 {
                Some(i64::MIN)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Conversions.
// ---------------------------------------------------------------------------

/// Parses a decimal value (text or numeric) into a Rat.
pub(crate) fn parse_dec(v: &Value) -> Option<Rat> {
    match v {
        Value::Null => None,
        Value::Int(x) => Some(Rat::from_int64(*x)),
        // A double by the shortest text that reads back as it, as Postgres casts one to
        // numeric: 0.1, not the binary fraction nearest it. A non-finite one is zero.
        Value::Float(x) if x.is_finite() => rat_from_string(&format!("{x}")),
        Value::Float(_) => Some(Rat::default()),
        Value::Text(x) => rat_from_string(x),
        Value::Blob(x) => rat_from_string(&String::from_utf8_lossy(x)),
        _ => None,
    }
}

pub(crate) fn rat_from_string(s: &str) -> Option<Rat> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Rat::set_string(s)
}

/// Renders a Rat as a plain decimal string, without an exponent and trimming to
/// a reasonable scale for non-terminating results (e.g. division).
pub(crate) fn dec_string(r: &Rat) -> String {
    if r.is_int() {
        return if r.neg {
            format!("-{}", r.a.to_decimal())
        } else {
            r.a.to_decimal()
        };
    }
    // A terminating decimal prints exactly; otherwise cap at 20 fractional digits.
    let mut prec = decimal_digits(r);
    if !(0..=20).contains(&prec) {
        prec = 20;
    }
    let s = r.float_string(prec as usize);
    if s.contains('.') {
        // Trim trailing zeros a terminating value gained.
        let s = s.trim_end_matches('0');
        return s.strip_suffix('.').unwrap_or(s).to_string();
    }
    s
}

/// The number of fractional digits needed to represent r exactly, or -1 if it
/// does not terminate in base 10 (denominator has a prime factor other than 2
/// or 5).
fn decimal_digits(r: &Rat) -> i64 {
    let den = r.b.clone();
    let (c2, c5) = (factor_count(&den, 2), factor_count(&den, 5));
    let mut rest = den;
    for p in [2u64, 5] {
        loop {
            let (q, m) = rest.div_word(p);
            if m != 0 {
                break;
            }
            rest = q;
        }
    }
    if rest.0.len() != 1 || rest.0[0] != 1 {
        return -1;
    }
    c2.max(c5)
}

fn factor_count(n: &Nat, p: u64) -> i64 {
    let mut m = n.clone();
    let mut c = 0;
    loop {
        let (q, r) = m.div_word(p);
        if r != 0 {
            return c;
        }
        m = q;
        c += 1;
    }
}

pub(crate) fn bin_dec(a: &Value, b: &Value, op: fn(&Rat, &Rat) -> Rat) -> Value {
    let x = parse_dec(a);
    let y = parse_dec(b);
    match (x, y) {
        (Some(x), Some(y)) => Value::Text(dec_string(&op(&x, &y))),
        _ => Value::Null,
    }
}

/// The digits after the point a numeric value is written with, its display scale.
pub(crate) fn scale_of(v: &Value) -> i64 {
    let text = match v {
        Value::Text(t) => t.trim().to_string(),
        Value::Float(f) if f.is_finite() => format!("{f}"),
        _ => return 0,
    };
    if text.contains(['e', 'E']) {
        return 0;
    }
    text.split_once('.').map_or(0, |(_, f)| f.len() as i64)
}

/// r rounded to scale digits after the point and written with exactly that many.
pub(crate) fn dec_with_scale(r: &Rat, scale: i64) -> String {
    let scale = scale.max(0);
    crate::builtins::casts::with_scale(&dec_string_exact(&round_half_away(r, scale)), scale)
}

/// A terminating decimal with every digit it has, where dec_string stops at 20 places;
/// one that does not terminate as dec_string writes it.
pub(crate) fn dec_string_exact(r: &Rat) -> String {
    let prec = decimal_digits(r);
    if prec <= 20 {
        return dec_string(r);
    }
    let s = r.float_string(prec as usize);
    let s = s.trim_end_matches('0');
    s.strip_suffix('.').unwrap_or(s).to_string()
}

/// r rounded to scale digits after the point, half away from zero as Postgres rounds
/// a numeric, in exact arithmetic whatever the number of digits.
pub(crate) fn round_half_away(r: &Rat, scale: i64) -> Rat {
    let scale = scale.max(0) as u64;
    let shift = Nat::pow(10, scale);
    let scaled = r.a.mul(&shift);
    let (mut q, rem) = scaled.div_rem(&r.b);
    // Half or more of the denominator left over rounds the magnitude up.
    if rem.shl(1).cmp(&r.b) != Ordering::Less {
        q = q.add(&Nat::from_u64(1));
    }
    Rat {
        neg: r.neg,
        a: q,
        b: shift,
    }
    .norm()
}

/// The weight and the first digit of a decimal in Postgres' base-10000 digits.
fn base_10000_lead(r: &Rat) -> (i64, i64) {
    let text = dec_string(r);
    let digits = text.trim_start_matches('-');
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let whole = whole.trim_start_matches('0');
    if !whole.is_empty() {
        let n = whole.len() as i64;
        let weight = (n - 1) / 4;
        let lead = (n - 4 * weight) as usize;
        return (weight, whole[..lead].parse().unwrap_or(0));
    }
    for (g, chunk) in frac.as_bytes().chunks(4).enumerate() {
        let mut group = String::from_utf8_lossy(chunk).into_owned();
        while group.len() < 4 {
            group.push('0');
        }
        let value: i64 = group.parse().unwrap_or(0);
        if value != 0 {
            return (-(g as i64) - 1, value);
        }
    }
    (0, 0)
}

/// The scale of a quotient, as Postgres' select_div_scale chooses it: at least 16
/// significant digits, and no fewer than either side's scale.
pub(crate) fn div_scale(x: &Rat, x_scale: i64, y: &Rat, y_scale: i64) -> i64 {
    let (w1, d1) = base_10000_lead(x);
    let (w2, d2) = base_10000_lead(y);
    let mut qweight = w1 - w2;
    if d1 <= d2 {
        qweight -= 1;
    }
    (16 - qweight * 4).max(x_scale).max(y_scale).clamp(0, 1000)
}

/// A numeric operator with the scale Postgres gives its result: the larger side's for
/// `+` and `-`, their sum for `*`, select_div_scale's for `/`.
pub(crate) fn dec_op(a: &Value, b: &Value, op: char) -> Value {
    let (Some(x), Some(y)) = (parse_dec(a), parse_dec(b)) else {
        return Value::Null;
    };
    let (sa, sb) = (scale_of(a), scale_of(b));
    Value::Text(match op {
        '+' => dec_with_scale(&x.add(&y), sa.max(sb)),
        '-' => dec_with_scale(&x.sub(&y), sa.max(sb)),
        '*' => dec_with_scale(&x.mul(&y), sa + sb),
        _ => dec_with_scale(&x.quo(&y), div_scale(&x, sa, &y, sb)),
    })
}

/// -1/0/1 comparing two decimal values (used by dec_cmp).
pub(crate) fn dec_cmp(a: &Value, b: &Value) -> i32 {
    dec_cmp_parsed(parse_dec(a), parse_dec(b), to_str(a).as_ref(), to_str(b).as_ref())
}

/// The DECIMAL collation: dec_cmp over two strings.
pub(crate) fn dec_cmp_str(a: &str, b: &str) -> i32 {
    dec_cmp_parsed(rat_from_string(a), rat_from_string(b), a, b)
}

fn dec_cmp_parsed(x: Option<Rat>, y: Option<Rat>, a: &str, b: &str) -> i32 {
    match (x, y) {
        (None, None) => match a.as_bytes().cmp(b.as_bytes()) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

fn to_str(v: &Value) -> std::borrow::Cow<'_, str> {
    match v {
        Value::Text(x) => std::borrow::Cow::Borrowed(x),
        Value::Blob(x) => String::from_utf8_lossy(x),
        _ => std::borrow::Cow::Borrowed(""),
    }
}

/// Rounds the magnitude m*2^e (plus the sticky information of what was already
/// lost) to `prec` mantissa bits, nearest-even.
fn float_round(m: Nat, e: i64, sticky: bool, prec: u64) -> (Nat, i64) {
    let bits = m.bit_len() as u64;
    if bits <= prec {
        return (m, e);
    }
    let cut = bits - prec;
    let rbit = m.bit(cut - 1);
    let sbit = sticky || m.low_bits_set(cut - 1);
    let mut kept = m.shr(cut);
    if rbit && (sbit || kept.bit(0)) {
        kept = kept.add(&Nat::from_u64(1));
    }
    (kept, e + cut as i64)
}

/// Rounds r to scale fractional digits (half-up). The midpoint arithmetic runs
/// in a 200-bit float, whose rounding changes the result near the limit, so it
/// is reproduced here.
pub(crate) fn dec_round(r: &Rat, scale: i64) -> Rat {
    const PREC: u64 = 200;
    // A negative scale counts as 10^0.
    let shift = if scale < 0 {
        Nat::from_u64(1)
    } else {
        Nat::pow(10, scale as u64)
    };
    let shift_rat = Rat {
        neg: false,
        a: shift,
        b: Nat::from_u64(1),
    };
    let scaled = r.mul(&shift_rat);

    // The scaled value as a 200-bit float.
    let (fm, fe) = if scaled.a.is_zero() {
        (Nat::zero(), 0)
    } else if scaled.is_int() {
        float_round(scaled.a.clone(), 0, false, PREC)
    } else {
        // At least PREC+2 quotient bits, the rest in the sticky bit.
        let s = PREC as i64 + 3 + scaled.b.bit_len() - scaled.a.bit_len();
        let (q, rem) = if s > 0 {
            scaled.a.shl(s as u64).div_rem(&scaled.b)
        } else {
            scaled.a.div_rem(&scaled.b.shl((-s) as u64))
        };
        float_round(q, -s, !rem.is_zero(), PREC)
    };

    // Add half, with the sign of the value: the magnitudes add up exactly,
    // then round.
    let (sum, sum_e) = if fm.is_zero() {
        (Nat::from_u64(1), -1)
    } else {
        let emin = fe.min(-1);
        let aligned = fm.shl((fe - emin) as u64);
        let half = Nat::from_u64(1).shl((-1 - emin) as u64);
        float_round(aligned.add(&half), emin, false, PREC)
    };

    // Truncate towards zero.
    let i = if sum_e >= 0 {
        sum.shl(sum_e as u64)
    } else {
        sum.shr((-sum_e) as u64)
    };
    let out = Rat {
        neg: scaled.neg,
        a: i,
        b: Nat::from_u64(1),
    }
    .norm();
    out.quo(&shift_rat)
}

fn bad_numeric(v: &Value) -> crate::error::Error {
    crate::error::Error::with_state(
        overlite_core::SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type numeric: \"{v}\""),
    )
}

/// round(x) and round(x, n) as Postgres: half away from zero, to n places, n below zero
/// rounding to tens, hundreds... An integer stays one, and anything else a double, which
/// SQLite compares with numbers: Django filters on `ROUND(x) > 1`. SQLite's round
/// rounded a negative n to whole units.
///
/// # Errors
///
/// 22P02 for text that is no number.
pub(crate) fn pg_round(a: &[Value]) -> crate::error::Result<Value> {
    Ok(match round_exact(a)? {
        Value::Text(t) => Value::Float(t.parse().unwrap_or(f64::NAN)),
        other => other,
    })
}

/// round as pg_round, a numeric's text keeping n places: what the select list sends.
///
/// # Errors
///
/// 22P02 for text that is no number.
pub(crate) fn round_exact(a: &[Value]) -> crate::error::Result<Value> {
    if a.is_empty() || a[0].is_null() || a.get(1).is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    let n = match a.get(1) {
        Some(Value::Int(n)) => *n,
        Some(other) => other.to_string().trim().parse().map_err(|_| bad_numeric(other))?,
        None => 0,
    };
    let r = parse_dec(&a[0]).ok_or_else(|| bad_numeric(&a[0]))?;
    let rounded = if n >= 0 {
        dec_round(&r, n)
    } else {
        let p = Rat::from_int64(10i64.pow(u32::try_from(-n).unwrap_or(18).min(18)));
        dec_round(&r.quo(&p), 0).mul(&p)
    };
    Ok(match &a[0] {
        Value::Int(_) => {
            nat_int64(rounded.neg, &rounded.a).map_or_else(|| Value::Text(dec_string(&rounded)), Value::Int)
        }
        Value::Float(_) => Value::Float(rounded.float64()),
        _ => Value::Text(crate::builtins::casts::with_scale(&dec_string(&rounded), n.max(0))),
    })
}

/// abs(x) of a numeric in the select list: its text without its sign, where SQLite's
/// abs gives a double.
///
/// # Errors
///
/// 22P02 for text that is no number.
pub(crate) fn abs_exact(v: &Value) -> crate::error::Result<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Int(i) => i
            .checked_abs()
            .map_or_else(|| Value::Float((*i as f64).abs()), Value::Int),
        Value::Float(f) => Value::Float(f.abs()),
        other => {
            let t = other.to_string();
            let t = t.trim();
            if rat_from_string(t).is_none() {
                return Err(bad_numeric(other));
            }
            Value::Text(t.strip_prefix('-').unwrap_or(t).to_string())
        }
    })
}
