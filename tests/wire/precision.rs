use crate::common::*;

// TestColumnPrecision: information_schema.columns reports the declared length of
// character types and the precision/scale of numeric types.
#[test]
fn column_precision() {
    let conn = connect(&start_server());
    conn.exec(
        "CREATE TABLE prec (a varchar(50), b numeric(10,2), c char(4), d int, e text, f numeric)",
        &[],
    )
    .unwrap();

    struct Prec {
        char_len: Option<i64>,
        num_prec: Option<i64>,
        num_radix: Option<i64>,
        num_scale: Option<i64>,
    }
    let get = |col: &str| -> Prec {
        let (char_len, num_prec, num_radix, num_scale) = conn
            .query_row(
                "SELECT character_maximum_length, numeric_precision, numeric_precision_radix, numeric_scale
			 FROM information_schema.columns WHERE table_name='prec' AND column_name=$1",
                &[&col],
            )
            .scan()
            .unwrap();
        Prec {
            char_len,
            num_prec,
            num_radix,
            num_scale,
        }
    };

    let a = get("a"); // varchar(50)
    assert_eq!(a.char_len, Some(50));
    assert_eq!(a.num_prec, None);

    let b = get("b"); // numeric(10,2)
    assert_eq!(b.char_len, None);
    assert_eq!(b.num_prec, Some(10));
    assert_eq!(b.num_radix, Some(10));
    assert_eq!(b.num_scale, Some(2));

    let c = get("c"); // char(4)
    assert_eq!(c.char_len, Some(4));

    let d = get("d"); // int: 32 binary digits, as Postgres reports it
    assert_eq!(d.char_len, None);
    assert_eq!(d.num_prec, Some(32));
    assert_eq!(d.num_radix, Some(2));
    assert_eq!(d.num_scale, Some(0));

    let f = get("f"); // unqualified numeric -> no fixed precision
    assert_eq!(f.num_prec, None);
    assert_eq!(f.num_scale, None);
}
