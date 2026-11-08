//! Backs the Postgres date/time functions that don't map cleanly onto a SQLite
//! built-in: date_trunc, date_part, to_char, age and AT TIME ZONE. now()/extract
//! are handled by the dialect layer. It carries the time support they need:
//! parsing by layout, Location (TZif data, POSIX TZ extend strings, the embedded
//! zone database) and calendar arithmetic.

use crate::catalog::lower_chars;
use crate::error::Result;
use overlite_core::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

const ALPHA: i64 = i64::MIN;
const OMEGA: i64 = i64::MAX;
const SECONDS_PER_DAY: i64 = 86400;
const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_MINUTE: i64 = 60;

/// The zone database, embedded so a host without one still resolves zone names.
static ZONEINFO_ZIP: &[u8] = include_bytes!("zoneinfo.zip");

// ---------------------------------------------------------------------------
// Location.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Zone {
    name: String,
    offset: i64,
    is_dst: bool,
}

#[derive(Clone, Debug)]
struct ZoneTransition {
    when: i64,
    index: usize,
}

#[derive(Clone, Debug, Default)]
struct Location {
    name: String,
    zone: Vec<Zone>,
    tx: Vec<ZoneTransition>,
    extend: String,
}

/// What `Location.lookup` returns.
#[derive(Debug)]
struct ZoneLookup<'a> {
    name: &'a str,
    offset: i64,
    start: i64,
    end: i64,
}

static UTC: LazyLock<Arc<Location>> = LazyLock::new(|| {
    Arc::new(Location {
        name: "UTC".to_string(),
        ..Default::default()
    })
});

static LOCAL: LazyLock<Arc<Location>> = LazyLock::new(|| Arc::new(init_local()));

/// The ZONEINFO environment variable, read once.
static ZONEINFO_ENV: LazyLock<String> = LazyLock::new(|| std::env::var("ZONEINFO").unwrap_or_default());

const PLATFORM_ZONE_SOURCES: [&str; 4] = [
    "/usr/share/zoneinfo/",
    "/usr/share/lib/zoneinfo/",
    "/usr/lib/locale/TZ/",
    "/etc/zoneinfo",
];

/// A location with one fixed offset.
fn fixed_zone(name: &str, offset: i64) -> Arc<Location> {
    Arc::new(Location {
        name: name.to_string(),
        zone: vec![Zone {
            name: name.to_string(),
            offset,
            is_dst: false,
        }],
        tx: vec![ZoneTransition { when: ALPHA, index: 0 }],
        extend: String::new(),
    })
}

impl Location {
    fn lookup(&self, sec: i64) -> ZoneLookup<'_> {
        if self.zone.is_empty() {
            return ZoneLookup {
                name: "UTC",
                offset: 0,
                start: ALPHA,
                end: OMEGA,
            };
        }

        if self.tx.is_empty() || sec < self.tx[0].when {
            let zone = &self.zone[self.lookup_first_zone()];
            let end = if self.tx.is_empty() { OMEGA } else { self.tx[0].when };
            return ZoneLookup {
                name: &zone.name,
                offset: zone.offset,
                start: ALPHA,
                end,
            };
        }

        // Binary search for entry with largest time <= sec.
        let tx = &self.tx;
        let mut end = OMEGA;
        let (mut lo, mut hi) = (0, tx.len());
        while hi - lo > 1 {
            let m = usize::midpoint(lo, hi);
            let lim = tx[m].when;
            if sec < lim {
                end = lim;
                hi = m;
            } else {
                lo = m;
            }
        }
        let zone = &self.zone[tx[lo].index];
        let start = tx[lo].when;

        // At the end of the known zone transitions, try the extend string.
        if lo == tx.len() - 1
            && !self.extend.is_empty()
            && let Some(r) = tzset(&self.extend, start, sec)
        {
            return r;
        }

        ZoneLookup {
            name: &zone.name,
            offset: zone.offset,
            start,
            end,
        }
    }

    /// The zone to use for times before the first transition time, or when
    /// there are no transition times.
    fn lookup_first_zone(&self) -> usize {
        // Case 1.
        if !self.first_zone_used() {
            return 0;
        }

        // Case 2.
        if !self.tx.is_empty() && self.zone[self.tx[0].index].is_dst {
            let mut zi = self.tx[0].index as i64 - 1;
            while zi >= 0 {
                if !self.zone[zi as usize].is_dst {
                    return zi as usize;
                }
                zi -= 1;
            }
        }

        // Case 3.
        for (zi, z) in self.zone.iter().enumerate() {
            if !z.is_dst {
                return zi;
            }
        }

        // There is no non-daylight-time zone.
        0
    }

    fn first_zone_used(&self) -> bool {
        self.tx.iter().any(|tx| tx.index == 0)
    }
}

/// Zone information from a POSIX TZ string, as of sec.
fn tzset(s: &str, last_tx_sec: i64, sec: i64) -> Option<ZoneLookup<'_>> {
    let (mut std_name, s) = tzset_name(s)?;
    let (std_offset, s) = tzset_offset(s)?;

    // The numbers in the tzset string are added to local time to get UTC, but
    // our offsets are added to UTC to get local time.
    let mut std_offset = -std_offset;

    if s.is_empty() || s.as_bytes()[0] == b',' {
        // No daylight savings time.
        return Some(ZoneLookup {
            name: std_name,
            offset: std_offset,
            start: last_tx_sec,
            end: OMEGA,
        });
    }

    let (mut dst_name, mut s) = tzset_name(s)?;
    let mut dst_offset;
    if s.is_empty() || s.as_bytes()[0] == b',' {
        dst_offset = std_offset + SECONDS_PER_HOUR;
    } else {
        let (o, rest) = tzset_offset(s)?;
        dst_offset = -o;
        s = rest;
    }

    if s.is_empty() {
        // Default DST rules per tzcode.
        s = ",M3.2.0,M11.1.0";
    }
    // The TZ definition does not mention ';' here but tzcode accepts it.
    if s.as_bytes()[0] != b',' && s.as_bytes()[0] != b';' {
        return None;
    }
    let (start_rule, s) = tzset_rule(&s[1..])?;
    if s.is_empty() || s.as_bytes()[0] != b',' {
        return None;
    }
    let (end_rule, s) = tzset_rule(&s[1..])?;
    if !s.is_empty() {
        return None;
    }

    // Start of year in seconds since Unix epoch, and seconds since then to sec.
    let (year, _, _) = civil_from_days(sec.div_euclid(SECONDS_PER_DAY));
    let yday = sec.div_euclid(SECONDS_PER_DAY) - days_from_civil(year, 1, 1) + 1;
    let ysec = (yday - 1) * SECONDS_PER_DAY + sec % SECONDS_PER_DAY;
    let ystart = sec - ysec;

    let mut start_sec = tzrule_time(year, &start_rule, std_offset);
    let mut end_sec = tzrule_time(year, &end_rule, dst_offset);
    // A flipping of "DST" and "STD" while retaining the labels: this happens in
    // southern hemispheres.
    if end_sec < start_sec {
        std::mem::swap(&mut start_sec, &mut end_sec);
        std::mem::swap(&mut std_name, &mut dst_name);
        std::mem::swap(&mut std_offset, &mut dst_offset);
    }

    // The start and end values are accurate close to a daylight savings
    // transition, but are otherwise just the start and end of the year. That
    // suffices for the only caller that cares, which is Date.
    Some(if ysec < start_sec {
        ZoneLookup {
            name: std_name,
            offset: std_offset,
            start: ystart,
            end: start_sec + ystart,
        }
    } else if ysec >= end_sec {
        ZoneLookup {
            name: std_name,
            offset: std_offset,
            start: end_sec + ystart,
            end: ystart + 365 * SECONDS_PER_DAY,
        }
    } else {
        ZoneLookup {
            name: dst_name,
            offset: dst_offset,
            start: start_sec + ystart,
            end: end_sec + ystart,
        }
    })
}

