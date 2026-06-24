## 1. Scope selection

- [x] 1.1 Add a failing test for `AuditScope` resolution: `--diff`/no-args → `Diff`, a path arg → `Path`, `--branch <base>` → `Branch`, `--pr <ref>` → `Pr` (`bin/smdjad/src/handlers/auditor.rs` tests)
- [x] 1.2 Implement the `AuditScope` enum and a `resolve_scope(params)` parser in `bin/smdjad/src/handlers/auditor.rs`
- [x] 1.3 Add a failing test that each scope produces a non-empty seed string: `Diff`/`Branch`/`Pr` seed from a unified diff, `Path`/repo seed from a `graph_query` symbol listing + `list_files` tree
- [x] 1.4 Implement `build_seed(scope)` — `git diff HEAD` for `Diff`, `git diff <base>...<head>` for `Branch`, PR-ref → branch range → `Branch` for `Pr`, and graph/list seeding for `Path`
- [x] 1.5 Add a failing test that an unresolvable PR ref returns an `RpcError` (not a partial audit); implement the error path

## 2. Structured finding schema + parsing

- [x] 2.1 Add a failing test that a fenced JSON array of `{severity,file,line,rule,rationale}` parses into `Vec<AuditFinding>` (`auditor.rs` tests)
- [x] 2.2 Define `AuditFinding { severity, file, line: Option<u32>, rule, rationale }` with a `Severity` enum (`critical|high|medium|low|info`)
- [x] 2.3 Add a failing test that a malformed/partial finding object is skipped (non-fatal) while valid siblings are kept; implement tolerant per-object parsing
- [x] 2.4 Add a failing test that findings de-duplicate on `(file, line, rule)` (and `(file, rule)` when line is absent), first occurrence wins; implement `dedup_findings`

## 3. Read-only audit loop

- [x] 3.1 Add a failing test that the loop's tool allowlist is exactly `{graph_query, read_file, list_files}` and any other tool call is rejected and fed back as an error observation
- [x] 3.2 Implement the bounded exploration loop in `auditor.rs`: seed → Review-role turn → optional allowed tool call via `execute_tool` (`bin/smdjad/src/executor/mod.rs:94`) → append result → repeat, bounded by `max_iterations` (default 12) and a token budget
- [x] 3.3 Add a failing test that the loop runs the session in `"review"` mode so `role_allows_write_bash` (`bin/smdjad/src/executor/fs_tools.rs:36`) denies write-arity bash; assert no `write_file`/`edit_file` dispatch is ever constructed
- [x] 3.4 Add a failing test that the loop halts at `max_iterations`; implement the iteration bound

## 4. Persist findings as audit events

- [x] 4.1 Add a failing test that each `AuditFinding` persists as an `AuditEvent` with `action_type = "audit_finding"`, `actor = "review"`, `tier = Some("deep")`, `tool_name = Some(rule)`, `operation_name = severity`, `error_kind = file` via `Ingot::insert_audit_event` (`crates/smedja-ingot/src/audit.rs`)
- [x] 4.2 Implement `persist_findings(ingot, session_id, &findings)`; write `turn_start`/`turn_end` markers around the run so `smj audit query` and the timeline view see it
- [x] 4.3 Add a failing test that a run with zero findings still persists `turn_start`/`turn_end` and reports an empty finding set

## 5. Markdown report rendering

- [x] 5.1 Add a failing test that `render_report(&findings)` emits a deterministic markdown report: a per-severity count header then sections `Critical → High → Medium → Low → Info`, each line `` `file:line` — **rule** — rationale ``
- [x] 5.2 Implement `render_report`; verify identical findings render byte-identical across calls (deterministic ordering)
- [x] 5.3 Add a failing test for `--format json` emitting the full typed `Vec<AuditFinding>` (no field loss); implement the JSON branch

## 6. `audit.run` daemon RPC

- [x] 6.1 Add a failing handler test for `audit.run`: given a scope + workspace, it returns `{ findings: [...], counts: {...}, report_path|report: ... }` and persists the findings
- [x] 6.2 Implement the `audit.run` handler in `auditor.rs` (resolve scope → seed → loop → dedup → persist → render → respond) and register it in `bin/smdjad/src/handlers/mod.rs`
- [x] 6.3 Add a failing test that when `--report <path>` is given the report is written to that path and the response carries the path; otherwise the report body is returned inline

## 7. `/review` front end (modified capability)

- [x] 7.1 Add a failing test for `/review` flag parsing in the TUI: no args → diff scope; `<path>` → path scope; `--branch <base>` → branch; `--pr <ref>` → PR (`bin/smedja-tui/src/main.rs`)
- [x] 7.2 Replace the `"review"` arm of `dispatch_slash` (`bin/smedja-tui/src/main.rs:926`) to call `audit.run` with the parsed scope instead of building a one-shot `git diff HEAD` prompt; ensure the session is in `"review"` mode
- [x] 7.3 Add a failing test that the command prints a structured findings summary (counts per severity) and the written report path; implement the summary rendering
- [x] 7.4 Add a failing test that `/review` with everything committed (empty `git diff HEAD`) no longer hard-refuses — it falls back to path/repo scope; implement the fallback

## 8. `smj audit` CLI twin

- [x] 8.1 Add a failing test for clap parsing of `smj audit [<path>] [--branch <base>] [--pr <ref>] [--diff] [--report <path>] [--format md|json]` (`bin/smj/src/main.rs`)
- [x] 8.2 Implement the `audit` subcommand following the `Cmd::Loop`/`Cmd::Audit` connect-and-call shape: connect to the daemon, call `audit.run`, write the report (or print it for stdout / `--format json`)
- [x] 8.3 Add a failing integration-style test that `smj audit <path>` against a workspace produces a markdown report (non-empty header, severity sections); implement until green

## 9. Verify

- [x] 9.1 `cargo fmt --all`
- [x] 9.2 `cargo clippy --workspace --all-targets -- -D warnings -W clippy::pedantic` clean for the touched crates (`smdjad`, `smedja-tui`, `smj`)
- [x] 9.3 `cargo test --workspace` — all green
- [x] 9.4 `openspec validate repo-auditor --strict` — clean
