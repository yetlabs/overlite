# Overlite vs PostgreSQL: what you get, what you won't

This document is for deciding **before** you migrate. It goes through PostgreSQL
subsystem by subsystem and says, for each, what Overlite gives you, what is planned,
and what it will never have because the data lives in SQLite.

Each table uses five words:

- **yes**: there, and held to PostgreSQL's answers by the test suite
- **partial**: there, with the differences named next to it
- **planned**: a known gap with work recorded against it (NOTES.md, ADR 0001)
- **no**: not there and not planned, though nothing in SQLite rules it out
- **never**: ruled out by SQLite's architecture

The full list of differences, with the reasons, is in [NOTES.md](NOTES.md#limits).
The README's [Limitations](README.md#limitations) has the short version.

## The short answer

Overlite speaks the PostgreSQL wire protocol over SQLite files. What is *language
and protocol* (SQL, types, catalogs, auth, client tooling) is emulated, and for
what applications send it is close: a web framework's own PostgreSQL test suite
and production applications behave as they do on PostgreSQL. What is *storage engine and process
architecture* (concurrent writers, row locks, replication, C extensions, planner
statistics) belongs to SQLite and will never be PostgreSQL's.

If your application talks to the database through an ORM, it is likely to work.
If it depends on PL/pgSQL, PostgreSQL-style triggers, PostGIS or pgvector, read
replicas, partitioned tables or many processes writing at once, Overlite is the
wrong tool.

## What has been measured

| Evidence | Result |
|---|---|
| A web framework's own PostgreSQL test suite | 99.5% of the tests that pass on PostgreSQL 17 also pass (18,087 of 18,179) |
| Production applications | migrations, fixtures, admin and day-to-day use behave as on PostgreSQL |
| Migration tooling | `migrate deploy`, `migrate status` and introspection (`db pull`) work |

Throughput against PostgreSQL is in the README's [Performance](README.md#performance)
table: reads keep pace, concurrent writes do not.

---

## Storage, concurrency and transactions

| Feature | State |
|---|---|
| Transactions, `BEGIN`/`COMMIT`/`ROLLBACK`, savepoints | yes |
| Readers do not block the writer (SQLite WAL) | yes |
| Advisory locks (`pg_advisory_lock` and family) | yes, held by the server across connections |
| `LISTEN` / `NOTIFY` | yes, between sessions of one server |
| `SELECT ... FOR UPDATE / SHARE`, `NOWAIT`, `SKIP LOCKED` | partial: every row lock is the database's write lock |
| Isolation levels | partial: accepted; a transaction reads like READ COMMITTED until it first writes |
| Deferred foreign keys, `SET CONSTRAINTS` | yes |
| Deferred `UNIQUE` / `PRIMARY KEY` | partial: recorded, but checked at each statement |
| More than one writer at a time | never |
| Row-level locks | never |

**How it works.** Each client gets its own thread and SQLite connection (100 at
most). `BEGIN` takes no lock; a transaction's first write takes SQLite's single
write lock and holds it until commit. A second writer waits, and after the busy
timeout (30 s) fails with `55P03`, as a PostgreSQL writer waiting on a row lock
would. Write skew cannot happen, because writes are serialized.

**What you lose.** Throughput under many concurrent writers: the benchmark in the
README shows 29k against PostgreSQL's 117k writes per second. `FOR UPDATE` on one
row waits for any writer, and `SKIP LOCKED` returns no rows while another
transaction holds the lock, so job queues built on `SKIP LOCKED` run one consumer at
a time.

## Planner, statistics, EXPLAIN

| Feature | State |
|---|---|
| Index selection, joins of every kind | yes (SQLite's planner) |
| `EXPLAIN`, `EXPLAIN ANALYZE`, `FORMAT TEXT/JSON/XML/YAML` | partial: SQLite's plan in PostgreSQL's shape, all costs and estimated rows 0 |
| `ANALYZE` | partial: accepted and does nothing |
| `VACUUM`, `REINDEX` | yes (SQLite's) |
| `pg_statistic` populated, cost estimates | never |
| Parallel query | never |

**Why.** The planner is the storage engine's. SQLite does not keep statistics in
PostgreSQL's shape or estimate costs, and it runs a query on one thread. Plans tuned
against PostgreSQL do not transfer; a large aggregation runs on one core.

## Procedural code and triggers

| Feature | State |
|---|---|
| `CREATE FUNCTION ... LANGUAGE sql` (run, listed by `\df`, shown by `\sf`) | yes |
| `LANGUAGE plpgsql` functions | no: accepted, never run; calling one fails |
| `CREATE TRIGGER ... EXECUTE FUNCTION` | no: refused with a syntax error |
| SQLite-syntax triggers (`CREATE TRIGGER ... BEGIN ... END`) | yes |
| `CREATE PROCEDURE`, `CREATE AGGREGATE` | no: accepted without effect; `CALL` is a syntax error |
| `SECURITY DEFINER` / `SECURITY INVOKER` on `LANGUAGE sql` | yes: a definer body reads with its owner's privileges and policies |
| `GRANT` / `REVOKE EXECUTE` on functions, `ALTER FUNCTION ... OWNER TO` | yes |
| PL/Python, PL/Perl, PL/v8 | never (they are extensions) |

**This is the sharpest edge.** A migration that creates a PL/pgSQL function
succeeds, and the function does nothing. A PostgreSQL trigger fails loudly at
`CREATE TRIGGER`, so you will find it before production, not after. Audit tables
and `updated_at` maintenance have to move into the application or into a SQLite
trigger. There is no plan to run PL/pgSQL.

## Replication, high availability, backup

| Feature | State |
|---|---|
| Streaming replication, hot standby | never |
| Logical replication, publications, CDC through replication slots | never |
| Point-in-time recovery at the PostgreSQL layer | never |
| `pg_dump` | yes |
| Backup at the SQLite layer | yes: the database is a SQLite file |

**Why.** These features read PostgreSQL's WAL, and SQLite's WAL is a different
format with nothing to decode into PostgreSQL's protocol.

**Instead.** Back up with `pg_dump`, or with SQLite's own tools on the file (its
online backup). A copy of the bare file is only complete with the server stopped,
because recent commits sit in the `-wal` file beside it until a checkpoint.
Replication tools for SQLite exist (Litestream, LiteFS); Overlite's tests do not
cover them. For change data capture, poll a table or emit events from the
application.

## Extensions

| Feature | State |
|---|---|
| `pgcrypto`, `uuid-ossp`, `hstore`, `citext`, `unaccent`, `pg_trgm` | yes, built in; `CREATE EXTENSION` lists them in `pg_extension` |
| `btree_gin`, `btree_gist`, `bloom` | partial: accepted; their indexes are btree |
| Any other extension (PostGIS, pgvector, TimescaleDB, pg_cron, ...) | never: refused with `0A000` |

**Why.** PostgreSQL extensions are C shared objects compiled against PostgreSQL's
internals; there is nothing in SQLite to load them into. What Overlite provides of
the common ones is reimplemented. `CREATE EXTENSION` refuses the rest, so a
migration that needs PostGIS fails at once instead of half working.

## Partitioning, inheritance, tablespaces

| Feature | State |
|---|---|
| `PARTITION BY`, `PARTITION OF` | no: refused with `0A000` |
| `INHERITS`, `UNLOGGED` tables | no: syntax error |
| Foreign data wrappers | no: refused with `0A000` |
| `CREATE TABLESPACE` | never (the `TABLESPACE` clause of `CREATE INDEX` is accepted and ignored) |
| Per-tenant physical isolation | yes, by another route |

**Instead.** `CREATE DATABASE` writes a new SQLite file next to the first, and a
connection to it can only see that file, which covers the per-tenant case, the most
common real reason for partitioning. Dropping a month's partition for retention has
no equivalent; use `DELETE`.

## Types

| Feature | State |
|---|---|
| integers, text, `varchar(n)`, `char(n)`, boolean, bytea, uuid | yes |
| `numeric` with exact arithmetic at PostgreSQL's scale | yes |
| date, time, timestamp, timestamptz with the session's time zone, `interval` | yes |
| json, jsonb and their operators | yes |
| arrays | partial: a `jsonb[]` keeps its elements as strings (planned) |
| enums, `hstore` | yes |
| ranges | partial: `*` and `+` are wrong, and a statement's Describe reports a range column as text (planned) |
| identity columns, serial, sequences, generated stored columns | partial: a key's next id follows the highest row |
| `tsvector`, `tsquery` | yes (see full-text search) |
| `inet`, `cidr`, `point` and other geometric types | partial: stored as text; the network functions exist, operators do not |
| composite types | no: `CREATE TYPE ... AS (...)` is recorded, values cannot be built |
| `CREATE DOMAIN`, XML | no |
| Integer result columns reported at their width | no: they go out as `int8`; the catalogs have the declared width |

**The ones to watch.** Where the translation cannot tell an expression's type (a
function it does not know, some subqueries), SQLite's typing decides: `SELECT 1/0`
is NULL, and a column can hold a value of another type. A network operator such as
`'10.0.0.1'::inet << '10.0.0.0/8'` gives a number, not an error.

## Indexes

| Feature | State |
|---|---|
| btree, unique, partial and expression indexes | yes |
| Operator classes (`varchar_pattern_ops`), `NULLS NOT DISTINCT` | yes |
| `CREATE INDEX CONCURRENTLY` | yes, as a plain index |
| `INCLUDE` | partial: recorded and reported, not stored in the index |
| `USING gin / gist / spgist / brin / bloom` | partial: becomes a btree; the method is only reported |

**What you lose.** GIN is what makes `jsonb @>`, array containment and `@@` fast in
PostgreSQL. Here those queries are right but read every row. Index an extracted
expression (`CREATE INDEX ON t ((profile->>'role'))`) where it matters.

## Full-text search

| Feature | State |
|---|---|
| `to_tsvector`, the four query builders, `@@`, `setweight`, `ts_rank`, `ts_rank_cd` | yes, held to PostgreSQL 17 on 1,190 texts |
| `simple` and `english` configurations | yes |
| Other languages, custom dictionaries, `ts_headline`, `ts_debug` | no |
| tsquery `&&`, `||`, `!!` | planned |

A call without a configuration uses `english`, whatever `default_text_search_config`
says.

## Collation and pattern matching

| Feature | State |
|---|---|
| Default text ordering | partial: byte order, as PostgreSQL's `C` collation (`\l` reports `C`) |
| Language collations (`"sv-x-icu"`, `sv_SE.utf8`, `CREATE COLLATION`) | partial: the Unicode Collation Algorithm in broad lines, held to PostgreSQL for Latin-script letters |
| `LIKE`, regular expressions (`~`, `~*`, `regexp_*`) | yes |
| `ILIKE` | partial: folds ASCII letters only, so `'ÉCOLE' ILIKE 'école'` is false |

If you store non-English text, give the columns or the `ORDER BY` a language
collation; without one, "Ávila" sorts after "zeta".

## Security and access control

| Feature | State |
|---|---|
| Roles, membership, per-role passwords (SCRAM) | yes |
| `GRANT` / `REVOKE` on tables, checked before a statement runs | yes |
| Row-level security policies | yes, `COPY` included |
| `pg_hba.conf` / `pg_hba.yaml`, TLS, scram-sha-256 / md5 / password / trust / reject | yes |
| Column-level `GRANT` (SELECT, INSERT, UPDATE) | yes; REFERENCES is recorded, not enforced |
| Privileges through `LANGUAGE sql` bodies, views (as the owner), triggers, `EXPLAIN` | yes |
| Role names | partial: compared without case |

Privileges and RLS are enforced, not decorative, through whatever a statement
reaches: a function's body, a view (with its owner's privileges), a trigger, an
EXPLAIN. A view whose base table's policies would filter its owner is refused rather
than read unfiltered.

