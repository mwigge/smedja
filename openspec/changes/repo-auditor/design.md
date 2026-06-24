## Context

`/review` is a one-shot prompt builder. Its arm in `dispatch_slash` (`bin/smedja-tui/src/main.rs:926`) runs `git diff HEAD`, bails when the diff is empty, wraps the diff in `format!("Review the following git diff:{focus}\n\n```diff\n{diff}```")`, and calls `submit(&message, state, client)` (`bin/smedja-tui/src/main.rs:336`), which runs a single provider turn. There is no scope beyond the working tree, no structured output, no persistence, and no report.

The pieces a real auditor needs already exist:

- **Read-only Review role.** `AgentRole::Review` routes to Claude/Deep (`crates/smedja-assayer/src/assayer.rs:89`). The daemon's executor already treats `"review"` mode as read-only: `role_allows_write_bash(session)` returns `false` for `session.mode == Some("review")` (`bin/smdjad/src/executor/fs_tools.rs:36`). The TUI sets this mode via `/agent review` (`apply_agent`, `bin/smedja-tui/src/main.rs:685`).
- **Read + graph tools.** `execute_tool` (`bin/smdjad/src/executor/mod.rs:94`) dispatches `read_file`, `list_files`, and `graph_query` from `LOCAL_TOOLS` (`bin/smdjad/src/executor/mod.rs:32`); `graph.query` is served by `bin/smdjad/src/handlers/graph.rs:94`.
- **Bounded loop pattern.** `crates/smedja-loop/src/engine.rs` drives a deterministic state machine and delegates side effects to `RoleRunner`/`StatusSink` traits, keeping provider/DB coupling out of the engine.
- **Findings sink.** `AuditEvent` (`crates/smedja-ingot/src/audit.rs`) is an append-only record with `action_type`, `actor`, `tool_name`, `input_tok`/`output_tok`, `tier`, `status`, and free-form correlation columns. `Ingot::insert_audit_event` persists it.
- **CLI shape.** `bin/smj/src/main.rs` already has `Cmd::Audit` (query) and `Cmd::Loop` (drive a change over the daemon) — `smj audit` follows the same `Client::connect` → `client.call(...)` pattern.

## Goals / Non-Goals

Goals:
- Expand `/review` into a scoped, read-only auditor producing structured, persisted findings and a markdown report.
- Reuse the read-only Review role and the existing `graph_query`/`read_file`/`list_files` tools — never write.
- Support four scopes: working-tree diff, path/whole-repo, branch range, PR.
- Provide a non-interactive `smj audit` twin that produces the same report.

Non-Goals:
- New write tools, a sandbox, or any mutation of the repository (the auditor is strictly read-only).
- Changing `AgentRole` routing or adding a new role.
- Auto-applying fixes — findings are advisory; remediation is a human/loop follow-up.
- Multi-provider failover (owned by `provider-failover`) and cold-stratum retrieval (owned by `wire-cold-context`).
- GitHub/GitLab API integration beyond resolving a PR ref to a branch range via the available git remote.

## Decisions

