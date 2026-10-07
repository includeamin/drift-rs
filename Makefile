.PHONY: web web-serve build fmt fmt-check lint test check release clean

build:
	cargo build --release

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-targets

check: fmt-check lint test

release: build

clean:
	cargo clean

web:
	wasm-pack build web --release --target web --out-dir www/pkg

web-serve: web
	cd web/www && python3 -m http.server 8000
