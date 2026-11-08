use crate::common::*;

// Postgres' regular expressions have word and string anchors other engines do
// not spell the same: \m and \M (start and end of a word), \y and \Y (a word
// boundary, or not one) and \Z (end of string). Each was a parse error.
#[test]
fn regex_escapes() {
    let conn = connect_extended(&start_server());

    for (pattern, subject, want) in [
        (r"\m(cds?|lps?)\M", "cds, dvds e lps", true),
        (r"\mlp\M", "help", false), // inside a word
        (r"\mlp\M", "lps", false),  // not the end of a word
        (r"\mvinil", "vinis e vinil", true),
        (r"vinil\M", "vinilico", false),
        (r"a\mb", "a b", false),  // \m needs a word character after it
        (r"a\m b", "a b", false), // ...and none before it
        (r"\M ", "lp ", true),
        (r"\M", "", false),
        (r"\ylp\y", "um lp.", true),
        (r"l\Yp", "lp", true),
        (r"l\Yp", "l p", false),
        (r"fim\Z", "o fim", true),
        (r"fim\Z", "fim dela", false),
        (r"\Aum", "um lp", true),
        (r"\\m", r"a\m", true),           // an escaped backslash stays a backslash
        (r"\mvinil\M", "vinilão", false), // accented letters are word characters
        (r"ão\M", "vinilão", true),
    ] {
        let got: i64 = conn
            .query_row(
                "SELECT CASE WHEN $1::text ~ $2::text THEN 1 ELSE 0 END",
                &[&subject, &pattern],
            )
            .scan()
            .unwrap_or_else(|e| panic!("{subject} ~ {pattern}: {e}"));
        assert_eq!(got == 1, want, "{subject:?} ~ {pattern:?}");
    }

    // Case-insensitive, negated and in a WHERE over a table.
    must_exec(&conn, "CREATE TABLE d (s text)");
    must_exec(
        &conn,
        "INSERT INTO d VALUES ('Discos de VINIL'), ('vinilico'), ('livros')",
    );
    let n: i64 = conn
        .query_row(r"SELECT count(*) FROM d WHERE s ~* '\mvinil\M'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
    let n: i64 = conn
        .query_row(r"SELECT count(*) FROM d WHERE s !~* '\mvinil\M'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 2);

    // A pattern bound as a parameter is case-insensitive under ~* too.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM d WHERE s ~* $1", &[&r"\mvinil\M"])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "~* with a parameter");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM d WHERE s !~* $1", &[&"vinil"])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "!~* with a parameter");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM d WHERE s ~ $1", &[&"vinil"])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "~ stays case-sensitive");
}