fn tzset_name(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    if s.as_bytes()[0] == b'<' {
        for (i, r) in s.char_indices() {
            if r == '>' {
                return Some((&s[1..i], &s[i + 1..]));
            }
        }
        None
    } else {
        for (i, r) in s.char_indices() {
            if matches!(r, '0'..='9' | ',' | '-' | '+') {
                if i < 3 {
                    return None;
                }
                return Some((&s[..i], &s[i..]));
            }
        }
        if s.len() < 3 {
            return None;
        }
        Some((s, ""))
    }
}

fn tzset_offset(s: &str) -> Option<(i64, &str)> {
    if s.is_empty() {
        return None;
    }
    let mut s = s;
    let mut neg = false;
    if s.as_bytes()[0] == b'+' {
        s = &s[1..];
    } else if s.as_bytes()[0] == b'-' {
        s = &s[1..];
        neg = true;
    }

    // The tzdata code permits values up to 24 * 7 here, although POSIX does not.
    let (hours, rest) = tzset_num(s, 0, 24 * 7)?;
    s = rest;
    let mut off = hours * SECONDS_PER_HOUR;
    if s.is_empty() || s.as_bytes()[0] != b':' {
        return Some((if neg { -off } else { off }, s));
    }

    let (mins, rest) = tzset_num(&s[1..], 0, 59)?;
    s = rest;
    off += mins * SECONDS_PER_MINUTE;
    if s.is_empty() || s.as_bytes()[0] != b':' {
        return Some((if neg { -off } else { off }, s));
    }

    let (secs, rest) = tzset_num(&s[1..], 0, 59)?;
    s = rest;
    off += secs;
    Some((if neg { -off } else { off }, s))
}

#[derive(Debug, Clone, Copy)]
enum RuleKind {
    Julian,
    Doy,
    MonthWeekDay,
}

#[derive(Debug, Clone, Copy)]
struct TzRule {
    kind: RuleKind,
    day: i64,
    week: i64,
    mon: i64,
    time: i64,
}

fn tzset_rule(s: &str) -> Option<(TzRule, &str)> {
    if s.is_empty() {
        return None;
    }
    let mut r = TzRule {
        kind: RuleKind::Julian,
        day: 0,
        week: 0,
        mon: 0,
        time: 0,
    };
    let mut s = s;
    if s.as_bytes()[0] == b'J' {
        let (jday, rest) = tzset_num(&s[1..], 1, 365)?;
        s = rest;
        r.kind = RuleKind::Julian;
        r.day = jday;
    } else if s.as_bytes()[0] == b'M' {
        let (mon, rest) = tzset_num(&s[1..], 1, 12)?;
        if rest.is_empty() || rest.as_bytes()[0] != b'.' {
            return None;
        }
        let (week, rest) = tzset_num(&rest[1..], 1, 5)?;
        if rest.is_empty() || rest.as_bytes()[0] != b'.' {
            return None;
        }
        let (day, rest) = tzset_num(&rest[1..], 0, 6)?;
        s = rest;
        r.kind = RuleKind::MonthWeekDay;
        r.day = day;
        r.week = week;
        r.mon = mon;
    } else {
        let (day, rest) = tzset_num(s, 0, 365)?;
        s = rest;
        r.kind = RuleKind::Doy;
        r.day = day;
    }

    if s.is_empty() || s.as_bytes()[0] != b'/' {
        r.time = 2 * SECONDS_PER_HOUR; // 2am is the default
        return Some((r, s));
    }

    let (offset, s) = tzset_offset(&s[1..])?;
    r.time = offset;
    Some((r, s))
}

fn tzset_num(s: &str, min: i64, max: i64) -> Option<(i64, &str)> {
    if s.is_empty() {
        return None;
    }
    let mut num = 0;
    for (i, r) in s.char_indices() {
        if !r.is_ascii_digit() {
            if i == 0 || num < min {
                return None;
            }
            return Some((num, &s[i..]));
        }
        num = num * 10 + (r as i64 - '0' as i64);
        if num > max {
            return None;
        }
    }
    if num < min {
        return None;
    }
    Some((num, ""))
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// The days in a month of a year.
fn days_in(m: i64, year: i64) -> i64 {
    match m {
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days before the first of month m, in a non-leap year.
fn days_before(m: i64) -> i64 {
    const T: [i64; 13] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365];
    T[(m - 1) as usize]
}

/// The number of seconds since the start of the year that the rule takes effect.
fn tzrule_time(year: i64, r: &TzRule, off: i64) -> i64 {
    let s = match r.kind {
        RuleKind::Julian => {
            let mut s = (r.day - 1) * SECONDS_PER_DAY;
            if is_leap(year) && r.day >= 60 {
                s += SECONDS_PER_DAY;
            }
            s
        }
        RuleKind::Doy => r.day * SECONDS_PER_DAY,
        RuleKind::MonthWeekDay => {
            // Zeller's Congruence.
            let m1 = (r.mon + 9) % 12 + 1;
            let mut yy0 = year;
            if r.mon <= 2 {
                yy0 -= 1;
            }
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            // dow is the day-of-week of the first day of r.mon; get the
            // day-of-month of the first "dow" day.
            let mut d = r.day - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..r.week {
                if d + 7 >= days_in(r.mon, year) {
                    break;
                }
                d += 7;
            }
            d += days_before(r.mon);
            if is_leap(year) && r.mon > 2 {
                d += 1;
            }
            d * SECONDS_PER_DAY
        }
    };
    s + r.time - off
}

/// Parses TZif data.
fn load_location_from_tzdata(name: &str, data: &[u8]) -> Option<Location> {
    fn take<'a>(d: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        if d.len() < n {
            return None;
        }
        let (h, t) = d.split_at(n);
        *d = t;
        Some(h)
    }
    fn big4(d: &mut &[u8]) -> Option<u32> {
        take(d, 4).map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
    }

    let mut d = data;
    if take(&mut d, 4)? != b"TZif" {
        return None;
    }
    let p = take(&mut d, 16)?;
    let version = match p[0] {
        0 => 1,
        b'2' => 2,
        b'3' => 3,
        _ => return None,
    };

    // Counts: UTC/local indicators, standard/wall indicators, leap seconds,
    // transition times, local time zones, abbreviation characters.
    const N_UTC_LOCAL: usize = 0;
    const N_STD_WALL: usize = 1;
    const N_LEAP: usize = 2;
    const N_TIME: usize = 3;
    const N_ZONE: usize = 4;
    const N_CHAR: usize = 5;
    let mut n = [0usize; 6];
    for slot in &mut n {
        *slot = big4(&mut d)? as usize;
    }

    // Version 2+ writes the data in a 32-bit format, then again in a 64-bit one.
    let mut is64 = false;
    if version > 1 {
        let skip = n[N_TIME] * 4
            + n[N_TIME]
            + n[N_ZONE] * 6
            + n[N_CHAR]
            + n[N_LEAP] * 8
            + n[N_STD_WALL]
            + n[N_UTC_LOCAL]
            + 4
            + 16;
        take(&mut d, skip)?;
        is64 = true;
        for slot in &mut n {
            *slot = big4(&mut d)? as usize;
        }
    }

    let size = if is64 { 8 } else { 4 };
    let txtimes = take(&mut d, n[N_TIME] * size)?;
    let txzones = take(&mut d, n[N_TIME])?;
    let zonedata = take(&mut d, n[N_ZONE] * 6)?;
    let abbrev = take(&mut d, n[N_CHAR])?;
    take(&mut d, n[N_LEAP] * (size + 4))?;
    take(&mut d, n[N_STD_WALL])?;
    take(&mut d, n[N_UTC_LOCAL])?;

    let mut extend = String::new();
    if d.len() > 2 && d[0] == b'\n' && d[d.len() - 1] == b'\n' {
        extend = String::from_utf8_lossy(&d[1..d.len() - 1]).into_owned();
    }

    let nzone = n[N_ZONE];
    if nzone == 0 {
        return None;
    }
    let mut zones = Vec::with_capacity(nzone);
    for z in zonedata.as_chunks::<6>().0 {
        let offset = i32::from_be_bytes([z[0], z[1], z[2], z[3]]) as i64;
        let is_dst = z[4] != 0;
        let b = z[5] as usize;
        if b >= abbrev.len() {
            return None;
        }
        let tail = &abbrev[b..];
        let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
        zones.push(Zone {
            name: String::from_utf8_lossy(&tail[..end]).into_owned(),
            offset,
            is_dst,
        });
    }

    let mut tx = Vec::with_capacity(n[N_TIME]);
    for i in 0..n[N_TIME] {
        let when = if is64 {
            let p = &txtimes[i * 8..i * 8 + 8];
            i64::from_be_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]])
        } else {
            let p = &txtimes[i * 4..i * 4 + 4];
            i32::from_be_bytes([p[0], p[1], p[2], p[3]]) as i64
        };
        let index = txzones[i] as usize;
        if index >= zones.len() {
            return None;
        }
        tx.push(ZoneTransition { when, index });
    }

    if tx.is_empty() {
        // Fake transition to cover all time, for fixed locations like "Etc/GMT0".
        tx.push(ZoneTransition { when: ALPHA, index: 0 });
    }

    Some(Location {
        name: name.to_string(),
        zone: zones,
        tx,
        extend,
    })
}