**Decision: scope selection produces a seed context, then one loop drives all scopes.**
The handler maps the requested scope to a `AuditScope` enum: `Diff` (`git diff HEAD`, today's default), `Path { root }` (path or whole repo), `Branch { base, head }` (`git diff <base>...<head>`), `Pr { ref }` (resolve to a branch range, then `Branch`). Each scope yields a seed string: diff/branch/PR seed from the unified diff; path/repo seed from a `graph_query` symbol listing plus a `list_files` tree. The same audit loop consumes the seed regardless of scope.
- Rationale: scope only differs in how the seed is gathered; the exploration + finding-aggregation logic is identical, so one loop avoids four code paths.
- Alternative: a distinct loop per scope. Rejected — duplicated loop control and divergent finding handling.

**Decision: the audit loop is a bounded read-only exploration loop, modelled on the loop engine.**
The loop runs the Review role: seed → provider turn → optional tool call (`graph_query`/`read_file`/`list_files` only) → append tool result → repeat, bounded by `max_iterations` (default small, e.g. 12) and an overall token budget. The loop refuses any tool outside the read-only allowlist. It mirrors `smedja-loop`'s side-effect-delegation shape (a runner that performs the turn, a sink that records findings) but lives in the auditor handler because it drives the provider/tool path directly rather than the multi-role pipeline.
- Rationale: bounded iteration prevents runaway cost; the read-only allowlist is the structural read-only guarantee on top of the `role_allows_write_bash` gate.
- Alternative: reuse `smedja-loop::Engine` verbatim. Rejected for v1 — the engine drives multi-role slices with a verification gate; the auditor is a single-role read loop. The `RoleRunner`/`StatusSink` *pattern* is reused, not the concrete engine.

**Decision: read-only is enforced by allowlist AND the existing review-mode gate.**
The loop only ever offers `graph_query`/`read_file`/`list_files`; if the model emits any other tool call it is rejected and fed back as an error observation. The session runs in `"review"` mode so `role_allows_write_bash` already denies write-arity bash. Two independent guarantees, no new enforcement surface.
- Rationale: defence in depth without inventing a new permission system.

**Decision: structured finding schema `AuditFinding { severity, file, line, rule, rationale }`.**
The model is instructed to emit findings as a fenced JSON array of objects with those fields; the handler parses them. `severity` is one of `critical|high|medium|low|info`; `file` is workspace-relative; `line` is optional (`u32`); `rule` is a short slug (e.g. `error-handling`, `unwrap-in-lib`); `rationale` is one sentence. Malformed or partial objects are skipped (non-fatal), matching the crusher's tolerant-parse philosophy.
- Rationale: a fixed, parseable schema is what turns prose review into queryable findings; tolerant parsing keeps a single bad object from failing the whole audit.

**Decision: findings persist as `AuditEvent`s, no schema change.**
Each `AuditFinding` is written via `Ingot::insert_audit_event` with `action_type = "audit_finding"`, `actor = "review"`, `tier = "deep"`, `tool_name = Some(rule)`, and the severity/file/line/rationale packed into the existing correlation columns (`operation_name = severity`, `error_kind = file`, plus rationale carried in the turn record). The audit run also writes `turn_start`/`turn_end` markers so `smj audit query` and the timeline view see it.
- Rationale: reuse the proven append-only sink and existing `smj audit`/timeline tooling; no migration. The 23-column `AuditEvent` already has free-form string columns suited to this.
- Alternative: a new `audit_findings` table. Rejected for v1 — no migration needed and existing query/export tooling works unchanged.

**Decision: deterministic markdown report grouped by severity.**
After dedup, findings render to markdown: a summary header (counts per severity), then sections `Critical → High → Medium → Low → Info`, each a list of `` `file:line` — **rule** — rationale ``. The report is written to the caller's `--report` path (or stdout for the CLI when omitted), and a short summary (counts + path) returns over the RPC.
- Rationale: deterministic ordering makes reports diffable across runs; markdown is the project's report convention (cf. drawio/pptx output types in the TUI).

**Decision: dedup on `(file, line, rule)`.**
Before persistence and rendering, findings are de-duplicated by the tuple `(file, normalized-line, rule)`; the first occurrence wins and its rationale is kept. Findings with no line dedup on `(file, rule)`.
- Rationale: an exploration loop may surface the same issue from multiple read paths; dedup keeps the report and the audit log honest.

**Decision: `smj audit` is a thin RPC client.**
`smj audit [<path>] [--branch <base>] [--pr <ref>] [--diff] [--report <path>] [--format md|json]` connects to the daemon and calls `audit.run` with the resolved scope, then writes the report (or prints it). It follows the `Cmd::Loop`/`Cmd::Audit` connect-and-call shape in `bin/smj/src/main.rs`.
- Rationale: keep audit logic server-side (one implementation); the CLI and TUI are both thin front ends, so behaviour cannot drift.

## Risks / Trade-offs

- [Risk] An unbounded exploration loop runs up provider cost → Mitigation: hard `max_iterations` cap and a token budget; each iteration's usage is recorded as an `AuditEvent` so cost is visible via `smj cost`/timeline.
- [Risk] The model emits a write tool call despite read-only intent → Mitigation: the loop's allowlist rejects it and the `"review"`-mode `role_allows_write_bash` gate denies write-arity bash regardless; the auditor never constructs `write_file`/`edit_file` dispatch.
- [Risk] Whole-repo scope produces an enormous seed → Mitigation: path/repo seeds from `graph_query` symbol summaries + a bounded `list_files`, not file contents; the loop reads files on demand within the iteration/token budget.
- [Risk] Malformed finding JSON breaks the run → Mitigation: tolerant per-object parsing; bad objects are skipped, the run still produces a report from the valid findings.
- [Risk] Packing findings into existing `AuditEvent` columns is lossy/ad-hoc → Mitigation: a documented column mapping; `--format json` emits the full typed `AuditFinding` so no field is lost for machine consumers. A dedicated table can follow later without breaking the RPC.
- [Risk] PR resolution depends on remote/host specifics → Mitigation: v1 resolves a PR ref to a branch range via the local git remote; unresolvable refs return a clear RPC error rather than a partial audit.
