# Architecture

overlite speaks the PostgreSQL wire protocol on the front and stores everything in
SQLite files on the back. There is no planner and no SQL parser: a statement is
classified by looking at its text, translated by text rewrites, and run by SQLite.

## Crates

Dependencies point down this list, and only down.

    overlite (root)     the binary: flags, signals, the log backend
    overlite-postgres   the protocol, sessions, auth, SQL translation, the accept loop
    overlite-engine     SQLite: connections, emulated catalog, functions, name resolution
    overlite-hba        pg_hba rules: parsing and matching
    overlite-core       values, the error type, text helpers

`overlite-hba` and `overlite-engine` do not see each other. Only the engine sees
rusqlite; the postgres crate reaches SQLite through `Sqlite` and `DbSession`, and takes
`InterruptHandle` from the engine.

### overlite-core

`Value`, `Column`, `ResultSet`, `Oid`, `SqlState`, `text`, and `sync`, which states the
policy on poisoned locks. `text` holds the escaping of bytes outside UTF-8 (below), the quoting of error messages, and
`lazy_regex!`, whose patterns read `\b \w \d \s` as ASCII.

### overlite-hba

`load` reads pg_hba.conf, or pg_hba.yaml when that is absent, into a `Policy`, and
`Policy::method` answers a `Conn`.

### overlite-engine

- `sqlite`: `Sqlite`, one per database, and `DbSession`, one per client, each owning a
  SQLite connection. Statement execution, binding, and the decoding of values.
- `blank`: `blank_literals`, `code_only` and `blank_comments`, the literal-blanking
  rule (below).
- `column_types`: the type of a computed result column, read from the select list.
- `cluster`: the `Cluster` trait, the set of databases of a process. `Dir` serves every
  `<name>.db` in a directory, opening on demand, and finds a database by its file's exact
  name, also where the filesystem does not tell case apart; `Single` serves one engine, as for
  `:memory:`.
- `db_state`: what connections of one database share: its name, oid band, schema
  list, and the RLS cache.
- `catalog`: the SQLite functions registered on each connection, `information_schema`
  and `pg_catalog` as temp views, and the `_overlite_*` tables holding what SQLite has
  nowhere to keep: roles, grants, owners, RLS, sequences, enums, comments.
- `builtins`: Rust implementations of what Postgres has and SQLite lacks: dates and
  zones, exact decimals, JSON, hstore, ranges, arrays, regular expressions, `pgfuncs` (the
  string, hash, math, network and statistics functions), `contrib` (unaccent and
  pg_trgm), `collation` (language collations), `casts` (the input
  function of each type, behind every cast but one to text), `srf` (the sets, each
  a JSON array walked by the eponymous tables `overlite_elements` and `overlite_pairs`),
  `temporal` (date, time and interval arithmetic and EXTRACT), `like` (the GLOB and LIKE
  patterns LIKE and ILIKE become), `canonical` (an array's and an hstore's stored form)
  and `fts` (full-text search:
  Postgres' default parser, the `simple` and `english` configurations, tsvector and
  tsquery in their text form, matching, ts_rank and ts_rank_cd).
