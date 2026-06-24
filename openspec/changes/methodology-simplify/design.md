## Context

The methodology system is a single-axis "mode" string per session that the
daemon maps onto a compile-time enum and runs as a write-time diff gate:

- `smedja_methodology::Mode` has five variants — `Tdd`, `Ponytail`, `Spec`,
  `Clean`, `Sre` (`crates/smedja-methodology/src/types.rs:3-16`).
- `parse_mode` maps the persisted `mode` string to `Mode`, returning `None`
  (ungated) for anything unrecognised (`bin/smdjad/src/methodology_gate.rs:14`).
- `run_gates` dispatches: `Tdd` → `tdd::check`, `Clean` → `clean::check`,
  `Ponytail` → `ponytail::check`, and `Spec | Sre` → `Ok(())` — i.e. no gate
  (`bin/smdjad/src/methodology_gate.rs:33-39`).
- The gate runs in the executor on `write_file` / `edit_file` after the
  path-traversal guard, gated behind the per-session `no_spec_gate` escape
  (`bin/smdjad/src/executor/mod.rs:318-368`); the escape and the spec-first
  bookkeeping live in `MethodologyState`
  (`crates/smedja-ingot/src/methodology.rs:13-21`).

The gate functions:

- `tdd::check` (`crates/smedja-methodology/src/tdd.rs:16`) counts added `+fn `
  lines vs. added `+#[test]` / `+mod tests` lines and fails when impl > 0 and
  tests == 0.
- `clean::check` (`crates/smedja-methodology/src/clean.rs:23`) scans added lines
  outside `#[cfg(test)]` for `.unwrap()` / `.expect(` / `println!`.
- `ponytail::check` (`crates/smedja-methodology/src/ponytail.rs:17`) is a thin
  wrapper that calls `clean::check_added_lines` with a different gate label — it
  is byte-for-byte the same enforcement as `clean`.

The TUI side:

- `SLASH_COMPLETIONS` lists `/tdd` and `/ponytail`
  (`bin/smedja-tui/src/main.rs:132-156`).
- The `/agent` arm calls `session.set_mode` for `impl|review|test|sre|explain`
  via `apply_agent` (`:997`, `:1027`) — none of which are gate modes.
- The `/tdd` arm (`:1106`) and `/ponytail` arm (`:1111`) only set local
  `state.mode` and never call `session.set_mode`, so the daemon never sees them.

The prompt-assembly seam:

- `TurnOrchestrator` builds a per-turn `WorkingMemory`, pushes pre-turn context
  (graph symbols, workspace skills) plus the user turn, then calls
  `seal_prefix()` (`bin/smdjad/src/orchestrator/mod.rs:416-512`). Everything
  before the seal becomes the cacheable stable prefix.
- Workspace skills are injected as one system message before the seal via
  `inject_workspace_skills` (`crates/smedja-memory/src/memory.rs:340`), which
  reads `<workspace>/.smedja/skills/*.md` through `load_workspace_skills`
  (`:313`).

The config-loading model:

- The security plane resolves a `[security]` block from
  `<workspace>/.smedja/config.toml` via `SecurityConfig::from_toml_str`
  (`crates/smedja-security/src/config.rs:52`) and `load_security_config`
  (`bin/smdjad/src/security.rs:22`). An absent block resolves to a safe default;
  parse failure logs and falls back to the default — it never blocks startup.

## Goals / Non-Goals

Goals:
- Make TDD and clean-code a foundational discipline that is present in the
  agent's instructions on every code-writing turn (steer-first).
- Keep a diff backstop, but make it advisory and egregious-only so it does not
  fight normal refactoring/helper/doc edits.
- Make the discipline on by default with a per-workspace escape in
  `.smedja/config.toml`, modelled on the existing security config.
- Remove the broken/misleading selectable modes and TUI commands (`/tdd`,
  `/ponytail`), the `Tdd`/`Ponytail` enum variants, and the dormant `Sre` mode.
- Demote `ponytail` to an on-demand review skill.

Non-Goals:
- The `spec` lifecycle and its `Mode::Spec` / `MethodologyState` bookkeeping —
  owned by the lean-specs / OpenSpec work; `Mode::Spec` and the spec-first gate
  in the executor are left untouched here.
- The `/agent impl|review|test|sre|explain` routing concept — that is agent
  routing, not methodology, and stays as-is (only its accidental coupling to a
  methodology mode for `sre` is severed).
- Per-session runtime toggling of the discipline — the escape is per-workspace
  config, not a live command.
- Cross-language gate rules — the backstop remains Rust-shaped, matching the
  current gate.

## Decisions

**Decision: TDD + clean are foundational, enforced steer-first.**
On every code-writing turn the orchestrator injects an always-on discipline
directive (write a failing test first; no `unwrap`/`expect`/`println!` in
library code; small focused functions; early-return over `else`) into the system
prefix before `seal_prefix()`
(`bin/smdjad/src/orchestrator/mod.rs:416-512`), alongside workspace skills. The
steering being present every turn is the primary enforcement; the diff gate is a
backstop.
- Rationale: an LLM that is reminded of the discipline on every turn produces
  conforming code far more reliably than a post-hoc reject. Putting the directive
  in the sealed prefix means it is cached and costs almost nothing per turn.
- Alternative considered: keep enforcement purely at the gate. Rejected — the
  gate only ever says "no" after the model already wrote non-conforming code; it
  teaches nothing and blocks legitimate edits (see the edge below).