fn get4(b: &[u8]) -> usize {
    if b.len() < 4 {
        return 0;
    }
    b[0] as usize | (b[1] as usize) << 8 | (b[2] as usize) << 16 | (b[3] as usize) << 24
}

fn get2(b: &[u8]) -> usize {
    if b.len() < 2 {
        return 0;
    }
    b[0] as usize | (b[1] as usize) << 8
}

/// The named file in an uncompressed zip.
fn tzinfo_from_zip<'a>(z: &'a [u8], name: &str) -> Option<&'a [u8]> {
    const ZECHEADER: usize = 0x06054b50;
    const ZCHEADER: usize = 0x02014b50;
    const ZTAILSIZE: usize = 22;
    const ZHEADERSIZE: usize = 30;
    const ZHEADER: usize = 0x04034b50;

    let tail = z.get(z.len().checked_sub(ZTAILSIZE)?..)?;
    if get4(tail) != ZECHEADER {
        return None;
    }
    let n = get2(&tail[10..]);
    let size = get4(&tail[12..]);
    let off = get4(&tail[16..]);

    let mut buf = z.get(off..off + size)?;
    for _ in 0..n {
        // Each central directory entry is 46 bytes plus its variable parts.
        if get4(buf) != ZCHEADER {
            break;
        }
        let meth = get2(&buf[10..]);
        let size = get4(&buf[24..]);
        let namelen = get2(&buf[28..]);
        let xlen = get2(&buf[30..]);
        let fclen = get2(&buf[32..]);
        let off = get4(&buf[42..]);
        let zname = &buf[46..46 + namelen];
        buf = &buf[46 + namelen + xlen + fclen..];
        if zname != name.as_bytes() {
            continue;
        }
        if meth != 0 {
            return None;
        }
        // Local file header, then the data.
        let h = z.get(off..off + ZHEADERSIZE + namelen)?;
        if get4(h) != ZHEADER
            || get2(&h[8..]) != meth
            || get2(&h[26..]) != namelen
            || &h[30..30 + namelen] != name.as_bytes()
        {
            return None;
        }
        let xlen = get2(&h[28..]);
        let start = off + ZHEADERSIZE + namelen + xlen;
        return z.get(start..start + size);
    }
    None
}

fn load_tzinfo_from_dir_or_zip(dir: &str, name: &str) -> Option<Vec<u8>> {
    #[allow(clippy::case_sensitive_file_extension_comparisons)] // a zone archive is named exactly so
    if dir.len() > 4 && dir.ends_with(".zip") {
        let z = std::fs::read(dir).ok()?;
        return tzinfo_from_zip(&z, name).map(<[u8]>::to_vec);
    }
    let path = if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    };
    std::fs::read(path).ok()
}

/// The sources in order, then the embedded database.
fn load_location_from(name: &str, sources: &[&str]) -> Option<Location> {
    for source in sources {
        if let Some(data) = load_tzinfo_from_dir_or_zip(source, name)
            && let Some(z) = load_location_from_tzdata(name, &data)
        {
            return Some(z);
        }
    }
    let data = tzinfo_from_zip(ZONEINFO_ZIP, name)?;
    load_location_from_tzdata(name, data)
}

/// The zone names of the embedded database, which a name in another case finds.
static ZONE_NAMES: LazyLock<Vec<String>> = LazyLock::new(|| zip_names(ZONEINFO_ZIP));

/// The names of the files in an uncompressed zip.
fn zip_names(z: &[u8]) -> Vec<String> {
    const ZTAILSIZE: usize = 22;
    let mut out = Vec::new();
    let Some(tail) = z.get(z.len().saturating_sub(ZTAILSIZE)..) else {
        return out;
    };
    if tail.len() < ZTAILSIZE || get4(tail) != 0x06054b50 {
        return out;
    }
    let (n, size, off) = (get2(&tail[10..]), get4(&tail[12..]), get4(&tail[16..]));
    let Some(mut buf) = z.get(off..off + size) else {
        return out;
    };
    for _ in 0..n {
        if buf.len() < 46 || get4(buf) != 0x02014b50 {
            break;
        }
        let (namelen, xlen, fclen) = (get2(&buf[28..]), get2(&buf[30..]), get2(&buf[32..]));
        let Some(name) = buf.get(46..46 + namelen) else {
            break;
        };
        out.push(String::from_utf8_lossy(name).into_owned());
        let Some(rest) = buf.get(46 + namelen + xlen + fclen..) else {
            break;
        };
        buf = rest;
    }
    out
}

/// A zone written as an offset from UTC, `+10:00`, `-03`, `+0530`, read as a POSIX
/// zone is, west positive: Postgres reads AT TIME ZONE '+10:00' as ten hours behind
/// UTC, and Django writes an east offset as '-10:00' for it.
fn offset_location(name: &str) -> Option<Arc<Location>> {
    let sign = match name.as_bytes().first()? {
        b'+' => -1,
        b'-' => 1,
        _ => return None,
    };
    let body = &name[1..];
    let parts: Vec<&str> = if body.contains(':') {
        body.split(':').collect()
    } else if body.len() > 2 {
        vec![body.get(..2)?, body.get(2..)?]
    } else {
        vec![body]
    };
    if parts.len() > 3
        || parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 2 || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let field = |i: usize| parts.get(i).map_or(Some(0), |p| p.parse::<i64>().ok());
    let (h, m, sec) = (field(0)?, field(1)?, field(2)?);
    if h > 15 || m > 59 || sec > 59 {
        return None;
    }
    Some(fixed_zone(name, sign * (h * 3600 + m * 60 + sec)))
}

/// A location by name: a zone of the database in any case, as Postgres reads zone
/// names, or an offset.
fn load_location(name: &str) -> Option<Arc<Location>> {
    load_location_exact(name).or_else(|| {
        if let Some(loc) = offset_location(name.trim()) {
            return Some(loc);
        }
        let canon = ZONE_NAMES.iter().find(|z| z.eq_ignore_ascii_case(name) && *z != name)?;
        load_location_exact(canon)
    })
}

