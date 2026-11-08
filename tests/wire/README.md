# Wire suite

One test binary that starts the real server and talks to it as a client would.
`common.rs` is the client: a hand-written frontend that follows the message
sequences of pgx, since the `postgres` crate cannot send untyped parameters, ask
for text results or per-column formats. `harness.rs` tests the client itself.

## Adding a test

Put it in the module of its feature, or add one to `main.rs`. Copy `compat.rs`.

- A failing test is a finding: do not bend it to pass, and do not `#[ignore]` it.
- Never write an assertion that cannot fail. `.unwrap()` on a query is an assertion;
  a bare `let _ =` is not.
- `common.rs` scan rules are pgx's and are part of what is tested: do not loosen
  a `FromCell`.
- Tests run in parallel, each against its own server. Pass environment through
  `Env` to `start_server_env`, never by changing this process.

## External programs

`psql` and `pg_dump` tests skip when the program is missing. `pg_dump` must be
version 15 or newer. In a Debian container:

    apt-get install -y postgresql-client

## Goldens

`tests/fixtures/parity-*.golden` hold the outputs of the old server, so
`parity_diff` and `parity_numeric` keep comparing against them. Known differences
are listed in those tests, and each fails if it stops differing.
