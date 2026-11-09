# overlite notes

Implementation notes. See ARCHITECTURE.md for how the code is laid out.

Compared against the old server at master 282b3f0 and the owner's uncommitted
work on top of it (DROP CONSTRAINT, column IF [NOT] EXISTS, ctid, quoted cast
operands, row-count parameter types).

## Status

It works with `psql` and `pgbench -n -f geto.sql` (simple, extended and prepared
modes) against a copy of the real 73.8M-row database, so it can be benchmarked.

## Build and run

Build natively with cargo (`make build`, `make check`), or in a container where there
is no cargo, as CI's Linux is:

    docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/target \
      -v ol-rs-target:/target -v ol-rs-cargo:/usr/local/cargo/registry \
      docker.io/library/rust:1 cargo build --release

The binary is at target/release/overlite (/target/release/overlite in the container). Same flags as the old server:
`overlite --host 0.0.0.0 --port 5432 --db /data/cnpj.db`.

The minimum Rust is 1.88, `rust-version` in Cargo.toml, set by what the dependency tree
requires. `cargo bench -p overlite-postgres` times the rewrite; it is not part of
`make check`.

## Choices that matter for a comparison

- One OS thread per client, blocking I/O; each client has its own SQLite
  connection (WAL, busy_timeout 30000, foreign_keys on, a transaction's write lock
  taken at its first write (below), opened
  with FULLMUTEX and URI flags).
- A statement is prepared and finalized on every execution: there is no
  statement cache.
- SQLite is 3.53.2 (rusqlite pinned to =0.40.2), as on the old server.
  LIBSQLITE3_FLAGS matches its `PRAGMA compile_options`. Remaining differences: the
  old server had DISABLE_INTRINSIC; this one has FTS3, LOAD_EXTENSION and HAVE_ISNAN.
  The C compiler optimizes SQLite natively, which is the point of the comparison.
- mimalloc is the global allocator; SQLite uses the system malloc (glibc on Linux).
- A CancelRequest calls sqlite3_interrupt directly; nothing runs per statement to
  watch for it.
- A full-table `LIKE` runs SQLite's own matching on every row, with no Rust in the loop
  (GLOB, over a pattern made once per statement), so only the engine's build can move
  it, and libsqlite3.a is byte-identical across the reorganized workspace. The same file timed run after run spreads by 10%: a lone
  timing compares nothing. Alternate the binaries, and swap their order.

## Behavior

`tests/wire/parity_diff.rs` holds this server to the old server's answers, where those
agree with Postgres, which decides. Where they do not, the statement moves to that
test's known differences, with the reason, rather than the golden being regenerated.
What is listed here and in Limits was checked against it; `SIGTERM`, bytes outside
UTF-8, zone offsets, quoting, JSON, regexes and pg_hba YAML otherwise match it.

- `COPY ... TO STDOUT` writes NULL as `\N`, so a NULL survives a dump and restore.
- Shutdown ends sessions first (5s at most, a session waiting out busy_timeout
  cannot be interrupted), so the last one out checkpoints the WAL and none is left
  behind. Clients see 57014, or a closed connection.
- Binary results follow Postgres: a numeric, an array and an hstore go out as
  numeric_send, array_send and hstore_send do, where the old server sent text or the
  stored JSON under format code 1. parity_diff counts the rows this changes. A stored
  value that cannot be one (`abc` in a numeric, a jagged array, a time that cannot be
  parsed) cannot exist in Postgres, so it has no answer to copy. Under format 1 it is
  22P03, ending the statement at that row: its text under that format code is bytes a
  client would misread. Text format still reads it, which is how to find the row. The
  rows of a SELECT before it have gone out, as they have in Postgres before a failure
  part way through. A write's RETURNING goes out only once it is all good, and a value
  with no form undoes the write, as a failing RETURNING expression does in Postgres. What is a real value in another spelling keeps
  its binary form: infinity, a uuid in braces or as raw bytes, an hstore bound as text.
- ALTER TABLE: extended Parse splits comma-separated actions as the simple path does,
  and IF [NOT] EXISTS is read only where an action opens. IF EXISTS on a table that is
  not there answers the tag and Postgres' notice, whatever the action, once for the
  statement. An action that skips itself (ADD COLUMN IF NOT EXISTS, DROP COLUMN IF
  EXISTS) says so with its own notice.
- DROP CONSTRAINT answers 42704 for a constraint that is not there unless IF EXISTS
  is given, and 2BP01 for a key a foreign key stands on unless CASCADE is given,
  which rebuilds the referencing tables without those foreign keys. A consequence:
  a constraint accepted by the no-op path was never kept, so dropping it is 42704.
- A result column reading or named ctid is labelled ctid, in the outermost select
  list and in RETURNING. A subquery still says rowid inside.
- A result column the statement does not name is labelled as Postgres labels it, in
  the outermost select list and in RETURNING: a call by its function (`count`, `max`,
  `btrim` for trim, `nextval`), a cast by its operand's name or else its type (`int4`),
  CASE, EXISTS and ARRAY by the keyword, current_user and its kin by their name, a
  scalar subquery by its column, and anything else `?column?`. SQLite labelled each
  with its text after the rewrite (`count(*)`, `datetime('now')`, `'postgres'`). The
  label is read from the statement as sent. parity_diff counts the 151 outcomes this
  changes. As in Postgres, `ORDER BY x` then means the output column named x.
- ALTER TABLE, DROP TABLE and SQL-level EXECUTE are held to the owner and privilege
  checks in every route, with 42501. COPY, GRANT, REVOKE and DROP POLICY are checked
  too.
- `_overlite_owners` and `_overlite_grants` are keyed by stored name: bare in public,
  `<schema>.<table>` elsewhere. SET SCHEMA, RENAME TO and ALTER SCHEMA RENAME move the
  rows, DROP TABLE and DROP SCHEMA remove them.
- Column privileges: `GRANT`/`REVOKE priv (col, ...) ON t` for SELECT, INSERT, UPDATE
  and REFERENCES (and ALL, which is those four) are rows of `_overlite_column_grants`,
  keyed like the table grants by stored name, and by the column's name as the table
  spells it. A role holding only column privileges uses exactly those columns, as in
  Postgres 17, checked with a `postgres:17-alpine`: a SELECT of any other column,
  `*`, a whole row, a WHERE, ORDER BY, RETURNING, subquery, join or COPY reading one is
  42501 `permission denied for table t`; an INSERT may name only its columns (the rest
  take their defaults, and DEFAULT VALUES needs any one); an UPDATE may set only its
  columns, and what it reads, in SET, WHERE or RETURNING, needs SELECT on those. A table
  privilege still covers every column. REVOKE on the table takes the column grants of
  that privilege too; REVOKE on a column leaves a table grant alone. Column grants move
  with RENAME COLUMN, RENAME TO, SET SCHEMA, ALTER SCHEMA RENAME and ALTER ROLE RENAME,
  and go with DROP COLUMN (so a column added again by that name has none) and DROP
  TABLE. `DELETE (a)` and `TRUNCATE (a)` are 0LP01, an unknown column 42703.
- What a SELECT, INSERT, UPDATE or DELETE reads and writes is read from its parse tree
  (`auth/columns.rs`), table by table and column by column, as Postgres collects it. A
  statement that reader does not follow (another kind, one Postgres' grammar refuses, or
  one holding a node it does not walk) is held to the tables its text names and to
  table-level grants only, so a role with only column privileges on such a table is
  refused rather than let through. Being Postgres' rules, these now hold for table
  grants too, where the text reader was looser: an UPDATE or DELETE whose WHERE or
  RETURNING reads a column, and an INSERT whose RETURNING does, needs SELECT on it
  besides UPDATE, DELETE or INSERT (a role granted only UPDATE can no longer filter on
  a column it cannot read), and the tables an
  INSERT ... SELECT, an UPDATE ... FROM or SET subquery, or a DELETE ... USING reads need
  SELECT, which the text reader did not ask for. A CTE is no longer taken for a table of
  its name.
