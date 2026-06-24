## Why

The `/review` slash command (`bin/smedja-tui/src/main.rs:926`) is the only review capability smedja ships, and it is shallow: it shells out to `git diff HEAD`, wraps the output in a `"Review the following git diff:"` prompt, and submits a single turn via `submit` (`bin/smedja-tui/src/main.rs:336`). The result is free-text prose — no structured findings, no severity, no file/line anchoring, no whole-repo scope, no persisted record, and no report artifact. If `git diff HEAD` is empty (everything committed) the command refuses outright with `"no unstaged changes to review"`, so it cannot review a branch, a PR, or the repository as it stands.

Meanwhile the building blocks for a real auditor already exist and are unused for this purpose:

- A read-only **Review role** — `AgentRole::Review` routes to Claude/Deep (`crates/smedja-assayer/src/assayer.rs:89`), and the daemon enforces read-only bash for `"review"` mode (`bin/smdjad/src/executor/fs_tools.rs:36` `role_allows_write_bash`).
- Code-intelligence and read tools the daemon already dispatches: `graph_query`, `read_file`, `list_files` (`bin/smdjad/src/executor/mod.rs:32` `LOCAL_TOOLS`; `graph.query` handler at `bin/smdjad/src/handlers/graph.rs:94`).
- A bounded, side-effect-delegating loop engine (`crates/smedja-loop/src/engine.rs`) whose `RoleRunner`/`StatusSink` split is the proven pattern for a deterministic agentic loop.
- An append-only findings sink — `AuditEvent` and `Ingot::insert_audit_event` (`crates/smedja-ingot/src/audit.rs`).

There is also no non-interactive twin: `smj` (`bin/smj/src/main.rs`) has `Audit` (query the log) and `Loop` (drive a change) subcommands, but nothing that audits code from a shell or CI.

This change turns `/review` into the front door of a full **repo/PR/branch AI code auditor** and adds a non-interactive `smj audit` CLI twin, both built on the read-only Review role so the auditor can read, query the graph, and reason — but never write.

## What Changes

- **New `repo-auditor` capability in `smdjad`**: an agentic, read-only audit loop. Given a scope, it runs the **Review role** (`AgentRole::Review`, read-only) through a bounded exploration loop using only `graph_query`, `read_file`, and `list_files`, aggregates the model's output into STRUCTURED findings (severity, file, line, rationale), persists each finding as a `smedja-ingot` `AuditEvent`, and emits a markdown report.
- **Scope selection**: the audit accepts one of four scopes — working-tree diff (`git diff HEAD`, today's behaviour), a path / the whole repo, a branch range (`git diff <base>...<head>`), or a PR (resolve PR → branch range). The scope produces the seed context handed to the loop; whole-repo and path scopes seed from `graph_query` symbol listings plus `list_files` rather than a raw diff.
- **Structured finding schema + dedup**: findings are parsed into a typed `AuditFinding { severity, file, line, rule, rationale }`. Identical findings (same file + line + rule) are de-duplicated before persistence and report rendering.
- **Markdown report**: the loop renders findings into a deterministic markdown report grouped by severity, written to a caller-supplied path (or stdout for the CLI), and summarised back to the caller.
- **Read-only guarantee**: the auditor MUST run under the Review role's read-only contract — it never invokes `write_file`/`edit_file` and never runs write-arity bash. This reuses the existing `role_allows_write_bash` read-only gate rather than adding a new enforcement path.
- **Modify `/review`**: the TUI slash command becomes a thin front end for the auditor. `/review` (no args) audits the working-tree diff; `/review <path>` audits a path/repo; `/review --branch <base>` audits a branch range; `/review --pr <ref>` audits a PR. It calls the new daemon RPC, streams progress, prints the structured findings summary, and reports the written report path — replacing the single free-text turn.
- **New `smj audit` CLI twin**: `smj audit [<path>] [--branch <base>] [--pr <ref>] [--diff] [--report <path>] [--format md|json]` drives the same daemon RPC non-interactively and writes the markdown report, mirroring the `Audit`/`Loop` subcommand patterns in `bin/smj/src/main.rs`.

Out of scope (referenced only): no new write tools or sandbox changes; no change to `AgentRole` routing rules; no provider-failover behaviour; the cold-stratum/semantic-retrieval work owned by `wire-cold-context` is not required here.

## Capabilities

### New Capabilities

- `repo-auditor`: `smdjad` runs a bounded, read-only audit loop under the Review role over a selected scope (diff / path / branch / PR), using only `graph_query`/`read_file`/`list_files`, aggregating structured `AuditFinding`s that are de-duplicated, persisted as `AuditEvent`s, and rendered to a markdown report; exposed via a daemon RPC and the `smj audit` CLI.

### Modified Capabilities

- `review-command`: the `/review` TUI slash command becomes a front end for the `repo-auditor` loop with scope flags (diff / path / branch / PR), replacing the single `git diff HEAD` free-text turn with a structured, persisted, report-producing audit.

## Impact

- `bin/smdjad/src/handlers/`: new `auditor.rs` handler exposing the `audit.run` RPC (scope selection, loop driving, report rendering); registered in `bin/smdjad/src/handlers/mod.rs`.
- `bin/smdjad/src/executor/mod.rs`: the auditor restricts its tool allowlist to `graph_query`/`read_file`/`list_files`; relies on the existing `role_allows_write_bash` read-only gate (`executor/fs_tools.rs:36`).
- `crates/smedja-ingot/src/audit.rs`: findings persist as `AuditEvent`s (new `action_type = "audit_finding"`), reusing the existing schema (no column change).
- `bin/smedja-tui/src/main.rs`: the `"review"` arm of `dispatch_slash` (~line 926) calls the auditor RPC with parsed scope flags instead of building a one-shot diff prompt.
- `bin/smj/src/main.rs`: new `Audit`-adjacent `audit` subcommand (`Cmd::Audit` gains a `Run` action, or a new top-level `Cmd::Audit`-style command) driving `audit.run`.
- README: the review feature description becomes "structured, read-only repo/PR/branch auditor" rather than "review a diff".