fn load_location_exact(name: &str) -> Option<Arc<Location>> {
    if name.is_empty() || name == "UTC" {
        return Some(UTC.clone());
    }
    if name == "Local" {
        return Some(LOCAL.clone());
    }
    if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    if !ZONEINFO_ENV.is_empty()
        && let Some(data) = load_tzinfo_from_dir_or_zip(&ZONEINFO_ENV, name)
        && let Some(z) = load_location_from_tzdata(name, &data)
    {
        return Some(Arc::new(z));
    }
    load_location_from(name, &PLATFORM_ZONE_SOURCES).map(Arc::new)
}

/// The local zone: $TZ, else /etc/localtime, else UTC.
fn init_local() -> Location {
    let tz = std::env::var("TZ");
    match tz.as_deref() {
        Err(_) => {
            if let Some(mut z) = load_location_from("localtime", &["/etc"]) {
                z.name = "Local".to_string();
                return z;
            }
        }
        Ok(tz) if !tz.is_empty() => {
            let tz = tz.strip_prefix(':').unwrap_or(tz);
            if tz.starts_with('/') {
                if let Some(mut z) = load_location_from(tz, &[""]) {
                    z.name = if tz == "/etc/localtime" {
                        "Local".to_string()
                    } else {
                        tz.to_string()
                    };
                    return z;
                }
            } else if !tz.is_empty()
                && tz != "UTC"
                && let Some(z) = load_location_from(tz, &PLATFORM_ZONE_SOURCES)
            {
                return z;
            }
        }
        _ => {}
    }
    Location {
        name: "UTC".to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Time.
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date; m in 1..=12.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month, day) of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// An instant with a location.
#[derive(Clone, Debug)]
struct TimeT {
    sec: i64,
    nsec: i64,
    loc: Arc<Location>,
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// The instant of a civil date and clock in loc: month and day are normalized.
fn civil_time(date: (i64, i64, i64), clock: (i64, i64, i64, i64), loc: &Arc<Location>) -> TimeT {
    let (year, month, day) = date;
    let (hour, min, sec, nsec) = clock;
    let m0 = month - 1;
    let year = year + m0.div_euclid(12);
    let m0 = m0.rem_euclid(12);
    let days = days_from_civil(year, m0 + 1, 1) + day - 1;
    let mut unix = days * SECONDS_PER_DAY + hour * SECONDS_PER_HOUR + min * SECONDS_PER_MINUTE + sec;

    // Look for zone offset for expected time, so we can adjust to UTC. The
    // lookup expects UTC, so first pass unix in the hope that it will not be
    // too close to a zone transition, and then adjust if it is.
    let z = loc.lookup(unix);
    let mut offset = z.offset;
    if offset != 0 {
        let utc = unix - offset;
        // If utc is valid for the time zone we found, then we have the right
        // offset. If not, get the correct offset by looking up utc.
        if utc < z.start || utc >= z.end {
            offset = loc.lookup(utc).offset;
        }
        unix -= offset;
    }
    TimeT {
        sec: unix,
        nsec,
        loc: loc.clone(),
    }
}

impl TimeT {
    fn now() -> TimeT {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        TimeT {
            sec: d.as_secs() as i64,
            nsec: d.subsec_nanos() as i64,
            loc: LOCAL.clone(),
        }
    }

    /// Seconds of the wall clock, as if the zone were UTC.
    fn abs(&self) -> i64 {
        self.sec + self.loc.lookup(self.sec).offset
    }

    fn days(&self) -> i64 {
        self.abs().div_euclid(SECONDS_PER_DAY)
    }

    fn date(&self) -> (i64, i64, i64) {
        civil_from_days(self.days())
    }

    fn year(&self) -> i64 {
        self.date().0
    }

    fn month(&self) -> i64 {
        self.date().1
    }

    fn day(&self) -> i64 {
        self.date().2
    }

    fn hour(&self) -> i64 {
        self.abs().rem_euclid(SECONDS_PER_DAY) / SECONDS_PER_HOUR
    }

    fn minute(&self) -> i64 {
        self.abs().rem_euclid(SECONDS_PER_HOUR) / SECONDS_PER_MINUTE
    }

    fn second(&self) -> i64 {
        self.abs().rem_euclid(SECONDS_PER_MINUTE)
    }

    fn nanosecond(&self) -> i64 {
        self.nsec
    }

    /// 0 = Sunday.
    fn weekday(&self) -> i64 {
        (self.days() + 4).rem_euclid(7)
    }

    fn year_day(&self) -> i64 {
        let (y, _, _) = self.date();
        self.days() - days_from_civil(y, 1, 1) + 1
    }

    fn iso_week(&self) -> (i64, i64) {
        // Find the Thursday of the calendar week.
        let mut d = 4 - self.weekday();
        if d == 4 {
            d = -3; // Sunday
        }
        let days = self.days() + d;
        let (year, _, _) = civil_from_days(days);
        let yday0 = days - days_from_civil(year, 1, 1);
        (year, yday0 / 7 + 1)
    }

    fn month_name(&self) -> &'static str {
        MONTH_NAMES[(self.month() - 1) as usize]
    }

    fn weekday_name(&self) -> &'static str {
        WEEKDAY_NAMES[self.weekday() as usize]
    }

    fn before(&self, u: &TimeT) -> bool {
        (self.sec, self.nsec) < (u.sec, u.nsec)
    }

    /// Drops the fraction of a second.
    fn truncate_second(&self) -> TimeT {
        TimeT {
            sec: self.sec,
            nsec: 0,
            loc: self.loc.clone(),
        }
    }

    /// The same instant in loc.
    fn in_loc(&self, loc: &Arc<Location>) -> TimeT {
        TimeT {
            sec: self.sec,
            nsec: self.nsec,
            loc: loc.clone(),
        }
    }

    /// `YYYY-MM-DD HH:MM:SS`.
    fn format_stamp(&self) -> String {
        let mut b = String::with_capacity(19);
        let (y, m, d) = self.date();
        append_int(&mut b, y, 4);
        b.push('-');
        append_int(&mut b, m, 2);
        b.push('-');
        append_int(&mut b, d, 2);
        b.push(' ');
        append_int(&mut b, self.hour(), 2);
        b.push(':');
        append_int(&mut b, self.minute(), 2);
        b.push(':');
        append_int(&mut b, self.second(), 2);
        b
    }

    /// The zone's abbreviation.
    fn format_zone(&self) -> String {
        let z = self.loc.lookup(self.sec);
        if !z.name.is_empty() {
            return z.name.to_string();
        }
        // No time zone known for this time: the -0700 format.
        let mut zone = z.offset / 60;
        let mut b = String::new();
        if zone < 0 {
            b.push('-');
            zone = -zone;
        } else {
            b.push('+');
        }
        append_int(&mut b, zone / 60, 2);
        append_int(&mut b, zone % 60, 2);
        b
    }
}

/// Appends an integer zero-padded to width, the sign in front.
fn append_int(b: &mut String, x: i64, width: usize) {
    if x < 0 {
        b.push('-');
    }
    b.push_str(&format!("{:0width$}", x.unsigned_abs()));
}

// ---------------------------------------------------------------------------
// Parsing by the layouts this file uses.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Std {
    End,
    LongYear,
    ZeroMonth,
    ZeroDay,
    Hour,
    ZeroMinute,
    ZeroSecond,
    FracSecond9,
    NumColonTz,
    Iso8601ColonTz,
}

/// A layout split into a literal prefix, then a field.
type Layout = &'static [(&'static str, Std)];

const DATE: [(&str, Std); 3] = [("", Std::LongYear), ("-", Std::ZeroMonth), ("-", Std::ZeroDay)];

macro_rules! layout {
    ($sep:literal $(, $rest:expr)*) => {
        &[
            DATE[0], DATE[1], DATE[2],
            ($sep, Std::Hour), (":", Std::ZeroMinute), (":", Std::ZeroSecond),
            $($rest,)*
            ("", Std::End),
        ]
    };
}