## Observability and operations

| Feature | State |
|---|---|
| `SHOW`, `current_setting`, `SET` | partial: settings Overlite does not use read as empty |
| `pg_settings` | partial: the settings `SHOW` answers |
| `pg_stat_activity`, `pg_locks`, `pg_stat_*` | no |
| `pg_typeof` | yes |
| `pg_size_pretty`, `pg_database_size` | no |
| Cancel requests | yes |
| Autovacuum, bloat, transaction-ID wraparound | never, and you don't want them |

**What you lose.** Monitoring dashboards and the activity panes of GUI tools see
nothing. **What you gain.** No vacuum tuning, no wraparound, no connection pooler,
no data directory: one binary and one file per database.

## Queries and SQL

| Feature | State |
|---|---|
| Joins incl. `FULL OUTER`, CTEs incl. `RECURSIVE`, window functions, set operations | yes |
| `ON CONFLICT`, `RETURNING`, `DISTINCT ON`, `DELETE ... USING`, `UPDATE ... FROM` | yes |
| `string_agg`, `array_agg`, `json_agg`, statistics aggregates, `generate_series`, `unnest` | yes |
| `COPY` in and out, `\copy` | yes |
| `PREPARE` / `EXECUTE`, extended protocol, binary format | yes |
| SQL cursors (`DECLARE`, `FETCH`, `MOVE`, `WITH HOLD`, `SCROLL`) | yes |
| Materialized views (`CREATE`, `REFRESH`) | yes |
| `TRUNCATE` with `RESTART IDENTITY` and `CASCADE` | yes |
| `EXCLUDE` constraints | partial: `=`, `<>`, `&&`, `-|-` only, checked by scanning the table |
| `LATERAL` over set-returning functions | yes |
| `LATERAL` subqueries that read the left side | no |
| `GROUPING SETS`, `ROLLUP` | no |
| Ordered-set aggregates (`percentile_cont ... WITHIN GROUP`) | no |
| Whole-row references (`row_to_json(t)`, `SELECT t FROM t`) | no |
| SQL/JSON path (`jsonb_path_query`) | no |
| `TABLESAMPLE` | no |

