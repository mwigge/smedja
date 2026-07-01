# Maintenance Guide

This guide maps the main Rust entrypoints to their owning modules and the checks
to run before changing them. Keep entrypoints thin: route commands and events to
domain modules instead of adding new logic to `main.rs`, `run.rs`, or `lib.rs`.

## smj CLI

The `smj` binary calls `smedja_cli::run()`. The CLI crate is structured as:

| File | Responsibility |
|------|----------------|
| `crates/smedja-cli/src/run.rs` | Top-level command routing only |
| `crates/smedja-cli/src/cli.rs` | `clap` root parser and `Cmd` enum |
| `crates/smedja-cli/src/cli/commands/*.rs` | Subcommand argument definitions grouped by domain |
| `audit.rs`, `sessions.rs`, `tasks.rs`, `loop_cmd.rs` | Workflow and session command dispatch |
| `usage.rs`, `timeline.rs`, `workspace.rs` | Reporting, local history, and workspace dispatch |
| `daemon.rs`, `mcp.rs`, `prices.rs`, `sandbox.rs`, `security.rs`, `terminal.rs`, `eval.rs` | Operational command dispatch |
| `tests.rs` | CLI parsing, formatter, security, workspace, and helper tests |

When adding a new `smj` command:

1. Add its argument shape under `cli/commands/` if it belongs to an existing
   group, or create a new grouped file and re-export it from `cli/commands.rs`.
2. Add only the top-level `Cmd` variant to `cli.rs`.
3. Put behavior in the closest domain module as `dispatch_<domain>`.
4. Add or update a parsing/behavior test in `crates/smedja-cli/src/tests.rs`
   or the domain module's own `#[cfg(test)]` block.
5. Run `cargo test -p smedja-cli`.

## smedja GPU Terminal

The `smedja` binary calls `st_app::run()`. The terminal app is structured as:

| File | Responsibility |
|------|----------------|
| `term/crates/st-app/src/lib.rs` | CLI parsing and event-loop startup only |
| `app/mod.rs` | App state and high-level helpers |
| `app/events.rs` | `winit::ApplicationHandler` implementation |
| `app/keyboard.rs` | Keyboard command handling |
| `app/redraw.rs` | Frame upload and redraw handling |
| `input.rs`, `mouse.rs` | Terminal input encoding |
| `tab.rs`, `split.rs` | Tab and split-pane state models |
| `status.rs`, `render.rs` | Status/title helpers and cell rendering |
| `tests.rs` | App-level input, title, status, and constructor tests |

When changing event handling, prefer a helper in `app/keyboard.rs`,
`app/redraw.rs`, `input.rs`, or `mouse.rs` before growing
`app/events.rs`. Add tests in the owning module when the behavior is pure; use
`term/crates/st-app/src/tests.rs` for app-level behavior that needs `App`.

Run `cargo test -p st-app` after terminal changes.

## Verification Matrix

Use the narrowest package test while iterating, then broaden when a change
touches shared contracts.

| Change area | Minimum check |
|-------------|---------------|
| `smedja-cli` command parsing/dispatch | `cargo test -p smedja-cli` |
| `st-app` terminal behavior | `cargo test -p st-app` |
| Shared crates or cross-crate contracts | `cargo test --workspace -- --test-threads=1` |
| Formatting and whitespace | `cargo fmt --check` and `git diff --check` |

The full workspace can contain tests that mutate process-global environment, so
serial execution is the preferred broad check.

## Test Placement Policy

Every behavior-bearing module should have either local unit tests or coverage
from the crate-level test module. Small dispatch modules may be covered through
CLI parser tests plus package compilation when their behavior depends on a live
daemon, Docker, or a graphical event loop.

Prefer local tests for pure helpers:

- parsing and formatting helpers
- state transitions in tab/split/session models
- path and environment resolution
- input and mouse byte encoding

Prefer crate-level tests for behavior that crosses modules:

- CLI parse trees
- app construction
- status/title composition
- filesystem fixtures such as workspace initialization or skill symlinks