- A `LANGUAGE sql` function's body is held to the privileges Postgres checks, as
  Postgres 17 answers them (tests/wire/function_privileges.rs). It is still inlined at
  each call, now by the session before the privilege check and the policies, at any
  depth (in a select list, WHERE, ORDER BY, RETURNING, a SET, a FROM as a set, a call
  inside a call), so what it reads is in the statement they see. SECURITY INVOKER (the
  default) reads with the caller's privileges, column by column, and through the
  caller's policies. SECURITY DEFINER reads with its owner's and through its owner's
  policies, and `current_user` in it is the owner; its arguments are the caller's to
  read. The owner is who created it, and ALTER FUNCTION ... OWNER TO changes it (to a
  role the owner may become, or by a superuser). EXECUTE is PUBLIC's until `REVOKE
  EXECUTE ON FUNCTION f FROM PUBLIC`; GRANT/REVOKE EXECUTE take FUNCTION, ROUTINE,
  PROCEDURE and ALL FUNCTIONS IN SCHEMA, and a role that does not own the function
  grants nothing, without an error, as in Postgres. CREATE OR REPLACE, DROP and ALTER
  FUNCTION of another role's function are 42501 `must be owner of function f`; ALTER
  FUNCTION takes OWNER TO and SECURITY DEFINER | INVOKER, anything else is 0A000. The
  engine inlines nothing: a call the session did not inline fails in SQLite. Before,
  a body was inlined after the check and the policies and read anything, so a role
  with no grant on a table read it through a function of its own; and a function named
  like a built-in (`lower`) replaced it in every statement, the server's own privilege
  queries included. Now a call of a built-in's name (SQLite's, this engine's, `pg_*`,
  `overlite_*`) or of a Postgres keyword (`x IN (a, b)`) is never a stored function's,
  as pg_catalog comes first in Postgres. The names a call may reach are kept per
  database; they were one list for the whole process.
- What else a statement reaches is checked too (tests/wire/privilege_routes.rs,
  tests/wire/explain_privileges.rs). EXPLAIN, with any options, is held to the
  statement it explains (42501, as in Postgres), and EXPLAIN ANALYZE runs a write as a
  write runs, its policies' INSERT check included; over the extended protocol it runs
  at Execute, each time, not at Parse. A view reads its tables with its owner's
  privileges: CREATE VIEW records its creator as owner (and DROP VIEW is the owner's),
  so a view of a role's over a table it cannot read reads nothing, for it or for whoever
  it is granted to. CREATE TABLE AS and CREATE MATERIALIZED VIEW read their query with
  the creator's privileges and through its policies; REFRESH is the owner's and reads
  with the owner's privileges. SQL-level EXECUTE of an INSERT goes through the
  policies' check (it did not). A SQLite trigger's body is checked with the privileges
  of the role whose write fires it, as a Postgres trigger function runs; CREATE
  TRIGGER needs the table's ownership or TRIGGER, DROP TRIGGER its ownership. An
  UPDATE's or DELETE's subqueries, FROM and USING are read through their tables'
  policies. A statement Postgres' grammar refuses (`INSERT OR REPLACE ... SELECT`) is
  refused with Postgres' syntax error before any grant is read (since stage 5c; it
  was held to SELECT on what it read). The server's own tables (`_overlite_*`, where
  the roles, the grants and the policies are), sqlite_dbpage and writing `sqlite_*`
  are a superuser's: before, any role could make itself one with `UPDATE
  _overlite_roles`. PRAGMA, ATTACH, DETACH and VACUUM INTO, which were a superuser's,
  are syntax errors for every role since stage 5c.
  Checked and unchanged: cursors, PREPARE/EXECUTE's reads, COPY (query) TO, SELECT
  INTO (refused before it runs), a DEFAULT, CHECK or generated column calling a
  function that reads (SQLite takes no subquery there, so the table is refused). There
  are no rules and no DO blocks: both are syntax errors.
 and move and go with
  the table like its owner. COPY TO reads through the policies and COPY FROM is 0A000
  for a role under RLS, as in Postgres. An older server looked RLS up by bare name, so
  a row written through the search_path did protect `s1.t`, and reading it as public's
  alone would have opened that table. So the first start on an older database runs
  RLS_STORED_NAMES_UPGRADE (crates/engine/src/catalog/rls.rs), once, marked by the empty
  `_overlite_rls_stored_names`: each bare row is copied to every schema table of that
  name, whose flags can only turn on. Failing stops the connection. A bare row reached
  all namesakes then, and each keeps that protection. Effects on an older database: a
  row written with a qualified name matched nothing then and is enforced now, so a role
  may see fewer rows; and a bare row with no public table stays, and would apply to a
  public table created later by that name.
- `_overlite_defaults` holds stored names too, and moves and goes with the table. An
  older server looked it up by bare name: a table made through the search_path shared
  its row with every namesake, the first row winning, and a table made with a qualified
  name got no default at all. Unlike owners, whose old rows may lapse because a table
  then fails closed, a default that lapses fails silently, into NULL keys. So the first
  start runs DEFAULTS_STORED_NAMES_UPGRADE (crates/engine/src/catalog/sequences.rs),
  once, marked by the empty `_overlite_defaults_stored_names`: each bare row is copied
  to every schema table of that name that has the column. Namesakes that shared a
  sequence go on sharing it, whose values stay unique, until an admin edits the rows.
  Failing stops the connection. Effects on an older database: a column whose default
  never applied gets it, and fails if that sequence is gone.
- `numeric[]` and `decimal[]` columns keep their declared type like `text[]`. A modifier
  before the brackets (`numeric(10,2)[]`, `varchar(20)[]`, `timestamp(3)[]`) is kept by
  quoting the whole type, which SQLite takes as its name. The catalogs report arrays as
  Postgres does: `ARRAY` and `_numeric` in information_schema, the array's OID,
  attndims and format_type in pg_attribute, a row in pg_type.
- What decides a statement's route reads code, not literals (blank_literals and
  code_only in crates/engine/src/blank.rs).
- Very large numerics: through dec_add, dec_mul and friends this server took 0.2 to
  0.6 times the old server's time at 1,000 and 10,000 digits, with identical results,
  so decimal.rs keeps its hand-written Nat/Rat.
- A flag given twice takes the last, as the old server did: the image's ENTRYPOINT
  passes `--host 0.0.0.0`, and a compose file may pass `--host` again.
- A comment is whitespace everywhere, as in Postgres. An apostrophe in a `--` comment
  before `$1` hid the parameter from the count, so Parse answered 42P02 (Prisma's `db
  pull`), and a `--` at the end of a `LANGUAGE sql` body commented out the parenthesis
  the body is inlined in.
- A parameter takes the type of the column it meets wherever that is: a subquery,
  EXISTS, IN, a join, a CTE, through an alias or `"schema"."table"."col"`, and in `col
  IN ($1, $2)` and `BETWEEN`. A bare column takes the type that the statement's tables
  having it agree on, and none if they differ. Both servers typed a one-table statement
  only, so an untyped `false` bound in a subquery stayed the text 'false'.
- A file is converted to the storage format the first time this server opens it
  (ARCHITECTURE.md, "Storage format"), after a backup beside it, `<name>.db.pre-v1.bak`.
  The old server stored a boolean bound as text as that text ('true', 'f'), and a
  timestamp as the client sent it (`.750`, a T, an offset); these become 1 and 0 and the
  canonical text. A database that this server has opened is not for the old server
  again: it would write those forms back. The conversion of a copy of escolarizei and of
  a 5,000,000-row table are timed in the commit that brought it.
- A boolean is a boolean to every comparison, filter, NOT, IS TRUE, CASE, aggregate and
  output; a string compared with a boolean column is read as a boolean (`b = 'false'`,
  and 22P02 for one that is none). A boolean written in any spelling is stored as 1 or
  0, and one no boolean reads is 22P02, as in Postgres: the old server stored the text.
  bool_and, bool_or and every are there.
- A column holding a value its type cannot read is left as it is by the conversion,
  listed in `_overlite_mixed_columns`, and logged with its count. A mixed boolean column
  keeps the readings of both forms, and its `=` searches the index with the forms in
  TRUE_FORMS and FALSE_FORMS (resolve/booleans.rs). Each start tries those columns
  again, so fixing the values is enough: `normalize-booleans` is gone.
- A select-list item whose value is a boolean goes out as one, t or f under OID 16: a
  comparison, a match, IS, IN, LIKE, BETWEEN, EXISTS, AND, OR, NOT, `true`, bool_or and
  the like, and COALESCE, NULLIF, GREATEST, LEAST or CASE over one. SQLite gives an
  integer, which went out as int8 1 and 0, so xorm misread `COALESCE(bool_or(x = 1),
  false)`. GREATEST and LEAST are there, skipping NULLs as Postgres does. parity_diff
  counts the 2056 outcomes this changes.
- `DELETE FROM t USING u, v WHERE ...` runs as a DELETE over the rowids the join finds,
  so a row the join meets twice goes once, as in Postgres. `UPDATE t x SET ... FROM u`
  takes its alias without AS. In both, RETURNING reads the target through its alias,
  and an item reading a joined table is that item over the join, for the row returned.
  Gitea deletes a repository's projects this way; both servers answered 42601.
- Cursors: DECLARE [BINARY] [ASENSITIVE | INSENSITIVE] [[NO] SCROLL] CURSOR [WITH |
  WITHOUT HOLD] FOR a query, FETCH and MOVE in every direction, CLOSE and CLOSE ALL, and
  pg_cursors, with Postgres' codes: 25P01 for a cursor without hold outside a
  transaction, 34000 for one that is not there, 42P03 for a name taken, and 55000 with
  its hint for a NO SCROLL cursor asked to go back (by Postgres' rule: any direction that
  ends up moving forward). A cursor is a portal to Describe, as psycopg's named cursors
  ask, and a FETCH is described with the cursor's columns. A BINARY cursor's rows go out
  in binary over the simple protocol. The statement stays prepared in SQLite and FETCH
  steps it, so a cursor read forward holds one FETCH's rows; going back resets it and
  steps again from the start. A commit reads a held cursor's rows into memory, all of
  them for one that scrolls and those not yet fetched for a NO SCROLL one, as Postgres
  does into a tuplestore, and closes the rest; a rollback closes what it declared, held
  or not. Django's `.iterator()` and dumpdata declare `NO SCROLL CURSOR WITH HOLD` outside
  a transaction, which is read whole at once, as in Postgres. Both servers refused every
  cursor with 0A000.
- Every write stores the canonical form of ARCHITECTURE.md, "Storage format", whatever
  its path, and refuses what the column's type does not read with Postgres' code: a
  uuid, date, time, interval, bytea, array or hstore in the spelling it came in, a
  `WITH ... INSERT`, `SET (a, b) = (...)` and a column's DEFAULT were stored as they
  came (`'{a,b}'` in an array column, `'\xdead'` as text, `'yes'` from a boolean
  DEFAULT), and `'abc'` in a uuid column was taken. DEFAULT in a VALUES list or a SET
  stands for the column's default, its sequence's next value, or NULL; SQLite has no
  such word, and both servers answered 42601. A time, timetz, interval and hstore
  parameter bound in binary is read (the hstore by this server's OID for it). parity_diff
  counts the rows of its corpus that such a write no longer stores.
- Full-text search, as Postgres 17 does it with its `simple` and `english`
  configurations: tsvector and tsquery in Postgres' text form (stored so, ARCHITECTURE.md,
  "Storage format"), to_tsvector, to_tsquery, plainto_tsquery, phraseto_tsquery and
  websearch_to_tsquery, `::regconfig`, `::tsvector` and `::tsquery`, setweight, strip,
  length, `||` of two vectors (positions shifted), `@@` either way round with phrases,
  prefixes, weights and NOT, and ts_rank and ts_rank_cd with a weights array and every
  normalization. The parser is a reading of Postgres' default one by its token types
  (words, hyphenated words and their parts, emails, URLs, hosts, paths, numbers in their
  forms, XML tags and entities it skips); `english` stems with Snowball's English
  stemmer (the rust-stemmers crate) and drops Postgres' 127 English stop words, kept in
  `builtins/fts/english.stop`. Ranks are computed as tsrank.c does, float4 where it
  rounds and double where it carries. Held to a real postgres:17 on 1,190 texts (the
  bakerydemo fixture's strings, HTML and URLs included, and edge cases): to_tsvector in
  both configurations, the four query builders, `@@`, ts_rank and ts_rank_cd over ten
  queries and four weightings, all equal. A GIN index on a tsvector is created as an
  ordinary SQLite index on its text: `@@` reads every row it is asked of. Both servers
  had none of it.
- Answers Django's test suite found wrong without an error, each held to Postgres 17 in
  `tests/wire/django_semantics.rs`: a quoted column named like a type (`"serial"`) kept
  its name; an array literal is read as array_in reads it, element by element, so a
  jsonb array in an E'' string no longer unnests to nothing, and a cast to a type of two
  words (`timestamp with time zone[]`) is an array cast; `'{a}'::text[]` goes out as an
  array; NULLs sort last ascending and first descending; a NULL in a primary key is
  23502; LIKE tells case apart and takes a backslash for its escape (ILIKE stays
  SQLite's ASCII folding); `->`, `#>` and `->>` give jsonb and text as Postgres has them
  and jsonb compares as jsonb, with `?|` and `?&` reading an untyped array literal; date,
  time and interval arithmetic, SUM and AVG of intervals, DATE_TRUNC and the type of
  MIN, MAX, COALESCE, GREATEST, LEAST and the window functions over them; an interval
  goes out as interval (OID 1186, interval_send in binary); EXTRACT is numeric with every
  field; now() is a timestamptz with its fraction; SET CONSTRAINTS IMMEDIATE checks the
  deferred foreign keys; numeric arithmetic, ABS and ROUND are exact, and RANDOM() a
  double in [0, 1).
