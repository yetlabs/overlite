use std::time::{Duration, Instant};

use crate::common::*;
use sha2::{Digest, Sha256};

/// One line per query below: digits, name, SHA-256 and length of the result on the
/// old server, and the time of one call there, which is for the log only.
const GOLDEN: &str = include_str!("../fixtures/parity-numeric-baseline.golden");

// time_sql: the mean time of a statement, and its result.
fn time_sql(c: &Conn, sql: &str, iters: u32) -> (Duration, Vec<u8>) {
    let t0 = Instant::now();
    let mut out = Vec::new();
    for _ in 0..iters {
        let rs = c.pg_exec(sql).unwrap_or_else(|e| panic!("{sql:.60}: {e}"));
        assert_eq!(rs[0].rows.len(), 1, "no row");
        out = rs[0].cell(0, 0).to_vec();
    }
    (t0.elapsed() / iters, out)
}

// parity_numeric_timing times large numerics through the SQL functions, and checks
// each result against the old server's. The times are logged, and nothing asserts
// them.
#[test]
fn parity_numeric_timing() {
    let rsc = connect(&start_server());
    let frac = |s: &str| format!("{}.{}", &s[..s.len() / 2], &s[s.len() / 2..]);
    let mut golden = GOLDEN.lines().filter(|l| !l.starts_with('#'));
    for n in [1000, 10000] {
        let (a, b) = (digits(n, 3), digits(n, 8));
        let iters = if n > 1000 { 20 } else { 200 };
        for (name, sql) in [
            ("add", format!("SELECT dec_add('{a}','{b}')")),
            ("mul", format!("SELECT dec_mul('{a}','{b}')")),
            ("sub", format!("SELECT dec_sub('{a}','{b}')")),
            ("div", format!("SELECT dec_div('{a}','{b}')")),
            ("cmp", format!("SELECT dec_cmp('{a}','{b}')")),
            ("fracadd", format!("SELECT dec_add('{}','{}')", frac(&a), frac(&b))),
            ("fracmul", format!("SELECT dec_mul('{}','{}')", frac(&a), frac(&b))),
            ("overhead", "SELECT dec_add('1','1')".to_string()),
        ] {
            let (r, out) = time_sql(&rsc, &sql, iters);
            let want: Vec<&str> = golden.next().expect("a golden line").split(' ').collect();
            assert_eq!(want[..2], [n.to_string().as_str(), name]);
            assert_eq!(out.len().to_string(), want[3], "{n} digits {name}: length");
            assert_eq!(hex::encode(Sha256::digest(&out)), want[2], "{n} digits {name}");
            let g: f64 = want[4].parse().unwrap();
            println!(
                "{n:6} digits {name:<8} old {g:12}ns new {r:12?} ratio new/old {:.2}",
                r.as_nanos() as f64 / g
            );
        }
    }
    assert!(golden.next().is_none(), "a golden line has no query");
}