- `resolve`: per-statement name resolution, for the answers that depend on the
  session: schema qualification, search_path, `current_schema()`,
  `current_database()`, `to_regclass()`, and `time_zone`, which prepares a probe over a
  statement's FROM to read a column's declared type: which way an AT TIME ZONE goes, and
  whether a timestamp with an offset is a time or a text. `names` reads the tables of a
  statement with their aliases, at any depth, and the column each parameter meets;
  `booleans` reads a boolean column stored as text as it reads one stored as 1 or 0;
  `temporal` writes a `+` or `-` with a side of a date or time type as overlite_tadd or
  overlite_tsub and a `*` or `/` as overlite_tmul, naming each side's type that the
  statement gives (a column's declared type by the same probe, or for an UPDATE's or an
  INSERT ... SELECT's columns their table's, a cast, a sum, a scalar subquery's item),
  a sum or product with a numeric side as overlite_numeric_op, exact at Postgres' scale,
  in every clause of a statement that reads or writes rows, and casts a select-list
  item whose type only Postgres knows (MIN, MAX, COALESCE, a window function,
  DATE_TRUNC over a date column, the sum of intervals, a scalar subquery) to that type,
  so the protocol sends it as one. It compares an interval with an interval by span
  (overlite_interval_cmp, and the INTERVAL collation for an ORDER BY key) and a date
  with a timestamp at its midnight, and in a session whose TimeZone is not UTC reads a
  timestamptz in that zone (below). `numeric` (inside `temporal`) writes each numeric
  expression that is not a column as `(CAST(overlite_numeric(e) AS DECIMALTEXT)
  COLLATE DECIMAL)`, the affinity and collation a numeric column has, so that it
  compares as a number;
  `tsvector` makes `length(col)` and `col || col` of a tsvector column count its
  lexemes and concatenate, by the same probe.
- `sql_funcs`: `LANGUAGE sql` functions, stored and inlined at their call sites.
- `storage`: the storage format below, and the conversion of a file that holds
  another, once, when it is opened.
- `deferred`: SET CONSTRAINTS, which checks the deferred foreign keys SQLite still has
  pending (SQLITE_DBSTATUS_DEFERRED_FKS) when they become IMMEDIATE.
- `cursor`: a query read a few rows at a time, the statement under a cursor: prepared
  once, stepped as FETCH asks, reset to go back. Each connection has a temp table
  `pg_cursors` the session keeps in step with the cursors it holds.

### overlite-postgres

- `server`: the accept loop, one OS thread per client, and `drain`.
- `protocol`: `Protocol`, startup, authentication, and the route of a simple query.
- `wire`: framing and message builders, TLS, the SQLSTATE of an error.
- `session`: `Session`, the extended protocol, and what lasts across statements:
  multi-statement batches, COPY, prepared statements, search_path, LISTEN/NOTIFY,
  advisory locks, cancellation, EXPLAIN (`explain`: a plan in Postgres' shape from
  SQLite's), and cursors (`cursor`: DECLARE, FETCH, MOVE, CLOSE,
  routed before anything else reads the statement, and the end of a transaction,
  which reads a held cursor's rows and closes the others).
- `auth`: SCRAM, roles, grants, privilege checks, row-level security, session identity.
- `ddl`: what SQLite cannot run as written: `exclude` (EXCLUDE constraints, as two
  triggers per constraint whose first carries the constraint in a comment), `objects` (materialized views,
  CREATE/DROP EXTENSION and COLLATION, and the refusals of partitions and foreign
  data), ALTER TABLE by table rebuild, DROP
  CONSTRAINT and DROP COLUMN with what stands on the column, sequences (a serial key's
  is SQLite's sqlite_sequence), identity columns, TRUNCATE, enums, COMMENT,
  CREATE/DROP/ALTER DATABASE, nextval defaults, column IF [NOT] EXISTS
  (`column_if_exists`, with `alter_actions` splitting an ALTER TABLE's actions), and
  `assign`, which fits written values to a column's modifier. `words` reads the names
  of the role, database, RLS and COMMENT statements: whole when quoted, folded when
  not. Other readers of names remain (`session::utility::unquote_ident` the most used);
  they are to be folded into it.
- `rewrite`: `rewrite_full`, 66 passes in a fixed order, each its own function,
  grouped by theme: `casts`, `datetime`, `json`, `arrays`, `operators`, `catalog`...
  The order matters, and comments in `rewrite_full` say why where it does. `tokens`
  is the crate's shared tokenizer (`code_tokens`): words, literals and quoted names,
  punctuation, each with its depth, comments left out.
- `types`: OIDs, and per type the rewrites and the text and binary encodings:
  arrays, numeric, hstore, timestamps, booleans, parameter types.

## Errors

The hba, engine and postgres crates each have their own error type, and its text is
what a client is sent. `hba::Error`
is a closed enum: it never reaches a client. `engine::Error` and `postgres::Error` are a
kind and a message, and the kind alone gives the SQLSTATE: `sql_state` reads no text,
and `errf!` takes the state first, so nothing can be built without one. A name quoted
in a message cannot change a code.

- An error of this server carries its state from where it is made.
- SQLite's take theirs once, in `sqlite_state` in `engine/error.rs`, from the extended
  result code. SQLITE_ERROR is the one code covering every compile error, so only for
  it is the message read, and then by its shape: each phrase in the place SQLite puts
  it, around the name it quotes.
- A SQLite function that fails is SQLITE_ERROR with text and nothing else, since
  sqlite3_result_error overwrites a result code. Its state crosses on the
  `FUNCTION_STATE` thread local, which is safe because a function runs on the thread
  stepping its statement.

## Logging

The crates log through `log`; the binary's backend in `src/logger.rs` writes one stamped
line to stderr. Failed queries are `debug` and every query `trace`, which OVERLITE_DEBUG
turns on: any value, and `all`.

## How a statement flows

A connection's thread runs `Protocol::serve`: startup, authentication against the
pg_hba `Policy` or the configured method, a `Session` with its own `DbSession`, then
`Session::run_loop`.

A Query message reaches `handle_query`, which splits it into statements. More than
one run inside an implicit transaction. Each statement takes this route through
`handle_simple_query`:

1. A blank statement answers EmptyQueryResponse and never reaches the engine.
   `statement_text`, the door Parse goes through too, drops the comments in front, turns
   every other comment to spaces, and `normalize_literals` turns E'', U&'' and a string
   continued on another line into plain literals, so that nothing after it knows
   comments or more than one kind of literal.
2. The session's own statements are tried in turn: transaction control, savepoints,
   then, unless the transaction is aborted, schema DDL, role DDL, GRANT, ALTER TABLE,
   RLS DDL, LISTEN/NOTIFY, sequences, types, COMMENT, SET ROLE, PREPARE/EXECUTE, COPY,
   search_path and the utility statements clients expect to succeed.
3. `check_privileges` holds what is left to the role's grants.
4. `apply_column_defaults` strips or injects nextval defaults and fits what a write
   stores to its columns' modifiers, and an INSERT into a table under RLS is checked by
   `try_rls_insert`.
5. `rewrite_for_exec` injects RLS filters (`apply_rls`), expands session identity,
   sequence calls, advisory locks and enum columns into literals, and calls `rewrite`.
6. `Session::exec` arms the canceler and hands the text to `DbSession`. `execute` runs
   function DDL, then the `resolve` passes and function inlining, and prepares, binds
   and steps the statement. Nothing is cached: a statement is prepared and finalized
   on every execution. A computed column has no decltype, so `computed_types`
   (`column_types.rs`) reads the select list: a cast decides the column's type, and the rest speak for it only when
   there is no row to sample.
7. `send_result_set` and `send_data_rows` encode the rows by OID and format.

The extended protocol takes the same steps in pieces: `handle_parse` classifies the
statement, `handle_bind` decodes parameters, `handle_execute` runs it. A point lookup
is the hot path through `rewrite`, the engine and `send_data_rows`, across three
crates, so the release profile keeps LTO.

## Invariants

### What a table's DDL keeps

SQLite reports no constraint names, deferral, operator class or identity, so the
catalog reads them back from the CREATE statements in sqlite_master, which SQLite keeps
as written through renames, and which a rebuild writes whole: foreign keys and UNIQUE
and CHECK constraints by their names (`catalog/constraints.rs`), and two comments the
server leaves there itself. An index column's operator class that is not its type's
default follows the column, `name /*varchar_pattern_ops*/`, and an identity column's
definition ends in `/*overlite identity by default bigint*/`, its kind and the type it
was declared with, since a key that is SQLite's rowid is INTEGER whatever it was.
Whatever rewrites a column's definition keeps the comment, or rewrites it.

The same holds for what SQLite takes no clause for. A UNIQUE or PRIMARY KEY's deferral
follows its key, `UNIQUE (a) /*overlite deferrable initially deferred*/` (or
`/*overlite deferrable*/`): pg_constraint reads it, and whatever implements SET
CONSTRAINTS reads it there too. A CREATE INDEX ends in what it asked for beyond the
btree it makes: `/*overlite include "a", b*/` (a covering index's columns),
`/*overlite nulls not distinct*/` (its key columns are then `ifnull(col, <blob>)`,
which the catalog reads back as the columns) and `/*overlite using gin*/` (the access
method pg_class.relam reports). index_where stops at the first of them.

A materialized view `v` is a table `v` and a view `_overlite_mv_v` holding its query;
the prefix keeps the view out of every catalog. `_overlite_extensions` and
`_overlite_collations` hold what CREATE EXTENSION and CREATE COLLATION made; a
collation is registered with SQLite when a statement first needs it
(`builtins/collation.rs`, through sqlite3_collation_needed), so a column keeps the
name it was declared with.

### When a transaction takes the write lock

`DbSession::begin` takes no lock: SQLite's transaction begins at the first statement
that is not a read (`is_read_only`), with BEGIN IMMEDIATE, through `begin_writing`,
which a cursor, schema DDL and a locking SELECT (`take_row_locks` in the session)
call up front. Until then each statement runs on its own and sees what is committed.
A deferred SQLite transaction would hold the snapshot of its first read, and a write
after another writer committed would then fail at once (SQLITE_BUSY_SNAPSHOT), past
any busy timeout; an immediate one at BEGIN would hold every other writer off for as
long as a transaction that only reads stays open.

### The fast path contract

`needs_no_rewrite` answers true only if `rewrite_full` would return the statement
unchanged. It recognizes a narrow shape in one allocation-free pass; anything else
answers false, so it can only be wrong by being slow. `rewrite` trusts it, so a false
true silently skips a translation.

Two passes run after either route, so a plain lookup gets them too, and are not in
`rewrite_full`: `rewrite_null_order` gives each sort key without NULLS FIRST or LAST the
one Postgres uses, and `rewrite_like` makes LIKE a GLOB and ILIKE a LIKE over the
patterns the engine writes. Each scans for its word first and costs nothing without it.

A pass reacting to a new word must therefore be seen by it. The tests in
`rewrite/fastpath.rs` put every word of this crate's source, walked recursively, into
statement templates, and also run every statement of the wire suite
(`tests/fixtures/rewrite-corpus.hex`) and random statements, asserting that nothing
skipped changes under `rewrite_full`.

### RLS and ownership keys

`_overlite_owners`, `_overlite_grants`, `_overlite_rls`, `_overlite_policies` and
`_overlite_defaults` are keyed by stored name: bare in public, `<schema>.<table>` elsewhere, which is the table's
real name in SQLite, as `stored_table_name` builds it. SET SCHEMA,
RENAME TO and ALTER SCHEMA RENAME move the rows with `move_table_rows_sql`; DROP TABLE
and DROP SCHEMA remove them with `forget_table_rows_sql`.

A bare name means the public table and no other. A database from before stored names
gets `RLS_STORED_NAMES_UPGRADE` once on its first open, marked by the empty table
`_overlite_rls_stored_names`; if that fails the connection fails, because reading the
old rows as public's would open tables they protected. `DEFAULTS_STORED_NAMES_UPGRADE`
does the same for nextval defaults, marked by `_overlite_defaults_stored_names`, because
reading those as public's would write NULL keys. Owners and grants have no such upgrade:
NOTES.md, Limits.

`RlsCache`, in `DbState`, answers whether a table has RLS on without a query. A change
this server makes invalidates it, and a TTL bounds how long a change from outside can
go unnoticed.

### The literal-blanking rule

What decides a statement's route reads code, not literals. `code_only` (engine
`blank.rs`) turns the text
of string literals, comments and quoted names to spaces, and `blank_literals` the same
but keeping quoted names. Both keep quotes and every byte's position, so an offset found
in the blanked text is an offset in the statement. A classifier reads `code_only`; what
needs a name reads `blank_literals`.

A comment is whitespace, and is gone before anything reads the statement:
`statement_text` (rewrite/blank.rs) turns each to spaces with `blank_comments`, so that a
quote, a `$n` or a keyword inside one counts to no scanner after it: the parameter count,
the classifiers, the fast path, the rewrite and the engine. What also reads raw text
(`code_only`, `blank_literals`, `count_params`, the splitter of a batch, a `LANGUAGE sql`
body as it is inlined) skips comments itself. `comments_are_whitespace_to_every_scanner`
in `session/mod.rs` runs statements whose comments hold quotes, `$n` and keywords through
each of them, beside the same statements without.

A rewrite pass works on text, and a literal is text to it. Most skip literals by
themselves. One that scans raw text and has no use for what a literal says goes through
`holding_literals` in `rewrite_full`, which runs it with each literal's text out of
sight. `literals_survive_the_whole_rewrite` in `rewrite/mod.rs` holds them all to it: it
reads the passes from `rewrite_full`, so a new one has to name the words that set it
off, and puts those words in literals in every clause. The one exception is the point of
its pass: NOTES.md, Limits.

### Bytes outside UTF-8

A client may send any bytes, and a `String` holds only UTF-8. A byte `0xNN` from `0x80`
travels as the code point U+10FFNN. `text_from_bytes` and `text_to_bytes` are the only
doors, on the way in from the wire and out to the wire and SQLite; everything between
treats the text as ordinary. `has_raw_bytes` is a byte search, so plain text pays no
copy.

### A file holds the storage format

A database file holds overlite's own storage format, below, and never a form some
server once wrote by accident. The format has a version, kept in the file in
`_overlite_storage_format`; this server writes version 1 (`storage::FORMAT_VERSION`).

A file without it, or with an older one, is converted when it is opened: at startup
for every database of the directory, and by `Sqlite::open` for one created later, so
before any session reads it. `storage::ensure_storage_format`:

1. copies the file to `<name>.db.pre-v<N>.bak` with SQLite's backup API, WAL included,
   through a `.tmp` beside it renamed into place. If that cannot be written (a full
   disk, a directory in the way) nothing is converted and the server does not start.
   `:memory:` and a file with no table yet have nothing to back up.
2. in one transaction, with foreign keys off and each table's triggers dropped and
   made again from their SQL, so that nothing fires, rewrites every value of every
   column of a known type that is not in its canonical form, by rowid ranges of 500,000
   with a line of progress for a large table. A value its type cannot read is left as
   it is, counted, and its column listed in `_overlite_mixed_columns`. A column whose
   canonical values a constraint refuses (two spellings of one unique key) is left
   whole and listed too. The transaction writes the version last.
3. logs the backup, each column it changed or listed, and the time.

A crash before the commit leaves the file as it was, and the next open does it again;
a file with the version is not touched, so a second open is a no-op. Each open tries
the mixed columns again: one an operator has fixed is converted and leaves the list.
A new database is marked when its first connection finds no table in it.

What is written from then on is written in the format: `ddl/assign.rs` passes every value
a write stores to a column of a type with a canonical form through `overlite_assign`,
whatever the path: a literal, a parameter in text or binary, COPY, INSERT ... VALUES or
SELECT, WITH ... INSERT or UPDATE, `SET a = ...` and `SET (a, b) = (...)`, ON CONFLICT
DO UPDATE, and a column's DEFAULT, which the INSERT that leaves the column out writes
itself unless it is a constant already in its form. A boolean in any spelling becomes 1
or 0, a modifier rounds or refuses, a date, time, interval, timestamp or uuid becomes its
text, a bytea a blob, an array a JSON array of elements in their form, an hstore a JSON
object; what the type's input function does not read is refused with its code (22P02,
22007, 22008, 22009, 22023, 42601). `assign` in `builtins/casts.rs` and
`builtins/canonical.rs` hold the forms.

The readings of `resolve/booleans.rs` follow from it. A boolean column in the format
holds 1 and 0, which SQLite compares as they are: `col = $1` and `col = true` stay as
written and search an index on col, and only a string compared with one (`col = 'f'`,
`col IN ('t', $1)`) becomes the 1 or 0 it reads as, or `pg_bool('...')`, 22P02. A mixed
column, any boolean column of a file not in the format, and a view's when the file has
a mixed column keep the readings for text: `col = value` becomes `col IN (1, 't',
'true', ...)` over every form of that truth value, and any other reading reads
`overlite_bool(col)`. A column that is a whole item of a select list is left alone: its
declared type makes it a boolean on the way out. Which columns are which comes from the
schema and the mixed list, kept per database until schema_version moves. bool_and,
bool_or and every are min and max over overlite_bool either way.

## Storage format

Version 1. The declared type is what CREATE TABLE leaves in `sqlite_master` (SQLite
keeps a declared type as written); the canonical form is what a value of that type is
stored as; the last column is what the conversion at open makes canonical. A value of
another form that the type's input function cannot read is left, and its column mixed.

| Postgres type | Declared in SQLite | Stored as | Converted at open from |
|---|---|---|---|
| boolean | `BOOLEAN`, `BOOL` | INTEGER 1 or 0 | text Postgres' boolin reads (`'t'`, `'true'`, `'yes'`, `'on'`, `'1'`... any case), REAL 1.0 and 0.0 |
| smallint, integer, bigint | as written | INTEGER | nothing: the column's affinity makes integer text one |
| serial, bigserial, smallserial | `INTEGER` (`INTEGER PRIMARY KEY AUTOINCREMENT` as a key) | INTEGER | nothing |
| real, double precision | as written | REAL; `NaN`, `Infinity`, `-Infinity` as TEXT | nothing |
| numeric, decimal | `DECIMALTEXT` `COLLATE DECIMAL` | TEXT, the decimal as given, no exponent added | nothing |
| numeric(p,s) | `DECIMALTEXT(p,s)` `COLLATE DECIMAL` | TEXT with exactly s fraction digits | text with other digits, rounded as an assignment does; past p, mixed |
| text, varchar(n), char(n) | as written | TEXT; char(n) without its padding | nothing; past n, mixed |
| bytea | `BYTEA` | BLOB | TEXT in hex (`'\xdeadbeef'`) or escape form |
| uuid | `UUID` | TEXT, lower case, 8-4-4-4-12 | upper case, braces, no hyphens |
| date | `DATE` | TEXT `YYYY-MM-DD` | any text date input reads (`'2024-01-02 03:04:05'`, `'Jan 2 2024'`) |
| time(p) | `TIME[(p)]` | TEXT `HH:MM:SS[.f]`, at most p (6) fraction digits, none trailing zero | other spellings, more digits |
| timetz(p) | `TIMETZ[(p)]` | TEXT `HH:MM:SS[.f]+hh[:mm]` | other spellings |
| timestamp(p) | `TIMESTAMP[(p)]` | TEXT `YYYY-MM-DD HH:MM:SS[.f]`, a space and no T, at most p (6) fraction digits, none trailing zero; an offset given is dropped and the wall clock kept, as Postgres does; `infinity`, `-infinity` | a T, a Z or an offset, a date alone, trailing zeros (`.750`), more digits than p |
| timestamptz(p) | `TIMESTAMPTZ[(p)]` | the same text, in UTC, with no offset | an offset or a zone name (converted to UTC), a T, trailing zeros |
| interval | `INTERVAL` | TEXT as interval_out writes it (`1 day 02:00:00`) | text interval input reads (`'1 day 2 hours'`) |
| json, jsonb | as written | TEXT, valid JSON, as written | nothing; invalid JSON, mixed |
| an array, `t[]` | as written (`text[]`, `"numeric(10,2)[]"`) | TEXT, a JSON array; an element of a type with a modifier fitted to it | Postgres' text form of a one-dimensional array (`'{a,"b c"}'`) |
| hstore | `HSTORE` | TEXT, a JSON object | nothing: its text form is still read |
| tsvector | `TSVECTOR` | TEXT as tsvector_out writes it: each lexeme once, quoted, in byte order, with its positions in order and their weights (`'cat':2,5A 'dog':3`) | nothing |
| tsquery | `TSQUERY` | TEXT as tsqueryout writes it (`'fat' & ( 'rat' \| 'cat' )`) | nothing |
| an enum | `TEXT CHECK (col IN (...))` | TEXT, the label | nothing |
| int4range, int8range, numrange, daterange, tsrange, tstzrange | as written | TEXT as range_out writes it: a discrete range `[)`, `empty`, bounds in their type's form and quoted where range_out quotes, a tstzrange's in UTC with `+00` | nothing: a range from before keeps its text, which the operators still read |
| inet, cidr, macaddr, money, the rest | as written | TEXT as given | nothing |

### What sessions already open see

`DbSession` builds its catalog views from the set of schemas, and a session its
pg_database from the set of databases. Both sets carry a generation, bumped when a
change commits, and a connection rebuilds on its next statement. A rollback bumps it
too, so the session that made the change drops the views it built on it. Reading the
generation is one atomic load per statement, on the hot path.

### The session's TimeZone

A session's TimeZone lives in the engine, in a thread local (`set_session_zone`): a
session runs on a thread of its own, and a SQLite function on the thread that steps
its statement, so the input and output functions see it without an argument. A
timestamptz is stored as bare UTC whatever the zone. The protocol renders one in the
zone with its offset; resolve/temporal.rs reads what a client gives as a timestamptz
without an offset (overlite_tz_in) in the zone, and a timestamptz read from a column
going on to a cast or a write with its offset (overlite_tz_text), so no zone misreads
it. The session restores the zone a rolled-back SET or a SET LOCAL changed, and
reports each change with ParameterStatus. now() holds the transaction's start the same
way, in a thread local the session marks at BEGIN and clears at its end.

### The WAL shutdown path

SQLite checkpoints and removes the WAL when the last connection to a database closes,
so a clean exit has to close them all.

1. SIGINT or SIGTERM (Ctrl-C on Windows) reaches `Stopper::stop`, which sets a flag and
   connects to the listener to wake `Server::serve`.
2. `Server::drain` shuts the read side of every socket, so an idle client's session
   ends, and calls `cancel_running` on every pass, since a batch may have moved on to
   its next statement. A running statement is interrupted and reported as 57014, and
   an open transaction rolls back. After 1s it shuts the write side too, for a client
   that stopped reading. After 5s it gives up and logs how many are left: a session
   waiting out busy_timeout cannot be interrupted, and a supervisor will not wait.
3. Each session's thread drops its `DbSession`, closing its connection.
4. `Cluster::close` drops the engines, and with each `Sqlite` the engine's own
   connection, the last one.

`tests/wire/parity_shutdown.rs` checks this from outside.

### A panic ends one connection

`tests/wire/panics.rs` bends the statements of the suite and the fields of the messages
around them, down both routes, and holds every connection to answering the next: a
client is not to reach a panic at all. Should one get through, a connection runs on its
own thread and nothing catches a panic there, so it ends that thread alone. The accept loop and every other session go on, and `Untrack` forgets the
connection on the way out. `handle_contains_panics` in `server.rs` pins this. A poisoned
lock is taken anyway, as `overlite_core::sync` says, so a panic does not reach the
sessions after it through a shared registry. SQLite functions are the other border:
rusqlite catches a panic in a scalar one, and `guarded` in `catalog` does so for the
window aggregates, whose `extern "C"` callbacks would otherwise abort the process.
