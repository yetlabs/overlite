<p align="center">
  <img src="assets/logo/overlite-mark.svg" alt="" width="160">
</p>

<h1 align="center">Overlite</h1>

<p align="center">
  <a href="https://github.com/yetlabs/overlite/actions/workflows/ci.yml"><img src="https://github.com/yetlabs/overlite/actions/workflows/ci.yml/badge.svg" alt="ci"></a>
  <a href="https://github.com/yetlabs/overlite/releases/latest"><img src="https://img.shields.io/github/v/release/yetlabs/overlite?label=release" alt="release"></a>
  <a href="https://hub.docker.com/r/yetlabs/overlite"><img src="https://img.shields.io/docker/v/yetlabs/overlite?label=docker&sort=semver" alt="docker"></a>
</p>

**Speak PostgreSQL, store SQLite.** overlite is a lightweight server that talks
the PostgreSQL wire protocol on the front and keeps all your data in a single
SQLite file on the back.

## Why

SQLite is a fantastic storage engine, but it isn't a server: no network access,
brittle locking over network filesystems, and you have to use SQLite-specific
tooling. overlite puts a PostgreSQL-speaking server in front of it so you get:

- **Real network access & concurrency** — one process owns the file and
  serializes access, so the multi-access and network-lock problems disappear.
- **The entire Postgres ecosystem for free** — connect with `psql`, DBeaver,
  and any PostgreSQL driver (pgx, JDBC, …). `\dt`, `\d`, `\copy` all work.
- **Dead simple to run** — a single static binary (SQLite linked in), no
  server to provision, no data directory. Point it at a file and go.

## Quick start

```sh
$ overlite postgres.db
```

It listens on `:5432` and creates the file if it isn't there. The file name is
the database name, so this one is `postgres`. Connect with any Postgres client:

```sh
psql "postgresql://postgres@localhost:5432/postgres?sslmode=disable"
```

### Docker

```sh
$ docker run --rm -p 5432:5432 -v "$(pwd)/data:/data" yetlabs/overlite postgres.db
```

