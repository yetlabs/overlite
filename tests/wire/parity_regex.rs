use std::time::{Duration, Instant};

use crate::common::*;

#[test]
fn parity_regex_timeout() {
    let conn = connect(&start_server());
    let input = "a".repeat(64);
    let t0 = Instant::now();
    let res = conn.pg_exec(format!(r"SELECT '{input}' ~ '^(?:\Ya|a)*b'"));
    let took = t0.elapsed();
    let err = res.unwrap_err();
    assert_contains!(err, format!("match timeout after 5s on input `{input}`"));
    assert!(took >= Duration::from_millis(4900), "{took:?}");
    assert!(took < Duration::from_secs(8), "{took:?}");
    println!("took {took:?}: {err}");
}
