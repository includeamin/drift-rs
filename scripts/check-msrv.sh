#!/usr/bin/env bash
# Checks that the crate builds on the minimum Rust version declared in
# Cargo.toml (`rust-version`), with each feature set.
#
# The committed Cargo.lock is too new for an old Cargo, and the newest versions
# of some dependencies need a newer compiler. So this builds in a scratch copy,
# with a lockfile resolved against the declared rust-version, and without the
# `web` crate and benches, whose dependencies have their own requirements.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
msrv=$(grep -m1 '^rust-version' "$root/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cp -r "$root/Cargo.toml" "$root/src" "$work/"
cd "$work"
sed -i 's/^members = .*/members = ["."]/; /^\[\[bench\]\]/,/^harness/d' Cargo.toml

rustup toolchain install "$msrv" --profile minimal
CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo generate-lockfile

for features in "--no-default-features" "--no-default-features --features yaml,toml,xml" "--no-default-features --features yaml-comments"; do
    echo "== rust $msrv: lib $features"
    cargo "+$msrv" check --locked --lib $features
done
echo "== rust $msrv: lib and binary, default features"
cargo "+$msrv" check --locked --lib --bins
echo "MSRV $msrv ok"