const L_SPACE_FRAC_TZ: Layout = layout!(" ", ("", Std::FracSecond9), ("", Std::NumColonTz));
const L_SPACE_FRAC: Layout = layout!(" ", ("", Std::FracSecond9));
const L_SPACE: Layout = layout!(" ");
const L_T_FRAC_ZTZ: Layout = layout!("T", ("", Std::FracSecond9), ("", Std::Iso8601ColonTz));
const L_T_ZTZ: Layout = layout!("T", ("", Std::Iso8601ColonTz));
const L_T: Layout = layout!("T");
const L_SPACE_TZ: Layout = layout!(" ", ("", Std::NumColonTz));
const L_DATE: Layout = &[DATE[0], DATE[1], DATE[2], ("", Std::End)];
const L_CLOCK: Layout = &[
    ("", Std::Hour),
    (":", Std::ZeroMinute),
    (":", Std::ZeroSecond),
    ("", Std::End),
];

/// time.skip: matches the literal prefix; a space in it takes any run of spaces.
fn time_skip<'a>(mut value: &'a [u8], prefix: &str) -> Option<&'a [u8]> {
    fn cutspace(mut s: &[u8]) -> &[u8] {
        while let [b' ', rest @ ..] = s {
            s = rest;
        }
        s
    }
    let mut prefix = prefix.as_bytes();
    while !prefix.is_empty() {
        if prefix[0] == b' ' {
            if !value.is_empty() && value[0] != b' ' {
                return None;
            }
            prefix = cutspace(prefix);
            value = cutspace(value);
            continue;
        }
        if value.is_empty() || value[0] != prefix[0] {
            return None;
        }
        prefix = &prefix[1..];
        value = &value[1..];
    }
    Some(value)
}

fn is_digit(s: &[u8], i: usize) -> bool {
    s.len() > i && s[i].is_ascii_digit()
}

/// time.getnum: the leading one- or two-digit number.
fn getnum(s: &[u8], fixed: bool) -> Option<(i64, &[u8])> {
    if !is_digit(s, 0) {
        return None;
    }
    if !is_digit(s, 1) {
        if fixed {
            return None;
        }
        return Some(((s[0] - b'0') as i64, &s[1..]));
    }
    Some((((s[0] - b'0') as i64) * 10 + (s[1] - b'0') as i64, &s[2..]))
}

fn comma_or_period(b: u8) -> bool {
    b == b'.' || b == b','
}

/// time.atoi over digits only (a leading sign cannot reach it here).
fn time_atoi(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut x = 0i64;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        x = x * 10 + (c - b'0') as i64;
    }
    Some(x)
}

fn parse_nanoseconds(value: &[u8], nbytes: usize) -> Option<i64> {
    if !comma_or_period(value[0]) {
        return None;
    }
    let nbytes = nbytes.min(10);
    let mut ns = time_atoi(&value[1..nbytes])?;
    // We need nanoseconds, which means scaling by the number of missing digits
    // in the format, maximum length 10.
    for _ in 0..10 - nbytes {
        ns *= 10;
    }
    Some(ns)
}

/// Parses value by layout: `local` is what a numeric offset is matched against.
fn parse_layout(layout: Layout, value: &str, default_loc: &Arc<Location>, local: &Arc<Location>) -> Option<TimeT> {
    let mut value = value.as_bytes();
    let (mut year, mut month, mut day, mut hour, mut min, mut sec, mut nsec) = (0, -1, -1, 0, 0, 0, 0);
    let mut z: Option<Arc<Location>> = None;
    let mut zone_offset: i64 = -1;

    for (idx, &(prefix, std)) in layout.iter().enumerate() {
        value = time_skip(value, prefix)?;
        match std {
            Std::End => {
                if !value.is_empty() {
                    return None; // extra text
                }
                break;
            }
            Std::LongYear => {
                if value.len() < 4 || !is_digit(value, 0) {
                    return None;
                }
                year = time_atoi(&value[..4])?;
                value = &value[4..];
            }
            Std::ZeroMonth => {
                (month, value) = getnum(value, true)?;
                if month <= 0 || 12 < month {
                    return None;
                }
            }
            Std::ZeroDay => {
                // Any one- or two-digit day; validated once all is parsed.
                (day, value) = getnum(value, true)?;
            }
            Std::Hour => {
                (hour, value) = getnum(value, false)?;
                if !(0..24).contains(&hour) {
                    return None;
                }
            }
            Std::ZeroMinute => {
                (min, value) = getnum(value, true)?;
                if !(0..60).contains(&min) {
                    return None;
                }
            }
            Std::ZeroSecond => {
                (sec, value) = getnum(value, true)?;
                if !(0..60).contains(&sec) {
                    return None;
                }
                // Special case: a fractional second but no fractional second in
                // the format.
                if value.len() >= 2
                    && comma_or_period(value[0])
                    && is_digit(value, 1)
                    && layout[idx + 1].1 != Std::FracSecond9
                {
                    let mut n = 2;
                    while n < value.len() && is_digit(value, n) {
                        n += 1;
                    }
                    nsec = parse_nanoseconds(value, n)?;
                    value = &value[n..];
                }
            }
            Std::FracSecond9 => {
                if value.len() < 2 || !comma_or_period(value[0]) || !value[1].is_ascii_digit() {
                    continue; // fractional second omitted
                }
                // Take any number of digits, even more than asked for.
                let mut i = 0;
                while i + 1 < value.len() && value[i + 1].is_ascii_digit() {
                    i += 1;
                }
                nsec = parse_nanoseconds(value, 1 + i)?;
                value = &value[1 + i..];
            }
            Std::NumColonTz | Std::Iso8601ColonTz => {
                if std == Std::Iso8601ColonTz && !value.is_empty() && value[0] == b'Z' {
                    value = &value[1..];
                    z = Some(UTC.clone());
                    continue;
                }
                if value.len() < 6 || value[3] != b':' {
                    return None;
                }
                let (sign, h, m) = (value[0], &value[1..3], &value[4..6]);
                value = &value[6..];
                let (hr, _) = getnum(h, true)?;
                let (mm, _) = getnum(m, true)?;
                if hr > 24 || mm > 60 {
                    return None;
                }
                zone_offset = (hr * 60 + mm) * 60;
                match sign {
                    b'+' => {}
                    b'-' => zone_offset = -zone_offset,
                    _ => return None,
                }
            }
        }
    }

    if month < 0 {
        month = 1;
    }
    if day < 0 {
        day = 1;
    }
    // Validate the day of the month.
    if day < 1 || day > days_in(month, year) {
        return None;
    }

    if let Some(z) = z {
        return Some(civil_time((year, month, day), (hour, min, sec, nsec), &z));
    }

    if zone_offset != -1 {
        let mut t = civil_time((year, month, day), (hour, min, sec, nsec), &UTC);
        t.sec -= zone_offset;
        // Look for local zone with expected offset.
        if local.lookup(t.sec).offset == zone_offset {
            t.loc = local.clone();
            return Some(t);
        }
        // Otherwise create fake zone with unknown name.
        t.loc = fixed_zone("", zone_offset);
        return Some(t);
    }

    Some(civil_time((year, month, day), (hour, min, sec, nsec), default_loc))
}

// ---------------------------------------------------------------------------
// The SQL functions.
// ---------------------------------------------------------------------------

