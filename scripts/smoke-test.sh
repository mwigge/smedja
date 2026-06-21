#!/usr/bin/env bash
# smoke-test.sh — must pass before any task can be marked done in DELIVERY.md
set -euo pipefail

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$ROOT"

echo "==> smoke-test: cargo test --workspace"
cargo test --workspace

# smedja-tui smoke integration tests live in tests/smoke.rs inside that crate.
# This will fail (exit 1) until those tests are written — that is intentional.
# A task is not done until this passes.
echo "==> smoke-test: cargo test -p smedja-tui --test smoke"
cargo test -p smedja-tui --test smoke

echo "==> smoke-test: all layers passed"
