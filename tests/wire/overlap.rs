use crate::common::*;

// TestRangeOverlapAndContainment: the "&&" overlap operator and range @> range
// containment over the text-range representation.
#[test]
fn range_overlap_and_containment() {
    let conn = connect(&start_server());

    let q = |sql: &str| -> bool { conn.query_row(sql, &[]).scan().unwrap() };

    // Range overlap.
    assert!(q("SELECT int4range(1,10) && int4range(5,15)"));
    assert!(!q("SELECT int4range(1,5) && int4range(5,10)")); // [1,5) vs [5,10): touch, no overlap
    assert!(q("SELECT int4range(1,5,'[]') && int4range(5,10)")); // [1,5] includes 5
    assert!(q("SELECT numrange(NULL, 10) && numrange(3, NULL)")); // (-inf,10) & (3,+inf)

    // Range contains range.
    assert!(q("SELECT int4range(1,20) @> int4range(5,10)"));
    assert!(!q("SELECT int4range(1,10) @> int4range(5,20)"));
    assert!(q("SELECT int4range(1,10) @> 5")); // range @> element still works

    // Array overlap.
    assert!(q("SELECT ARRAY[1,2,3] && ARRAY[3,4,5]"));
    assert!(!q("SELECT ARRAY[1,2] && ARRAY[3,4]"));
}
