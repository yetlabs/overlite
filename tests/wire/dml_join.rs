use crate::common::*;

/// The rows of one statement as `a|b` lines, and its tag.
fn run(conn: &Conn, sql: &str, params: &[&str]) -> (Vec<String>, String) {
    let r = conn
        .exec_params(sql, params, &[], &[], &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let rows = (0..r.rows.len())
        .map(|i| (0..r.fields.len()).map(|j| r.text(i, j)).collect::<Vec<_>>().join("|"))
        .collect();
    (rows, r.tag.as_str().to_string())
}

fn fixture(conn: &Conn) {
    must_exec(conn, "CREATE TABLE project (id bigserial PRIMARY KEY, repo_id bigint)");
    must_exec(
        conn,
        "CREATE TABLE project_issue (id bigserial PRIMARY KEY, project_id bigint, issue_id bigint)",
    );
    must_exec(conn, "CREATE TABLE repo (id bigint PRIMARY KEY, name text)");
    must_exec(conn, "INSERT INTO repo VALUES (1, 'one'), (2, 'two')");
    must_exec(conn, "INSERT INTO project (repo_id) VALUES (1), (2), (2)");
    must_exec(
        conn,
        "INSERT INTO project_issue (project_id, issue_id) VALUES (1, 10), (2, 20), (2, 21), (3, 30)",
    );
}

// Gitea deletes a repository's project rows with DELETE ... USING, which SQLite does
// not have: every repository delete and user purge failed on it. Answers are
// Postgres 17's for the same statements.
#[test]
fn delete_using_as_postgres() {
    let conn = connect_extended(&start_server());
    fixture(&conn);
    assert_eq!(
        run(
            &conn,
            "DELETE FROM project_issue USING project WHERE project.id = project_issue.project_id AND project.repo_id = $1",
            &["2"]
        ),
        (vec![], "DELETE 3".to_string())
    );
    assert_eq!(
        run(&conn, "SELECT id, project_id FROM project_issue ORDER BY id", &[]).0,
        vec!["1|1"]
    );
    // A target row the join meets twice is deleted once.
    assert_eq!(
        run(
            &conn,
            "DELETE FROM project USING repo WHERE repo.id >= 1 AND project.repo_id = 1",
            &[]
        ),
        (vec![], "DELETE 1".to_string())
    );
    assert_eq!(run(&conn, "SELECT id FROM project ORDER BY id", &[]).0, vec!["2", "3"]);
}

#[test]
fn delete_using_aliases_several_tables_and_returning() {
    let conn = connect_extended(&start_server());
    fixture(&conn);
    let (rows, tag) = run(
        &conn,
        "DELETE FROM project_issue AS pi USING project p, repo r WHERE p.id = pi.project_id \
         AND r.id = p.repo_id AND r.name = $1 AND pi.issue_id <> 30 RETURNING pi.id, pi.issue_id, r.name",
        &["two"],
    );
    assert_eq!(tag, "DELETE 2");
    let mut rows = rows;
    rows.sort();
    assert_eq!(rows, vec!["2|20|two", "3|21|two"]);
    assert_eq!(
        run(&conn, "SELECT id FROM project_issue ORDER BY id", &[]).0,
        vec!["1", "4"]
    );
    // A JOIN in the USING list, and a WITH in front.
    run(
        &conn,
        "WITH gone AS (SELECT 1 AS repo) DELETE FROM project_issue x USING project p JOIN repo r ON r.id = p.repo_id, gone \
         WHERE p.id = x.project_id AND r.id = gone.repo",
        &[],
    );
    assert_eq!(run(&conn, "SELECT id FROM project_issue", &[]).0, vec!["4"]);
}

#[test]
fn update_from_with_alias_and_returning() {
    let conn = connect_extended(&start_server());
    fixture(&conn);
    let (rows, tag) = run(
        &conn,
        "UPDATE project_issue x SET issue_id = r.id * 100 FROM project p JOIN repo r ON r.id = p.repo_id \
         WHERE p.id = x.project_id AND r.name = $1 RETURNING x.id, x.issue_id, r.name AS rname",
        &["two"],
    );
    assert_eq!(tag, "UPDATE 3");
    let mut rows = rows;
    rows.sort();
    assert_eq!(rows, vec!["2|200|two", "3|200|two", "4|200|two"]);
    let (rows, _) = run(
        &conn,
        "UPDATE project p SET repo_id = 5 WHERE p.id = 1 RETURNING p.repo_id",
        &[],
    );
    assert_eq!(rows, vec!["5"]);
}
