# AGENTS.md

Guidance for coding agents working in this repository. `CLAUDE.md` is a symlink
to this file, so Claude Code reads the same conventions.

## Commits

Write commit messages as [Conventional Commits](https://www.conventionalcommits.org):
`<type>: <summary>`, imperative mood, lowercase summary, no trailing period.

Types used here: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`. Prefer `style` for formatting-only changes (rustfmt) and
`chore` for housekeeping that touches no behavior.

The body is optional but valued: explain why the change is needed and what it
fixes, not what the diff already shows.

## Verifying a change

`make check` runs exactly what CI runs, over the whole workspace: formatting,
clippy, rustdoc with warnings denied, the unit tests and doctests of every crate
in `crates/` and the wire suite in `tests/wire`, which drives the real binary
with a Postgres client. Run it before
committing.

ARCHITECTURE.md says what each crate owns and which invariants the code keeps.

Some wire tests skip themselves when an external binary is missing, such as
`pg_dump`, and CI has it. A green local run is not proof CI is green; check for
skips when touching catalog views.

A plain `cargo test` or `cargo clippy` at the root covers the binary only: pass
`--workspace`.

`cargo clippy --fix` removes imports only tests use. Check the test build after it.

On a machine without cargo, run it in a container:

    docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/target \
      -v ol-rs-target:/target -v ol-rs-cargo:/usr/local/cargo/registry \
      docker.io/library/rust:1 make check
