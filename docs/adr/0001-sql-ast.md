# 0001: Translate SQL from a parse tree, not from text

Status: accepted; stages 0 and 1 landed on main (below), stage 1's week of the owner's apps pending
Date: 2026-11-08

## Context

overlite translates Postgres SQL to SQLite SQL with text: `rewrite_full` runs 66 passes in
a fixed order (crates/postgres/src/rewrite), the engine's `resolve` passes read the result as
text again for types and names (crates/engine/src/resolve), and three scanners decide what
is code (`blank.rs`, `rewrite/tokens.rs`, `resolve/names.rs`). Most of the bugs fixed so far
come from that shape, not from any one pass:

- a literal changed by a pass that found its word inside it (`holding_literals` and
  `literals_survive_the_whole_rewrite` exist to fence this);
- a comment read as code (hence `statement_text` blanking comments for every scanner);
- types lost in subqueries: `param_columns` puts all of a statement's tables in one list,
  and a bare column of a subquery is looked for in the outer query's tables too;
- labels: `result_labels` re-derives Postgres' FigureColname from text before the passes run
  and splices `AS` back in after them, and gets casts wrong (below).

Each pass re-tokenizes the statement, and the order of the passes carries meaning that only
comments in `rewrite_full` record. A parse tree gives every pass the same, correct reading of
the statement once.

## The corpus

`tests/fixtures/ast-corpus/*.hex`, one hex-encoded statement per line, deduplicated exactly,
each statement kept only in the first source it appears in (order: wire, rewrite, django,
wagtail, gitea, owner). Literals are kept for test suites; for gitea and the owner's apps,
letters in literals and comments became `x`, digits `0`, numbers `1` or `1.5`, so only
shapes remain (checked: no SCRAM, no hashes, no values, emails blanked).