Mount the **directory**, not the file. SQLite writes recent commits to
`postgres.db-wal` beside the database and checkpoints them into `postgres.db`
later; with only the file mounted, the `-wal` lands in the container's own
layer, and recreating the container throws away whatever had not been
checkpointed yet. The same goes for every database `CREATE DATABASE` writes
next to it (see [Many databases](#many-databases)).

The file is created if it isn't there, and stays a plain SQLite file on the
host. The image binds `0.0.0.0` inside the container. Podman needs `:Z` on the
mount for SELinux to allow writing to it.

### compose.yaml

```yaml
services:
  overlite:
    image: yetlabs/overlite
    command: postgres.db
    ports:
      - "5432:5432"
    volumes:
      - ./data:/data
```

```sh
$ docker compose up
```

## Configuration

overlite follows PostgreSQL conventions, so it drops into the same tooling and
container setups.

| Flag | Env | Default | Description |
|---|---|---|---|
| `--driver` | `OVERLITE_DRIVER` | `postgres` | wire protocol to speak |
| `--host` | — | `127.0.0.1` | listen address (`0.0.0.0` to expose) |
| `--db` | — | `postgres.db` | SQLite file; its name becomes the database name |
| — | `POSTGRES_PORT` | `5432` | listen port (the driver's default, e.g. postgres = 5432) |
| — | `POSTGRES_USER` | `postgres` | role shown as owner |
| — | `POSTGRES_PASSWORD` | *(unset)* | when set, requires password auth (else trust) |
| — | `POSTGRES_SSL` | *(unset)* | `on` enables TLS with a self-signed cert (clients use `sslmode=require`) |
| — | `POSTGRES_SSL_CERT` / `POSTGRES_SSL_KEY` | *(unset)* | PEM cert/key to serve instead of self-signed |
| `--max-open-databases` | — | `64` | how many database files stay open at once (see [Many databases](#many-databases)) |
| — | `OVERLITE_HBA_DIR` | `.` | directory holding `pg_hba.conf` and/or `pg_hba.yaml` (see below); overrides the global auth method |

The port belongs to the driver — postgres defaults to 5432 — and is only
overridden if you set `<DRIVER>_PORT` (e.g. `POSTGRES_PORT`).

```sh
POSTGRES_PASSWORD=secret POSTGRES_PORT=5544 overlite shop.db
# -> database "shop", user "postgres", password auth, on :5544
```

## Host-based auth (`pg_hba`)

Drop a `pg_hba.conf` (classic Postgres format) or a `pg_hba.yaml` into
`OVERLITE_HBA_DIR` to decide the auth method — or a rejection — per connection,
by type / database / user / client CIDR. Rules are evaluated top-to-bottom;
**the first match wins**, and an unmatched connection is refused (as Postgres
does). If both files are present, `pg_hba.conf` takes precedence.

```conf
# TYPE  DATABASE  USER   ADDRESS         METHOD
host    all       all    127.0.0.1/32    trust
hostssl shop      app    10.0.0.0/8      scram-sha-256
host    all       all    0.0.0.0/0       reject
```

```yaml
hba:
  - { type: host,    database: all,  user: all, address: 127.0.0.1/32, method: trust }
  - { type: hostssl, database: shop, user: app, address: 10.0.0.0/8,   method: scram-sha-256 }
  - { type: host,    database: all,  user: all, address: 0.0.0.0/0,     method: reject }
```

Methods `trust`, `reject`, `scram-sha-256`, `md5`, and `password` are enforced;
`peer`/`cert` are accepted without their verification. Each role authenticates
against **its own password** — `CREATE ROLE alice LOGIN PASSWORD 'x'` stores a
SCRAM verifier (never plaintext), and roles without one fall back to
`POSTGRES_PASSWORD`.

## Many databases

A server started with one file serves one database. It also serves every other
`*.db` file beside it, and `CREATE DATABASE` writes a new one there:

```sh
$ overlite postgres.db
```
```sql
CREATE DATABASE shop;           -- writes ./shop.db
\c shop
CREATE TABLE orders (id int);   -- lands in shop.db, nowhere else
DROP DATABASE shop;             -- removes the file
```

The directory is taken from the file, so there is nothing else to configure.
Each database is a separate SQLite file with its own connection, which is what
makes this the isolation a schema cannot give: a query on one database has no
way to name a table in another, however it is written, and a tenant's data can
be handed over or deleted by moving one file.

Files are opened as they are asked for and the idle ones are closed once
`--max-open-databases` (default 64) are open, so a server can hold far more
databases than it keeps open at once. Reopening one costs a handshake.

`postgres` is always served whether or not the file exists, because a client has
to connect to something in order to create the first database — that is what
`psql -d postgres` and every migration tool expect.

## Schemas

By default all schemas live in the one file — the file you point at is the
`public` schema, and other schemas are name-prefixed tables in it:

```sql
CREATE SCHEMA sales;            -- ordinary write (works inside a transaction)
CREATE TABLE sales.orders (...);
SELECT * FROM sales.orders;     -- cross-schema queries & foreign keys just work
ALTER TABLE sales.orders SET SCHEMA archive;   -- move between schemas
ALTER SCHEMA sales RENAME TO revenue;
```

This makes `CREATE`/`DROP SCHEMA` transactional and lets foreign keys cross
schemas. The file stays plain-SQLite readable (`sales.orders` is a table named
`"sales.orders"`).

For physical isolation between tenants, give each one a *database* rather than a
schema: `CREATE DATABASE shop` writes `shop.db` beside the file overlite was
started with, and a connection to it can only ever see that file. See [Many databases](#many-databases).

## Status

High level, at a glance — including what's still needed to be **Postgres-ready
for a real production system**. ✅ done · 🟡 partial · ⬜ not yet.

Across the full feature matrix — **157 items: ✅ 142 · 🟡 14 · ⬜ 1**
(90% done, 99% at least partial):

| Area | ✅ | 🟡 | ⬜ |
|---|--:|--:|--:|
| Wire protocol | 14 | 0 | 0 |
| Authentication | 9 | 0 | 0 |
| DML (queries) | 12 | 1 | 0 |
| DDL (schema) | 30 | 5 | 0 |
| Data types | 11 | 4 | 0 |
| Transactions | 8 | 1 | 0 |
| Schemas | 10 | 0 | 0 |
| Catalog / introspection | 21 | 1 | 1 |
| Functions & dialect | 21 | 1 | 0 |
| Tooling (psql/pg_dump/GUIs) | 6 | 1 | 0 |

Every ✅ is exercised end-to-end against real `psql` and pgx (`make test`). The
remaining 🟡/⬜ items — the gap to a drop-in production Postgres — are listed in
full under [Limitations](#limitations).

Deciding whether to migrate? [**overlite vs
PostgreSQL**](POSTGRES_ROADMAP.md) goes subsystem by subsystem — what you get,
what is planned, and what will never be there because it belongs to SQLite.

## Limitations

Everything not listed here is implemented (see Status). This is the complete set
of gaps — 🟡 **partial** (works, with caveats) · ⬜ **not implemented**.

### Types

- 🟡 **`numeric` infix arithmetic** — `+` `-` `*` `/` use SQLite's float operators
  (which can't be overridden); storage, ordering, and `sum`/`avg` are exact, and
  `dec_add`/`dec_mul`/… give exact results explicitly. Scale isn't enforced.
- 🟡 **`hstore` `->`** returns the JSON-quoted scalar (use `->>` for text); a bare
  insert needs the `::hstore` cast.
- 🟡 **range & geometric/network types** — ranges work (text-stored, constructors,
  accessors, `@>`, `&&`), but the closed `[a,b]` form is ambiguous with a
  2-element array; `point`/`box`/… and `inet`/`cidr`/`macaddr` aren't modeled.
- 🟡 **enum columns** — enforced via `TEXT` + `CHECK` and reported with the
  enum's own type (so a dump round-trips), but no ordering/comparison.
- 🟡 **composite types** — `CREATE TYPE … AS (…)` shows in `pg_type`/`\dT`, but the
  fields aren't modeled.
- 🟡 **`interval`** — `ts ± interval '1 day'` and `age()` work; no bare `interval`
  value type.
- 🟡 **integer OIDs** — every integer advertises as `int8` and `numeric` as
  `float8` in `RowDescription` (catalog oids exceed `int4`).

### Functions, triggers, extensions

- 🟡 **PL/pgSQL functions / procedures / aggregates** — accepted so migrations
  proceed, but the body isn't executed (no procedural engine). `CREATE FUNCTION …
  LANGUAGE sql` **is** fully supported (executed, shown in `\df`/`\sf`, dumped).
- 🟡 **`CREATE TRIGGER`** — accepted; Postgres trigger functions (PL/pgSQL) aren't
  executed.
- 🟡 **`CREATE`/`DROP EXTENSION`** — a no-op; the common functions (e.g.
  `gen_random_uuid`) are provided directly.

### DDL & constraints

- 🟡 **`information_schema.check_constraints`** — empty (SQLite checks are
  indistinguishable from the enum-backing `IN (…)` checks).

### Query & runtime

- 🟡 **`LATERAL` correlated subquery** — can't reference the left side (a SQLite
  limit); `LATERAL` over set-returning functions works.
- 🟡 **configurable isolation levels** — `SET`/`BEGIN ISOLATION LEVEL` accepted,
  no effect (SQLite serializes writes).
- 🟡 **DBeaver** — connects and browses/reads data; full validation in progress.
- ⬜ **planner statistics** (`pg_statistic`, `pg_statistic_ext`) — empty; SQLite
  keeps stats in a different shape (`sqlite_stat1`). (Everything backed by real
  data — `pg_auth_members`, `pg_policy`, `pg_depend`/`pg_shdepend` — is populated.)