- `= ANY($1)` and `<> ALL($1)` read an array parameter in Postgres' text form
  (`{a,"b,c"}`), which node-postgres always sends, as well as a JSON array. Both servers
  matched nothing.

- Identity columns: an integer key with no sequence options is SQLite's AUTOINCREMENT
  rowid, as a serial key is; any other identity column gets a sequence of its own,
  `<table>_<column>_seq`, as its nextval default, which DEFAULT VALUES fills too.
  attidentity, information_schema's is_identity and identity_generation, format_type
  (a bigint stays bigint), pg_depend and pg_get_serial_sequence answer as Postgres does.
  ALTER COLUMN ADD GENERATED, DROP IDENTITY [IF EXISTS], SET GENERATED and TYPE work,
  and GENERATED ALWAYS refuses a value with 428C9 unless OVERRIDING SYSTEM VALUE.
- A serial key's sequence is SQLite's sqlite_sequence for its table: nextval, currval,
  setval and pg_get_serial_sequence reach it, and Django's reset after a load,
  `SELECT setval(pg_get_serial_sequence('"t"','id'), coalesce(max("id"), 1), max("id")
  IS NOT null) FROM "t"`, reads its arguments from the rows and gives the next id
  Postgres would. A default on a sequence the column does not own is no serial, as in
  Postgres.
- TRUNCATE takes several tables, ONLY, RESTART or CONTINUE IDENTITY and CASCADE or
  RESTRICT, in a savepoint of its own, so a transaction undoes it. A table another
  one's foreign key points at is 0A000 unless that one goes too or CASCADE empties it.
