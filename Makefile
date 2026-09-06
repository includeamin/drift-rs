.PHONY: build fmt fmt-check lint test check release clean

build:
	cargo build --release

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-targets

check: fmt-check lint test

release: build

clean:
	cargo clean