| source | raw statements | unique in source | after global dedup | in repo |
|---|---|---|---|---|
| wire (SQL literals in tests/wire/*.rs) | 3632 | 2972 | 2972 | all |
| rewrite (tests/fixtures/rewrite-corpus.hex) | 806 | 805 | 249 | all |
| django (suite SQL logs, two runs + sem/dj4) | 81498 | 23731 | 23720 | 3070, one per shape |
| wagtail (suite run logs) | 66665 | 12513 | 12508 | 717, one per shape |
| gitea (app log and run logs) | 4553 | 1003 | 1001 | all |
| owner (the owner's apps' server logs) | 354 | 67 | 66 | all |
| total | 157508 | | 40516 | 8075 (4.5 MB) |

Django and Wagtail are sampled in the repo by statement shape (the statement with literals,
quoted names and list lengths collapsed; first of each shape kept); the full 25.7 MB set
stays outside the repo and was used for the parser comparison below.

## Options

Three parsers, measured with a harness outside the repo over the full corpus (40516
statements), release build, thin LTO, Apple M-series, 16 cores. Parse time is the median of
5 runs per statement over a deterministic sample of 10129 statements (median length 196 B).
pg_query's time includes its protobuf decode and the table/function summary
`ParseResult::new` computes on every parse.

| | pg_query 18.1 (libpg_query, PG 18 grammar) | sqlparser 0.63 (PostgreSqlDialect) | squawk-syntax 2.72 |
|---|---|---|---|
| parses | 40411 (99.74%) | 40153 (99.10%) | 40423 (99.77%) |
| valid Postgres it rejects | 0 (it is the grammar) | 269 | 0 |
| invalid Postgres it accepts | 0 | 11 | 12 (bad escapes in E'' only) |
| p50 / p90 / p99 parse | 5.4 / 16.0 / 57.4 µs | 4.2 / 12.0 / 44.8 µs | 2.1 / 5.1 / 16.5 µs |
| binary, harness alone (0.51 MB without) | 4.67 MB (+4.2) | 5.33 MB (+4.8) | 2.03 MB (+1.5) |
| clean release build of the harness | 26 s | 18 s | 7 s |
| build needs | C compiler, libclang (bindgen) | nothing | nothing |
| license | MIT (libpg_query BSD-3, Postgres' code PostgreSQL license) | Apache-2.0 | Apache-2.0 OR MIT |
| MSRV | not declared | not declared | 1.99 (the workspace declares 1.88) |
| maintained by | pganalyze; versioned with Postgres majors since 2014 | Apache DataFusion; frequent releases that break the AST | one main author; releases weekly, crates are the linter's internals |
| tree | Postgres' raw parse tree: names folded, literals decoded, `int` → `pg_catalog.int4`, no parentheses, no comments; locations | typed Rust AST, keeps spellings; spans | lossless CST (rowan): every byte, comments, parentheses; typed accessors |
| printer | C deparser to Postgres SQL, not per node; we write our own | `Display` per node, fixed; mutate then print | the text itself; edits by node |
| PL/pgSQL bodies | `parse_plpgsql` | string | `plpgsql` module |

All three parse every construct of the probe list: comments, `$n`, `::`, `E''`, `U&''`, dollar
quoting, `@>`, `?|`, `#>`, `#>>`, `-|-`, `&&`, `OPERATOR(pg_catalog.~)`, COPY (both
directions, options), DECLARE ... CURSOR WITH HOLD, FETCH, ALTER TABLE with several actions,
ADD CONSTRAINT ... DEFERRABLE, CREATE FUNCTION (sql and plpgsql bodies), EXCLUDE, GENERATED,
TABLESAMPLE, GROUPING SETS, `0x1F`, `1_000`. What they make of them differs:

- Operator grouping, ten probes against Postgres' own (`- 2 ^ 2`, `a + b || c`, `a || b = c`,
  `a < b BETWEEN c AND d`, `a = b IS NULL`, `a LIKE b || c`, `NOT a = b`, `x::int + 1`,
  `a -|- b && c`, `2 ^ 3 ^ 2`): squawk 10 of 10; sqlparser groups `- 2 ^ 2` as `-(2 ^ 2)`,
  -4 where Postgres says 4.
- sqlparser prints `0x1F` as `X'1F'`, a blob, where Postgres 16+ reads the integer 31.
- sqlparser's 269 rejections: SET CONSTRAINTS (108, every Django migration with a deferred
  key), ALTER ... COLLATE / DROP IDENTITY / SET SCHEMA / ALTER SEQUENCE RENAME (79), GRANT
  role TO ... WITH ADMIN OPTION (21), MOVE/FETCH forms (17), CREATE INDEX ... INCLUDE ...
  TABLESPACE (10), window frames with EXCLUDE (8), FOR NO KEY UPDATE (3), COMMENT ON
  CONSTRAINT (4).
- pg_query's tree is not the text: `E'a\'b'`, `$$x$$` and `U&'...'` arrive as the string
  they mean, `interval '1 day'` as a cast, `!=` as `<>`. That is what a printer wants, and
  what a test comparing text with text does not.
- Builds: pg_query does not build on the image the Dockerfile uses (`rust:1-bookworm`):
  bindgen panics "Unable to find libclang". With `apt-get install libclang-dev` it builds
  (32 s). It builds natively on macOS (Xcode's libclang). Windows was not built here:
  its build.rs has MSVC include paths, and bindgen needs LLVM, which GitHub's x64 Windows
  image ships; the windows-11-arm runner of release.yml is unverified.

## Decision

Postgres' own grammar, through pg_query, is the parser; overlite writes its own SQLite
printer over the raw parse tree.

- The tree is right by construction. A misgrouped expression or a rejected statement is a
  wrong answer or a refused client, and the comparison found both in sqlparser. squawk accepts
  what Postgres accepts on this corpus and grouped every probe right, but that is measured,
  not given: its grammar is a second implementation by one author.
- The tree is semantic: names folded, literals decoded into values, constants typed, type
  names canonical. Labels, parameter typing and casts need exactly that, and it removes the
  class of bug where a pass reads a literal's text.
- It tracks Postgres majors with the same release numbers, and comes with PL/pgSQL parsing and
  fingerprinting.

The costs are accepted: +3.0 MB of binary and +9 s of clean build; 5.4 µs p50 per parse, which the translation cache
takes off every repeated statement (below); and libclang in every build environment (one line
in the Dockerfile; LLVM on the Windows runners). If stage 0 cannot build the Windows arm64
release, squawk is the fallback: the printer and the passes are written against our own
walk, and squawk's numbers above are the best of the three.

## The spike (branch `ast-spike`)

`crates/postgres/src/ast/`:

- `emit.rs`: the printer. SELECT (joins of every kind, subqueries in FROM and in
  expressions, EXISTS, IN and NOT IN over a list or a subquery, CTEs with RECURSIVE and
  MATERIALIZED, UNION/INTERSECT/EXCEPT [ALL], VALUES, DISTINCT, GROUP BY, HAVING, ORDER BY
  with Postgres' NULLS ordering written out, LIMIT/OFFSET with OFFSET alone as `LIMIT -1
  OFFSET`, FETCH FIRST), INSERT (VALUES, SELECT, DEFAULT VALUES, RETURNING), UPDATE and
  DELETE (WHERE, RETURNING); expressions: columns, `$n`, constants, booleans, NULL,
  `= <> < > <= >= + - * / ||`, unary minus, AND/OR/NOT, IS [NOT] NULL, IS [NOT] DISTINCT FROM,
  BETWEEN, LIKE/ILIKE (left to `rewrite_like` as before), CASE, COALESCE, NULLIF, a few plain
  functions and every `overlite_*` the session injects, and casts to text, boolean
  (`pg_bool`), smallint/integer/bigint/real/double precision/numeric[(p,s)]/varchar[(n)],
  date, timestamp[tz] and uuid (`overlite_cast`). Parentheses come from SQLite's precedence,
  so `a + b || c` is written `(a + b) || c`. Anything else answers `Err(<first node>)`.
- `labels.rs`: FigureColname on the tree; an unnamed result column gets `AS "label"` in the
  outermost list or RETURNING.
- `params.rs`: the column each `$n` meets, with scopes: a column is looked up in its own
  query level first, and the table found is named by a key of its own, so the same alias in
  two levels cannot be confused. Describe uses it, the text reader as fallback.
- `rewrite()`: fast path first, untouched (`needs_no_rewrite`, no parse); then a per-thread
  translation cache keyed by statement text (4096 entries, statements up to 16 KiB; sound
  because no pass reads session state); then the AST path if it covers the statement,
  else `rewrite_full`. `rewrite_null_order` and `rewrite_like` still run after either.
  `OVERLITE_AST=0` and `OVERLITE_REWRITE_CACHE=0` turn each off.
- `diff.rs`: the differential test.

### Differential result

Every statement of the in-repo corpus, through `statement_text` as the server does, then both
routes. Compared as normalized tokens (spacing, keyword case, optional AS, needless quotes
and the engine's spelling of a cast type do not count).

| | statements |
|---|---|
| fast path (no parse, unchanged) | 518 |
| unsupported, left to the text passes | 4508 |
| translated | 3040 |
| identical | 1571 |
| identical except parentheses | 1429 |
| intentionally different | 39 |
| unexplained (the test fails on any) | 0 |

The 39 intended differences, each a reason in `INTENDED`:

- 21: `NOT (x IN (subquery))` is written `x NOT IN (subquery)`: one node in the tree.
- 11: a parenthesized UNION member without ORDER BY or LIMIT is written bare, where the text
  path wraps it in `SELECT * FROM (...)`.
- 4: labels the text path got wrong: `SELECT 0.1::numeric + 0.2::numeric` is `?column?` in
  Postgres (the text path said `numeric`), `'1.5'::numeric = 1.5` too (the text path said
  `5`), `$1::int + 1` too (`int4`).
- 2: a parenthesized column `(second)` is a column; SQLite names it.
- 1: `SELECT "a?b" FROM t`: the text path rewrote the `?` inside a quoted name into
  `jsonb_exists(a, b)`.

Three findings of the first run were the printer's bugs, now unsupported, and are the reason
the slice grows only behind this test: `%` (pg_trgm's similarity on text, a remainder on
numbers: only types tell), a set operation in the right arm (`a UNION b INTERSECT c`: Postgres
binds INTERSECT tighter, SQLite reads left to right), and a quoted name holding a dot (a
stored name of another schema).

Parameter typing over the corpus: of 547 statements whose parameters meet a column, the
tree finds the same columns in 543, more in 3, other in 1, and ties 1199 references to one
table where the text reader ties 362.

The unsupported ones, by the first node met: utility statements 2358, functions outside the
plain list 569, other cast types 244, other operators 238, catalog relations 198, qualified
functions 191, FROM items (functions, LATERAL) 126, other expressions 87, parse errors 87,
LIKE ESCAPE 58, ON CONFLICT 58, window/aggregate clauses 56, arrays 53, system columns 44,
FOR UPDATE 40, the rest under 25 each.

### Performance

All on one Apple M-series machine, 16 cores, release builds.

**Build and binary.** A clean `cargo build --release` of overlite: 24 s on main, 33 s on the
branch (+9 s, libpg_query's C and bindgen). The binary: 11.74 MB on main, 14.78 MB on the
branch (+3.0 MB; thin LTO drops the deparser the harness kept).

**pgbench**, 4 clients, 4 threads, 8 s per run, against main's binary and the branch's
binary, each over its own copy of a 100,000-row table, run in turn (old, new) three times.
`lookup` is `SELECT id, v FROM kv WHERE id = :id` (on the fast path: no parse);
`lookup2` is `SELECT id, upper(v), length(v) FROM kv WHERE id = :id ORDER BY id DESC LIMIT 1`
(off the fast path, in the AST slice). tps per round:

| script, mode | main (text passes) | branch (AST + cache) |
|---|---|---|
| lookup, simple | 117914 / 117257 / 113768 | 113429 / 118505 / 118607 |
| lookup, prepared | 99653 / 99132 / 99575 | 100439 / 99817 / 100181 |
| lookup2, simple | 74591 / 74607 / 74727 | 72433 / 71171 / 72460 (−3%) |
| lookup2, prepared | 61899 / 57479 / 61943 | 74112 / 71832 / 73916 (+19%) |

The fast path costs nothing new. lookup2 prepared gains because every Execute runs
`rewrite_for_exec` again (session/mod.rs, `handle_execute`): main translates a prepared
statement on each execution, the branch's cache answers. Simple lookup2
loses 3% even with the cache hitting: the cache's hash and copy of the text are not free,
and the printed statement carries labels and NULLS FIRST that SQLite then prepares on
every execution. A shared, `Arc<str>`-valued cache is the obvious next measurement.

**Statement mix** (`time_the_statement_mix`, in-process, the corpus through `statement_text`,
three alternating rounds, µs per statement, the fast path included in all three):

| mix | text passes | AST path, every statement parsed | `rewrite()` with the cache warm |
|---|---|---|---|
| first 3000 statements (fits the 4096-entry cache) | 11.0 / 10.5 / 9.8 | 13.5 / 13.0 / 13.1 | 0.16 / 0.14 / 0.15 |
| all 8066 (does not fit; the cache starts over) | 26.7 / 25.8 / 26.2 | 28.0 / 27.7 / 27.7 | 28.4 / 28.8 / 28.2 |

Uncached, the AST path costs 2-3 µs more per statement than the text passes today: the
parse, and for the 60% outside the slice the parse wasted before the text passes run. Once
cached, translation is 0.15 µs. A cache that empties when full is the spike's; an LRU or a
shared cache keeps a working set larger than the bound from thrashing as the second row does.

## Migration plan

Each stage ends with `make check` green with the AST path on, the differential test at zero
unexplained, and the stage's corpus share translated. A pass is deleted only when the
differential test shows the text path never runs it on a statement the AST path does not
cover, which `rewrite_full` can count per pass in a test build.

0. **Build everywhere.** libclang in the Dockerfile builder; the release matrix (Linux x64 and
   arm64 cross, macOS x64 and arm64, Windows x64 and arm64 MSVC, crt-static) builds with
   pg_query. Exit: release.yml green on every target. If Windows arm64 cannot, switch to squawk
   before anything else lands.
1. **The spike's slice on main**, behind `OVERLITE_AST` defaulting on, with the cache. Grow
   the corpus from the wire suite automatically (every statement it sends). Exit: the numbers
   above or better on main; a week of the owner's apps on it with no difference logged
   (log the text path's output beside the AST path's for a sampled share).
2. **Names and types first**, the passes that hurt most as text: labels (`labels.rs` ->
   `ast/labels.rs`, then delete `result_labels`/`apply_labels` and their call in
   `rewrite_for_exec`), parameter typing (delete `param_columns` text reader and the
   regexes of `param_types.rs`), `casts`, `typed_cast`, `bool_cast`, `null_order`, `offset`,
   `like`. Exit: those passes deleted; `literals_survive_the_whole_rewrite` loses their words.
3. **Expressions.** Operators by operand type where the tree plus the catalog tells
   (`%`, `@>`, `?`, `||` on tsvector, `->`/`#>`, `&&`, `-|-`): json, arrays, ranges, fts,
   trgm, regex, OPERATOR(), AT TIME ZONE, EXTRACT, now() and the niladic functions,
   string_agg, TRIM, COLLATE, ctid. This needs a small type environment (the column types
   `resolve` already probes, read once per statement); `resolve/temporal.rs`,
   `numeric.rs`, `booleans.rs`, `tsvector.rs`, `time_zone.rs` move onto it. Exit: those
   passes and their engine counterparts deleted.
4. **Query shapes.** DISTINCT ON, DELETE USING, UPDATE FROM, LATERAL and set-returning
   functions, derived column aliases, generate_series, ARRAY(subquery), set members with
   ORDER BY/LIMIT, row locks, ON CONFLICT. Exit: every DML statement of the corpus translated.
5. **Catalog and DDL.** pg_catalog / information_schema names, regclass and friends,
   CREATE TABLE/INDEX/ALTER forms (`ddl_syntax`, `default_expr`), and the session's
   classifiers (`route_of`, the utility statements) on the tree instead of first words.
   Exit: `rewrite_full` is empty and deleted with `holding_literals`, `tokens.rs`, and the
   blanking scanners where nothing reads raw text any more; `needs_no_rewrite` becomes "the
   printer would print it unchanged" or goes, if the cache makes it moot (measure).

## Stage 0 result: builds

Measured 2026-11-08 on the same machine (Apple M-series, 16 cores; podman's VM, 8 CPUs,
8 GiB, for Linux).

| target | how | result |
|---|---|---|
| aarch64-apple-darwin | `cargo build --release --target`, `+crt-static` as release.yml | built (Xcode's libclang), ran |
| x86_64-apple-darwin | the same, cross on arm64 | built, ran under Rosetta, AST path answered |
| linux/arm64 image | `docker build --platform linux/arm64` (podman) | built, `static-pie linked`, AST path answered |
| linux/amd64 image | `docker build --platform linux/amd64`, cross from arm64 | built, `static-pie linked`, x86-64, AST path answered under emulation |
| x86_64-pc-windows-gnu | cross in `rust:1` with mingw-w64, libclang-dev and clang | built (`overlite.exe`, pg_query symbols in it); not run |
| x86_64-pc-windows-msvc, aarch64-pc-windows-msvc | GitHub runners only | not built here: no MSVC on this machine |

- The Dockerfile's builder installs libclang-dev. Its cross build needs nothing more:
  bindgen passes `--target`, and the cross gcc's sysroot (`libc6-dev-*-cross`) supplies
  the headers.
- The mingw build first failed with `'stdbool.h' file not found`: libclang loaded from
  Debian's multiarch path does not find its own resource directory, and for a Linux
  target gcc's include directory hides that. bindgen asks a `clang` executable for its
  include paths when it finds one, which is why the Windows runners set `CLANG_PATH`
  next to `LIBCLANG_PATH`.
- libpg_query's parse entry redirects stderr through a pipe around each parse
  (`pg_query_parse.c`), which would be unsafe in a threaded server, but only under
  `#ifndef DEBUG`, and `pg_query_internal.h` defines DEBUG: it is compiled out, and
  that is also why the Windows code, which has no `pipe()`/`fcntl`, compiles.
- CI (`.github/actions/libclang`, used by ci.yml and release.yml): libclang-dev on
  Ubuntu; nothing on macOS; on Windows the image's x64 LLVM, or the `woa64` LLVM
  release on windows-11-arm (the build script is arm64 there and cannot load an x64
  libclang.dll), with `LIBCLANG_PATH`/`CLANG_PATH`, then the MSVC developer
  environment of the runner's architecture. ci.yml now also runs `make check` on
  macOS and builds every target and runs the unit tests on windows-latest and
  windows-11-arm. None of this has run on GitHub yet: stage 0's exit, release.yml green
  on every target, is still to be seen on the first tag. If windows-11-arm cannot build
  libpg_query with MSVC, squawk remains the fallback, as decided above.

## Stage 1 result

On main, the AST path on by default (`OVERLITE_AST=0` turns it off).

**The cache.** The spike's cache was per thread, emptied when full, and keyed by text
alone. It is now one cache per process (`rewrite/cache.rs`): 256 shards behind their
own locks, least recently used out first, bounded at 16384 statements and 64 MiB; a
statement is kept the second time it is seen (a Bloom filter per shard), so texts
sent once, as a client interpolating its values sends them, neither pay for being
kept nor push out those that repeat. Measured in-process (`time_the_cache`, every
thread doing nothing but lookups, the worst case for a lock), ns per hit:

| threads | per thread | one lock | 256 shards |
|---|---|---|---|
| 1 | 84 / 90 / 91 | 81 / 83 / 88 | 100 / 98 / 98 |
| 4 | 121 / 99 / 97 | 1252 / 1256 / 1230 | 152 / 151 / 148 |
| 16 | 148 / 146 / 142 | 3010 / 2889 / 2865 | 528 / 507 / 519 |

One lock is ruled out; the shards cost 10-20 ns more than a cache per thread at the
loads a server sees (a statement spends tens of µs elsewhere), and in exchange memory
is bounded once rather than per connection, and a new connection finds its
application's statements translated.

What a translation depends on, found by reading every pass: the text, which already
holds what the session expands before `rewrite` (enum columns, sequences, identity,
RLS); the collations CREATE COLLATION made (`rewrite_collate` asks the engine), now
held by a generation each entry keeps; and the clock, in a table's DDL only (a
DEFAULT `'today'::date` is folded). Only statements that read or write rows are
cached, so DDL is translated every time. search_path, the session's TimeZone and the
catalog's types are applied after `rewrite` on every execution.
tests/wire/translation_cache.rs runs one text across each change; its collation case
fails when the cache ignores the generation, as the spike's did.

**The −3%.** The spike's explanation was wrong: for `lookup2` both routes print the
same SQL, labels and NULLS FIRST included. pgbench's simple protocol writes `:id` into
the text, so every statement is new, the cache never hits, and every statement pays
the parse: libpg_query's parse and protobuf encode 5.9 µs, prost's decode 1.6 µs, and
the pg_query crate's summary walk 0.8 µs that nothing reads, against 7.7-8.0 µs for the
text passes. Fixed by calling libpg_query directly (`ast/parse.rs`, the same entry and
prost types, no summary), by not caching statements seen once, and, for statements
outside the slice, by remembering their shape (the text with literal values left
out, `ast/shape.rs`) so the next of that shape is not parsed only to be refused.

pgbench as above (4 clients, 8 s, each binary on its own fresh copy of the table, main
then HEAD in each round), main 8256d68 against stage 1; tps per round:

| script, mode | main | stage 1 | |
|---|---|---|---|
| lookup (fast path), simple | 112672 / 112985 / 112886 | 112855 / 112936 / 112671 | ±0% |
| lookup (fast path), prepared | 94078 / 94581 / 94346 | 94842 / 94607 / 94759 | +0.4% |
| lookup2 (AST slice), simple | 71929 / 72874 / 72892 | 72639 / 72876 / 72595 | +0.2% |
| lookup2 (AST slice), prepared | 59730 / 59309 / 59531 | 70431 / 70212 / 69885 | +17.9% |
| mix, simple | 13104 / 13146 / 13167 | 13244 / 13134 / 13186 | +0.4% |
| mix, prepared | 10987 / 10962 / 10997 | 13063 / 13054 / 13061 | +18.9% |

The mix is six statements per transaction: the fast-path lookup, lookup2, a count with
BETWEEN and LIKE (AST slice), and three the slice leaves to the text passes (`=
ANY(ARRAY[...])`, `string_agg`, `json_build_object`).

The statement mix in-process (`time_the_statement_mix`, µs per statement, three
rounds; "text passes" is what main runs):

| mix | text passes | AST path, no cache | `rewrite()`, cache warm |
|---|---|---|---|
| first 3000 statements | 10.0 / 9.9 / 10.1 | 11.4 / 11.1 / 11.5 | 4.25 / 4.20 / 4.21 |
| all 8066 | 26.4 / 26.2 / 26.4 | 27.6 / 27.3 / 27.2 | 8.36 / 8.51 / 8.30 |
| first 3000 that read or write rows | 20.2 / 20.6 / 19.3 | 18.5 / 18.4 / 18.6 | 0.23 / 0.23 / 0.22 |
| all 5643 that read or write rows | 29.0 / 28.7 / 28.1 | 26.8 / 26.4 / 26.6 | 0.28 / 0.26 / 0.26 |

With DDL in the mix the warm figure is DDL's own translation, which is not cached;
over the statements that are, the whole corpus now fits and stays at 0.3 µs, where
the spike's cache started over and stayed at 28 µs.

**The corpus.** `make ast-corpus` records every statement the wire suite sends
(OVERLITE_RECORD_STATEMENTS, at Query and at Parse; the panics test, which bends
statements at random, left out) and adds what the corpus lacks to `wire.hex`: 7384
statements, 15450 in all. The differential test, in `make check`, over them: fast path
3620, left to the text passes 8425, translated 3405: identical 1934, identical but
parentheses 1431, intended 40 (21 NOT IN, 11 set members, 5 labels, 2 parenthesized
columns, 1 quoted name), unexplained 0. The new statements found one printer bug,
fixed: `INSERT ... OVERRIDING SYSTEM VALUE` lost its clause; it is now left to the
text passes.

**Exit criteria.** The numbers above or better on main: met. A week of the owner's
apps with no difference logged: not done, it needs the week. The means is in place:
`OVERLITE_AST_SHADOW=n` translates one statement in n through the text passes too and
logs a warning with both outputs for any difference the differential test would not
explain (off by default, since the line holds the statement and its literals).

## Risks

- **Fidelity.** The tree drops what SQLite does not need but a later text reader might: the
  engine's `resolve` passes still read SQL text until stage 3, and read the printer's text
  now. `make check` runs with the AST path and the cache on and is green (the wire suite's 524
  tests included); the printer's style is plain
  SQL those readers already meet. Literals are printed from their values, so a literal's
  spelling can change (`E'a\nb'` becomes a string with a newline in it), never its meaning.
  Parentheses are SQLite's, not the client's.
- **Grammar version.** pg_query 18 parses PG 18; a client of an older server sends nothing it
  rejects. A newer client could send PG 19 syntax before pg_query 19: it then falls back, while
  the text passes exist, and later fails as Postgres 18 would.
- **Performance.** A statement off the fast path and not in the cache pays a parse. The
  cache is per thread (a client's thread), so it starts cold per connection; a shared cache
  is the next step if connection churn shows up. Unsupported statements pay the parse and the
  text passes until their stage lands (the cache keeps the result either way).
- **Windows and cross builds.** bindgen at build time needs libclang for the build host and
  the target's headers (BINDGEN_EXTRA_CLANG_ARGS for the Docker arm64 cross build).
  The C code itself builds with MSVC per pg_query's build.rs. Stage 0 gates it.
- **Binary size.** +3.0 MB (11.74 to 14.78 MB); a server, not a concern, but noted for the Docker image.

## What changes in ARCHITECTURE.md at the end

- The opening line: there is a parser. A statement is parsed by Postgres' grammar, classified
  and translated on the tree, printed as SQLite SQL, and run by SQLite.
- `rewrite` becomes `ast`: the printer, labels, parameter typing, the type environment, the
  per-node translations by theme (casts, json, arrays, ...), and the cache.
- "How a statement flows": steps 1, 2 and 5 work on the tree; `statement_text` and the
  blanking of comments go; the session's classifiers read nodes.
- "The fast path contract" is restated for the printer, or removed with the fast path.
- "The literal-blanking rule" is removed: no pass reads a literal's text.
- The engine's `resolve` passes become annotations of the tree (types, scopes) instead of text
  scans; `blank.rs` and `resolve/names.rs` go.
