set shell := ["bash", "-uc"]

check:
	cargo check --tests --package eventsourced --all-features
	cargo check --tests --package eventsourced-nats
	cargo check --tests --package eventsourced-postgres
	cargo check --tests --package eventsourced-projection

fmt toolchain="+nightly":
	cargo {{toolchain}} fmt

fmt-check toolchain="+nightly":
	cargo {{toolchain}} fmt --check

lint:
	cargo clippy --no-deps --all-features -- -D warnings

test:
	cargo test --all-features

coverage:
	cargo llvm-cov --workspace --lcov --output-path lcov.info --all-features

fix:
	cargo fix --allow-dirty --allow-staged --all-features

doc toolchain="+nightly":
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo {{toolchain}} doc --no-deps --all-features

all: check fmt lint test doc