## Catalogs and tooling

| Feature | State |
|---|---|
| `psql`: `\dt`, `\d`, `\df`, `\sf`, `\du`, `\dn`, `\l`, `\copy` | yes |
| Clients: drivers for Go, Python and Node.js, `pgbench`, `pg_dump` | yes |
| ORMs | yes (see measurements above) |
| `pg_class`, `pg_attribute`, `pg_type`, `pg_proc`, `pg_index`, `pg_constraint`, `pg_depend`, ... | yes |
| `information_schema` (11 views) | partial: `check_constraints` is empty |
| `server_version` | reports 16 |

---

## Silent differences: the list to actually worry about

Errors are cheap: you see them and fix them. These are accepted without complaint
and behave differently.

1. **PL/pgSQL functions are created but never run.**
2. **`USING gin / gist` builds a btree.** Answers are right; performance is not
   what you sized for.
3. **Row locks are the database's write lock.** `SKIP LOCKED` skips nothing while
   another transaction writes.
4. **Text sorts by byte order** unless you name a language collation; `ILIKE`
   folds ASCII only.
5. **SQLite's typing decides where the translation cannot see a type**: `1/0` is
   NULL, `'abc'` fits in an integer column.
6. **A function called in a view runs with the view owner's privileges**, not the
   querying role's: its body is inlined into the view when the view is made. A
   superuser reading a view whose owner lacks a privilege is not refused.
