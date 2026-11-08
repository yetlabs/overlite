use std::collections::HashMap;

use crate::common::*;

// distinct_on_star: SELECT DISTINCT ON (col) * over a single table keeps the
// first row per group per the ORDER BY (SQLite has no DISTINCT ON, and a "*"
// select list previously errored outright).
#[test]
fn distinct_on_star() {
    let conn = connect(&start_server());
    conn.exec(r#"CREATE TABLE events (user_id int, ts int, action text)"#, &[])
        .unwrap();
    conn.exec(
        r#"INSERT INTO events VALUES
		(1, 10, 'a'), (1, 30, 'c'), (1, 20, 'b'),
		(2, 5, 'x'), (2, 8, 'y')"#,
        &[],
    )
    .unwrap();

    // Latest action per user (highest ts).
    let rows = conn
        .query(
            r#"SELECT DISTINCT ON (user_id) * FROM events ORDER BY user_id, ts DESC"#,
            &[],
        )
        .unwrap();
    let mut got: HashMap<i64, String> = HashMap::new();
    for row in &rows {
        let (uid, _ts, action): (i64, i64, String) = row.scan().unwrap();
        got.insert(uid, action);
    }
    assert_eq!(got, HashMap::from([(1, "c".to_string()), (2, "y".to_string())]));

    // With a WHERE and a table alias.
    let (_, _, action): (i64, i64, String) = conn
        .query_row(
            r#"SELECT DISTINCT ON (e.user_id) * FROM events e WHERE e.user_id = 1 ORDER BY e.user_id, e.ts"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(action, "a"); // lowest ts for user 1
}
