.PHONY: test build run fmt lint doc check clean

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
