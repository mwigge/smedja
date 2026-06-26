#!/bin/sh
# VT-conformance smoke test for smedja's terminal grid (st-pty).
#
# Runs the headless golden grid tests, then diffs any recorded byte-stream
# fixtures (term/crates/st-pty/tests/fixtures/<name>.vt) against their golden
# snapshots (<name>.golden) via the `vtdump` example. No GPU/PTY required.
#
# To add a fixture: capture a real app's output bytes to <name>.vt, then
#   cargo run -q --example vtdump -p st-pty < <name>.vt > <name>.golden
# and eyeball the golden once.
set -e
cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

echo "== grid conformance unit tests =="
cargo test -q -p st-pty conformance

echo "== recorded fixtures =="
fixtures="term/crates/st-pty/tests/fixtures"
if [ ! -d "$fixtures" ] || [ -z "$(ls -A "$fixtures"/*.vt 2>/dev/null)" ]; then
    echo "  (no .vt fixtures yet — add captured app streams here)"
    exit 0
fi

fail=0
for vt in "$fixtures"/*.vt; do
    [ -e "$vt" ] || continue
    golden="${vt%.vt}.golden"
    out=$(cargo run -q --example vtdump -p st-pty < "$vt")
    if [ -f "$golden" ] && [ "$out" = "$(cat "$golden")" ]; then
        echo "  PASS $(basename "$vt")"
    else
        echo "  FAIL $(basename "$vt")  (regenerate: cargo run -q --example vtdump -p st-pty < '$vt' > '$golden')"
        fail=1
    fi
done
exit "$fail"
