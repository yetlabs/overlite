# 0001: Translate SQL from a parse tree, not from text

Status: accepted, done: stages 0 to 5c landed on main (below); stage 1's week of the owner's apps pending
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

## Stage 2 result

For every statement the AST path translates, names and types now come from the tree,
and the text readers of each run only for the statements left to the text passes and
on the fast path. The passes themselves stay: 7785 statements of the corpus still need
them (below), and stage 5 deletes what is left of `rewrite_full`.

- **NULLS ordering and LIKE.** `rewrite_null_order` and `rewrite_like` no longer run
  over the AST path's output: the printer writes each sort key's NULLS (it did since
  the spike) and every LIKE and ILIKE as the engine matches them, ESCAPE included
  (`a ~~ like_escape(p, e)` in the tree). `rewrite` runs `text_tail` after the text
  passes and on the fast path only. The fast path now leaves a LIKE whose pattern is
  not a literal or a parameter to the AST path.
- **OFFSET** was the printer's since the spike (`LIMIT -1 OFFSET`, `LIMIT ALL`, `OFFSET
  n ROWS`, FETCH FIRST in either order).
- **Casts.** The printer writes a cast to every type the engine's input functions read:
  the integer, float, numeric, boolean, char and varchar types by any spelling, date,
  time, timetz, timestamp and timestamptz with a precision, interval without fields,
  uuid, json, jsonb with its COLLATE JSONB, bytea, name, inet, cidr, the six range
  types, and hstore (`hstore_in` of a literal, anything else as it is). Arrays, reg*
  types, tsvector and an interval with fields stay with the text passes (stages 3 and
  5).
- **Labels.** A statement the session expands before `rewrite` (`nextval('s')` into a
  number, an RLS filter, an enum check) was labelled by `result_labels`/`apply_labels`
  on the text. `rewrite_expanded` now reads the labels from the tree of the statement
  as sent and prints the expansion's tree with them (`emit::statement_labelled`) when
  the AST path covers the expansion, keyed in the cache by both texts. Describe's
  neutralized statement takes the same way.
- **Parameter types.** Describe types the parameters of a statement the AST path
  translates from its tree alone (`params::facts`): the INSERT slot each fills, by
  column list or position, the column each meets by scope, and the row counts of
  LIMIT, OFFSET and FETCH. The regexes of `param_types.rs` (`INSERT_INTO`,
  `INSERT_BARE`, `LIMIT_OFFSET`, the catalog's `COMPARED`) and the text reader
  `param_columns` run for the other statements only.
  `parameter_slots_match_the_text_readers_over_the_corpus` holds the tree's slots and
  counts to the regexes': of the 77 translated statements with either, the tree finds
  the same in 75 and more in 2, never fewer.
- **The slice, wider.** Functions no pass touches (each name looked for in the rewrite,
  types and session code: string, math and aggregate functions, date_trunc, to_char,
  age), GREATEST and LEAST, IS [NOT] TRUE and FALSE.

**The corpus** (`make ast-corpus` added the 203 statements the wire suite now sends
that it lacked: 15653 statements), main before this stage against it:

| | fast path | text passes | AST path |
|---|---|---|---|
| before stage 2 | 3809 | 8435 | 3409 |
| stage 2 | 3808 | 7785 | 4060 |

Of the 4060: identical 2397, identical but parentheses 1581, intended 82, unexplained 0.
The statement that left the fast path is `LIKE CASE ...` (below). The intended
differences, by reason:

- 26 labels: FigureColname calls an expression over a cast or a column `?column?` and
  a CASE `case`, where the text path named it by its operand (`b LIKE 'x'` was `b`,
  a CASE whose arms cast to jsonb was `jsonb`).
- 23 `NOT (x IN (subquery))` written `x NOT IN (subquery)`; 11 set members needing no
  wrapper; 2 parenthesized columns; 1 quoted name (as in stage 1).
- 13 jsonb: the text path gave `::jsonb` its COLLATE JSONB and not `::JSONB`, Django's
  spelling, so `'[3]'::JSONB < '[10]'::JSONB` compared the texts and was false.
- 4 casts of casts: `'[0,10)'::int4range::int4range` (Django's range fields) left a `::`
  in the SQLite SQL, or after a `NULL::int8range` the text path wrapped the whole
  VALUES row in a CAST; both syntax errors.
- 2 LIKE patterns: the text pass reads a pattern up to a word that may end one, and
  `LIKE CASE WHEN ...` became `GLOB overlite_like_glob(CASE, '\') WHEN ...`.

Each of those bugs has a wire test in tests/wire/ast_fixes.rs that fails on the
commit before this stage: the LIKE over a CASE, the range casts, the capitalized
jsonb, the labels of an expanded statement (`SELECT b LIKE 'x%', nextval('s')`), and
`LIMIT ($1)`, whose parameter the regex left unspecified and the tree types bigint.

Unsupported now, by first node: utility statements 3299, operators 2682 (2419 of them
`~`, the regex tests of the wire suite), functions 550, catalog relations 256,
qualified functions 232, FROM items 127, parse errors 106, the rest under 81 each; cast
types fell from 310 to 25 and LIKE ESCAPE from 58 to 0.

**Performance.** pgbench as in stage 1 (4 clients, 4 threads, 8 s, each binary on its
own copy of the table, main e907372 against stage 2, the order of the two swapped in
round 2), tps per round:

| script, mode | main | stage 2 | means |
|---|---|---|---|
| lookup (fast path), simple | 114583 / 115115 / 115123 | 113923 / 114799 / 115373 | −0.2% |
| lookup (fast path), prepared | 97960 / 98260 / 97398 | 98861 / 98208 / 96857 | +0.1% |
| lookup2 (AST slice), simple | 72947 / 73257 / 73100 | 74351 / 74251 / 73698 | +1.4% |
| lookup2 (AST slice), prepared | 71133 / 71243 / 70829 | 71565 / 71349 / 71151 | +0.4% |
| mix, simple | 13091 / 13142 / 13128 | 13201 / 12980 / 13093 | −0.2% |
| mix, prepared | 13166 / 13238 / 13179 | 13255 / 13120 / 12967 | −0.6% |

All within the spread of one binary's own rounds. The statement mix in-process
(`time_the_statement_mix`, µs per statement, three rounds, main's test binary then
this one's, the corpus before its growth):

| mix | text passes | AST path, no cache | `rewrite()`, cache warm |
|---|---|---|---|
| all 15450, main | 17.1 / 17.0 / 17.3 | 18.1 / 18.0 / 18.2 | 5.28 / 5.16 / 5.21 |
| all 15450, stage 2 | 17.2 / 16.9 / 17.2 | 16.2 / 16.6 / 15.9 | 5.19 / 5.28 / 5.12 |
| the 12068 that read or write rows, main | 17.0 / 16.7 / 16.7 | 15.8 / 15.7 / 15.6 | 0.20 / 0.18 / 0.19 |
| the 12068 that read or write rows, stage 2 | 16.8 / 16.6 / 16.5 | 13.3 / 13.2 / 13.4 | 0.18 / 0.19 / 0.19 |

The uncached AST route is 1.5-2.4 µs faster per statement: more statements skip the
text passes, and none pays the two token scans after the printer.

**Exit criteria.** `make check` green with the AST path on, and the differential test
at zero unexplained: met. The stage's share of the corpus translated: 644 statements
more, every cast and LIKE ESCAPE of the slice's statements among them. "Those passes
deleted; `literals_survive_the_whole_rewrite` loses their words": not met, by the
order of this plan: `result_labels`/`apply_labels`, the regexes of `param_types.rs`,
`casts`, `typed_cast`, `bool_cast`, `null_order`, `offset` and `like` still serve the
7785 statements of the corpus the slice leaves to the text passes, and the fast path
(`null_order`, `like`). They no longer run on a statement the AST path translates,
which is what stage 2 can do while the text passes route exists; they go when that
route does (stage 5), or each as soon as `rewrite_full` counts no statement needing
it.

## Stage 3 result

A statement the AST path translates with a session's context is now typed from its
tree and the catalog, and the printer writes from the types what the engine's type
passes wrote from guesses; those passes (`resolve/time_zone.rs`'s AT TIME ZONE and
offset values, `tsvector.rs`, `temporal.rs` with `numeric.rs`, `booleans.rs`) no longer
run on its output. They still run for the statements on the fast path, those left to
the text passes and those translated without a context (the session's own probes).

- **The environment.** The engine keeps each database's columns with their declared
  types (`table_types.rs`, `TableTypes`: which booleans may hold text, which column is
  the rowid), read once and dropped by every CREATE, ALTER or DROP this server runs and
  by a rollback. The session gives each translation a context (`ast/env.rs`): that
  snapshot and the search_path, under whose stamp the translation cache keeps the
  result. `ast/types.rs` types each expression: a column by the table of its own scope
  through aliases, joins, subqueries in FROM and CTEs, an expression by Postgres' result
  types for the operators and functions of the slice, a literal by its form and then by
  what it meets. A typed translation ends in the engine's mark (`TYPED_MARK`), which
  also carries the result columns' integer widths.
- **What moved onto it**: numerics (exact arithmetic with a numeric side, the numeric
  form around a numeric that is not a column, a scalar subquery included, dec_greatest
  and dec_least, long literals); date, time and interval arithmetic and comparison
  (overlite_tadd/tsub/tmul with each side's kind, interval_cmp and the INTERVAL
  collation, a date beside a timestamp at its midnight, a literal or a parameter beside
  a date, timestamp or timestamptz read as one, in `=`, `<`, IN and BETWEEN); the
  session's TimeZone, written whatever the zone and read when the statement runs
  (overlite_tz_in, overlite_tz_text, and overlite_tz_wall, new, the wall clock in the
  zone), so one cached translation serves every session; AT TIME ZONE, EXTRACT,
  date_part and DATE_TRUNC by their operand's type; booleans met by a string,
  bool_and and bool_or (a statement reading a boolean column that may hold text goes
  to the text passes); tsvector length and `||`; `%` as similarity on text and a
  remainder on integers; json operators where the left side is known json; range
  operators and comparisons (overlite_range_op) and array comparison
  (overlite_array_cmp, new); the select-list casts that tell the protocol a computed
  column's type; integer widths of result columns; parameter types for Describe.
- **The slice, wider**: `~`, `~*`, `!~`, `!~*`; `->`, `->>`, `#>`, `#>>`, `@>`, `<@`,
  `?`, `?|`, `?&` over json; `-|-`, `<<`, `>>`, `&<`, `&>`, `&&` over ranges; AT TIME
  ZONE, EXTRACT, date_part, now() and its kin, CURRENT_DATE and CURRENT_TIMESTAMP.

**The corpus** (`make ast-corpus` added 377 statements: 16030), main d2c13ca against
stage 3:

| | fast path | text passes | AST path |
|---|---|---|---|
| before stage 3 | 3878 | 8032 | 4120 |
| stage 3 | 3878 | 5360 | 6792 |

The differential test now compares the typed translation with the text passes' output
finished by the engine's type passes (`type_passes_without_tables`), both under an
empty catalog, as the corpus' schemas are not here. Of the 6792: identical 4885,
identical but parentheses 1626, intended 281, unexplained 0. The intended differences
the types make, by reason (compare.rs, `TYPED` and `INTENDED`): 133 the session's zone
written whatever the zone; 33 AT TIME ZONE by its operand's type, its cast kept,
labelled `timezone`; 32 select-list items cast to a type the text pass did not read
(EXTRACT under an aggregate, ROUND with digits, a subquery's column); 3 temporal
arithmetic over a CASE or a parenthesized sum; 2 `~` that rewrite_full left in place;
1 each: an interval literal compared by span, a literal read as a timestamp, `%`
around the right operands, a range operator over ranges. The rest are stage 1 and 2's
(23 NOT IN, 19 jsonb spelling, 13 labels, 10 set members, 4 casts of casts, 2 LIKE,
2 parenthesized columns, 1 quoted name). Refusals that depend on the catalog (`%`,
json and range operators over what is not known, 190 under the empty catalog) are not
remembered by shape.

**Bugs fixed**, each a test in tests/wire/ast_types.rs that fails on d2c13ca and is
held to Postgres 17: a date column compared with a timestamp or timestamptz column
without the words the pass looked for; a literal compared with a date, timestamp or
timestamptz (`'2024-01-02'`, a `T`, `'Jan 2 2024'`, an offset in IN and BETWEEN, a
parameter); an interval column compared with `'23 hours'`; an interval times a CASE;
AT TIME ZONE of a CASE; `%` between text columns; a timestamp compared with a
timestamptz, and date_part of one, in a non-UTC session; ranges compared by bounds and
in another spelling, and the range operators that were syntax errors; arrays compared
element by element; integer widths.

**Client-visible changes** (NOTES.md, Behavior): integer result columns at their width
(int2, int4, int8) in typed statements, a rowid key int8; AT TIME ZONE labelled
`timezone`; COALESCE of a double and a numeric a double; ROUND with digits a numeric
(ROUND(365, -1) is 370); stddev and variance of integers numerics. pgx scans an int4
into int64, int32 and int as from Postgres; psycopg reads any width as int; Django's
inspectdb reads `SELECT * FROM t LIMIT 1`, which is on the fast path, unchanged.

**Performance.** pgbench as before (4 clients, 4 threads, 8 s, each binary on its own
copy of a 100,000-row `kv (id int PRIMARY KEY, v text, ts timestamptz, n
numeric(12,2))`, the order swapped in round 2), main d2c13ca against stage 3, tps per
round; `lookup3` is `SELECT id, v FROM kv WHERE id = :id AND ts >= '2024-01-01
00:00:00+00' AND n > 0.5`:

| script, mode | main | stage 3 | means |
|---|---|---|---|
| lookup (fast path), simple | 114632 / 113473 / 113940 | 112982 / 112815 / 114928 | −0.4% |
| lookup (fast path), prepared | 96892 / 96085 / 97914 | 96165 / 95682 / 96934 | −0.7% |
| lookup2 (AST slice), simple | 73882 / 72374 / 74287 | 80545 / 79241 / 81000 | +9.2% |
| lookup2 (AST slice), prepared | 69838 / 70760 / 69628 | 85161 / 86051 / 81790 | +20.3% |
| mix, simple | 12996 / 12878 / 13080 | 13359 / 13245 / 13438 | +2.8% |
| mix, prepared | 12887 / 13012 / 13022 | 13723 / 13703 / 12869 | +3.5% |
| lookup3, simple | 71640 / 71419 / 68231 | 77913 / 80216 / 79496 | +12.5% |
| lookup3, prepared | 67303 / 67862 / 67039 | 84588 / 85730 / 84707 | +26.1% |

The fast path is within the spread of its rounds. A typed statement no longer pays the
engine's type passes and their probes on each execution, which is the gain of lookup2
and lookup3.

**Exit criteria.** `make check` green, the differential at zero unexplained, the
corpus share translated up by 2672: met. "Those passes and their engine counterparts
deleted": not met. The engine's type passes still serve the fast path, the text
passes' 5360 statements and the context-less translations; they go with those routes.
Also not done: the fast path does not consult the environment, so a plain lookup such
as `WHERE tz > '2024-01-02'` keeps its text comparison and int8; ARRAY_AGG and array
literals are not in the slice; a function the environment does not type, and json,
range or `%` operators over what the catalog does not know, go to the text passes;
sum(bigint) and avg(int) stay int8 and double.

**Stage 4 should start with** the fast path: `needs_no_rewrite` answers for the text
passes, and now that a typed translation differs from the text, a lookup over a date,
time, timestamptz, boolean, array or range column should leave the fast path when the
environment would print it differently (a per-table flag in `TableTypes` keeps `kv`
lookups where they are). Then the planned query shapes, with arrays (ARRAY[...],
`= ANY`, ARRAY_AGG and its typing) among them.

## Stage 4 result

Every statement that reads or writes rows is the AST path's now, but for the
catalog's (stage 5) and a short list below; and a plain statement leaves the fast path
where the typed translation would say something else of it.

- **The fast path, typed.** `TableTypes` keeps the names of the columns whose type the
  typed translation reads (a date, time, timestamp, interval, array or range, an
  integer narrower than a bigint, which goes out at its width) and the tables with a
  narrow integer; `rewrite::fast_path` sends a statement naming one (or a star over
  such a table) to the AST path. Word lengths and first bytes are masked before a
  word is hashed, so a lookup over other columns pays almost nothing: `kv`'s `SELECT
  id, v ... WHERE id = :id` stays on the fast path though `kv` has a timestamptz.
- **Arrays.** ARRAY[...] (json_array), casts to an array type (the engine's
  overlite_assign, whatever the operand), `= ANY` and `<> ALL` over a list, an array
  or a subquery, subscripts of any expression and slices of a column, ARRAY(subquery),
  array_agg as the JSON array arrays are stored as (with DISTINCT, ORDER BY, FILTER,
  OVER; NULL over no rows; a boolean as JSON's true or false), unnest and the other
  set-returning functions in FROM, array_length and cardinality typed, `@>`, `<@`, `&&`
  (json_contains, overlite_overlaps) and `||` (overlite_array_cat, new). The mark
  carries an array's element type, a boolean and an integer width, so a computed
  column goes out typed (an int[] as int4[], 1007).
- **Query shapes.** Set members with their own ORDER BY, LIMIT or WITH and a set
  operation on the right (read from a subquery); DISTINCT ON (ROW_NUMBER() over the ON
  keys, LIMIT after it); window functions, named windows and every frame; FILTER and
  an aggregate's ORDER BY; set-returning functions in FROM, in a subquery naming their
  columns, or in place where they read a table on their left, their columns renamed
  (`u.x` is `u.value`); derived column lists (`_aliasN` CTEs); ON CONFLICT (a SELECT
  before it gets `WHERE true`); UPDATE ... FROM; DELETE ... USING (by rowid); a write's
  WITH and alias, RETURNING by the table's name; row locks (dropped: the session takes
  them); ctid as a table's rowid; the SQL/JSON constructors, COLLATE, TRIM, the
  niladic user and schema functions, `@@`, X'..'.
- **Functions.** Any function a text pass does not rewrite by name is written as it
  came (`emit::REWRITTEN` lists those it does; `the_printer_knows_every_function_the_
  text_passes_rewrite` runs every word of the sources as a call through `rewrite_full`
  and fails on one missing). The environment types the functions overlite provides
  by Postgres' signatures: window functions, string, json, fts, range, array and
  sequence functions; avg of integers and sum of a bigint are numerics (avg exact,
  `sum / count` at Postgres' scale), a real beside a numeric a double.
- **What the catalog does not know** is read as the text passes read it: a json
  operator over a column of an unknown table, `&&`, `%` and the range operators.

**The corpus** (`make ast-corpus` added 211 statements: 16241). The differential test
now reads a statement that wraps a query (EXPLAIN, DECLARE, `COPY (query) TO`) by that
query, which is what the session translates. Main 8a056a5 against stage 4, both on
this corpus:

| | fast path | text passes | AST path |
|---|---|---|---|
| before stage 4 | 4068 | 5379 | 6794 |
| stage 4 | 4115 | 3973 | 8153 |

Of the 8153: identical 5664, identical but parentheses 1934, intended 555, unexplained
0. The new intended differences, by reason (compare.rs, `TYPED` and `INTENDED`): 47
Postgres' syntax the text passes left (a subscript of an expression, a slice,
ARRAY((subquery)), `<> ALL (subquery)`, `@>` of arrays); 36 a row lock on lines of its
own; 26 a cast to an array type; 19 `WHERE true` before ON CONFLICT; 19 array_agg; 6
ctid through a subquery or a quoted `"ctid"`; 5 avg of integers; 3 DISTINCT ON before
LIMIT; 2 each a window's NULLS and EXCLUDE NO OTHERS, a jsonb aggregate's FILTER, a
path operator over a cast, a subscript's label; 1 a set operation on the right; and
the numeric rule now also undoes exact arithmetic beside a float.

**What the text passes still translate**, 3973 statements, by the first node the
printer refuses: 3302 utility statements (CREATE TABLE 816, ALTER TABLE 815, CREATE
INDEX 571, DROP 136, GRANT 106, SET 92, COMMENT 65, RENAME 60, CREATE SCHEMA 52,
CREATE DATABASE 50, FETCH 48, the rest under 47 each), 310 catalog relations, 112 parse
errors (statements Postgres rejects too), 81 system columns, 75 functions, 29
operators, 21 cast types, 9 multi-statement strings, the rest under 9 each. Of the 558
that read or write rows, 461 read the catalog (its relations, pg_get_*def,
obj_description, oid, xmin, reg* casts): stage 5's. The other 97 (Django 36, the
rewrite corpus 16, the wire suite 45): a set-returning function in a select list
(`SELECT unnest(a)`, most of the 29 functions), the bitwise and math operators (`#`,
`^`, `&`, `|`, `<<`, `>>`, `|/`) and pg_trgm's `<%` and `<->` (29), a range operator
over an element or with no side known to be a range, and json containment of an
unknown column by a scalar (12), multi-statement strings (9), a slice of an expression
(3), a stored name with a dot (3), OVERRIDING, RETURNING beside FROM or USING, a
partial derived column list, `SET (a, b) =`, a join's alias (1-2 each).

**Bugs fixed**, each a test in tests/wire/ast_shapes.rs that fails on 8a056a5 and is
held to Postgres 17: a plain lookup comparing a timestamptz or date column with a
literal compared texts, and sent an int column as int8 (the fast path); subscripts of
expressions, slices, `||` of arrays, `<> ALL (subquery)`, ARRAY((subquery)); array_agg
read by array_length and a subscript, its elements quoted; avg of integers and sum of
a bigint; DISTINCT ON before LIMIT; `1 UNION 2 INTERSECT 3`; a jsonb aggregate's
FILTER; a row lock on its own lines; `"ctid"`. Found on the way: exact arithmetic beside
a real (stage 3's printer made `r * 1.5` a numeric, Postgres a double).

**Client-visible changes** (NOTES.md, Behavior): avg of integers a numeric at
Postgres' scale, sum of a bigint a numeric; an int[] goes out as int4[] with four-byte
elements in binary; array_agg, slices, `||` of arrays, akeys and avals as arrays of
their element type; a comparison or test in a select list as a boolean (isempty too);
a subscript labelled by its array; a statement that names a typed column leaves the
fast path (`SELECT * FROM t`, which Django's inspectdb reads, sends an int column as
int4; a serial key, the rowid, stays int8); `CAST('["a"]' AS TEXT[])` reads the array (tests/wire/
parity_diff.rs counts those 30 outcomes apart from the old server's).

**Performance.** pgbench as before (4 clients, 4 threads, 8 s, each binary on its own
copy of the 100,000-row `kv`, the order swapped in round 2), main 8a056a5 against
stage 4, tps per round:

| script, mode | main | stage 4 | means |
|---|---|---|---|
| lookup (fast path), simple | 114204 / 115306 / 113949 | 114428 / 114759 / 114781 | +0.1% |
| lookup (fast path), prepared | 98474 / 97491 / 97543 | 97604 / 98261 / 97649 | +0.0% |
| lookup2 (AST slice), simple | 81647 / 81538 / 82065 | 80647 / 80815 / 80744 | −1.2% |
| lookup2 (AST slice), prepared | 88116 / 88195 / 88105 | 87522 / 87876 / 87522 | −0.6% |
| mix, simple | 13586 / 13668 / 13649 | 14199 / 14164 / 14126 | +3.9% |
| mix, prepared | 14168 / 14014 / 14066 | 14534 / 14457 / 14571 | +3.1% |
| lookup3, simple | 80661 / 80589 / 80807 | 80574 / 80047 / 80124 | −0.5% |
| lookup3, prepared | 86246 / 86435 / 86156 | 86354 / 85730 / 86234 | −0.2% |

The mix gains: its `= ANY(ARRAY[...])`, string_agg and json_build_object are the AST
path's now. The fast path, with the typed check, is level; a first measurement
without the masks was −1.1% on it. lookup2 simple, which parses and prints every
statement (pgbench writes the value into the text), loses about 1%, consistent over
the rounds and over a rerun after the printer stopped allocating per column
(80913 / 80771 / 80489 against 81707 / 81718 / 81848): the printer does more per
statement now (scopes for ctid and the set-returning functions' renames). The
statement mix in-process (`time_the_statement_mix`, µs per statement, three rounds,
main's test binary then this one's, both on the grown corpus):

| mix | text passes | AST path, no cache | `rewrite()`, cache warm |
|---|---|---|---|
| all 16241, main | 16.6 / 16.4 / 16.4 | 14.6 / 14.6 / 14.6 | 5.01 / 5.01 / 5.06 |
| all 16241, stage 4 | 16.3 / 16.2 / 16.3 | 11.5 / 11.6 / 11.6 | 4.24 / 4.16 / 4.05 |
| the 12686 that read or write rows, main | 16.2 / 16.0 / 15.3 | 11.8 / 11.5 / 11.6 | 0.22 / 0.20 / 0.21 |
| the 12686 that read or write rows, stage 4 | 16.4 / 15.8 / 15.7 | 9.3 / 9.3 / 9.3 | 0.21 / 0.20 / 0.20 |

The release binary grew from 15.16 MB to 16.25 MB.

**Exit criteria.** `make check` green, the differential at zero unexplained: met.
"Every DML statement of the corpus translated": not met in full. 461 of the 558 DML
statements left read the catalog and are stage 5's by this plan; 97 are the shapes
listed above, of which the applications' corpora hold 36, all Django's (its tests of
the bitwise operators, pg_trgm, range lookups); Wagtail, Gitea and the owner's apps
leave none outside the catalog. The
passes are not deleted: they serve those statements and the utility statements until
stage 5.

**Stage 5 should start with** the catalog's statements, which are now most of what
reads rows on the text passes: pg_catalog and information_schema relations and their
columns (oid, xmin, the reg* casts), pg_get_*def, obj_description and
col_description, as nodes of the printer with the catalog views the engine already
has. Then the utility statements on the tree (`route_of` and the session's
classifiers), CREATE TABLE/INDEX/ALTER with `ddl_syntax` and `default_expr`, and
`rewrite_full` deleted with the scanners. The leftover row shapes above (select-list
set-returning functions, the math operators) are small and could go first.

## Stage 5a result

Every statement of the corpus that reads or writes rows is the AST path's now, the
catalog's included, but for what Postgres itself refuses and three named below; the
text passes keep DDL and the utility statements (stage 5b).

- **The catalog.** pg_catalog's relations are the engine's views under their bare
  names, information_schema's under the dotted name of their temp view (the names
  rewrite_information_schema knew; any other is written as it came), a column
  qualified by either the same (`catalog_relation`). oid, tableoid and xmin are
  written as they came: the views have them, and a user table's `oid` is a column.
  A reg* cast (regclass, regtype, regproc, regprocedure, regnamespace, regrole) is a
  lookup in the view its type names (`reg_cast`): a number is the oid, a system
  catalog's name its fixed oid, a quoted name the object of that name, anything else
  an oid written as its object's name; regoperator, oid and the rest are casts the
  engine reads. The functions the text passes rewrote by name (`CATALOG_CALLS`:
  obj_description, shobj_description, col_description, pg_get_indexdef,
  pg_get_constraintdef, the partition sets, pg_options_to_table) are written as they
  wrote them, in FROM too, WITH ORDINALITY included; array_to_string of
  ARRAY(subquery) is its rows joined; `OPERATOR(pg_catalog.op)` is op; a catalog
  boolean met by a string is its 1 or 0 where the environment does not type the
  column. Everything else (pg_get_expr, format_type, pg_table_is_visible, the
  has_*_privilege family...) was already written as it came. Describe types a
  parameter meeting a catalog column (relname name, oid oid, `= ANY` their arrays)
  from the tree's references.
- **The leftover shapes.** Set-returning functions in a select list (the set joined
  after FROM, lateral; several zipped, as rewrite_set_returning wrote them); `&`, `|`,
  `<<`, `>>` as SQLite's (a new precedence level between comparison and `+`), `#` and
  `^` as overlite_xor and power, `|/`, `||/`, `@`, `~` as calls or SQLite's, typed at
  Postgres' widths; pg_trgm's `<%`, `%>`, `<<%`, `%>>` and its distances; tsquery's
  `&&`, `||`, `!!` (overlite_tsquery_op, new); containment by what the catalog does not
  know decided by the other side, as Postgres' resolution would: a range or a range's
  literal containing a known scalar is json_contains (which reads range @> element at
  run time), `-|-`, `&<`, `&>` are ranges' alone, `<<`/`>>` beside a range's literal a
  range's; a slice of any expression (overlite_array_slice, new); an array cast to text
  in Postgres' form (overlite_array_text, new); several statements in one Parse, each
  translated; a quoted name with a dot written whole; OVERRIDING SYSTEM VALUE dropped
  (the session's identity_insert takes it out before translating); RETURNING beside
  FROM or USING (an item reading a joined relation a subquery over the join for the
  row, as rewrite_update_from wrote it); `SET (a, b) = (...)` and `= (SELECT ...)`; a
  join alias; a star under a derived column list, through the catalog; jsonb_typeof and
  json_typeof by Postgres' names; ctid through a CTE.
- **Found on the way**: a star over a FROM whose set-returning function is written in
  place (stage 4) lost the function's named columns, which Django's get_constraints
  reads through `SELECT * FROM pg_index i, unnest(...) koi(key, ...)`: the star now
  carries them (`star_renames`), as rewrite_lateral did.

**The corpus** (`make ast-corpus` added 65 statements: 16306), main 9142966 against
stage 5a, both on it:

| | fast path | text passes | AST path |
|---|---|---|---|
| before stage 5a | 4164 | 3988 | 8154 |
| stage 5a | 4164 | 3427 | 8715 |

The 561 statements moved are 600 that read or write rows, less the 39 named below.
Per source (text passes / AST path): django 430/2619 to 382/2667, wagtail 181/536 to
172/545, gitea 511/486 to 508/489, owner 27/37 to 24/40, rewrite 88/133 to 20/201,
wire 2751/4343 to 2321/4773.

Of the 8715: identical 6072, identical but parentheses 1972, intended 671 (556
before), unexplained 0. The new intended differences, by reason (compare.rs): 55 a
reg* literal's name read as Postgres reads a name (an unquoted one folded and compared
without case, the catalog keeping names as created; `schema."Name"`, which the text
pass cut at its last quote); 9 tsquery's operators; 8 labels of a CASE named by its
ELSE through casts and calls (psql reads by position); 8 labels of each statement of a
string holding several; 6 range operators over what the text pass took for json; 5
set-returning functions in place where the text pass wrote a subquery; 4 Postgres'
syntax the text pass left (a slice of an expression); 4 array_agg; 3 casts to an array
type; 3 quoted names with a dot (`"information_schema.tables"`, whose quotes the text
pass doubled); 2 OVERRIDING SYSTEM VALUE; 2 jsonb_typeof; 1 each: an array cast to
text, array_to_string of ARRAY(subquery) beyond the first, a partition set WITH
ORDINALITY, ctid through a CTE, a label `?column?`, the session's zone.

**What the text passes still translate**, 3427 statements: 3307 utility statements
(ALTER TABLE 873, CREATE TABLE 824, CREATE [UNIQUE] INDEX 571, DROP 185, GRANT and
REVOKE 152, SET 95, other ALTERs 82, COMMENT 65, CREATE SCHEMA 52, DATABASE 50, ROLE 33,
POLICY 30, FUNCTION 30, SEQUENCE 20, the cursors' FETCH, MOVE and CLOSE 57, COPY 30,
TRUNCATE 25, PREPARE and EXECUTE 24, the rest under 20 each), 81
utility texts Postgres cannot parse, and 39 that read or write rows:

- 35 Postgres refuses too: 31 parse errors (a `%s` or `?` placeholder never
  substituted, a truncated statement, `current_user()`), `json_object('a', 1)` (42883),
  `0=%s` (`=%` is no operator), `?1` (no prefix `?`), and two derived column lists
  longer than their columns (42P10).
- 2 `SELECT * FROM (SELECT * FROM t) AS v(a)` over a table the empty catalog of the
  test does not know: a refusal by the catalog (`TYPE_REFUSAL_STAR`, not remembered by
  shape), translated in a server that has t.
- 1 Django `(SELECT ...) UNION (SELECT ...)` of 101 members: prost's decode stops at
  its recursion limit of 100 levels (`SelectStmt.larg` nests once per member; 99
  members fail, 60 pass), a "decode error".

**Bugs fixed**, each a test in tests/wire/ast_catalog.rs that fails on 9142966 and is
held to Postgres 17: psql's `\d` of any table with a foreign key failed on its trigger
listing (`pg_partition_ancestors(...) WITH ORDINALITY`, a syntax error in SQLite); an
array cast to text gave the stored JSON (`[1,2]`, now `{1,2}`); a slice of anything but
a column was a syntax error; jsonb_typeof said `integer`, `text`, `true`; tsquery's
`&&` was an overlap, `||` a concatenation of texts, `!!` a syntax error;
`'MyTab'::regclass` missed `mytab` and `'public."My T"'::regclass` missed `My T`;
`5 & 3` went out as int8. tests/wire/srf.rs held a star under a shorter derived column
list to 0A000; it now answers as Postgres, over a table the catalog knows.

**Client-visible changes** (NOTES.md, Behavior): the fixes above; the bitwise
operators at their operands' width; a CASE labelled by its ELSE.

**Performance.** pgbench as before (4 clients, 4 threads, 8 s, each binary on its own
copy of the 100,000-row `kv`, the order swapped in round 2), main 9142966 against
7d134dc (937816d after it touches only a FROM's partition sets), tps per round:

| script, mode | main | stage 5a | means |
|---|---|---|---|
| lookup (fast path), simple | 114927 / 114172 / 112399 | 114353 / 114764 / 114631 | +0.7% |
| lookup (fast path), prepared | 96763 / 96825 / 97340 | 97525 / 97494 / 95849 | −0.0% |
| lookup2 (AST slice), simple | 81058 / 80894 / 79953 | 80558 / 80550 / 80309 | −0.2% |
| lookup2 (AST slice), prepared | 87691 / 87668 / 86841 | 87385 / 87861 / 86750 | −0.1% |
| mix, simple | 14148 / 14058 / 13953 | 14056 / 14088 / 14101 | +0.2% |
| mix, prepared | 14614 / 14426 / 14396 | 14514 / 14530 / 14535 | +0.3% |
| lookup3, simple | 79983 / 79763 / 79992 | 79351 / 79570 / 79756 | −0.4% |
| lookup3, prepared | 86188 / 84079 / 84700 | 85996 / 83164 / 85540 | −0.1% |

All within a binary's own spread: none of these statements changed route. The
catalog's: psql 18 over one connection, 100 times `\d a` and `\d c` (a table with a
key, a unique and a check, and a plain one) and 200 times `\dt`, each binary on its
own copy of the database, in turn, three rounds, seconds (the outputs of the two
binaries identical):

| loop | main | stage 5a |
|---|---|---|
| `\d a`, `\d c` ×100 | 1.139 / 1.097 / 1.086 | 0.983 / 0.988 / 0.994 (−10%) |
| `\dt` ×200 | 0.094 / 0.095 / 0.096 | 0.092 / 0.090 / 0.086 (−5%) |

`\d b` (a foreign key) failed on main; it answers now.

**Exit criteria.** `make check` green, the differential at zero unexplained, every
statement that reads or writes rows translated but those Postgres refuses, one the
test's empty catalog cannot type, and one too deep to decode: met. `rewrite_full` is
not deleted: it serves the utility statements (5b).

**Stage 5b, the utility statements on the tree.**

1. Measure first what reaches `rewrite_full` in a server, not in the corpus: most
   utility statements are the session's before `rewrite` (transaction control,
   savepoints, schema, role, GRANT, RLS, LISTEN/NOTIFY, sequences, types, COMMENT, SET,
   PREPARE/EXECUTE, COPY, cursors). Count, in a test build over the wire suite, the
   statements `rewrite` hands to `rewrite_full` by node kind (record them beside
   OVERLITE_RECORD_STATEMENTS), and give the differential test a mode over them.
2. Classify on the tree: `route_of`, `handle_simple_query`'s chain and the
   `ddl::words` readers (`unquote_ident` and its kin) read the parsed node kind and its
   names, from one parse per statement shared with the translation (the cache keyed by
   text holds the parse result too, or the route). The literal-blanking classifiers
   (`code_only` callers) go with it.
3. CREATE TABLE as a printer node: column types as `ddl_syntax` writes them (serial,
   citext, numeric as DECIMALTEXT with its collation, arrays, identity's comment),
   DEFAULT through `default_expr`'s rules (a printed expression, folded constants),
   CHECK, UNIQUE and PRIMARY KEY with their deferral comment, FOREIGN KEY, EXCLUDE
   through ddl/exclude, TABLESPACE dropped; the comments `catalog/constraints.rs`
   reads back are the contract, so the differential compares the stored DDL.
4. CREATE [UNIQUE] INDEX (USING's comment, INCLUDE, NULLS NOT DISTINCT, operator
   classes, WHERE, IF NOT EXISTS), then ALTER TABLE: `alter_actions` splitting by the
   tree's cmds, the rebuild path given nodes instead of text, column IF [NOT] EXISTS.
5. The rest by count: DROP, TRUNCATE, COMMENT, GRANT/REVOKE, CREATE/ALTER/DROP of
   schema, database, role, policy, sequence, type, extension, collation, view and
   materialized view (whose query is a SELECT the printer has), CREATE FUNCTION (its
   SQL body through the printer when it is inlined). Exit: over the recorded server
   statements, `rewrite_full` receives none but those Postgres refuses.

**Stage 5c, deleting the text route.**

1. Answer what Postgres refuses from the tree: a parse error with Postgres' message
   and position (42601) without SQLite; an `Unsupported` that is Postgres' own
   refusal (no such operator 42883, a derived column list too long 42P10) a typed
   refusal carrying its SQLSTATE, sent as the error. The catalog-dependent refusals
   (`TYPE_REFUSAL_*`: `%` and json, range or a star over what the catalog does not
   know, a boolean column holding text) need an answer each: a run-time dispatch as
   json_contains does, or the readings of resolve/booleans.rs written by the printer.
2. Deep trees: prost's recursion limit stops a 99-member UNION. Decode with prost's
   `no-recursion-limit` and bound the depth ourselves (libpg_query's stack check, the
   printer's recursion on a thread with a known stack), with a wire test of a
   200-member UNION and the panics test's nesting.
3. The untyped translation (the session's probes, with no context) has to cover what
   the typed one does: give the probes a context, or run the differential untyped too.
4. Then delete `rewrite_full` and its passes, `holding_literals`,
   `literals_survive_the_whole_rewrite`, `rewrite::labels` (result_labels,
   apply_labels), `param_types`' regexes and `type_by_text`, `text_tail` once the fast
   path's statements are printed (or the fast path itself, if the cache makes it moot:
   measure lookup simple and prepared), the engine's type passes
   (`resolve_types`: temporal, numeric, booleans, tsvector, time_zone) once nothing
   reaches the engine untyped, `rewrite/tokens.rs` and the blanking scanners
   (`blank.rs`, `resolve/names.rs`) where nothing reads raw text, and OVERLITE_AST,
   OVERLITE_AST_SHADOW and the differential test with them: the corpus stays, as a
   snapshot of the printer's output reviewed on change.

## Stage 5b result

DDL is translated from the tree, and the session routes every statement by its node;
what still reaches `rewrite_full` in a running server is what Postgres' grammar does not
read and nine statements whose typing depends on the catalog.

**Measured first** (`make text-passes`: OVERLITE_RECORD_TEXT_PASSES records what
`rewrite` hands to `rewrite_full`, `count_what_reached_the_text_passes` counts it by
node kind), over the wire suite less its panics test, statements and distinct ones:

| node kind | 6491d37 | stage 5b |
|---|---|---|
| CreateStmt | 759 / 471 | 0 |
| AlterTableStmt (ADD COLUMN) | 145 / 108 | 0 |
| IndexStmt | 73 / 67 | 0 |
| DropStmt (table 36, index 3, trigger 2, function 2, view 1) | 59 / 44 | 0 |
| CreateFunctionStmt | 34 / 30 | 0 |
| ViewStmt | 17 / 13 | 0 |
| RenameStmt (column) | 18 / 12 | 0 |
| ConstraintsSetStmt, VacuumStmt, CreateTableAsStmt | 13 / 6 | 0 |
| SelectStmt | 17 / 11 | 17 / 11 |
| parse error | 86 / 43 | 22 / 15 |
| total | 1221 / 805 | 39 / 26 |

The corpus overstated it as expected: of its 3307 utility statements only these kinds
reached the text passes; the rest are the session's before `rewrite`. 28 of the 43
"parse errors" were the server's own texts: the identity key the session wrote as
`INTEGER ... AUTOINCREMENT` before translating, a column's definition a rebuild
translated alone, and COPY's bare table name.

**What moved.**

- `ast/ddl.rs`: CREATE TABLE, CREATE INDEX, ALTER TABLE (ADD COLUMN, ALTER COLUMN TYPE
  ... USING, SET DEFAULT, ADD CONSTRAINT CHECK; the rest as written), DROP of tables,
  views, indexes and triggers, RENAME, CREATE VIEW, CREATE TABLE AS; a function's DDL,
  SET CONSTRAINTS and VACUUM as written. Not a printer of the whole statement: the
  stored DDL is a contract with the catalog and with existing databases, so the
  statement is kept as the client wrote it and only what SQLite does not take is
  rewritten, placed by the tree, its extent read by Postgres' scanner
  (`pg_query::scan_tokens`, comments, literals and quoted names its own): a column's
  type (serial, numeric as DECIMALTEXT with its collation, citext, an array's sizes, a
  modified element type quoted, `pg_catalog.`), COLLATE, a DEFAULT (a literal bare,
  anything else in parentheses, a cast of a literal folded, a timestamptz's offset
  literal in UTC), CHECK, a generated column's, an index's keys and WHERE through the
  printer (`emit::expression`: untyped, SQLite's CAST and `datetime('now')` in a
  table's DDL as the text passes wrote them; an expression that prints as the same
  SQL token for token is kept as written), a key's deferral and an index's method,
  INCLUDE and NULLS NOT DISTINCT in the comments the catalog reads, TABLESPACE,
  CONCURRENTLY, WITH (...), UNLOGGED and ALTER TABLE's IF EXISTS / ONLY dropped. The
  session's identity key is written `serial ... PRIMARY KEY`, which the grammar reads;
  a column a rebuild adds is translated inside a CREATE TABLE.
- `ast/classify.rs`: the session's classifiers (transaction control, savepoints,
  cursors, the DDL of schemas, roles, sequences, types and objects, databases, GRANT,
  RLS, COMMENT, SET ROLE, PREPARE, COPY, TimeZone, search_path, EXPLAIN, EXCLUDE) read
  the node, their words only where the grammar does not read the statement (SQLite's
  dialect, which is still served). A statement that reads or writes rows is none of
  them and is not parsed for them. `parse::shared` (the privilege check's per-thread
  cache of trees, moved) gives the classifiers and the privilege check one parse; the
  translation reads one they kept and keeps none of its own (`kept_or_new`: keeping
  each pgbench text cost lookup2 simple 5%). The handlers still read their statement's
  text (`ddl::words`, `unquote_ident`, `split_alter_actions`): stage 5c's.

**Differential.** DDL is compared as the text SQLite stores, before the engine's type
passes on both sides (`diff::is_ddl`). On the corpus grown by 34 statements (16340),
6491d37 against stage 5b:

| | fast path | text passes | AST path |
|---|---|---|---|
| 6491d37 | 4175 | 3442 | 8723 |
| stage 5b | 4175 | 997 | 11168 |

Of the 11168: identical 8489, identical but parentheses 1994, intended 685, unexplained
0. The new intended differences (compare.rs): 2 a TEMP or UNLOGGED table typed as any
table; 1 a column called tablespace; 1 a function's DDL without the NULLS LAST the
text pass wrote after its dollar-quoted body; 1 a cast to tsvector of a column in an
index key is its operand, as in every statement. `CAST(x AS int)` and `CAST(x AS
integer)` now compare equal (`cast_type_names`). The 997 left: 846 the session's
statement kinds (they never reach `rewrite`), 112 parse errors, 24 ALTER TABLE ... ADD
CONSTRAINT EXCLUDE and 6 materialized views (ddl/exclude.rs and ddl/objects.rs read
their text), 1 a partition, and the 8 that read rows as in stage 5a.

The classifiers have a differential of their own (`classifiers_read_the_tree_as_the_
text_did`): over the 16228 corpus statements the grammar reads, and 13 probes, each of
19 classifiers by the tree against its text. They agree on every corpus statement; the
probes find the one intended difference, SET LOCAL ROLE and SET SESSION ROLE.

**Bugs fixed**, each a test that fails on 6491d37 and is held to Postgres 17:
tests/wire/classify.rs: `SET LOCAL ROLE r` and `SET SESSION ROLE r` were a SET of
nothing, and what followed ran as the session's role (a superuser for most
connections) past grants and row-level security; they set the role now, LOCAL until
the transaction ends, a rollback undoes a SET ROLE, and SET LOCAL outside a
transaction warns (25P01), over both protocols. tests/wire/ast_ddl.rs: `id serial NOT
NULL PRIMARY KEY` (Django's and Wagtail's first migrations) was no rowid with a
sequence and refused every INSERT without its id (23502); `DEFAULT 1 + 2` and
`DEFAULT 'x' || 'y'` were syntax errors; a column called tablespace was cut out of its
table; a TEMP table's numeric lost its scale and its citext its case-insensitivity;
CREATE UNLOGGED TABLE was a syntax error; a numeric column added through a rebuild
lost its scale.

**Client-visible changes** (NOTES.md): the fixes; an expression of a table's DDL is
stored as printed where it is not the client's SQL token for token
(pg_get_constraintdef, column defaults, pg_get_indexdef show it); a function's DDL is
stored as written.

**Performance.** pgbench as before (4 clients, 4 threads, 8 s, each binary on its own
copy of the 100,000-row `kv`, the order swapped in round 2), 6491d37 against stage 5b
at 700e3d0, tps per round:

| script, mode | 6491d37 | stage 5b | means |
|---|---|---|---|
| lookup (fast path), simple | 111862 / 112746 / 112366 | 107393 / 115662 / 115768 | +0.5% |
| lookup (fast path), prepared | 93086 / 95785 / 96365 | 96105 / 96568 / 96319 | +1.3% |
| lookup2 (AST), simple | 80464 / 79258 / 79375 | 81941 / 81854 / 81897 | +2.8% |
| lookup2 (AST), prepared | 84944 / 86027 / 86268 | 86113 / 86493 / 86366 | +0.7% |
| mix, simple | 14021 / 13945 / 13899 | 14369 / 14370 / 14288 | +2.8% |
| mix, prepared | 14330 / 14230 / 14379 | 14323 / 14302 / 14307 | −0.0% |
| lookup3, simple | 78743 / 78890 / 79324 | 81555 / 81743 / 81714 | +3.4% |
| lookup3, prepared | 84459 / 84462 / 82792 | 85023 / 84194 / 83261 | +0.3% |

The first run, with the translation keeping its parses, had lookup2 simple at −5.4%
(78780 / 78500 / 78242 against 75141 / 71740 / 75835), mix simple −2.6%, lookup3
simple −2.4%; `kept_or_new` removed it. DDL: 1,000 statements (100 times CREATE TABLE
with serial, numeric, a defaulted timestamptz, an array and a CHECK, two CREATE INDEX
with an operator class, an expression and a WHERE, two ADD COLUMN, a RENAME COLUMN,
CREATE VIEW, and three DROPs) through psql over one connection, a fresh database each,
in turn, seconds: 6491d37 2.230 / 2.201 / 2.184, stage 5b 2.220 / 2.177 / 2.237: level.

**Exit criterion.** Over the wire suite's recorded statements, `rewrite_full` receives
39 (26 distinct), not met in full:

- Postgres refuses, 15 distinct: SQLite's own dialect the session serves (`CREATE
  TRIGGER ... BEGIN ... END` 3, `integer primary key autoincrement` 2, `EXPLAIN QUERY
  PLAN`, `LIMIT 0, 1`), syntax errors (`SELEC 1`, an unterminated literal or
  parenthesis, `SELECT 1 +`, `-> 1 ->`, `(SELECT 1; SELECT 2)`, a broken JSON_OBJECT,
  `"a\"b"`), and two derived column lists longer than their columns (42P10).
- The typed translation refuses by the catalog, 9 distinct: a boolean column holding
  text (`TYPE_REFUSAL_BOOLEAN`: `SELECT b FROM t ORDER BY id`, `SELECT i, f, b ...`,
  and two over storageformat.rs's `"Flag"`), a json operator over what is not known to
  be json (`TYPE_REFUSAL_JSON`: hstore's `?` twice and `->>`, `->` of an int[]), and
  `d AT TIME ZONE 'UTC'` of a date ("AT TIME ZONE of a time").

**Stage 5c, deleting the text route.**

1. The nine typed refusals: a boolean column holding text read by the printer as
   resolve/booleans.rs reads it (both forms); hstore's `?`, `?|`, `?&`, `->`, `->>` as
   the engine's hstore functions when the column is hstore, a json operator over an
   array as an array subscript or a refusal Postgres gives (42883); AT TIME ZONE of a
   date as of the timestamp at its midnight. A wire test each.
2. What Postgres refuses: answer a parse error from the tree with Postgres' message,
   position and 42601, and `derived column aliases` with 42P10, without SQLite. SQLite's
   own dialect (triggers with a body, AUTOINCREMENT, EXPLAIN QUERY PLAN, `LIMIT a, b`)
   is a decision: keep a minimal pass-through for what Postgres' grammar refuses (no
   translation: SQLite runs it as written, which is what every pass left of it), or
   refuse it as Postgres does and list the break in NOTES.md. The pass-through keeps
   behavior; the wire tests that send it (privilege_routes, triggers) say which.
3. The handlers' readers on the node: `ddl::words`, `unquote_ident`,
   `split_alter_actions`/`alter_actions` (from AlterTableStmt's cmds), `parse_copy`,
   `zone_change`, `role_target`, the schema, role, sequence, type, database, COMMENT,
   GRANT and RLS parsers, `create_serial_columns`, `create_identity_columns` and
   `record_nextval_defaults` (the column plan of a CREATE TABLE read from its
   ColumnDefs), ddl/exclude.rs and ddl/objects.rs (EXCLUDE, materialized views,
   partitions). Each against the corpus as classify's differential does, then the
   `*_text` classifiers go.
4. Deep trees, the untyped probes and the deletions as stage 5a's plan lists them
   (items 2 to 4 there): `rewrite_full` and its passes, `holding_literals`,
   `rewrite::labels`, `param_types`' regexes, `text_tail` (measure the fast path),
   the engine's type passes, `rewrite/tokens.rs`, `blank.rs`, `resolve/names.rs`,
   OVERLITE_AST and the shadow.

## Stage 5c result: the migration is done

There are no text passes. A client's statement takes the fast path, is translated from
its tree, or is refused: with Postgres' own error where Postgres refuses it, 0A000 for
a node the printer has no form for. SQLite's `CREATE TRIGGER ... BEGIN ... END` is the
one statement run as written.

**1. The nine typed refusals**, each answered by the printer now (tests/wire/
ast_refusals.rs): a boolean column holding text is read in both forms as
resolve/booleans.rs read it (`col = <truth value>` as `col IN` every spelling, any
other reading `overlite_bool(col)`, a whole select item as it is; only where the
catalog has such a column); hstore's `->` (overlite_hstore_get, new: a value as text,
of an array of keys their values), `?`, `?|`, `?&`, `@>`, `<@` over an hstore column,
its `->>`, which hstore has not, 42883 as in Postgres; a json operator over an array
42883 (`operator does not exist: integer[] -> integer`, with its position), as are
`%` and the range operators over known types Postgres has no operator for; AT TIME
ZONE of a date as of its midnight in the session's zone (timezone(text, timestamptz)
over the cast).

**2. What Postgres refuses.** A parse error is libpg_query's message and cursor
position (the ErrorResponse's P field, now sent with a HINT where there is one), with
the grammar's own codes where it raises another (`role name "public" is reserved`,
42939; the table in `parse::grammar_state`), answered before any classifier or grant
reads the statement (`ast::grammar_refusal`), as Postgres parses first; one that reads
or writes rows by its translation, which parses it anyway. A derived column list too
long is 42P10 with Postgres' message. A fast-path statement SQLite calls a syntax
error gets Postgres' error (`postgres_syntax_error`). SQLite's dialect, by the owner's
decision: refused as Postgres refuses it (AUTOINCREMENT, EXPLAIN QUERY PLAN, `LIMIT a,
b` with its hint, PRAGMA, ATTACH, VACUUM INTO, INSERT OR ..., DROP TRIGGER without ON,
`current_user()`, PREPARE of DDL), except its trigger, which is the only trigger
while PL/pgSQL does not run: `ast::sqlite_trigger` reads its head with the grammar,
the body replaced by `EXECUTE FUNCTION overlite_sqlite_trigger_body()` (SQLite's TEMP
and IF NOT EXISTS left out, its WHEN given parentheses), so both forms are a
CreateTrigStmt, told apart by the function: a PL/pgSQL to come takes a real one beside
it. The fast path, whose words took `INSERT OR IGNORE`, now also asks that the grammar
read each new shape of statement it takes (`rewrite::parses`, by `ast::shape`).

**3. The handlers' readers on the tree** (branch ast5c-handlers, merged): roles, GRANT
and REVOKE, owners, RLS DDL, schemas, SET ROLE, the no-op ALTERs, sequences, types and
enum columns, databases, COMMENT, COPY, TimeZone, TRUNCATE, extensions, collations,
materialized views and partitions, CREATE TABLE's serial, identity and nextval
columns, ALTER TABLE's target and actions; LISTEN/NOTIFY, PREPARE/EXECUTE, a SELECT's
row lock and the batch splitter on main. Each was held to its text reader over the
corpus in a test deleted with the reader (counts in each commit's body); ddl/words.rs
and the classifiers' `*_text` fallbacks are gone (`unquote_ident` stays for names
read from a table's stored DDL). Where a
handler writes back a piece of the client's SQL (an EXCLUDE's elements, a column's
definition) it cuts it with Postgres' scanner. The privilege-relevant ones have wire
tests (tests/wire/privilege_trees.rs), which found a privilege escalation: a role
creating `"secret copy"` was recorded as the owner of `secret`.

**4. Deletions.** prost decodes without its recursion limit, and `parse` refuses a
tree nested past 1000 protobuf messages before decoding it (54001; read without
recursion; an 8 MiB thread overflowed at about 5000): a 200-member UNION runs.
Deleted: rewrite_full and its 66 passes, holding_literals and
literals_survive_the_whole_rewrite, rewrite::labels, rewrite/tokens.rs, the
param_types regexes and type_by_text, text_tail (the fast path leaves ORDER BY and
LIKE to the printer), the engine's type passes (resolve/temporal.rs, numeric.rs,
booleans.rs, tsvector.rs, time_zone.rs) and resolve/names.rs, OVERLITE_AST,
OVERLITE_AST_SHADOW, the shapes remembered as outside, compare.rs and the
differential test, `make text-passes`. A translation without a session context reads
an empty catalog; the session's own SQL is translated with its context
(`rewrite_typed`). The corpus is a snapshot of the printer's output
(tests/fixtures/ast-corpus/printed.txt, `ast::snapshot`): of 16349 statements, 15170
translated, 2 refused as Postgres refuses them (42P10), 1177 left to the session or
refused (1008 the session's statement kinds, 133 parse errors, 24 EXCLUDE and 6 CREATE
TABLE AS forms the session's handlers take, 2 stars over tables the empty catalog does
not know, 4 others).

**The fast path, measured.** With fast_path answering false, pgbench's lookup (a point
lookup the fast path takes, its literal new each time, so a parse each) ran 93273 /
93807 tps in simple mode against 108509 / 109183 with it (−14%), and 91121 / 91058
against 90448 / 91005 prepared (level: a prepared statement is translated once). It
stays: what it sends is held to the
printer (rewrite/fastpath.rs runs every word of the crate's source, the wire corpus
and random statements through both and compares Postgres' deparse of each, which found
shapes it took and the printer reads otherwise: a name before a parameter or a literal
is a typed literal, `user`, a row in parentheses, SELECT INTO, shifts, a product in a
select list), and a column of any type the translation reads takes a statement off it
(TableTypes::typed_differs: all but text, a bigint or the rowid and a float).

**What still reads text**: the session's door (`statement_text`, rewrite/blank.rs and
strings.rs) and what follows it, the expansions (`apply_rls`, function inlining,
session identity, sequence calls, advisory locks), the parameter count and the fast
path's scan, through the engine's blank.rs; the privilege check's fallbacks
(`required_privileges`, the stored triggers' and views' readers); the readers of a
table's stored DDL (SQLite's text in sqlite_master); the engine's readers of the
printer's SQL (name resolution, column_types). These are not translations of a
client's statement, and are what a next step would put on the tree.

**Bugs fixed**, each with a wire test held to Postgres 17: the parse errors and
SQLite-dialect refusals above; hstore's `->` gives text and `->>` is refused; the json
operator over an array; the handlers' (NOTES.md, Behavior: ownership of a table named
with a space, GRANT/POLICY TO CURRENT_USER, a policy named with a space, two RLS
actions in one ALTER, folded channel and prepared names, COMMENT IS '', ALTER SEQUENCE
IF EXISTS and CYCLE, serial columns of a table named with a space, a DEFAULT cut at a
space, ALTER COLUMN "d e", ALTER TYPE citext, ALTER TYPE " ", COPY NULL 'csv header').

**Client-visible changes** (NOTES.md, Behavior; README, Limitations): Postgres' syntax
errors with position, SQLite's dialect refused but its trigger, 0A000 for what the
printer has no form for, 54001 past the nesting bound, names folded unless quoted.

**Performance.** pgbench as before (4 clients, 4 threads, 8 s, each binary on its own
copy of the 100,000-row `kv`, the order swapped in round 2), 6779015 against b79c9f4,
tps per round:

| script, mode | 6779015 | stage 5c | means |
|---|---|---|---|
| lookup (fast path), simple | 104721 / 105105 / 104573 | 108470 / 109164 / 108726 | +3.8% |
| lookup (fast path), prepared | 88533 / 88938 / 88780 | 90930 / 90863 / 90684 | +2.3% |
| lookup2, simple | 77220 / 77607 / 77181 | 79904 / 80106 / 79999 | +3.4% |
| lookup2, prepared | 80623 / 81354 / 80715 | 81261 / 81648 / 79521 | −0.1% |
| mix, simple | 13424 / 13236 / 13397 | 13928 / 13933 / 13761 | +3.9% |
| mix, prepared | 13323 / 13324 / 13193 | 13318 / 13408 / 13383 | +0.7% |
| lookup3, simple | 77292 / 77092 / 77162 | 80108 / 79416 / 78874 | +3.0% |
| lookup3, prepared | 78971 / 79506 / 79204 | 79331 / 79767 / 79717 | +0.5% |

The first run after the merge had every simple-mode script 8 to 17% down: the
handlers' tree readers parsed every statement, a SELECT included, and a Query ending
in a semicolon was scanned to be split; b79c9f4 removed both, and the simple path
skips the engine's type passes it ran before. DDL: the 1,000 statements of stage 5b
through psql, seconds: 6779015 2.186 / 2.174 / 2.181, stage 5c 2.190 / 2.188 /
2.167: level. Binary: 16,532,240 bytes at 6779015, 15,795,680 at stage 5c (−0.74 MB,
release, macOS arm64).

**Lines.** The crates and the binary, 6779015 to b79c9f4: 107 files changed, 4670 insertions(+), 19668 deletions(-). The wire
tests: 13 files changed, 716 insertions(+), 38 deletions(-). Rust in the repository: 104,405 lines to 90,086. Deleted
files: 26 of rewrite/ (all but mod.rs, cache.rs, fastpath.rs, blank.rs and
strings.rs), resolve/temporal.rs, numeric.rs, booleans.rs, tsvector.rs, time_zone.rs
and names.rs, ast/compare.rs, diff.rs and shadow.rs, ddl/words.rs. The corpus'
snapshot (printed.txt, 16,349 lines) is new.

**Exit criterion.** Nothing reaches a text pass because there are none: every corpus
statement is translated by the tree, passed through deliberately (SQLite's trigger),
refused with Postgres' error, or the session's own (snapshot above); the wire suite and
the privilege tests are green.


## Risks

- **Fidelity.** The tree drops what SQLite does not need but a later text reader might: the
  engine's `resolve` passes still read SQL text until stage 3, and read the printer's text
  now. `make check` runs with the AST path and the cache on and is green (the wire suite's 524
  tests included); the printer's style is plain
  SQL those readers already meet. Literals are printed from their values, so a literal's
  spelling can change (`E'a\nb'` becomes a string with a newline in it), never its meaning.
  Parentheses are SQLite's, not the client's.
- **Grammar version.** pg_query 18 parses PG 18; a client of an older server sends nothing it
  rejects. A newer client could send PG 19 syntax before pg_query 19: it fails as Postgres 18
  would.
- **Performance.** A statement off the fast path and not in the cache pays a parse. The
  cache is per thread (a client's thread), so it starts cold per connection; a shared cache
  is the next step if connection churn shows up.
- **Windows and cross builds.** bindgen at build time needs libclang for the build host and
  the target's headers (BINDGEN_EXTRA_CLANG_ARGS for the Docker arm64 cross build).
  The C code itself builds with MSVC per pg_query's build.rs. Stage 0 gates it.
- **Binary size.** +3.0 MB (11.74 to 14.78 MB); a server, not a concern, but noted for the Docker image.

## What changes in ARCHITECTURE.md at the end (done in stage 5c)

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
