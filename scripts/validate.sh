#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"

cargo fmt --all -- --check
cargo clippy --locked --release --all-targets --all-features -- -D warnings
cargo test --locked --release --all-targets -- --test-threads=2
cargo build --locked --release
