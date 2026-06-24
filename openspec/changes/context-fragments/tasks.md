## 1. Fragment grammar and parser

- [x] 1.1 Add a failing test for the fragment tokeniser: `@file <path>`, `@git`, `@branch`, `@shell <cmd to end of line>` are recognised only when `@` begins a token; `foo@bar` and email addresses are left verbatim (`recognises_known_fragments_only_at_token_boundary`)
- [x] 1.2 Add a failing test that an unrecognised `@word` token is preserved unchanged (`unknown_fragment_left_verbatim`)
- [x] 1.3 Implement the fragment parser in a new `bin/smdjad/src/fragments.rs` module: scan the message, emit a sequence of literal-text and recognised-fragment spans
- [x] 1.4 Confirm 1.1–1.2 pass

## 2. `@file` resolution with workspace-boundary safety

- [x] 2.1 Add a failing test: `@file <relative-path>` inside the workspace expands to a fenced block containing the file contents (`file_fragment_injects_contents`)
- [x] 2.2 Add a failing path-traversal-denied test: `@file ../../etc/passwd` (and an absolute path outside the workspace) expands to the `path outside workspace` error marker and reads no file (`file_fragment_path_traversal_denied`)
- [x] 2.3 Add a failing test: `@file` pointing at a directory or an unreadable path expands to an error marker, not partial content (`file_fragment_non_file_errors`)
- [x] 2.4 Implement the `@file` resolver: route the path through `executor::fs_tools::assert_within_workspace`; on `Ok` read with `tokio::fs`; on `Err` emit the error marker
- [x] 2.5 Confirm 2.1–2.3 pass

## 3. `@git` and `@branch` resolution

- [x] 3.1 Add a failing test: `@git` expands to a fenced block containing `git status --short` and `git diff HEAD` output for a temp git workspace (`git_fragment_injects_status_and_diff`)
- [x] 3.2 Add a failing test: `@branch` expands to the current branch name (and upstream when present) for a temp git workspace (`branch_fragment_injects_current_branch`)
- [x] 3.3 Implement the `@git`/`@branch` resolvers using `exec_bash` against the session workspace
- [x] 3.4 Confirm 3.1–3.2 pass

## 4. `@shell` resolution with cowork gating

- [x] 4.1 Add a failing test: with cowork disabled, `@shell echo hi` expands to a fenced block containing `hi` (`shell_fragment_injects_output`)
- [x] 4.2 Add a failing test: with cowork enabled and the command approved via `CoworkGate`, `@shell` output is injected; when denied it expands to a `@shell denied` marker (`shell_fragment_respects_cowork_decision`)
- [x] 4.3 Implement the `@shell` resolver: when cowork is enabled, present the command via `CoworkGate::intercept`; run approved commands through `exec_bash` in the workspace
- [x] 4.4 Confirm 4.1–4.2 pass

## 5. Token-size guards

- [x] 5.1 Add a failing test: a fragment whose content exceeds the per-fragment byte/line cap is truncated with a visible `[smedja: truncated N bytes]` marker (`fragment_content_truncated_at_cap`)
- [x] 5.2 Add a failing test: total injected content across fragments is capped per message, with later fragments truncated once the aggregate cap is reached (`aggregate_fragment_cap_enforced`)
- [x] 5.3 Add a failing test: `SMEDJA_FRAGMENT_MAX_BYTES` overrides the per-fragment cap (`env_overrides_fragment_cap`)
- [x] 5.4 Implement the per-fragment and per-message caps with truncation markers and env overrides
- [x] 5.5 Confirm 5.1–5.3 pass

## 6. Wire expansion into `turn.submit`

- [x] 6.1 Add a failing test for `handlers::turn::submit`: a `content` containing `@file <path>` is recorded as the expanded text, not the raw `@file` token (`submit_expands_fragments_before_recording_task`)
- [x] 6.2 Add a failing test: a `content` with no fragments is recorded byte-for-byte unchanged (`submit_passes_through_when_no_fragments`)
- [x] 6.3 Implement the expansion pass in `handlers::turn::submit`: resolve the session workspace, run `fragments::expand(content, workspace, cowork)` over `content` before constructing the `Task`
- [x] 6.4 Confirm 6.1–6.2 pass

## 7. TUI help text

- [x] 7.1 Update the TUI help / slash-command documentation in `bin/smedja-tui/src/main.rs` to list the `@file`/`@git`/`@branch`/`@shell` syntax; confirm `submit` still sends the raw text unchanged

## 8. Verify

- [x] 8.1 Run `cargo test -p smdjad` — all green (parser, resolver, cap, and submit tests pass)
- [x] 8.2 Run `cargo test --workspace` — no failures introduced by the wiring
- [x] 8.3 Run `cargo clippy -p smdjad -- -D warnings` — clean for the touched code
- [x] 8.4 Run `openspec validate context-fragments --strict` — clean