- An index column's operator class (`varchar_pattern_ops`) is accepted; one that is not
  its type's default is kept for pg_get_indexdef and pg_indexes, which write a
  definition as Postgres does (`CREATE INDEX i ON public.t USING btree (a DESC)`) and
  list the primary key's and UNIQUE constraints' indexes too.
- Django's introspection runs as against Postgres: conkey and confkey are int2[],
  `c.confkey[1]` subscripts a qualified array, ARRAY(SELECT ...) is the array of the
  rows (it was the first row) and goes out, as array_agg does, as an array whose element
  type its values say, `unnest(a, b) WITH ORDINALITY x(k, o, n)` names its columns,
  `'pg_class'::regclass` is 1259, and a RowDescription carries a column's type modifier
  and size. pg_constraint lists CHECK constraints, and a foreign key, UNIQUE or CHECK
  keeps the name it was given, on its column or apart, with a foreign key's DEFERRABLE
  INITIALLY DEFERRED.
- DROP COLUMN takes along the indexes and table constraints that name the column, as
  Postgres does; a key another table's foreign key stands on is 2BP01 unless CASCADE.
  ALTER COLUMN TYPE makes the rows fit the new type (numeric scale, varchar length,
  22001 for a value too long), and a collation goes as in CREATE TABLE: "C", "POSIX",
  ucs_basic and "default" are SQLite's byte order.
- A cast in a column default (`SET DEFAULT '<hex>'::uuid`) is stored as its value, or as
  a CAST in the parentheses SQLite wants.
- `SELECT "t"."nope" FROM "t"` is 42703 `column t.nope does not exist`; a quoted
  qualifier was taken for a missing FROM entry.
- Transactions and locks: BEGIN takes no lock. SQLite's transaction begins at the
  transaction's first statement that writes (BEGIN IMMEDIATE then), which waits for the
  write lock as a Postgres writer waits for a row lock, and after busy_timeout gives up
  with 55P03 `canceling statement due to lock timeout`. The statements before it read
  what is committed, each when it runs, as READ COMMITTED does. Under BEGIN IMMEDIATE
  at BEGIN, a transaction that only read held every other writer off: Django's test
  transaction made a second connection's BEGIN fail with "database is locked". A
  cursor's transaction still takes the lock when the cursor opens. SELECT ... FOR UPDATE
  (NO KEY UPDATE, SHARE, KEY SHARE) in a transaction takes the write lock, a lock on
  every row: it waits, NOWAIT fails at once with 55P03 `could not obtain lock on row in
  relation "t"`, and SKIP LOCKED returns no row while another transaction holds the
  lock. FOR UPDATE OF a, b parses.
- DDL Django's schema editor sends: an array type's sizes (`int[3][3]`) are taken and
  ignored; UNIQUE and PRIMARY KEY take DEFERRABLE / INITIALLY, kept in a comment for
  pg_constraint (the key is still checked at once); CREATE INDEX takes INCLUDE,
  CONCURRENTLY, storage parameters `WITH (...)`, TABLESPACE, NULLS [NOT] DISTINCT and any
  access method, which pg_class.relam and pg_indexes report; ADD COLUMN with UNIQUE,
  PRIMARY KEY, an identity or a stored generated column rebuilds the table; a rebuild
  keeps generated columns, which the catalogs list (attgenerated); VALIDATE CONSTRAINT
  is taken; DROP TABLE takes a list. CREATE/REFRESH/DROP MATERIALIZED VIEW keep the rows
  in a table and the query in a hidden view `_overlite_mv_<name>`; pg_class says 'm',
  pg_matviews lists it. PARTITION BY, PARTITION OF and the foreign-data statements are
  0A000.
- CREATE EXTENSION takes pgcrypto, uuid-ossp, hstore, citext, unaccent, pg_trgm,
  btree_gin, btree_gist and bloom, lists them in pg_extension, and refuses any other
  with 0A000 `extension "x" is not available`. unaccent folds the Latin letters;
  pg_trgm's functions and operators compute pg_trgm's trigrams and formula (held to
  postgres:17's values), `%` being the similarity operator when one side is a string; a
  citext column compares with SQLite's NOCASE.
- Language collations: an ICU name (`sv-x-icu`), a libc locale (`sv_SE.utf8`) and what
  CREATE COLLATION makes are SQLite collations under their own names: the Unicode
  Collation Algorithm's levels in broad lines, the letters a language sorts apart, and a
  nondeterministic `ks-level1`/`ks-level2` collation that holds equal what differs below
  its level. pg_collation lists them and pg_attribute.attcollation a column's.
- Functions added: LEFT, RIGHT, STRPOS, REVERSE, ASCII, CHR, MD5, SHA224 to SHA512,
  pgcrypto's DIGEST, ENCODE, DECODE, LPAD, RPAD, REPEAT, initcap, split_part,
  translate, btrim, to_hex, starts_with, string_to_array, the regexp_ family, COT and
  the degree trigonometry, cbrt, gcd, lcm, factorial, div, width_bucket, the inet
  functions, num_nulls, the statistics aggregates (stddev, variance, corr, covar,
  regr_), bit_and/or/xor, ANY_VALUE, STRING_AGG(DISTINCT x, sep); the operators `#`,
  `^`, `|/`, `||/` and `@`. The server reports version 16.
- EXPLAIN answers a plan in Postgres' shape, from EXPLAIN QUERY PLAN, in TEXT, JSON, XML
  or YAML, with ANALYZE's rows and times.
- DISTINCT ON takes qualified names, positions and output names, and runs in a
  subquery.
- A SERIAL column that is not the key gets `<table>_<column>_seq` as its nextval
  default, NOT NULL, and owns it (`/*overlite owned sequence*/` on the column), which
  pg_get_serial_sequence and pg_depend report; it was a bare INTEGER, whose rows got
  NULL.
- JSON_ARRAY and JSON_OBJECT, the SQL/JSON constructors Django writes for Postgres 16,
  are jsonb_build_array and jsonb_build_object with Postgres' ABSENT/NULL ON NULL
  defaults.
- An unmatched ON CONFLICT target is 42P10, a ProgrammingError to psycopg, where
  SQLite's message went out as XX000.
- A statement the AST path translates is typed from its tree and the catalog
  (docs/adr/0001-sql-ast.md, stage 3), and answers as Postgres 17 does where the text
  passes guessed (tests/wire/ast_types.rs): a date column compared with a timestamp
  column at its midnight, whatever words the statement has; an untyped literal or a
  parameter compared with a date, timestamp or timestamptz read by that type's input
  function (`'2024-01-02'` is midnight, `'2024-01-02T00:00:00'` and `'Jan 2 2024'` are
  what they say), in `=`, `<`, IN and BETWEEN; an interval column compared with a
  literal by its span; an interval times a CASE; AT TIME ZONE of any expression by its
  type; `%` between two text columns as pg_trgm's similarity; a timestamp compared
  with a timestamptz, and date_part of a timestamptz, in the session's zone; ranges
  compared by their bounds, a range in another spelling equal to its canonical one,
  and `-|-`, `<<`, `>>`, `&<`, `&>` taken; arrays compared element by element. Each
  was a silent wrong answer, or an error for the range operators.
- Client-visible, in a statement the AST path translates: an integer result column
  goes out at its width, int2, int4 or int8, by its column's type or its expression's
  (`i + 1`, `length(t)` int4, `count(*)` int8), as Postgres sends it, where every
  integer was int8; a table's rowid key keeps int8 (a serial and a bigserial key are
  both INTEGER PRIMARY KEY in SQLite). pgx scans an int4 into int64, int32 and int as
  from Postgres, and into an interface{} as int32; psycopg reads any width as int.
  AT TIME ZONE is labelled `timezone`, as Postgres labels it (it was its operand's
  name). COALESCE of a double and a numeric is a double, ROUND with digits a numeric
  (ROUND(365, -1) is 370, where SQLite's gave 365.0), stddev and variance of integers
  numerics. The fast path's statements and those left to the text passes keep the
  int8 and the labels they had.
- Since stage 4 (docs/adr/0001-sql-ast.md), a plain statement that names a column of a
  date, time, timestamp, interval, array or range type, or an integer narrower than a
  bigint (or a star over a table with one), leaves the fast path for the typed
  translation: `SELECT id FROM t WHERE tz > '2024-01-02'` reads the literal as a
  timestamptz where it compared texts, and `SELECT * FROM t` sends an int column as
  int4, the type Django's inspectdb reads for an IntegerField (a serial key, SQLite's
  rowid, stays int8). A lookup over other columns keeps the fast
  path.