/// Converts a stored (UTC) instant to the wall-clock time in the given zone: the
/// timestamptz -> timestamp direction of `ts AT TIME ZONE 'zone'`.
pub(crate) fn at_time_zone(ts: &str, zone: &str) -> Option<String> {
    let loc = load_location(zone)?;
    let tm = parse_utc_stamp(ts)?;
    // Microseconds at most: truncated, trailing zeros trimmed.
    let t = tm.in_loc(&loc);
    let mut out = t.format_stamp();
    let frac = format!("{:09}", t.nsec);
    let frac = frac[..6].trim_end_matches('0');
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

/// Reads a wall clock as the time in the given zone and renders the instant as
/// timestamptz does: the timestamp -> timestamptz direction of `ts AT TIME ZONE
/// 'zone'`.
pub(crate) fn from_time_zone(ts: &str, zone: &str) -> Option<String> {
    let loc = load_location(zone)?;
    let replaced = ts.replacen('T', " ", 1);
    let s = replaced.trim();
    let tm = [L_SPACE_FRAC, L_SPACE, L_DATE]
        .into_iter()
        .find_map(|layout| parse_layout(layout, s, &loc, &UTC))?;
    let t = tm.in_loc(&UTC);
    let mut out = t.format_stamp();
    let frac = format!("{:09}", t.nsec);
    let frac = frac[..6].trim_end_matches('0');
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out.push_str("+00");
    Some(out)
}

/// Parses a bare (UTC) timestamp text in the forms overlite stores.
fn parse_utc_stamp(s: &str) -> Option<TimeT> {
    let mut replaced = s.trim().replacen('T', " ", 1);
    // The short offset a timestamptz renders; the layouts read "+00:00".
    if matches!(replaced.as_bytes().iter().rev().nth(2), Some(b'+' | b'-')) && replaced.contains(':') {
        replaced.push_str(":00");
    }
    let s = replaced.as_str();
    for layout in [L_SPACE_FRAC_TZ, L_SPACE_TZ, L_SPACE_FRAC, L_SPACE, L_DATE] {
        if let Some(tm) = parse_layout(layout, s, &UTC, &UTC) {
            return Some(tm);
        }
    }
    None
}

const TIME_LAYOUTS: [Layout; 8] = [
    L_SPACE_FRAC_TZ,
    L_SPACE_FRAC,
    L_SPACE,
    L_T_FRAC_ZTZ,
    L_T_ZTZ,
    L_T,
    L_DATE,
    L_CLOCK,
];

fn parse_time(v: &Value) -> Option<TimeT> {
    if v.is_null() {
        return None;
    }
    let text = v.to_string();
    let s = text.trim();
    for l in TIME_LAYOUTS {
        if let Some(t) = parse_layout(l, s, &UTC, &LOCAL) {
            return Some(t);
        }
    }
    None
}

/// Postgres date_trunc(unit, ts): zeroes out everything below `unit`.
fn date_trunc(unit: &str, t: &TimeT) -> String {
    let (y, m, d) = t.date();
    match lower_chars(unit.trim()).as_str() {
        "year" => civil_time((y, 1, 1), (0, 0, 0, 0), &t.loc).format_stamp(),
        "quarter" => {
            let q = (m - 1) / 3 * 3 + 1;
            civil_time((y, q, 1), (0, 0, 0, 0), &t.loc).format_stamp()
        }
        "month" => civil_time((y, m, 1), (0, 0, 0, 0), &t.loc).format_stamp(),
        "week" => {
            // ISO week starts on Monday.
            let wd = (t.weekday() + 6) % 7;
            let dd = civil_time((y, m, d), (0, 0, 0, 0), &t.loc);
            let (yy, mm, ddd) = dd.date();
            civil_time(
                (yy, mm, ddd - wd),
                (dd.hour(), dd.minute(), dd.second(), dd.nanosecond()),
                &t.loc,
            )
            .format_stamp()
        }
        "day" => civil_time((y, m, d), (0, 0, 0, 0), &t.loc).format_stamp(),
        "hour" => civil_time((y, m, d), (t.hour(), 0, 0, 0), &t.loc).format_stamp(),
        "minute" => civil_time((y, m, d), (t.hour(), t.minute(), 0, 0), &t.loc).format_stamp(),
        // second (and unknown)
        _ => t.truncate_second().format_stamp(),
    }
}

/// Postgres to_char(ts, fmt) via a token scanner over the format string: it
/// matches the longest known token at each position, honors an "FM" prefix
/// (suppress zero-padding / name-padding), and passes "double-quoted" text
/// through literally. Computed fields are produced directly.
fn to_char(t: &TimeT, format: &str) -> String {
    let f = format.as_bytes();
    let mut b: Vec<u8> = Vec::with_capacity(f.len() + 8);
    let mut i = 0;
    while i < f.len() {
        if f[i] == b'"' {
            // Quoted literal text.
            let mut j = i + 1;
            while j < f.len() && f[j] != b'"' {
                b.push(f[j]);
                j += 1;
            }
            i = j + 1;
            continue;
        }
        let mut fm = false;
        let mut rest = &f[i..];
        if rest.len() >= 2 && (rest[0] == b'F' || rest[0] == b'f') && (rest[1] == b'M' || rest[1] == b'm') {
            fm = true;
            rest = &rest[2..];
        }
        if let Some((val, n)) = match_to_char_token(t, rest, fm) {
            b.extend_from_slice(val.as_bytes());
            i += n;
            if fm {
                i += 2;
            }
            continue;
        }
        if fm {
            // "FM" not followed by a token: drop the modifier.
            i += 2;
            continue;
        }
        b.push(f[i]);
        i += 1;
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// One to_char format token and its producer.
#[derive(Debug)]
struct TcToken {
    tok: &'static str,
    f: fn(&TimeT, bool) -> String,
}

/// Ordered longest-first so e.g. "HH24" wins over "HH". Numeric tokens are
/// conventionally upper-case; the name tokens carry their own case.
static TC_TOKENS: &[TcToken] = &[
    TcToken {
        tok: "IYYY",
        f: |t, fm| tc_num(fm, 4, t.iso_week().0),
    },
    TcToken {
        tok: "YYYY",
        f: |t, fm| tc_num(fm, 4, t.year()),
    },
    TcToken {
        tok: "HH24",
        f: |t, fm| tc_num(fm, 2, t.hour()),
    },
    TcToken {
        tok: "HH12",
        f: |t, fm| tc_num(fm, 2, tc12(t)),
    },
    TcToken {
        tok: "MONTH",
        f: |t, fm| tc_name(fm, &t.month_name().to_uppercase()),
    },
    TcToken {
        tok: "Month",
        f: |t, fm| tc_name(fm, t.month_name()),
    },
    TcToken {
        tok: "month",
        f: |t, fm| tc_name(fm, &t.month_name().to_lowercase()),
    },
    TcToken {
        tok: "DDD",
        f: |t, fm| tc_num(fm, 3, t.year_day()),
    },
    TcToken {
        tok: "DAY",
        f: |t, fm| tc_name(fm, &t.weekday_name().to_uppercase()),
    },
    TcToken {
        tok: "Day",
        f: |t, fm| tc_name(fm, t.weekday_name()),
    },
    TcToken {
        tok: "day",
        f: |t, fm| tc_name(fm, &t.weekday_name().to_lowercase()),
    },
    TcToken {
        tok: "MON",
        f: |t, _| t.month_name()[..3].to_uppercase(),
    },
    TcToken {
        tok: "Mon",
        f: |t, _| t.month_name()[..3].to_string(),
    },
    TcToken {
        tok: "mon",
        f: |t, _| t.month_name()[..3].to_lowercase(),
    },
    TcToken {
        tok: "YYY",
        f: |t, _| tc_last(t.year(), 3),
    },
    TcToken {
        tok: "DY",
        f: |t, _| t.weekday_name()[..3].to_uppercase(),
    },
    TcToken {
        tok: "Dy",
        f: |t, _| t.weekday_name()[..3].to_string(),
    },
    TcToken {
        tok: "dy",
        f: |t, _| t.weekday_name()[..3].to_lowercase(),
    },
    TcToken {
        tok: "YY",
        f: |t, _| tc_last(t.year(), 2),
    },
    TcToken {
        tok: "MM",
        f: |t, fm| tc_num(fm, 2, t.month()),
    },
    TcToken {
        tok: "MI",
        f: |t, fm| tc_num(fm, 2, t.minute()),
    },
    TcToken {
        tok: "SS",
        f: |t, fm| tc_num(fm, 2, t.second()),
    },
    TcToken {
        tok: "MS",
        f: |t, fm| tc_num(fm, 3, t.nanosecond() / 1_000_000),
    },
    TcToken {
        tok: "US",
        f: |t, fm| tc_num(fm, 6, t.nanosecond() / 1_000),
    },
    TcToken {
        tok: "DD",
        f: |t, fm| tc_num(fm, 2, t.day()),
    },
    TcToken {
        tok: "HH",
        f: |t, fm| tc_num(fm, 2, tc12(t)),
    },
    TcToken {
        tok: "WW",
        f: |t, fm| tc_num(fm, 2, (t.year_day() - 1) / 7 + 1),
    },
    TcToken {
        tok: "IW",
        f: |t, fm| tc_num(fm, 2, t.iso_week().1),
    },
    TcToken {
        tok: "CC",
        f: |t, fm| tc_num(fm, 2, (t.year() + 99) / 100),
    },
    TcToken {
        tok: "ID",
        f: |t, _| tc_iso_dow(t).to_string(),
    },
    TcToken {
        tok: "AM",
        f: |t, _| tc_meridiem(t, true),
    },
    TcToken {
        tok: "PM",
        f: |t, _| tc_meridiem(t, true),
    },
    TcToken {
        tok: "am",
        f: |t, _| tc_meridiem(t, false),
    },
    TcToken {
        tok: "pm",
        f: |t, _| tc_meridiem(t, false),
    },
    TcToken {
        tok: "TZ",
        f: |t, _| t.format_zone(),
    },
    TcToken {
        tok: "Q",
        f: |t, _| ((t.month() - 1) / 3 + 1).to_string(),
    },
    TcToken {
        tok: "J",
        f: |t, _| julian_day(t).to_string(),
    },
    TcToken {
        tok: "D",
        f: |t, _| (t.weekday() + 1).to_string(),
    },
    TcToken {
        tok: "Y",
        f: |t, _| tc_last(t.year(), 1),
    },
];

/// The tokens whose letter case is significant (month/day names and meridiem).
/// Numeric tokens match case-insensitively (YYYY == yyyy).
static TC_NAME_CASE: LazyLock<HashMap<&'static str, bool>> = LazyLock::new(|| {
    HashMap::from([
        ("MONTH", true),
        ("Month", true),
        ("month", true),
        ("DAY", true),
        ("Day", true),
        ("day", true),
        ("MON", true),
        ("Mon", true),
        ("mon", true),
        ("DY", true),
        ("Dy", true),
        ("dy", true),
        ("AM", true),
        ("PM", true),
        ("am", true),
        ("pm", true),
    ])
});

fn match_to_char_token(t: &TimeT, s: &[u8], fm: bool) -> Option<(String, usize)> {
    for e in TC_TOKENS {
        let n = e.tok.len();
        if s.len() < n {
            continue;
        }
        let seg = &s[..n];
        if TC_NAME_CASE.get(e.tok).copied().unwrap_or(false) {
            if seg == e.tok.as_bytes() {
                return Some(((e.f)(t, fm), n));
            }
            continue;
        }
        if seg.eq_ignore_ascii_case(e.tok.as_bytes()) {
            return Some(((e.f)(t, fm), n));
        }
    }
    None
}

/// Zero-pads v to width, unless FM is set.
fn tc_num(fm: bool, width: usize, v: i64) -> String {
    if fm {
        return v.to_string();
    }
    format!("{v:0width$}")
}

/// The last n digits of a (zero-padded) year.
fn tc_last(year: i64, n: usize) -> String {
    let s = format!("{year:04}");
    if s.len() > n {
        return s[s.len() - n..].to_string();
    }
    s
}

/// Pads month/day names to 9 chars (Postgres default), unless FM.
fn tc_name(fm: bool, name: &str) -> String {
    if fm || name.len() >= 9 {
        return name.to_string();
    }
    format!("{}{}", name, " ".repeat(9 - name.len()))
}

fn tc12(t: &TimeT) -> i64 {
    let h = t.hour() % 12;
    if h == 0 {
        return 12;
    }
    h
}

fn tc_meridiem(t: &TimeT, upper: bool) -> String {
    let s = if t.hour() >= 12 { "pm" } else { "am" };
    if upper {
        return s.to_uppercase();
    }
    s.to_string()
}

/// The ISO day of week (Monday=1 .. Sunday=7).
fn tc_iso_dow(t: &TimeT) -> i64 {
    let d = t.weekday();
    if d != 0 {
        return d;
    }
    7
}

/// The Julian Day Number for t's date.
fn julian_day(t: &TimeT) -> i64 {
    let (y, m, d) = t.date();
    let a = (14 - m) / 12;
    let yy = y + 4800 - a;
    let mm = m + 12 * a - 3;
    d + (153 * mm + 2) / 5 + 365 * yy + yy / 4 - yy / 100 + yy / 400 - 32045
}

// Scalar function adapters.

pub(crate) fn date_trunc_fn(args: &[Value]) -> Result<Value> {
    if args.len() != 2 || args[0].is_null() || args[1].is_null() {
        return Ok(Value::Null);
    }
    if let Value::Text(text) = &args[1]
        && crate::builtins::temporal::looks_like_interval(text)
    {
        return crate::builtins::temporal::interval_trunc(&args[0].to_string(), text);
    }
    let Some(t) = parse_time(&args[1]) else {
        return Ok(Value::Null);
    };
    Ok(Value::Text(date_trunc(&args[0].to_string(), &t)))
}

pub(crate) fn to_char_fn(args: &[Value]) -> Result<Value> {
    if args.len() != 2 || args[0].is_null() || args[1].is_null() {
        return Ok(Value::Null);
    }
    let Some(t) = parse_time(&args[0]) else {
        return Ok(Value::Text(args[0].to_string()));
    };
    Ok(Value::Text(to_char(&t, &args[1].to_string())))
}

/// age(ts) = now() - ts and age(a, b) = a - b, rendered as a Postgres calendar
/// interval ("1 year 2 mons 5 days").
pub(crate) fn age_fn(args: &[Value]) -> Result<Value> {
    match args.len() {
        1 => {
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            let Some(b) = parse_time(&args[0]) else {
                return Ok(Value::Null);
            };
            Ok(Value::Text(age_interval(&TimeT::now(), &b)))
        }
        2 => {
            if args[0].is_null() || args[1].is_null() {
                return Ok(Value::Null);
            }
            let (Some(a), Some(b)) = (parse_time(&args[0]), parse_time(&args[1])) else {
                return Ok(Value::Null);
            };
            Ok(Value::Text(age_interval(&a, &b)))
        }
        _ => Ok(Value::Null),
    }
}

/// The calendar interval a - b as Postgres formats it, breaking the difference
/// into years, months, days, and a time component.
fn age_interval(a: &TimeT, b: &TimeT) -> String {
    let neg = a.before(b);
    let (a, b) = if neg { (b, a) } else { (a, b) };
    let mut sec = a.second() - b.second();
    let mut min = a.minute() - b.minute();
    let mut hour = a.hour() - b.hour();
    let mut day = a.day() - b.day();
    let mut month = a.month() - b.month();
    let mut year = a.year() - b.year();

    if sec < 0 {
        sec += 60;
        min -= 1;
    }
    if min < 0 {
        min += 60;
        hour -= 1;
    }
    if hour < 0 {
        hour += 24;
        day -= 1;
    }
    if day < 0 {
        // Borrow the number of days in the month preceding a.
        day += days_in_month(a.year(), a.month() - 1);
        month -= 1;
    }
    if month < 0 {
        month += 12;
        year -= 1;
    }

    let mut parts: Vec<String> = Vec::new();
    let add = |parts: &mut Vec<String>, n: i64, unit: &str| {
        if n != 0 {
            let s = if n != 1 && n != -1 {
                format!("{unit}s")
            } else {
                unit.to_string()
            };
            parts.push(format!("{n} {s}"));
        }
    };
    let sign = if neg { -1 } else { 1 };
    add(&mut parts, sign * year, "year");
    // Postgres abbreviates months as "mon"/"mons".
    if month != 0 {
        let u = if month == 1 { "mon" } else { "mons" };
        parts.push(format!("{} {}", sign * month, u));
    }
    add(&mut parts, sign * day, "day");
    if hour != 0 || min != 0 || sec != 0 {
        parts.push(format!("{:02}:{:02}:{:02}", sign * hour, min, sec));
    }
    if parts.is_empty() {
        return "00:00:00".to_string();
    }
    parts.join(" ")
}

/// The number of days in month m of year y (m may be 0 or 13, wrapping to the
/// adjacent year).
fn days_in_month(y: i64, m: i64) -> i64 {
    civil_time((y, m + 1, 0), (0, 0, 0, 0), &UTC).day()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn to_char_tokens() {
        // 2021-03-09 is a Tuesday; 14:05:07.123456; day-of-year 68; ISO week 10.
        let ts = civil_time((2021, 3, 9), (14, 5, 7, 123456000), &UTC);

        for (format, want) in [
            ("YYYY-MM-DD", "2021-03-09"),
            ("YYYY-MM-DD HH24:MI:SS", "2021-03-09 14:05:07"),
            ("HH12:MI AM", "02:05 PM"),
            ("hh12:mi am", "02:05 pm"),
            ("DDD", "068"),
            ("WW", "10"),
            ("IW", "10"),
            ("Q", "1"),
            ("CC", "21"),
            ("D", "3"),  // Tuesday, Sunday=1
            ("ID", "2"), // ISO Tuesday=2
            ("MS", "123"),
            ("US", "123456"),
            ("Mon", "Mar"),
            ("MON", "MAR"),
            ("Dy", "Tue"),
            ("FMMonth", "March"),
            ("Month", "March    "), // padded to 9
            ("FMDD", "9"),
            (r#""year " YYYY"#, "year  2021"),
            ("YY", "21"),
        ] {
            assert_eq!(to_char(&ts, format), want, "to_char(ts, {format:?})");
        }
    }
}

// ---------------------------------------------------------------------------
// The session's time zone.
// ---------------------------------------------------------------------------

std::thread_local! {
    /// The TimeZone of the session this thread serves: a session runs on a thread of
    /// its own, and a SQLite function runs on the thread stepping its statement.
    static SESSION_ZONE: std::cell::RefCell<(String, Arc<Location>)> =
        std::cell::RefCell::new(("UTC".to_string(), UTC.clone()));
    static SESSION_IS_UTC: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Makes name the TimeZone of the session on this thread, and answers the name as
/// SHOW gives it: a zone in the database's spelling, or the offset as written.
///
/// # Errors
///
/// 22023 for a name that is no zone.
pub fn set_session_zone(name: &str) -> Result<String> {
    let name = name.trim();
    let canon = if name.eq_ignore_ascii_case("utc") || name.eq_ignore_ascii_case("default") || name.is_empty() {
        "UTC".to_string()
    } else {
        ZONE_NAMES
            .iter()
            .find(|z| z.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    };
    let loc = load_location(&canon).ok_or_else(|| {
        crate::error::Error::with_state(
            overlite_core::SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid value for parameter \"TimeZone\": \"{name}\""),
        )
    })?;
    SESSION_IS_UTC.with(|u| u.set(canon == "UTC"));
    SESSION_ZONE.with(|z| *z.borrow_mut() = (canon.clone(), loc));
    Ok(canon)
}

std::thread_local! {
    /// A change of TimeZone a statement made through set_config: the zone before it,
    /// and whether it was local to the transaction. The session reports it.
    static ZONE_CHANGED: std::cell::RefCell<Option<(String, bool)>> = const { std::cell::RefCell::new(None) };
}

/// set_config('TimeZone', value, is_local), from SQL.
pub(crate) fn set_zone_from_sql(value: &str, local: bool) -> Result<String> {
    let before = session_zone();
    let name = set_session_zone(value)?;
    ZONE_CHANGED.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some((before, local));
        }
    });
    Ok(name)
}

