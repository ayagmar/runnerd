#!/bin/sh
# This version and the previous release read each other's JSON (crates/wire-compat, ADR 0060). The crate is a
# workspace of its own, so the workspace's fmt, clippy and tests do not reach it.
set -eu

manifest=crates/wire-compat/Cargo.toml
cargo fmt --check --manifest-path "$manifest"
cargo clippy --manifest-path "$manifest" --target-dir target/wire-compat --all-targets --locked -- -D warnings
cargo test --manifest-path "$manifest" --target-dir target/wire-compat --locked