- Answers that were wrong or refused and are Postgres' now, in statements the AST path
  translates since stage 4 (tests/wire/ast_shapes.rs): a subscript of an expression
  `(a)[2]` and a slice `a[2:3]`; `a || b` of arrays and of an array and an element,
  which concatenated their texts; `<> ALL (subquery)`; `ARRAY((subquery))`;
  array_agg, a JSON array as arrays are stored (array_length, subscripts and `= ANY`
  read it; an element with a space is quoted), with ORDER BY, FILTER and OVER, NULL
  over no rows; DISTINCT ON keeps a row per group before LIMIT counts them;
  `a UNION b INTERSECT c` binds INTERSECT first; a jsonb aggregate's FILTER; a
  row-locking clause on lines of its own; `"ctid"` quoted; real times numeric a
  double.
- Client-visible since stage 4, in a statement the AST path translates: avg of
  integers is a numeric at Postgres' scale (`2.0000000000000000`, where it was the
  double 2) and sum of a bigint a numeric, as Postgres types them; an int[] goes out
  as int4[] (1007) with four-byte elements in binary, where it was int8[]; array_agg,
  a slice, `||` of arrays, akeys and avals go out as arrays of their element's type;
  a comparison or a test in a select list (`a = b`, `isempty(r)`) goes out as a
  boolean where it could go out as the integer 1 or 0; a subscript is labelled by its
  array (`tags[2]` is `tags`, as Postgres names it), a DISTINCT ON item without a name
  by Postgres' label. A cast of a JSON array's text to an array type
  (`CAST('["a"]' AS TEXT[])`) reads the array, which the text passes left as text.
- Since stage 5a (docs/adr/0001-sql-ast.md), the catalog's reads (psql's `\d` family,
  pg_dump, the ORMs' introspection) are translated from the tree into the same SQL
  over the same views, and so are the row shapes stage 4 left. Answers that were wrong
  or refused and are Postgres' now (tests/wire/ast_catalog.rs): psql's `\d` of a table
  with a foreign key (its trigger listing reads `pg_partition_ancestors(...) WITH
  ORDINALITY`, which reached SQLite as a syntax error); an array cast to text
  is Postgres' text form (`ARRAY['a b','c']::text` is `{"a b",c}`, where it was the
  JSON array it is stored as); a slice of any expression (`(string_to_array(s,
  ';'))[2:3]`, `(array_agg(x))[2:2]`); jsonb_typeof and json_typeof say `number`,
  `string` and `boolean` (they said SQLite's `integer`, `real`, `text`, `true`);
  tsquery's `&&`, `||` and `!!` join and negate queries (they were an overlap, a
  concatenation of texts and a syntax error); a reg* literal folds an unquoted name
  and reads past a schema (`'MyTab'::regclass` finds `mytab`, `'public."My T"'`
  finds `My T`); a star over a table the catalog knows takes a shorter derived column
  list, its first columns renamed (it was 0A000).
