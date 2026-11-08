use crate::common::*;

// Full-text search as Wagtail's database backend and django.contrib.postgres.search
// use it: tsvector and tsquery in Postgres' text form, the simple and english
// configurations, setweight, ||, @@ either way round, ts_rank and ts_rank_cd with
// weights, length, and a GIN index. Every expected value is what Postgres 17 answered
// for the same statement.

fn rows(c: &Conn, sql: &str) -> Vec<String> {
    let r = c.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let last = r.last().unwrap();
    (0..last.rows.len())
        .map(|i| {
            (0..last.fields.len())
                .map(|j| last.text(i, j))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn one(c: &Conn, sql: &str) -> String {
    rows(c, sql).join("\n")
}

#[test]
fn vectors_queries_and_matching() {
    let server = start_server();
    let c = connect(&server);
    assert_eq!(
        one(
            &c,
            "SELECT to_tsvector('simple'::regconfig, COALESCE('Roberta Johnson', '')) AS v"
        ),
        "'johnson':2 'roberta':1"
    );
    assert_eq!(
        one(
            &c,
            "SELECT setweight(to_tsvector('simple'::regconfig, 'Roberta'), 'D') || setweight(to_tsvector('simple'::regconfig, 'Johnson'), 'A') AS v"
        ),
        "'johnson':2A 'roberta':1"
    );
    assert_eq!(
        one(
            &c,
            "SELECT to_tsvector('english', 'Cookbook: Christmas Mince Pies (Meat-Free)')"
        ),
        "'christma':2 'cookbook':1 'free':7 'meat':6 'meat-fre':5 'minc':3 'pie':4"
    );
    assert_eq!(
        one(
            &c,
            "SELECT to_tsvector('email me at foo.bar@example.com or http://www.example.com/path/to?x=1 now')"
        ),
        "'/path/to?x=1':8 'email':1 'foo.bar@example.com':4 'www.example.com':7 'www.example.com/path/to?x=1':6"
    );
    assert_eq!(
        one(
            &c,
            "SELECT to_tsquery('english', 'fat:AB & !rat:*'), plainto_tsquery('english', 'The Fat Rats')"
        ),
        "'fat':AB & !'rat':*|'fat' & 'rat'"
    );
    assert_eq!(
        one(
            &c,
            "SELECT phraseto_tsquery('english', 'The cat in the hat'), websearch_to_tsquery('english', 'fat or rat -cat \"sad dog\" or')"
        ),
        "'cat' <3> 'hat'|'fat' | 'rat' & !'cat' & 'sad' <-> 'dog'"
    );
    assert_eq!(
        one(
            &c,
            "SELECT 'a:1 b'::tsvector, strip('a:1,3B c:2'::tsvector), length('a:1 b c'::tsvector), '(a|b)&c'::tsquery"
        ),
        "'a':1 'b'|'a' 'c'|3|( 'a' | 'b' ) & 'c'"
    );
    assert_eq!(
        one(
            &c,
            "SELECT to_tsvector('english', 'The Fat Rats') @@ plainto_tsquery('english', 'fat rat'), ts_rank(to_tsvector('english', 'The Fat Rats'), to_tsquery('english', 'fat & rat'))"
        ),
        "t|0.09910322"
    );
    assert_eq!(
        one(
            &c,
            "SELECT to_tsquery('english', 'fox') @@ to_tsvector('english', 'the fox'), to_tsvector('english', 'the quick brown fox') @@ to_tsquery('english', 'brown <-> quick')"
        ),
        "t|f"
    );
    for (sql, code, message) in [
        ("SELECT ''':1'::tsvector", "42601", "syntax error in tsvector: \"':1\""),
        ("SELECT 'a & '::tsquery", "42601", "no operand in tsquery: \"a & \""),
        (
            "SELECT to_tsvector('nope', 'x')",
            "42704",
            "text search configuration \"nope\" does not exist",
        ),
        (
            "SELECT ts_rank('{0.1,0.2}', 'a'::tsvector, 'a'::tsquery)",
            "2202E",
            "array of weight is too short",
        ),
        (
            "SELECT setweight('a:1'::tsvector, 'x')",
            "22023",
            "unrecognized weight: \"x\"",
        ),
    ] {
        let e = c.exec(sql, &[]).expect_err(sql);
        assert_pg_error(&e, code, message);
    }
    // Parameters, over the extended protocol.
    let ext = connect_extended(&server);
    let r = ext
        .exec_params(
            "SELECT to_tsvector($1::regconfig, $2) @@ plainto_tsquery($1::regconfig, $3)",
            &["english", "wild yeast breads", "bread"],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(r.text(0, 0), "t");
}

// Wagtail's index: tsvector columns with GIN indexes, its upsert, the norms of its
// titles, and its search.
#[test]
fn wagtail_search_index() {
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    let server = start_server_on(&path, 0);
    let c = connect(&server);
    must_exec(
        &c,
        "CREATE TABLE ie (content_type_id int, object_id varchar(50), title tsvector NOT NULL, autocomplete tsvector NOT NULL, body tsvector NOT NULL, title_norm double precision NOT NULL, title_text text, body_text text, UNIQUE (content_type_id, object_id))",
    );
    must_exec(&c, "CREATE INDEX ie_title_gin ON ie USING gin (title)");
    must_exec(&c, "CREATE INDEX ie_body_gin ON ie USING gin (body)");
    let upsert = "INSERT INTO ie (content_type_id, object_id, title, autocomplete, body, title_norm, title_text, body_text)
        (VALUES (42, '62', setweight(to_tsvector(COALESCE('Tracking Wild Yeast', '')), 'B'), setweight(to_tsvector('simple'::regconfig, COALESCE('Tracking Wild Yeast', '')), 'D'), (setweight(to_tsvector(COALESCE('Yeasts, with their single-celled growth habit, can be contrasted with molds.', '')), 'D') || setweight(to_tsvector(COALESCE('The art of cultivating yeast', '')), 'D')), 1.0, 'Tracking Wild Yeast', 'Yeasts...'), (55, '81', setweight(to_tsvector(COALESCE('Hot Cross Bun', '')), 'B'), setweight(to_tsvector('simple'::regconfig, COALESCE('Hot Cross Bun', '')), 'D'), setweight(to_tsvector(COALESCE('The Greeks in 6th century AD may have marked cakes with a cross.', '')), 'D'), 1.0, 'Hot Cross Bun', 'The Greeks'))
        ON CONFLICT (content_type_id, object_id)
        DO UPDATE SET title = EXCLUDED.title, title_norm = 1.0, autocomplete = EXCLUDED.autocomplete, body = EXCLUDED.body, title_text = EXCLUDED.title_text, body_text = EXCLUDED.body_text";
    must_exec(&c, upsert);
    must_exec(&c, upsert);
    assert_eq!(
        rows(
            &c,
            "SELECT object_id, title, autocomplete, body FROM ie ORDER BY object_id"
        ),
        [
            "62|'track':1B 'wild':2B 'yeast':3B|'tracking':1 'wild':2 'yeast':3|'art':15 'cell':6 'contrast':11 'cultiv':17 'growth':7 'habit':8 'mold':13 'singl':5 'single-cel':4 'yeast':1,18",
            "81|'bun':3B 'cross':2B 'hot':1B|'bun':3 'cross':2 'hot':1|'6th':4 'ad':6 'cake':10 'centuri':5 'cross':13 'greek':2 'mark':9 'may':7",
        ]
    );
    // Stored as that text: the storage format's form of a tsvector.
    let db = rusqlite::Connection::open(&path).unwrap();
    let stored: String = db
        .query_row(
            "SELECT typeof(title) || ':' || title FROM ie WHERE object_id = '81'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "text:'bun':3B 'cross':2B 'hot':1B");
    // A tsvector written as text is stored in its form, and one that is none refused.
    must_exec(&c, "UPDATE ie SET autocomplete = 'b:2 a:1,1' WHERE object_id = '81'");
    let stored: String = db
        .query_row("SELECT autocomplete FROM ie WHERE object_id = '81'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, "'a':1 'b':2");
    let e = c
        .exec("UPDATE ie SET autocomplete = ''':x' WHERE object_id = '81'", &[])
        .unwrap_err();
    assert_sqlstate(&e, "42601");
    must_exec(
        &c,
        "UPDATE ie SET autocomplete = 'bun:3 cross:2 hot:1' WHERE object_id = '81'",
    );

    // The title norms: length of a tsvector column counts its lexemes.
    assert_eq!(
        one(
            &c,
            r#"SELECT AVG(LENGTH("ie"."title"))::float8 AS "title_length__avg" FROM "ie" WHERE LENGTH("ie"."title") > 0"#
        ),
        "3"
    );
    must_exec(
        &c,
        r#"UPDATE "ie" SET "title_norm" = (3.0 / LENGTH("ie"."title")) WHERE ("ie"."title_norm" = 1.0 AND LENGTH("ie"."title") > 0)"#,
    );
    assert_eq!(rows(&c, "SELECT title_norm FROM ie ORDER BY object_id"), ["1", "1"]);

    // The search: autocomplete by prefix, and a ranked match over title || body.
    assert_eq!(
        rows(
            &c,
            r#"SELECT object_id FROM ie WHERE "ie"."autocomplete" @@ (to_tsquery('simple'::regconfig, '''bread'':*')) OR "ie"."autocomplete" @@ (to_tsquery('simple'::regconfig, '''trac'':*')) ORDER BY 1"#
        ),
        ["62"]
    );
    assert_eq!(
        rows(
            &c,
            r#"SELECT object_id, ts_rank(('{0.1,0.2,0.4,1.0}')::float4[], "ie"."title", to_tsquery('english'::regconfig, '''yeast''')), ts_rank(('{0.1,0.2,0.4,1.0}')::float4[], "ie"."body", to_tsquery('english'::regconfig, '''yeast''')) FROM ie WHERE ("ie"."title" || "ie"."body") @@ (to_tsquery('english'::regconfig, '''yeast''')) ORDER BY 1"#
        ),
        ["62|0.24317084|0.075990885"]
    );
    assert_eq!(
        rows(
            &c,
            r#"SELECT object_id, "ie"."title" || "ie"."body" FROM ie ORDER BY 1"#
        ),
        [
            "62|'art':18 'cell':9 'contrast':14 'cultiv':20 'growth':10 'habit':11 'mold':16 'singl':8 'single-cel':7 'track':1B 'wild':2B 'yeast':3B,4,21",
            "81|'6th':7 'ad':9 'bun':3B 'cake':13 'centuri':8 'cross':2B,16 'greek':5 'hot':1B 'mark':12 'may':10",
        ]
    );
    assert_eq!(
        one(
            &c,
            r#"SELECT ts_rank_cd(body, plainto_tsquery('english', 'greek cross')), body @@ phraseto_tsquery('english', 'marked cakes'), body @@ websearch_to_tsquery('english', '"marked cakes" -bread or cross') FROM ie WHERE object_id = '81'"#
        ),
        "0.009090909|t|t"
    );
    must_exec(&c, "DROP INDEX ie_title_gin");
}
