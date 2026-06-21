# Delivery

## Definition of Done

A task checkbox may only be set to `[x]` when **all** of the following are true:

1. A failing test exists that covers the behaviour (red phase).
2. The implementation makes that test pass (green phase).
3. `scripts/smoke-test.sh` exits 0.

If `scripts/smoke-test.sh` does not pass, do not check the box.

## Current Feature Status

| Feature | Dimension | Status |
|---------|-----------|--------|
| GPU terminal (`smedja`) | text rendering | broken — glyphs silently skipped when not in atlas |
| GPU terminal (`smedja`) | startup time | ~10 s (FontSystem::new() blocks main thread) |
| smedja-tui | connects to daemon | yes |
| smedja-tui | streaming | no |
| smedja-tui | connect banner | no (empty screen on startup) |
| smedja-tui | status bar model | shows `[unknown]` — hardcoded None |
| agent bridge (st-agent → smdjad) | wired | no — tier/model are `None` with "future work" comment |
| OTel | gen_ai.* semantic conventions | partial — missing conversation.id, agent.name, operation.name, TTFT, tool call IDs |
| VTE | escape sequences handled | ~20 — missing cursor home, delete line, index, many OSC sequences |
| VTE | conformance tests | none |
| TUI | functional tests | none |
| GPU terminal | headless tests | none |

## Test Layers

```bash
# Unit and integration tests for all crates
cargo test --workspace

# TUI smoke integration tests (currently expected to fail — write the tests first)
cargo test -p smedja-tui --test smoke

# All layers via the gate script
./scripts/smoke-test.sh
```

## How to Check a Task Done

1. Write a test that fails without the feature (`cargo test` goes red).
2. Implement the feature until `cargo test` goes green.
3. Run `./scripts/smoke-test.sh` from the repo root — it must exit 0.
4. Set the checkbox to `[x]` in the relevant `tasks.md`.

Do not skip step 3. A passing unit test without a passing smoke run is not done.
