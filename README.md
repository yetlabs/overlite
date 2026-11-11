<img src="assets/logo/overlite-mark.svg" alt="Overlite" width="120">

# Overlite

[![ci](https://github.com/yetlabs/overlite/actions/workflows/ci.yml/badge.svg)](https://github.com/yetlabs/overlite/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/yetlabs/overlite?label=release)](https://github.com/yetlabs/overlite/releases/latest)
[![image](https://img.shields.io/badge/image-ghcr.io%2Fyetlabs%2Foverlite-blue)](https://github.com/yetlabs/overlite/pkgs/container/overlite)

**Speak PostgreSQL, store SQLite.** Overlite is a lightweight server that talks
the PostgreSQL wire protocol on the front and keeps all your data in a single
SQLite file on the back.

## Why

SQLite is a fantastic storage engine, but it isn't a server: no network access,
brittle locking over network filesystems, and you have to use SQLite-specific
tooling. Overlite puts a PostgreSQL-speaking server in front of it so you get:

- **Real network access & concurrency** — one process owns the file and
  serializes access, so the multi-access and network-lock problems disappear.
- **The entire Postgres ecosystem for free** — connect with `psql`, DBeaver,
  and any PostgreSQL driver (pgx, JDBC, …). `\dt`, `\d`, `\copy` all work.
- **Dead simple to run** — a single static binary (SQLite linked in), no
  server to provision, no data directory. Point it at a file and go.

## Performance

Point lookups on a 73.8M-row table, one machine (16 cores), `pgbench` as the
client. PostgreSQL 14 runs its default configuration with the table warm in
memory; SQLite is the same file opened in-process, with no network and the
statement prepared once, which is the ceiling a server over it can reach.

| | Overlite | PostgreSQL | SQLite in-process |
|---|--:|--:|--:|
| 1 client | 23k q/s | 24k q/s | 288k q/s |
| 8 clients | 118k q/s | 129k q/s | 165k q/s |
| 16 clients | 149k q/s | 163k q/s | 149k q/s |
| 64 clients | 138k q/s | 171k q/s | 135k q/s |
| 8 clients, prepared | 100k q/s | 138k q/s | — |
| new connection | 9.2 ms | 5.2 ms | 0.2 ms |
| concurrent writes (8 clients) | 53k q/s | 15k–44k q/s | 135k q/s |
| `LIKE '%…%'` over every row | 5.2 s | 5.5 s | 5.4 s |

With one client a server pays a network round trip per query that SQLite in a
process does not. From 16 clients on, Overlite answers as many lookups as SQLite
opened directly, and within 10% of PostgreSQL up to 16 clients. Writes wait in a
queue per database and each commit is synced, as SQLite's `synchronous=FULL`
does; PostgreSQL's write figure varied between runs with its checkpoints.

## Compatibility

Overlite is a Rust server for Linux, macOS and Windows. It is tested end to end
with real clients and real applications, and held to PostgreSQL 17's answers
rather than to SQLite's. SQL is translated from PostgreSQL's own parse tree
([ADR 0001](docs/adr/0001-sql-ast.md)).

| Evidence | Result |
|---|---|
| ✅ A web framework's own PostgreSQL test suite | 99.5% of the tests that pass on PostgreSQL 17 also pass (18,087 of 18,179) |
| ✅ Production applications | migrations, fixtures, admin and day-to-day use, dump and reload: same answers as PostgreSQL |
| ✅ Clients | `psql`, `pg_dump`, `pgbench`, and drivers for Go, Python and Node.js, text and binary formats |
| ✅ This repository | over 600 wire tests that drive the real binary as a client, plus unit tests and a snapshot of the SQL translation |

## Limitations

🟡 works, with a difference · ⬜ not implemented. The full list, with the
reasons, is in [NOTES.md](NOTES.md#limits). The ones a client is most likely to
meet:

### Answers that differ without an error

- 🟡 **Types the translation cannot see**: an expression whose type is not known
  (a function it does not know, some subqueries) compares by SQLite's rules, e.g. a
  numeric out of an unknown function against a number.
- 🟡 **Ranges**: a range column is described as `text` before the statement runs (its
  rows go out as the range type).
- 🟡 **Timestamps past the year 9999** compare as text, before every earlier year.
- 🟡 **Identity and serial keys** are SQLite rowids: the next id follows the highest
  row, even one inserted with an explicit id.
- 🟡 **Transactions**: a transaction reads like READ COMMITTED until it first writes;
  every row lock is the database's write lock, so `SKIP LOCKED` skips nothing.
- 🟡 **Full-text search** knows the `simple` and `english` configurations only.
- 🟡 **Role names** compare without case.

### Accepted where PostgreSQL refuses

- 🟡 SQLite's typing still decides some expressions: `SELECT 1/0` is NULL, and a column
  can hold a value of another type (`'abc'` in an integer column).
- 🟡 `CREATE INDEX ... USING gin|gist|brin` makes a btree index; the method is only
  reported.
- 🟡 SQLite-syntax triggers (`CREATE TRIGGER ... BEGIN ... END`) run as written; the
  rest of SQLite's own syntax (`AUTOINCREMENT`, `EXPLAIN QUERY PLAN`, `LIMIT a, b`,
  `PRAGMA`, `INSERT OR ...`) is refused with PostgreSQL's syntax error.

### Not implemented

- ⬜ **PL/pgSQL**: `CREATE FUNCTION ... LANGUAGE plpgsql` is accepted but calling it
  fails, and a PostgreSQL-style `CREATE TRIGGER ... EXECUTE FUNCTION` is refused
  (`LANGUAGE sql` functions work).
- ⬜ **Deferred `UNIQUE`** constraints are checked at each statement (deferred foreign
  keys are checked at commit).
- ⬜ **Composite and geometric types**, planner statistics, `ts_headline`, table
  partitioning and foreign data wrappers (the last two answer with an error).

### Reported differently

- 🟡 A key column written `INTEGER PRIMARY KEY` goes out as `int8` (other integer
  columns keep their width).
- 🟡 `EXPLAIN` gives SQLite's plan in PostgreSQL's shape, with all costs at 0.
- 🟡 Error codes are PostgreSQL's; the text of errors that come from SQLite is SQLite's
  (a syntax error is PostgreSQL's own, with its position).

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
$ docker run --rm -p 5432:5432 -v "$(pwd)/data:/data" ghcr.io/yetlabs/overlite postgres.db
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
    image: ghcr.io/yetlabs/overlite
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

Overlite follows PostgreSQL conventions, so it drops into the same tooling and
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
schema: `CREATE DATABASE shop` writes `shop.db` beside the file Overlite was
started with, and a connection to it can only ever see that file. See [Many databases](#many-databases).
