#!/usr/bin/env bash
# Run all test layers. Layer 3 (GPU) only when --gpu flag is passed.
set -euo pipefail

GPU=false
for arg in "$@"; do
    [[ "$arg" == "--gpu" ]] && GPU=true
done

echo "=== Layer 1+2: st-pty unit + VTE conformance ==="
cargo test -p st-pty

echo "=== Layer 1: st-glyph unit ==="
cargo test -p st-glyph

echo "=== Layer 1: st-render unit (non-GPU) ==="
cargo test -p st-render

if [[ "$GPU" == "true" ]]; then
    echo "=== Layer 3: st-render GPU smoke ==="
    LIBGL_ALWAYS_SOFTWARE=1 cargo test -p st-render --features gpu-tests
fi

echo "=== Layer 4: TUI functional ==="
cargo test -p smedja-tui

echo "=== Layer 5: Smoke ==="
cargo test -p smedja-tui --test smoke

echo "All layers passed."
