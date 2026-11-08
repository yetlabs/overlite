# overlite vs PostgreSQL — what you get, what you won't

This document exists so you can decide **before** you migrate. It is organized by
decision, not by feature: every major PostgreSQL subsystem is listed with an
honest verdict and, where the answer is "no", the reason and the workaround.

The goal is for you to be able to say: *"I won't have X, Y and Z with overlite —
and that's fine for what I'm building."*

Legend: ✅ **have it** · 🔜 **planned** · 🤔 **open question** · ❌ **never**
(architectural — follows from storing in SQLite)

## The one-paragraph answer

overlite speaks the PostgreSQL wire protocol over a SQLite file. Everything that
is *language and protocol* — SQL, types, catalog, auth, tooling — can be
emulated, and mostly is. Everything that is *storage engine and process
architecture* — MVCC, planner, replication, extensions — belongs to SQLite and
will never be PostgreSQL's. If your app is a CRUD service talking to an ORM,
you'll likely never notice. If you depend on PL/pgSQL, PostGIS, read replicas or
partitioned tables, overlite is the wrong tool and always will be.

## Coverage, three ways

One number is misleading, so here are three:

| Measure | Coverage |
|---|--:|
| The feature matrix this project tracks (see README Status) | **~90%** |
| A typical CRUD app through an ORM (Rails / Django / Prisma / pgx / JDBC) | **~85%** |
| PostgreSQL's actual surface (types, functions, DDL, PL, operational) | **~35–45%** |

The third number is low on purpose and isn't a defect: PostgreSQL has ~700
built-in functions, a procedural language, an extension ecosystem, and a
replication stack. overlite implements the part an application uses, not the
part a DBA operates.

---

## Storage, concurrency and transactions

| Feature | State |
|---|---|
| ACID transactions, `BEGIN`/`COMMIT`/`ROLLBACK`, savepoints | ✅ |
| Readers never block writers (SQLite WAL) | ✅ |
| Multiple concurrent writers | ❌ |
| `SELECT … FOR UPDATE` / `FOR SHARE` (row locks) | ❌ real semantics · 🔜 accepted as no-op |
| Configurable isolation levels | ❌ (accepted, no effect) |
| Advisory locks (`pg_advisory_lock` and family, session and transaction level) | ✅ |
| Deadlock detection | ❌ (nothing to deadlock — writes serialize) |

**Why.** SQLite in WAL mode allows one writer at a time with any number of
concurrent readers, each reading a consistent snapshot. overlite serializes
writes behind that (100 max connections, 5s busy timeout). This is stronger than
PostgreSQL's default isolation in one sense — write skew can't happen — and much
weaker in another: a write-heavy workload queues instead of proceeding in
parallel.

**What you lose.** Throughput under concurrent writes. If you have dozens of
processes writing constantly, you want PostgreSQL. Row-level pessimistic locking
(`SELECT … FOR UPDATE`, Django's `select_for_update`, Rails' `lock!`) has no
equivalent — though with a single writer the guarantee it buys you is largely
implicit already.

## Planner, optimizer, statistics

