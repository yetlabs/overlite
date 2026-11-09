.PHONY: test build run fmt lint doc check clean ast-corpus

# The release workflow sets VERSION to the tag.
VERSION ?=
ifneq ($(VERSION),)
export OVERLITE_VERSION := $(VERSION)
endif

test:
	cargo test --release --workspace

build:
	cargo build --release

run:
	cargo run --release

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# What ci.yml runs, so a failure there can be reproduced with one command.
check: lint doc test
	cargo fmt --all --check

clean:
	cargo clean

# Records every statement the wire suite sends (its panics test bends them at random,
# so not that one) and adds those the AST corpus lacks to
# tests/fixtures/ast-corpus/wire.hex; the printer's snapshot of it is written anew with
# OVERLITE_SNAPSHOT=update cargo test --release -p overlite-postgres --lib snapshot.
ast-corpus:
	rm -f target/wire-statements.hex
	OVERLITE_RECORD_STATEMENTS=$(CURDIR)/target/wire-statements.hex cargo test --release --test wire -- --skip panics::
	OVERLITE_RECORDED=$(CURDIR)/target/wire-statements.hex cargo test --release -p overlite-postgres --lib grow_the_corpus -- --ignored --nocapture