/// The change of TimeZone the statements since the last call made through
/// set_config, if one did: the zone before it and whether it was local.
#[must_use]
pub fn take_zone_change() -> Option<(String, bool)> {
    ZONE_CHANGED.with(|c| c.borrow_mut().take())
}

/// The session's TimeZone, as SHOW gives it.
#[must_use]
pub fn session_zone() -> String {
    SESSION_ZONE.with(|z| z.borrow().0.clone())
}

/// Whether the session's zone is UTC, where nothing needs converting.
pub(crate) fn session_is_utc() -> bool {
    SESSION_IS_UTC.with(std::cell::Cell::get)
}

/// Whether the session's zone is UTC, for the protocol's output.
#[must_use]
pub fn session_zone_is_utc() -> bool {
    session_is_utc()
}

/// The offset east of UTC, in seconds, of the session's zone at a UTC instant.
#[must_use]
pub fn session_offset_at(utc: &chrono::NaiveDateTime) -> i64 {
    SESSION_ZONE.with(|z| z.borrow().1.lookup(utc.and_utc().timestamp()).offset)
}

/// A wall clock in the session's zone as the UTC instant it is.
pub(crate) fn session_wall_to_utc(wall: &chrono::NaiveDateTime) -> Option<chrono::NaiveDateTime> {
    if session_is_utc() {
        return Some(*wall);
    }
    let zone = session_zone();
    let s = from_time_zone(&wall.format("%Y-%m-%d %H:%M:%S%.f").to_string(), &zone)?;
    chrono::NaiveDateTime::parse_from_str(s.trim_end_matches("+00"), "%Y-%m-%d %H:%M:%S%.f").ok()
}