| Feature | State |
|---|---|
| Cost-based planner, index selection, joins | ✅ (SQLite's) |
| `EXPLAIN` | ✅ (SQLite's output shape, not PostgreSQL's) |
| `ANALYZE` collecting statistics | ✅ (`sqlite_stat1`) |
| `pg_statistic` / `pg_statistic_ext` populated | ❌ |
| `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` | ❌ |
| Parallel query, JIT compilation | ❌ |
| Bitmap index scans, index-only scans over visibility map | ❌ |

**Why.** The planner is the storage engine's. SQLite's is good, simple, and
deliberately less adaptive: it leans on indexes and doesn't parallelize.
Emulating PostgreSQL's plan output would be a lie about what actually runs.

**What you lose.** Query-plan tooling and analytics performance. A large
aggregation that PostgreSQL splits across 8 workers runs on one core here. Plans
you tuned against PostgreSQL don't transfer — retune against SQLite's `EXPLAIN
QUERY PLAN`.

## Procedural languages — the big open question

| Feature | State |
|---|---|
| `CREATE FUNCTION … LANGUAGE sql` (executed, `\df`, `\sf`, dumped) | ✅ |
| `LANGUAGE plpgsql` functions | 🤔 accepted, **body never runs** |
| `CREATE TRIGGER` with a PL/pgSQL function | 🤔 accepted, **never fires** |
| `CREATE PROCEDURE` / `CALL`, custom aggregates | 🤔 accepted, no-op |
| PL/Python, PL/Perl, PL/v8 | ❌ |

**Why it's marked 🤔 and not ❌.** A PL/pgSQL subset interpreter —
`DECLARE`/`BEGIN`/`IF`/`LOOP`/`RETURN`/`RAISE`, assignment, embedded queries,
`NEW`/`OLD` in triggers — is a large but bounded project (weeks, not months) and
it's the single change that would move overlite from "app database" to
"migration-compatible database". Triggers are coupled to it: audit tables and
`updated_at` maintenance are the most common real use, and they silently do
nothing today.

**This is the sharpest edge in the whole product.** Your migration runs green, a
trigger is created, and no row is ever audited. If you rely on PL/pgSQL, do not
migrate until this lands or is declared ❌.

## Replication, HA, backup

| Feature | State |
|---|---|
| Physical / streaming replication, hot standby | ❌ |
| Logical replication, `CREATE PUBLICATION`/`SUBSCRIPTION`, WAL decoding | ❌ |
| CDC via logical slots (Debezium et al.) | ❌ |
| Point-in-time recovery | ❌ (at the PostgreSQL layer) |
| Backup | ✅ copy the file, or `pg_dump` |

**Why.** These are WAL-format-level features. SQLite's WAL is a different format
with a different lifecycle; there is nothing to decode into PostgreSQL's
protocol.

**The workaround is genuinely good.** Replicate at the SQLite layer:
[Litestream](https://litestream.io) (continuous S3 streaming + PITR), LiteFS
(FUSE-level replication), or rqlite. You lose PostgreSQL-shaped tooling, not the
capability itself. CDC has no equivalent — poll a table or write events at the
application layer.

## Extensions

| Feature | State |
|---|---|
| `CREATE EXTENSION` accepted so migrations run | ✅ (no-op) |
| Functions the common extensions provide (`gen_random_uuid`, …) | ✅ built in directly |
| PostGIS, pgvector, TimescaleDB, pg_stat_statements, pg_cron, … | ❌ |
| Surfacing SQLite's own engines under PostgreSQL names | 🤔 |

**Why.** PostgreSQL extensions are C shared objects compiled against its
internals. overlite is a single binary with SQLite linked in — there is
no socket to plug them into, by design.

**What's interesting.** SQLite has its own answers — FTS5 (full-text), R\*Tree
(spatial), sqlite-vec (vectors). Presenting those under PostgreSQL-facing names
is real work but plausible; see full-text search below.

## Partitioning and table inheritance

| Feature | State |
|---|---|
| `CREATE TABLE … PARTITION BY RANGE/LIST/HASH` | ❌ |
| Partition pruning, attach/detach partition | ❌ |
| `INHERITS`, `UNLOGGED`, tablespaces | ❌ |
| Per-tenant physical isolation | ✅ different mechanism |

**Why.** SQLite has no partitioned tables and no planner hook to prune them.

**The workaround.** `OVERLITE_MULTITENANT_SCHEMA=true` gives each schema its own
attached file (`shop.db` + `shop.sales.db`), which covers the per-tenant
isolation case — the most common real reason people reach for partitioning.
Time-series partitioning for retention (drop last month's partition) has no
equivalent; use `DELETE` + `VACUUM`.

## Type system

| Feature | State |
|---|---|
| int / text / bool / bytea / uuid / json / jsonb / arrays / timestamps | ✅ |
| `numeric` storage, ordering, `sum`/`avg` exact | ✅ |
| `numeric` infix `+ - * /` exact | 🤔 uses float (SQLite can't override operators) |
| Ranges, `hstore`, enums, composite types | 🔜 partial — see README Limitations |
| `interval` as a first-class value | 🔜 partial (arithmetic and `age()` work) |
| `CREATE DOMAIN` | 🔜 |
| `GENERATED … AS IDENTITY` | 🔜 (`SERIAL` works today) |
| Geometric (`point`, `box`), network (`inet`, `cidr`, `macaddr`) | ❌ not modeled |
| XML type and functions | ❌ |
| Integer OIDs reported exactly (`int4` vs `int8`) | 🤔 everything advertises `int8` |

**The silent one.** `'(1,2)'::point` and `'10.0.0.1'::inet` are *accepted* — the
value is stored as text — but the operators of those types don't exist. The cast
succeeding gives false confidence.

## Indexes

| Feature | State |
|---|---|
| B-tree, unique, partial, expression indexes | ✅ |
| `CREATE INDEX … USING gin / gist / brin / hash` | 🤔 accepted, **becomes a B-tree** |
| Covering indexes (`INCLUDE`) | ❌ |
| `CREATE INDEX CONCURRENTLY` | 🔜 errors today; will be accepted as a plain `CREATE INDEX` |

**What you lose.** GIN is what makes `jsonb @> …` and array containment fast in
PostgreSQL. Here those queries work but scan. Index an extracted expression
(`CREATE INDEX ON t ((profile->>'role'))`) instead.

## Full-text search

| Feature | State |
|---|---|
| `tsvector`, `tsquery`, `to_tsvector`, `@@`, `ts_rank` | 🔜 |
| Dictionaries, stemming, custom search configurations | 🤔 |

Nothing today — `to_tsvector` doesn't exist. SQLite's FTS5 is a strong engine and
mapping the common surface onto it is the single highest-value planned item after
collation. Full parity (configurable dictionaries, `ts_rank_cd`, weights) is
unlikely.

## Collation and locale

| Feature | State |
|---|---|
| Case-insensitive collations | ✅ (mapped to `NOCASE`) |
| `COLLATE "pt_BR"` / ICU locale ordering | 🔜 **currently dropped silently** |
| `LIKE`, regex operators | ✅ |
| `ILIKE` / `NOT ILIKE` / `~~*` | 🔜 **not implemented — syntax error** |

**Read this if you store non-English text.** A locale collation clause is
currently *removed* from the query (`rewrite_collate` in `rewrite/operators.rs`), so `ORDER BY nome COLLATE
"pt_BR"` sorts by byte order — "Ávila" lands after "Zeta", with no error. The
driver supports registering custom collations, so this is fixable with
`x/text/collate` and is a correctness priority, not a nicety.

## Security and access control

| Feature | State |
|---|---|
| Roles, `CREATE ROLE`, membership, per-role passwords (SCRAM) | ✅ |
| `GRANT`/`REVOKE` **enforced** before statements run | ✅ |
| Row-level security — policies enforced by expression injection | ✅ |
| `pg_hba.conf` / `pg_hba.yaml`, TLS, SCRAM / md5 / trust / reject | ✅ |
| `SECURITY DEFINER` functions | ❌ |
| Column-level privileges | ❌ |

This area is closer to parity than most people expect: privileges and RLS are not
decorative, they're checked at the protocol layer.

## Observability and operations

| Feature | State |
|---|---|
| `pg_settings`, `SHOW`, `current_setting` | ✅ |
| `pg_stat_activity`, `pg_locks` | 🔜 |
| `pg_stat_user_tables`, `pg_stat_statements`, `pg_stat_bgwriter` | ❌ |
| `pg_size_pretty`, `pg_database_size`, `pg_sleep` | 🔜 |
| `VACUUM`, `ANALYZE`, `REINDEX` | ✅ (SQLite semantics) |
| Autovacuum, bloat, transaction-ID wraparound | ❌ — and you don't want them |

**What you lose.** Monitoring dashboards and GUI "activity" panes are blind.
**What you gain.** An entire class of operational work disappears: no vacuum
tuning, no wraparound emergencies, no connection pooler, no `shared_buffers`.

## Queries and SQL surface

| Feature | State |
|---|---|
| Joins incl. `FULL OUTER`, CTEs incl. `RECURSIVE`, window functions | ✅ |
| `ON CONFLICT`, `RETURNING`, `DISTINCT ON`, `generate_series` | ✅ |
| `string_agg`, `array_agg(… ORDER BY …)`, `json_agg`, `LISTEN`/`NOTIFY`, `COPY` | ✅ |
| `ILIKE` | 🔜 **not implemented — syntax error** |
| Generated stored columns, `PREPARE`/`EXECUTE`, extended protocol, portals | ✅ |
| SQL cursors (`DECLARE … CURSOR`, `FETCH`, `MOVE`) | 🔜 |
| Materialized views (`CREATE MATERIALIZED VIEW`, `REFRESH`) | 🔜 |
| `GROUPING SETS` / `ROLLUP` / `CUBE` | 🔜 |
| Ordered-set aggregates (`percentile_cont … WITHIN GROUP`) | 🔜 |
| Whole-row reference (`row_to_json(t)`, `SELECT t FROM t`) | 🔜 |
| `LATERAL` correlated to the left side | ❌ (SQLite limit) |
| `TABLESAMPLE` | ❌ |

## Catalog and tooling

| Feature | State |
|---|---|
| `psql` — `\dt \d \df \sf \dT+ \du \dn \l \copy` | ✅ |
| `pg_dump` / restore round-trip | ✅ |
| pgx, JDBC, node-postgres, psycopg, ActiveRecord, Django, Prisma | ✅ |
| `pg_class`, `pg_attribute`, `pg_type`, `pg_proc`, `pg_index`, `pg_constraint`, … | ✅ |
| `information_schema` (11 views) | ✅ mostly — `check_constraints` is empty |
| DBeaver / pgAdmin browsing | ✅ browse and read; full validation ongoing |

---

## Silent differences — the list to actually worry about

Errors are cheap: you see them and fix them. These are accepted without
complaint and behave differently, which is worse. Read this list twice.

1. **PL/pgSQL function bodies never execute** — the function exists, calls
   return nothing useful.
2. **Triggers never fire** — `CREATE TRIGGER` succeeds; your audit table stays
   empty.
3. **`COLLATE "<locale>"` is dropped** — accented text sorts by byte order.
4. **`USING gin`/`gist` becomes a B-tree** — the query is correct, the
   performance isn't what you sized for.
5. **`numeric` infix arithmetic goes through float** — storage, ordering and
   `sum`/`avg` are exact; `a * b` in a `SELECT` is not. Use `dec_mul`/`dec_add`
   where exactness is contractual (money totals).
6. **`ALTER TABLE ADD CONSTRAINT`** rebuilds the table to apply the constraint,
   so adding one to a large table costs a copy — and an existing row that
   violates it makes the statement fail rather than pass silently.
7. **Isolation level clauses** are accepted with no effect.
8. **`CREATE EXTENSION`** is a no-op — succeeds even for PostGIS.
9. **`::point` / `::inet` casts** succeed; the operators don't exist.
10. **`information_schema.check_constraints`** is empty.

**Planned:** an `OVERLITE_STRICT=true` mode that turns every silent acceptance in
this list into an error. If you're evaluating a migration, that flag is the
honest way to find out what you actually depend on.

---

## Should you use overlite?

**Good fit**
- CRUD apps and internal tools behind an ORM
- Single-node SaaS; per-tenant isolation via the multi-tenant schema mode
- Dev/test/CI environments that need to look like production PostgreSQL
- Edge and embedded deployments where running PostgreSQL isn't practical
- Replacing SQLite in an app that has outgrown "no network access"

**Bad fit**
- Anything depending on PL/pgSQL, triggers, or stored procedures
- PostGIS, pgvector, TimescaleDB, or any extension
- Read replicas, HA/failover, or CDC into a data pipeline
- Write-heavy concurrency (many processes writing simultaneously)
- Analytics needing parallel query over large tables
- Full-text search today
- Partitioned time-series with retention by partition drop

**Decide with the strict-mode question in mind:** if the ten silent differences
above would all be errors, would your app still start? If yes, migrate. If no,
you now know exactly which X, Y and Z you'd be giving up.

---

## Planned order of work

Ranked by (unblocks real integrations) ÷ (effort):

1. **`ILIKE`** — extremely common, and SQLite's `LIKE` is already ASCII
   case-insensitive: a dialect rewrite away
2. **Locale collations** — correctness bug, non-English data sorts wrong silently
3. **`OVERLITE_STRICT` mode** — makes every other gap discoverable before deploy
4. **Advisory locks** — unblocks Rails and Flyway migration locking
5. **`GENERATED … AS IDENTITY`** — what modern ORMs emit instead of `SERIAL`
6. **`FOR UPDATE`/`FOR SHARE` accepted as no-op** — unblocks pessimistic-locking code paths
7. **`CREATE INDEX CONCURRENTLY`** accepted as a plain index
8. **`pg_stat_activity` / `pg_locks`** — GUIs and monitoring stop being blind
9. **SQL cursors** — the extended-protocol portal machinery already does the hard part
10. **Materialized views**, **`CREATE DOMAIN`**, **whole-row references**
11. **Full-text search over FTS5** — the largest planned item
12. **`GROUPING SETS`**, ordered-set aggregates

**Open question, decided by demand:** the PL/pgSQL subset interpreter. It is the
difference between "app database" and "migration-compatible database". Until it
lands, triggers and procedural functions are the honest reason to stay on
PostgreSQL.