- Client-visible since stage 5a: the bitwise operators of integers go out at their
  operands' width (`5 & 3` int4, a shift of a bigint int8; they were int8); a CASE
  whose ELSE is a cast or a call is labelled by it, as Postgres' FigureColname does
  (psql's `\d` reads its columns by position). A Parse holding several statements is
  translated statement by statement, as before.
- Since stage 5b (docs/adr/0001-sql-ast.md), DDL is translated from the tree (CREATE
  TABLE, CREATE INDEX, ALTER TABLE, DROP, views) and the session's statements are
  routed by their node; a table's stored DDL stays as the client wrote it but where
  SQLite needs it otherwise. Answers that were wrong or refused and are Postgres' now
  (tests/wire/ast_ddl.rs, tests/wire/classify.rs): `SET LOCAL ROLE r` and `SET
  SESSION ROLE r` set the role (they were a SET of nothing: what followed ran as the
  session's role, past grants and row-level security), LOCAL until the transaction
  ends, and a rollback undoes a SET ROLE or SESSION AUTHORIZATION; SET LOCAL ROLE
  outside a transaction warns (25P01) and changes nothing; `id serial NOT NULL
  PRIMARY KEY` (Django's and Wagtail's first migrations) is the rowid with
  AUTOINCREMENT and its own sequence, as `serial PRIMARY KEY` was (an INSERT without
  the id was 23502); a DEFAULT that is an expression but no call (`1 + 2`, `'x' ||
  'y'`) is taken (a syntax error); a column called tablespace; a TEMP table's
  numeric keeps its scale and a citext compares without case; CREATE UNLOGGED TABLE
  makes a table (a syntax error); a numeric column added by ALTER TABLE through a
  rebuild keeps its scale.
- Client-visible since stage 5b: a DEFAULT, a CHECK, a generated column's expression
  and an index's key or WHERE are stored as the printer writes them where that is not
  the client's SQL token for token (`CAST(x AS integer)` where the text passes kept
  `CAST(x AS int)`, a name's needless quotes dropped); pg_get_constraintdef, column
  defaults and pg_get_indexdef show that text. A CHECK holding a cast to numeric is
  a CAST to numeric (the text passes wrote the column type DECIMALTEXT into it). A
  function's DDL is stored as written (the text passes added NULLS LAST after a body
  ending in ORDER BY).
- Since stage 5c (docs/adr/0001-sql-ast.md) there are no text passes: a statement is
  translated from its tree, refused, or (SQLite's trigger) run as written. A
  statement Postgres' grammar refuses is answered as Postgres answers it, before any
  classifier or grant reads it, an open transaction's abort included: its message
  (`syntax error at or near "SELEC"`, `unterminated quoted string at or near "'abc"`),
  the position the ErrorResponse's P field gives, its HINT where Postgres has one
  (`LIMIT #,# syntax is not supported`), its code (42601, or the grammar's own:
  `CREATE ROLE public` is 42939), where SQLite's message came before (`near "b":
  syntax error`). A statement nested past 1000 messages of the tree (subqueries some 165
  deep, a sum of some 500 terms) is 54001 `stack depth limit exceeded`; a UNION of 200
  members, which the decoder's recursion limit refused past 99, runs. A node the
  translation has no form for is 0A000 `<what> is not supported`
  (tests/wire/ast_refusals.rs).
- Client-visible since stage 5c: SQLite's own dialect is refused as Postgres refuses
  it, the owner's decision: `INTEGER PRIMARY KEY AUTOINCREMENT` (use serial or
  GENERATED ... AS IDENTITY), EXPLAIN QUERY PLAN (EXPLAIN answers in Postgres' shape),
  `LIMIT a, b`, PRAGMA, ATTACH, VACUUM INTO, INSERT OR IGNORE/REPLACE, DROP TRIGGER
  without ON, `current_user()`, `PREPARE p AS <DDL>`; each ran on SQLite before. SQLite's
  `CREATE TRIGGER ... BEGIN ... END` is kept, run as written without translation: it
  is the only trigger while PL/pgSQL does not run; its head (timing, events, table,
  FOR EACH, WHEN) must read as a Postgres trigger's, so SQLite's TEMP, IF NOT EXISTS
  and a WHEN without parentheses are taken, a trigger without BEFORE or AFTER is not.
  Postgres' `CREATE TRIGGER ... EXECUTE FUNCTION` is 0A000 as before.
- Since stage 5c the session's handlers read their statement's tree (roles, GRANT and
  REVOKE, owners, RLS, schemas, sequences, types, databases, COMMENT, COPY, TimeZone,
  TRUNCATE, extensions, collations, materialized views, CREATE TABLE's serial and
  identity columns, ALTER TABLE): a name is folded as Postgres folds it unless quoted
  (a GRANT's table, a LISTEN channel, a PREPARE name, an enum type, a sequence; enum
  lookups compare without case, so older files keep working). Answers that were wrong
  and are Postgres' now (tests/wire/privilege_trees.rs, handler_trees.rs): a role
  creating `"secret copy"` was recorded as the owner of `secret`, past every grant on
  it; `GRANT ... TO CURRENT_USER` and `CREATE POLICY ... TO CURRENT_USER` name the
  current role (the second matched none); a policy named with a space was a syntax
  error; `ALTER TABLE t ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY` did only the
  last action; `LISTEN Foo` did not receive `NOTIFY FOO`; `EXECUTE BYID` did not find
  `PREPARE byId`; `COMMENT ... IS ''` stored an empty comment instead of dropping it;
  `ALTER SEQUENCE IF EXISTS s` altered a sequence named "if", and an ALTER SEQUENCE that
  did not name CYCLE turned it off; the serial and identity columns of a table named
  with a space lost their default; a DEFAULT dropped was cut at a space inside a
  literal; `ALTER COLUMN "d e"` read the name up to the space; `ALTER ... TYPE citext`
  compares without case; ALTER TYPE of a name that is only white space is 42704 (it
  succeeded); COPY's `NULL 'csv header'` set no CSV or HEADER option.
- Since stage 5c, also from the tree: hstore's `->` gives a value as text and of an
  array of keys their values, `?|`, `?&`, `@>` and `<@` read an hstore column as one;
  hstore's `->>`, which hstore has not, is 42883 (it read the stored JSON); a json
  operator over an array (`a -> 0` of an int[]) is 42883 `operator does not exist:
  integer[] -> integer` with its position (it read the array's JSON); AT TIME ZONE of
  a date is of its midnight in the session's zone, a timestamp (as Postgres casts
  it); a derived column list longer than its relation's columns is 42P10 with
  Postgres' message; a boolean column holding text is read as before (every spelling
  of a truth value), now by the printer. A json, range or `%` operator over operands
  of known types Postgres has no such operator for is 42883 (it reached SQLite).

## Limits

Intentional, each for a case not worth more code. Grouped by what a client sees, the
silent ones first: those answer without an error, so nothing tells a client it got
something other than what Postgres gives.

Since stage 3 of the AST migration (docs/adr/0001-sql-ast.md), a statement the AST
path translates is typed from its tree and the catalog, and since stage 5c every
statement is (the fast path runs as written only a statement over columns whose types
the translation reads the same, and SQLite's trigger runs as written): the limits
below that come from a type the translation cannot see hold where the catalog does
not know a column. In a typed
statement what is still not known is a column of a table the catalog does not have,
a function the environment does not type; a json, range, array or `%` operator over
such a value is read as the text passes read it.

### Silent: a different answer and no error

- A timestamp with an offset is read as a time where a timestamp column receives or
  meets it: in a write, a comparison with a column (`=`, `<`, on either side), a cast,
  and a default. An expression's type is not known, so anything else keeps the text:
  `tz IN ('...+02')`, `tz BETWEEN ...` and `lower(s) < '...+02'` compare as text.
- AT TIME ZONE goes by the declared type of a column, through aliases and subqueries,
  and of a cast. An operand that is any other expression, but for the `+ interval` of a
  column, is taken for an instant: nothing here knows its type.
- The session's TimeZone (SET TIME ZONE, SET timezone, the startup parameter) is kept
  per session: a timestamptz goes out in it, and one a client gives without an offset (a
  literal, a parameter, a date, a tstzrange bound) is read in it. A timestamptz is stored
  as bare UTC, so what tells one read from a column apart from text a client gives is
  the statement's typing (resolve/temporal.rs): an expression of a timestamptz the typing
  does not see through (a CASE over one, a subquery's in a write) is read as a wall
  clock of the zone, in a session whose zone is not UTC. A time zone given as a number of
  hours or an INTERVAL is 0A000. Binary timestamptz is UTC microseconds either way.
- now(), transaction_timestamp() and CURRENT_TIMESTAMP hold the start of an explicit
  transaction (BEGIN), to the microsecond; outside one they are SQLite's clock for the
  statement, to the millisecond, and so is statement_timestamp(). The implicit
  transaction of a multi-statement Query message does not hold them.
- A date compared with a timestamp, and an interval compared with an interval, are
  typed by the engine's pass only in a statement that says `date`, `interval` or ORDER
  BY somewhere or does date arithmetic: a point lookup pays no probe of its columns.
  Elsewhere, two bare columns of those types (`d < tz`) compare as their text (silent),
  outside the AST path's typed statements.
- An interval compares by its span (a month as 30 days) where the statement types a side
  as an interval (a column, a cast, a sum or product), and an interval ORDER BY key by
  the INTERVAL collation; an interval only a literal says (`'1 mon' > '29 days'`) compares
  as text. interval * and / number follow interval_mul.
- `+` and `-` over dates, times and intervals know a side's type from its column, its
  cast or the sum it is, and otherwise from its value; a parameter or literal that could
  be a time or an interval (`'01:00:00'`) is a time beside a time and an interval
  otherwise. A month is added as Postgres adds it, keeping the day in the month.
- A numeric that is not a column is written as `(CAST(overlite_numeric(e) AS
  DECIMALTEXT) COLLATE DECIMAL)` (resolve/numeric.rs), which compares as a number with a
  number and exactly with another numeric. What makes an expression a numeric is the
  statement's typing: a cast, a numeric column under SUM, MIN, MAX, COALESCE, GREATEST,
  LEAST, ABS, ROUND, a CASE, or arithmetic with a numeric side. A numeric that comes out
  of anything else (a function this typing does not know, a subquery in a comparison)
  is still decimal text, and compares with a number by SQLite's type order. Arithmetic is
  exact with Postgres' scale in every clause; a quotient's scale is select_div_scale's. A
  float literal in ROUND is SQLite's double, so `ROUND(1.255, 2)` is 1.25 where
  Postgres' numeric gives 1.26, and ROUND of a double rounds half away from zero, where
  Postgres' rounds half to even.
- A casts' numeric literal is a real to SQLite, so `2.5::int` is 3 as numeric rounds, and
  `2.5::float8::int` is 3 too, where Postgres gives 2. jsonb keeps its text as written,
  where Postgres normalizes spaces and key order.
- `char(n)` and `name` casts and `'x'::char(4) || 'y'` keep their padding in a
  concatenation, where Postgres drops it for text.
- A column's modifier binds what a statement writes: `numeric(p,s)` rounds and answers
  22003, `varchar(n)` and `char(n)` answer 22001, arrays element by element, through
  `overlite_assign` on every write path. It is enforced on write only: no stored row is
  rewritten, so a row from before keeps what it holds, past the modifier or not, until
  it is written. `char(n)` stores no padding, so `length()` of one differs from
  Postgres; its comparisons do not.
- jsonb compares and sorts as jsonb where `->`, `#>` or a cast to jsonb gives it (the
  JSONB collation); a jsonb column compared with a string literal is still text to text.
- A statement reading a boolean column that may hold text (a mixed one) is left to the
  text passes, whose readings follow. In a mixed boolean column, a boolean is known by name: a qualified one by the table
  its qualifier names, a bare one when every table of the statement that has a column so
  named declares it boolean, and not where the statement labels something else with that
  name. A column renamed on the way (`WITH x AS (SELECT b AS flag ...)`) is not followed,
  so a stored 'true' read through `flag` reads as SQLite reads it. `col = value` there
  searches the index with the forms in TRUE_FORMS and FALSE_FORMS: 1 or 0, and t, true,
  y, yes, on (f, false, n, no, off) in lower case, upper case and capitalized. Over such
  a column, `SELECT DISTINCT b` and UNION see the stored values, so 'true' and 1 make two
  rows.
- A JSON built from a boolean column holds 1 or 0 where Postgres has true or false.
  bool_and and bool_or take no FILTER or OVER.
- The value of a lateral function's column reads through its alias, so `SELECT *` over
  one labels it `value`, where Postgres gives the name of the alias list.
- In a DELETE ... USING or UPDATE ... FROM, `RETURNING *` lists the target's columns
  alone, where Postgres adds every joined table's: SQLite's RETURNING sees the target
  only. Name the joined columns to have them.
- `SET (a, b) = (SELECT ...)` stores what the subquery gives as it comes: only a row of
  values is fitted to its columns. An array given as JSON, as this server writes an
  `ARRAY[...]`, is taken as an array, where Postgres reads `'[1]'` as a malformed one.
- A cursor that says neither SCROLL nor NO SCROLL scrolls, and pg_cursors says so,
  where Postgres decides by the plan (`SELECT 1` does not). A held cursor's rows are in
  the session's memory, where Postgres spills a large tuplestore to disk. ROLLBACK TO a
  savepoint leaves a cursor declared after it open. A cursor reads through its
  transaction, so a write of the same session after the DECLARE may be seen by a later
  FETCH, where Postgres reads the snapshot of the DECLARE; going back re-reads the
  query, so it sees those writes too.
- A transaction that has not yet written reads each statement's own snapshot, as READ
  COMMITTED does; REPEATABLE READ and SERIALIZABLE are not given a snapshot of their
  own until the first write. Every row lock is the database's write lock: FOR UPDATE of
  one row waits for any writer, and SKIP LOCKED returns nothing while another
  transaction holds the lock.
- Full-text search knows the `simple` and `english` configurations only, and a call
  without one takes `english`: `SET default_text_search_config` changes nothing. There
  is no ts_headline, ts_debug or dictionary of one's own. tsvector, tsquery and the ranks
  go out as text and float8, where Postgres says tsvector, tsquery and float4: a rank is
  the double of the decimal Postgres prints, so arithmetic over it differs past
  float4's digits. Two stored vectors that SQLite's own `||` ran together (an operand
  that is not plainly a tsvector) are read apart where one's positions end and the
  next lexeme starts, which a left vector without positions does not show. `text @@
  tsquery` reads the text as a tsvector, not through to_tsvector.
- A key identity or serial column is a rowid: its next id is past the highest row, so
  an explicit id moves it on, where Postgres' sequence would not see the id, and ADD
  GENERATED goes on past the rows there are, where Postgres starts at 1. OVERRIDING USER
  VALUE is 0A000, and an UPDATE of a GENERATED ALWAYS column is not refused.
- TRUNCATE is a DELETE, so a table's ON DELETE triggers fire, and RESTART IDENTITY
  restarts the sequence of every nextval default of the table, owned or not.
- ROLLBACK TO a savepoint does not undo a SET ROLE or SET LOCAL ROLE made after it;
  the end of the transaction does (a SET ROLE is undone by a rollback, SET LOCAL at
  either end, as in Postgres). `set_config('role', ...)` changes no role.
- A role's name compares without case, as `_overlite_roles.rolname` was created NOCASE
  and existing databases keep the table: `"Mixed"` and `mixed` are one role, where
  Postgres has two. Rebuilding the table at startup would rewrite stored data.

### What Django's suite found wrong and is not done yet

Each answers without an error unless marked so; found with Django's own tests and held
to Postgres 17's answers. "Silent" means a wrong answer, not an error.

- Arrays (silent): ARRAY_AGG goes out as text (OID 25) with FILTER, a window or ORDER BY,
  timestamptz[] and range[] elements go out untyped, `<` and `>` of arrays compare the
  JSON text outside a typed statement (ARRAY_AGG is not in the AST slice yet), `&&` in HAVING over ARRAY_AGG finds nothing, and JSONB_AGG over no rows is
  `[]` where Postgres gives NULL.
- Ranges (silent): a range goes out as text (OID 25) with no binary form; lower() and
  upper() of a daterange or tsrange are text; outside a typed statement, `<`, `>` and `=`
  of ranges compare text, `@>` between two casts is a syntax error (an error) and `-|-`,
  `<<`, `>>`, `&<`, `&>` as operators are a syntax error (an error); a range stored before canonical ranges keeps
  its text (`[1,5]` beside `[1,6)`).
- Full-text search (silent): tsquery `&&` and `||` combine into a wrong query; `!!` is a
  syntax error.
- JSONB (silent): a key with a backslash through E'' finds nothing, `?&` with keys that
  look like numbers matches every row, DISTINCT ON over a jsonb expression orders and
  returns it as text.
- DISTINCT ON with LIMIT and OFFSET returns no row (silent).
- ADD COLUMN bytea DEFAULT '\x..'::bytea fills existing rows with the wrong bytes
  (silent).
- STRICT_WORD_SIMILARITY and `<<<->` are float8, where Postgres gives real (silent in
  the digits past float4's).
- Catalogs (silent): smallserial reads as integer, inet as text, a column's COLLATE "C"
  is not in pg_attribute, information_schema.columns.column_default of a serial is NULL,
  an index's reloptions are NULL, result-column OIDs after ADD COLUMN are int8, the
  sequence list misses some.
- In a session whose TimeZone is not UTC: DATE_TRUNC and EXTRACT of a timestamptz the
  typing does not see (see above) go by UTC (silent).
- A result column labelled by a cast inside a comparison (`23.09::numeric > 40` is
  labelled numeric, Postgres says ?column?) and `CREATE TABLE AS SELECT now()`, which
  stores datetime('now'), are other classes seen on the way, not chased.

### Accepted where Postgres refuses

- A statement SQLite takes and Postgres refuses stays accepted, where nothing here
  types the operands: `SELECT 1/0` is NULL, `1 + 'a'` is 1, a column may hold what its
  type does not (`'abc'` in an int, `WHERE id = 'a'`), a missing column in a view or a
  PREPARE is only found when it runs, and an unknown type in CREATE TABLE is taken. The
  table in `tests/wire/sqlstate.rs` holds the statements whose code is right.
- An `integer PRIMARY KEY` not declared NOT NULL that an INSERT leaves out still takes
  the next rowid, as clients written for SQLite expect, where Postgres refuses the NULL.
  `id serial, PRIMARY KEY (id)` is such a key, so it takes one too; a NULL given to any
  key is 23502.
- COMMENT ON a kind with no catalog here (extension, cast, operator...) is accepted
  without looking it up, so a dump restores.
- A nondeterministic collation compares, but LIKE under one is not refused as Postgres
  refuses it; the collation order is the Unicode Collation Algorithm's in broad lines,
  not ICU's tables, and held to Postgres only for Latin-script letters.
- CREATE INDEX ... USING gin, gist, spgist, brin or bloom makes SQLite's btree index:
  the method is reported, not provided. A refreshed materialized view is emptied and
  refilled; REFRESH ... CONCURRENTLY does the same. A materialized view made WITH NO
  DATA reads as empty where Postgres refuses to read it.

### Refused or failing where Postgres answers

- A table's column list, `FROM t AS x(a, b)`, is a syntax error: only a derived table
  and a function take one. A derived table's list shorter than its columns is 0A000
  when a star over a relation the catalog does not know hides how many there are.
- A schema name holds letters, digits and underscores: a table of schema `s` is stored
  as `s.t`, so the name has to be one word. Any other name is 0A000, where Postgres
  takes it.
- A set-returning function builds its rows at once, as a JSON array that the tables of
  `builtins/srf.rs` walk, so a call's memory is its result. A record from `json_each`
  in a select list is 0A000, as a row has no column type to go in. `generate_series`
  in a select list is held to ten million rows, and takes numbers.
- An EXCLUDE constraint is two triggers: it holds the operators `=`, `<>`, `&&` and
  `-|-` (any other is 0A000), and a DEFERRABLE one is checked as each row is written. It
  has no index, so pg_index and pg_indexes do not list one, and each row written scans
  the table.
- SET CONSTRAINTS checks deferred foreign keys: a UNIQUE is always checked at once, as
  SQLite has no deferred one. The constraints a name list makes IMMEDIATE are checked
  then, but only ALL holds the statements after it to them.
- A deferrable UNIQUE or PRIMARY KEY is recorded and reported but checked at each
  statement.
- ALTER DATABASE RENAME cannot be rolled back, so it is 0A000 in a transaction. A
  database name is held to 63 bytes with 42622, where Postgres truncates with a notice.
- An operator class is known by its `_ops` suffix; one with parameters is not taken.
- PL/pgSQL is not run: CREATE FUNCTION ... LANGUAGE plpgsql is still taken without
  effect, and a call to one fails.
- A `LANGUAGE sql` body the privilege check cannot read (more than one statement, or
  one Postgres' grammar refuses) is 0A000 to a caller that does not own the function,
  where Postgres runs it; calls nested deeper than 16 are 54001. A function named as a
  built-in or a keyword is stored and never called, and so is a body inside a view,
  trigger or column default: those are inlined when the view, trigger or table is
  made, as before. A function called in a view so runs with the view owner's
  privileges, where Postgres runs it with the querying role's. EXPLAIN without ANALYZE
  is held to the bodies of the functions it would call, where Postgres plans a call
  without reading its body. has_function_privilege answers true.
- A view whose owner the policies of a table it reads would filter is refused, 0A000,
  to a role that is checked: SQLite expands the view where no filter can be added. A
  superuser (or an unmanaged role) reading a view owned by a role without the
  privileges it needs is not refused, where Postgres refuses it: a superuser is never
  checked. A view or function from before owners were kept has none, and is an
  administrator's as a table with no owner row is: its query is not checked.
- There is no migration of `_overlite_owners` and `_overlite_grants` to stored names.
  In a database from before them, a row for a table outside public holds its bare
  name, and a bare name now means the public table of that name. So that row stops
  counting: the table has no owner, and its old grants stop applying. It is then admin
  only (a superuser, or a role unknown to `_overlite_roles`, which bypasses all
  checks), never open to everyone, until an admin grants on it or fixes the row:
  `UPDATE _overlite_owners SET tablename = 's1.t' WHERE tablename = 't'`. If a public
  table of that name exists, or is later created by `CREATE TABLE IF NOT EXISTS`, the
  old row applies to it, since nothing in the row tells the two apart.
- A server before column privileges stored `GRANT SELECT (a) ON t TO carol` as a grant
  of SELECT on all of t, and carol can read every column of it still. Those rows are
  kept as they are: a row in `_overlite_grants` holds a grantee, a table and a
  privilege, and one written for a column GRANT is the same row a table GRANT writes,
  so narrowing them would take away grants made on purpose. An admin reviews them with
  `SELECT grantee, tablename, privilege FROM _overlite_grants ORDER BY 2, 1` (or
  information_schema.column_privileges, where they show on every column), and turns
  one meant for columns into what was meant with `REVOKE SELECT ON t FROM carol;
  GRANT SELECT (a) ON t TO carol`. One trace may help: the old reader split the column
  list at its commas, so `SELECT (a, b)` also left a row whose privilege is `B)`; a
  privilege ending in `)` marks a table whose SELECT (or other) row for that grantee
  came from a column GRANT. Such a row grants nothing. The other way round, a server
  from before column privileges reading a newer database finds no column grants at all
  (it never reads `_overlite_column_grants`), so those roles lose access rather than
  gain it.
- REFERENCES, as a column or a table privilege, is recorded and reported but not
  enforced: no privilege is checked when a foreign key is created. WITH GRANT OPTION is
  not kept, on columns as on tables. DROP ROLE leaves the role's grants in place, where
  Postgres refuses it while they exist, so a role created again by that name has them.
- On a filesystem that does not tell case apart (macOS's default APFS), two databases
  whose names differ only in case cannot both exist: each is the file `<name>.db`, so
  creating or renaming to the second is 42P04, where Postgres takes both. A name is
  still found only by its exact case.

### Catalogs and what is reported

- An integer column goes out as int8 whatever its width, as its values are sampled, in
  a statement the text passes run. A statement the AST path types sends the width, but
  for a rowid key, int8 there too, which Django's inspectdb reads as a BigAutoField. The catalogs have the declared width.
- Postgres' sum of a bigint and avg of integers are numerics; in a statement the text
  passes run they are an int8 and a double, as SQLite computes them.
- A real is kept as a double: `r * 1.5` of a real 0.1 is 0.15000000000000002, where
  Postgres widens the real's 0.10000000149011612.
- Two UNIQUE constraints over the same key are both in pg_constraint, but SQLite makes
  one index for them, so pg_index lists one.
- A renamed table's key, UNIQUE and sequence names follow its new name, where Postgres
  keeps the old ones (`shop_feedback_pkey` on `shop_opinion`).
- pg_get_serial_sequence takes literals, and setval reads its arguments from rows only in
  `SELECT setval(...) FROM ...` with no other item and no parameter.
- EXPLAIN's costs and estimated rows are 0: SQLite estimates none.
- pg_attribute.attacl, information_schema.column_privileges, has_table_privilege,
  has_column_privilege and has_any_column_privilege answer from the grants, so psql's
  `\dp` lists column privileges as Postgres prints them. pg_class.relacl is still NULL:
  `\dp` shows no table privileges, and column_privileges shows them only through their
  columns. A table named in has_*_privilege without its schema is the public one, not
  the search_path's.

### Messages

- SQLite's words stay in the messages of its errors (`SQL logic error: no such table: x
  (1)`): the SQLSTATE is Postgres', the text is not. The errors of this server's own
  paths use Postgres' words.
- Word-anchor regexes (`\m \M \y \Y`) run on fancy-regex, whose parser differs from
  the old server's engine. For malformed patterns that also use an anchor the common
  errors match, rarer ones keep fancy-regex's wording, and a forward reference
  (`\1(a)`) is refused where the old engine took it. A pattern fancy-regex hands to its
  linear engine answers where the old engine would time out; one that backtracks times
  out at 5s, but its thread cannot be stopped, so it runs on until
  ABANDONED_MATCH_LIMIT.
- A regex nested past 1000 groups is refused with the regex crate's wording; the
  old server said `expression nests too deeply`, and the exact depth of the limit may
  differ.
- A regex error quoting a byte outside UTF-8 shows it as `\U0010ffff`-style text:
  rusqlite takes a function's error as a String, so it reaches SQLite re-encoded.
  Only an error message that is itself about such a byte is affected.
- A COPY CSV parse error counts columns in escaped text, so its column number is
  high when bad bytes precede it. The message only; the row is still refused.
- `GRANT` is parsed with a regex that stops at a byte outside UTF-8 differently from
  the old server's, so the role named in its "does not exist" error differs. No data
  is involved.
- pg_hba YAML `!!binary` scalars keep their base64 text: serde_yaml hides tags
  from a string visitor, and a base64 auth method or role name is not a real
  config. A file's decode errors are reported first only, not all together.
- A code point U+10FF80..U+10FFFF built inside the server rather than received
  would read as an escaped byte. JSON decoding handles it; nothing else makes them.

### Not carried over

- The old server's manual pgdescribe harness was not kept; it was not part of the
  server.