/// Today in the session's zone, as CURRENT_DATE gives it.
pub(crate) fn session_today() -> String {
    let now = chrono::Utc::now().naive_utc();
    (now + chrono::Duration::seconds(session_offset_at(&now)))
        .format("%Y-%m-%d")
        .to_string()
}

/// A stored timestamptz (bare UTC, or with `+00`) as timestamptz_out writes it in the
/// session's zone: its wall clock there and the offset, `2011-09-01 13:20:30+03`.
pub(crate) fn session_tz_text(stored: &str) -> Option<String> {
    let t = stored.trim().trim_end_matches("+00").replacen('T', " ", 1);
    let utc = chrono::NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d").map(|d| d.and_time(chrono::NaiveTime::MIN)))
        .ok()?;
    let off = session_offset_at(&utc);
    let wall = utc + chrono::Duration::seconds(off);
    let mut out = wall.format("%Y-%m-%d %H:%M:%S").to_string();
    let micros = wall.and_utc().timestamp_subsec_micros();
    if micros != 0 {
        out.push_str(format!(".{micros:06}").trim_end_matches('0'));
    }
    let a = off.unsigned_abs();
    out.push_str(&format!("{}{:02}", if off < 0 { '-' } else { '+' }, a / 3600));
    if !a.is_multiple_of(3600) {
        out.push_str(&format!(":{:02}", a / 60 % 60));
        if !a.is_multiple_of(60) {
            out.push_str(&format!(":{:02}", a % 60));
        }
    }
    Some(out)
}
