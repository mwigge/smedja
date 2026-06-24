## 1. Resolver helper (Red → Green)

- [x] 1.1 Add a failing test in `bin/smedja-tui/src/main.rs` test module asserting that `resolve_cowork` returns `true` only when the RPC response contains `"resolved": true`, `false` for `"resolved": false`, missing field, and a simulated transport error (use a stub/mock `Client` call or a thin wrapper that takes the parsed `Value`)
- [x] 1.2 Implement `resolve_cowork(client, session_id, method, params) -> bool` near `dispatch_slash` in `bin/smedja-tui/src/main.rs`: call the RPC, read `resolved` via `.get("resolved").and_then(Value::as_bool).unwrap_or(false)`, return `false` on `Err`
- [x] 1.3 Run `cargo test -p smedja-tui` and confirm 1.1 passes

## 2. Route widget approve/deny through the resolver

- [x] 2.1 Add a failing test for the `handle_key` cowork branch asserting that on `y` with `resolved: true` the first item is removed and an `approved:` system message is pushed, and on `resolved: false` the item is retained with an `item not found` message (drive `handle_key` or extract the decision logic into a testable `apply_cowork_decision` fn that takes the parsed result)
- [x] 2.2 Replace the `KeyCode::Char('y' | 'Y')` arm (~1275) so it captures `tool`/`id` from `pending_cowork.first()`, calls `resolve_cowork(..., "cowork.approve", ...)`, removes the item only when it returns `true`, and pushes `approved: <tool>` / `item not found: <tool>`
- [x] 2.3 Replace the `KeyCode::Char('n' | 'N')` arm (~1287) symmetrically for `cowork.deny` (reason `"denied"`), pushing `denied: <tool>` / `item not found: <tool>`
- [x] 2.4 Run `cargo test -p smedja-tui` and confirm 2.1 passes

## 3. Surface the modify flow

- [x] 3.1 Add a failing test asserting that submitting modify with `resolved: true` removes the item and pushes `modify sent: <instruction>`, and with `resolved: false` retains the item
- [x] 3.2 Update the `cowork_modify_mode` `KeyCode::Enter` arm (~1247) to capture the instruction and tool, call `resolve_cowork(..., "cowork.modify", ...)`, remove the item only on `true`, push `modify sent: <instruction>` / `item not found: <tool>`, then clear `cowork_modify_mode` and `cowork_modify_input`
- [x] 3.3 Run `cargo test -p smedja-tui` and confirm 3.1 passes

## 4. Widget render coverage

- [x] 4.1 Add a failing render test in `bin/smedja-tui/src/cowork_widget.rs` asserting the decision footer shows `[y]`, `[n]`, and `[m]` for a single pending item (extends the existing `widget_renders_approve_deny_modify_shortcuts` style) and that modify mode shows the current instruction buffer
- [x] 4.2 Confirm the assertion passes against the existing `CoworkWidget::render` (no widget behaviour change expected); adjust only the test if the contract is already satisfied

## 5. Verify

- [x] 5.1 `cargo fmt --all` — clean
- [x] 5.2 `cargo clippy -p smedja-tui -- -D warnings -W clippy::pedantic` — clean for the touched code
- [x] 5.3 `cargo test -p smedja-tui` — all green
- [x] 5.4 `cargo test --workspace` — no regressions introduced by the change
- [x] 5.5 `openspec validate cowork-inline-widget --strict` — clean