7. **`ANALYZE` and isolation levels are accepted** and change nothing.
8. **Identity and serial keys follow the highest row**, so an explicit id moves
   the next one on.
9. **Network and geometric values are text**: their operators give answers, not
   errors.
10. **Range `*` and `+` are wrong**, and instants past the year 9999 compare as
    text.

NOTES.md lists the rest, each held to PostgreSQL 17's answer in the wire tests.

---

## Planned

Only what has work recorded against it:

- **What the framework test suite found and is not done yet** (NOTES.md): range
  `*` and `+`; `jsonb[]` elements kept as strings; instants past the year 9999
  compared as text; tsquery `&&`, `||` and `!!`; `DISTINCT ON` with `LIMIT` and
  `OFFSET`; a bytea column default; catalog details (an index's access
  method, collations, serial `column_default`, the sequence of a renamed serial);
  `DATE_TRUNC` and `EXTRACT` in a non-UTC session.
- **Concurrent writes**: about a quarter of PostgreSQL's throughput today (see the
  README's [Performance](README.md#performance)).

PL/pgSQL, PostgreSQL-style triggers, `GROUPING SETS` and the monitoring views are
not planned.

## Should you use Overlite?

**Good fit**
- CRUD applications and internal tools behind an ORM
- Single-node services, with a database file per tenant if you need isolation
- Development, test and CI environments that should answer like PostgreSQL
- Edge and embedded deployments where running PostgreSQL is not practical
- An application on SQLite that now needs network access

**Bad fit**
- Anything that depends on PL/pgSQL, PostgreSQL triggers or stored procedures
- PostGIS, pgvector, TimescaleDB or any extension not listed above
- Read replicas, failover or CDC into a data pipeline
- Many processes writing at the same time
- Analytics that need parallel query over large tables
- Partitioned time series with retention by dropping partitions
- Full-text search in languages other than English