**Decision (the key edge): foundational ≠ naive hard-block.**
Today's `tdd::check` fails ANY added `+fn ` with no nearby test
(`crates/smedja-methodology/src/tdd.rs:16-44`). That rule was tolerable as an
*opt-in* mode; as an *always-on* foundation it is actively harmful — it would
reject a refactor that extracts a private helper, a doc-comment edit that happens
to touch a `fn` line, or any change to a file whose tests live elsewhere. A
merciless always-on gate fights the developer and trains them to set the escape.
Therefore the foundational gate SHALL be **steering-unconditional + gate-sane**:
- The steering directive is injected unconditionally on every code-writing turn
  (when `methodology.tdd` is on).
- The backstop SHALL only raise when the change adds *substantial* new
  implementation with *zero* tests anywhere in the whole change, and SHALL
  surface as an advisory warning rather than a blunt reject — the value is the
  steering always being present, not the gate being merciless.
- `clean`'s backstop (`.unwrap()`/`.expect(`/`println!` outside `#[cfg(test)]`)
  is already narrow and correct, so it remains a hard backstop; only the TDD
  backstop is relaxed.

**Decision: default-on with a per-workspace escape, modelled on `SecurityConfig`.**
Add a `MethodologyConfig { tdd: bool, clean: bool }` resolved from a
`[methodology]` block in `<workspace>/.smedja/config.toml`, both fields
defaulting to `true` when the block or field is absent. The parser mirrors
`SecurityConfig::from_toml_str` (`crates/smedja-security/src/config.rs:52`) and
is loaded the way `load_security_config` loads the security block
(`bin/smdjad/src/security.rs:22`): a missing or unparseable file resolves to the
all-true default and never blocks startup. Setting `tdd = false` (resp.
`clean = false`) suppresses both the steering directive and the backstop for that
discipline.
- Rationale: foundational-by-default respects that the discipline is the house
  standard, while the escape mirrors the existing `--no-spec-gate` hatch
  (`crates/smedja-ingot/src/methodology.rs:18-20`) so smedja is opinionated
  without being dictatorial.
- Alternative considered: a per-session RPC toggle. Rejected — that reintroduces
  the toggle-a-non-negotiable anti-pattern this change removes. A committed
  per-workspace file is an explicit, reviewable opt-out.

**Decision: remove the selectable modes and their TUI commands.**
Delete the `/tdd` and `/ponytail` slash arms
(`bin/smedja-tui/src/main.rs:1106-1115`) and their `SLASH_COMPLETIONS` entries
(`:146`, `:154`); remove `Mode::{Tdd, Ponytail}`
(`crates/smedja-methodology/src/types.rs:5-8`) and their `parse_mode`/`run_gates`
arms (`bin/smdjad/src/methodology_gate.rs:16-17`, `:35-37`).
- Rationale: the commands were no-ops that printed false success; the modes are
  now foundations, not selections. A command that toggles a non-negotiable is an
  anti-pattern, and the names collide with the global Claude Code `/tdd` and
  `/ponytail` skills, so removal also resolves a naming clash.

**Decision: ponytail becomes an on-demand review skill.**
Ship `.smedja/skills/ponytail.md` describing the YAGNI / delete-over-add review
lens, surfaced through the existing skill-injection path
(`crates/smedja-memory/src/memory.rs:340`). Delete
`crates/smedja-methodology/src/ponytail.rs`.
- Rationale: ponytail was never extra enforcement — `ponytail::check` is a
  byte-identical clone of `clean::check_added_lines`
  (`crates/smedja-methodology/src/ponytail.rs:17`,
  `crates/smedja-methodology/src/clean.rs:34`). Its real value is the advisory
  lens, which is exactly what a skill is for. Demoting it loses no enforcement.

**Decision: retire the dormant `sre` mode.**
Remove `Mode::Sre` (`crates/smedja-methodology/src/types.rs:15`) and its
`parse_mode`/`run_gates` arms (`bin/smdjad/src/methodology_gate.rs:20`, `:38`);
sever the TUI `/agent sre` path from setting a methodology mode
(`bin/smedja-tui/src/main.rs:997-1009`) — the agent-routing meaning of `sre`
stays.
- Rationale: `run_gates` returns `Ok(())` for `Sre`
  (`bin/smdjad/src/methodology_gate.rs:38`) — it has never checked anything. A
  dormant mode that silently passes is worse than no mode.

## Risks / Trade-offs

- [Risk] Always-on steering grows the sealed prefix on every turn → Mitigation:
  the directive is a short fixed string in the cached prefix; its KV-cache cost
  amortises to near-zero across the turn's tool loop.
- [Risk] Relaxing the TDD backstop lets some genuinely test-free implementation
  through → Mitigation: the steering still demands tests every turn; the backstop
  catches the egregious case; the `clean` hard backstop is unchanged. Net
  enforcement on real test-free features is preserved while false positives on
  refactors/helpers/docs are eliminated.
- [Risk] Removing `Mode::{Tdd, Ponytail, Sre}` is a breaking change for any
  persisted session whose `mode` string is `"tdd"`/`"ponytail"`/`"sre"` →
  Mitigation: `parse_mode` already returns `None` (ungated) for unrecognised
  strings (`bin/smdjad/src/methodology_gate.rs:21`), so a stale persisted value
  degrades gracefully to "no selectable gate" — and the foundational discipline
  now covers TDD/clean regardless.
- [Risk] A workspace could disable the discipline wholesale via config →
  Mitigation: this is the intended, explicit, reviewable escape; it is committed
  to the repo and visible in review, unlike a transient per-session flag.
