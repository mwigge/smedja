#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$ROOT"

FAILURES=()

run_check() {
  local name="$1"; shift
  echo "  checking: $name"
  if ! "$@" 2>&1; then
    FAILURES+=("$name")
  fi
}

echo "==> pre-commit"

run_check "cargo fmt" cargo fmt --check --all

if command -v cargo-sort &>/dev/null; then
  run_check "cargo sort" cargo sort --check --workspace
else
  echo "  (cargo-sort not installed — skipping; run: cargo install cargo-sort)"
fi

run_check "cargo clippy" cargo clippy \
  --workspace --all-targets --all-features \
  -- -D warnings -W clippy::pedantic \
     -A clippy::module_name_repetitions \
     -A clippy::must_use_candidate

run_check "cargo test" cargo test --workspace --quiet

if command -v cargo-machete &>/dev/null; then
  echo "  checking: cargo machete (advisory)"
  cargo machete 2>&1 || echo "  (machete found unused deps — advisory only)"
fi

echo "  checking: no println! in crate src/"
if grep -rn 'println!' --include="*.rs" crates/ 2>/dev/null | grep -v '//.*println' | grep -q .; then
  FAILURES+=("println! found in library crate source")
  grep -rn 'println!' --include="*.rs" crates/ 2>/dev/null | grep -v '//.*println' >&2 || true
fi

if [[ ${#FAILURES[@]} -gt 0 ]]; then
  echo "FAILED:"
  for f in "${FAILURES[@]}"; do echo "  - $f"; done
  exit 1
fi

echo "==> all checks passed"
