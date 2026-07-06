#!/usr/bin/env bash
#
# Ratcheting file-size gate (clean-as-you-code, SonarQube-style).
#
#   scripts/file-size-gate.sh              # enforce over the whole tree
#   scripts/file-size-gate.sh --staged     # enforce over staged .rs files only
#   scripts/file-size-gate.sh --regenerate # rewrite the baseline (ratchet down)
#
# Semantics (mirrors smedja_methodology::file_size::enforce):
#   * a file at or below `threshold`                         -> allowed
#   * a baselined file at or below its recorded ceiling      -> allowed (grandfathered)
#   * a baselined file grown past its ceiling                -> BLOCK
#   * a non-baselined file over `threshold`                  -> BLOCK
#
# Threshold comes from .smedja/quality.toml (default 600). The baseline lives in
# .smedja/file-size-baseline.toml. `--regenerate` snapshots the current tree so
# the ceiling only ever ratchets DOWN as files are split.
set -uo pipefail

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$ROOT"

QUALITY=".smedja/quality.toml"
BASELINE=".smedja/file-size-baseline.toml"

read_threshold() {
  local t=""
  if [[ -f "$QUALITY" ]]; then
    t="$(grep -E '^[[:space:]]*file_size_threshold[[:space:]]*=' "$QUALITY" 2>/dev/null \
         | head -1 | grep -oE '[0-9]+' || true)"
  fi
  echo "${t:-600}"
}

THRESHOLD="$(read_threshold)"

# Rust source files, always excluding build output and other agents' worktrees.
tracked_rs() {
  git ls-files '*.rs' | grep -v '^\.claude/worktrees/' || true
}

regenerate() {
  {
    cat <<'EOF'
# File-size gate baseline — grandfathered oversized files.
#
# GENERATED FILE. Do not edit by hand. Regenerate with:
#     scripts/file-size-gate.sh --regenerate
#
# The gate ALLOWS each listed file at or below its recorded line count and
# FAILS if it grows past it. Any file NOT listed here that crosses `threshold`
# also FAILS. The baseline is a ceiling that only ratchets DOWN.
EOF
    echo "threshold = $THRESHOLD"
    echo
    echo "[files]"
    tracked_rs | while IFS= read -r f; do
      [[ -f "$f" ]] || continue
      n=$(wc -l < "$f")
      if (( n > THRESHOLD )); then
        printf '"%s" = %d\n' "$f" "$n"
      fi
    done | sort
  } > "$BASELINE"
  local count
  count=$(grep -cE '^".*" = [0-9]+' "$BASELINE" || true)
  echo "regenerated $BASELINE — $count file(s) grandfathered (threshold $THRESHOLD)"
}

# Load baseline ceilings into an associative array.
declare -A CEIL
load_baseline() {
  [[ -f "$BASELINE" ]] || return 0
  local line
  while IFS= read -r line; do
    if [[ "$line" =~ ^\"(.+)\"[[:space:]]*=[[:space:]]*([0-9]+) ]]; then
      CEIL["${BASH_REMATCH[1]}"]="${BASH_REMATCH[2]}"
    fi
  done < "$BASELINE"
}

changed_files() {
  # Staged, added/copied/modified .rs files (the commit's changed set).
  git diff --cached --name-only --diff-filter=ACM -- '*.rs' | grep -v '^\.claude/worktrees/' || true
}

check() {
  load_baseline
  local -a files=()
  if [[ "${1:-}" == "--staged" ]]; then
    mapfile -t files < <(changed_files)
  else
    mapfile -t files < <(tracked_rs)
  fi

  local fails=0
  local f n ceiling
  for f in "${files[@]}"; do
    [[ -n "$f" ]] || continue
    [[ "$f" == *.rs ]] || continue
    [[ -f "$f" ]] || continue
    n=$(wc -l < "$f")
    (( n > THRESHOLD )) || continue
    ceiling="${CEIL[$f]:-}"
    if [[ -z "$ceiling" ]]; then
      echo "  BLOCK: $f ($n L) exceeds threshold $THRESHOLD and is not baselined"
      fails=1
    elif (( n > ceiling )); then
      echo "  BLOCK: $f ($n L) grew past baseline ceiling $ceiling (threshold $THRESHOLD)"
      fails=1
    fi
  done

  if (( fails )); then
    echo "file-size gate FAILED — split the file(s), or (if a split legitimately"
    echo "raised a ceiling) run: scripts/file-size-gate.sh --regenerate"
    return 1
  fi
  echo "file-size gate passed (threshold $THRESHOLD)"
  return 0
}

case "${1:-}" in
  --regenerate) regenerate ;;
  --staged)     check --staged ;;
  --all|"")     check ;;
  *) echo "usage: $0 [--staged|--all|--regenerate]" >&2; exit 2 ;;
esac
