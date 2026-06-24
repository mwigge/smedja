# Simplify the methodology system to a foundational discipline

## Why

The methodology system is a confusing two-axis tangle of selectable "modes" that
are partly broken. Today a session carries a single `mode` string (`"tdd"`,
`"ponytail"`, `"spec"`, `"clean"`, `"sre"`) which the daemon maps onto
`smedja_methodology::Mode` and runs as a write-time diff gate
(`bin/smdjad/src/methodology_gate.rs:14` `parse_mode`, `:33` `run_gates`;
`bin/smdjad/src/executor/mod.rs:318-368`). Two of those modes do nothing and two
are user-facing footguns:

- **The TUI `/tdd` and `/ponytail` commands are no-ops that mislead.** Unlike
  `/agent`, which calls `session.set_mode` (`bin/smedja-tui/src/main.rs:1027`),
  the `/tdd` arm (`:1106`) and `/ponytail` arm (`:1111`) only set local TUI
  state (`state.mode`) and never tell the daemon — so they never gate. They
  print "mode set" while changing nothing on the server.
- **`/agent tdd` returns "unknown agent mode".** `apply_agent`
  (`bin/smedja-tui/src/main.rs:997`) only accepts `impl|review|test|sre|explain`,
  so the one path that *does* reach `session.set_mode` cannot select the TDD
  gate at all.
- **`ponytail` is a byte-identical clone of `clean`.** `ponytail::check`
  (`crates/smedja-methodology/src/ponytail.rs:17`) just calls
  `clean::check_added_lines` with a different gate label
  (`crates/smedja-methodology/src/clean.rs:34`). It enforces nothing the clean
  gate does not.
- **`sre` is dormant.** `run_gates` returns `Ok(())` for `Mode::Sre`
  (`bin/smdjad/src/methodology_gate.rs:38`); it never checks anything.

A toggle command for a non-negotiable engineering discipline is an anti-pattern,
and the `/tdd` / `/ponytail` names also collide with the global Claude Code
`/tdd` and `/ponytail` skills, so a user who types them reasonably expects the
global behaviour and gets a silent local no-op instead.

This change reframes the system: test-driven development and clean-code
discipline are **foundational** — always present in the agent's instructions on
every code-writing turn — not selectable modes. They are enforced **steer-first**
(the directive is injected into the sealed system prefix every turn) with the
existing diff gate kept as a *backstop*, not a blunt always-reject. They are
**on by default with a per-workspace escape** (`.smedja/config.toml` →
`[methodology] tdd = false` / `clean = false`), mirroring the existing
per-session `no-spec-gate` hatch — so the discipline is foundational-by-default
without smedja dictating to every user. The selectable `tdd`/`ponytail` modes
and their TUI commands are removed; `ponytail` becomes an on-demand review skill;
the dormant `sre` mode is retired.

The `spec` lifecycle is **out of scope** here — it folds into the lean-specs /
OpenSpec work separately.

## What Changes

- **TDD + clean become foundational, steer-first.** On every code-writing turn
  the orchestrator injects an always-on TDD/clean discipline directive into the
  sealed system prefix (the same prefix that carries workspace skills today —
  `bin/smdjad/src/orchestrator/mod.rs:416-512`). The steering is the primary
  mechanism; the diff gate is a backstop.
- **The diff gate becomes sane, not merciless.** Today `tdd::check`
  (`crates/smedja-methodology/src/tdd.rs:16`) fails ANY added `+fn ` line with no
  nearby `#[test]`/`mod tests`. Always-on plus that crude rule would block
  refactors, helper extraction, and doc edits. The backstop SHALL only act when a
  change adds substantial new implementation with zero tests anywhere in the
  change (and MAY warn rather than hard-reject), so it stops the egregious case
  without fighting normal editing.
- **Default-on with a per-workspace escape.** A new `[methodology]` block in
  `.smedja/config.toml` (`tdd`, `clean`, both defaulting to `true`) resolved
  through a `MethodologyConfig` modelled on `SecurityConfig::from_toml_str`
  (`crates/smedja-security/src/config.rs:52`) and loaded the way
  `load_security_config` loads the security block
  (`bin/smdjad/src/security.rs:22`). Setting `tdd = false` / `clean = false`
  drops both the steering and the backstop for that discipline.
- **Remove the selectable `tdd`/`ponytail` modes and TUI commands.** Delete the
  `/tdd` and `/ponytail` slash arms (`bin/smedja-tui/src/main.rs:1106-1115`) and
  their `SLASH_COMPLETIONS` entries (`:146`, `:154`); remove the `Tdd` and
  `Ponytail` variants from `smedja_methodology::Mode`
  (`crates/smedja-methodology/src/types.rs:5-8`) and their `parse_mode`/
  `run_gates` arms (`bin/smdjad/src/methodology_gate.rs:16-17`, `:35-37`); delete
  `crates/smedja-methodology/src/ponytail.rs`.
- **Ponytail becomes a review skill, not a gate.** Ship a workspace skill file
  (`.smedja/skills/ponytail.md`) describing the advisory review lens
  (YAGNI / delete-over-add), loaded on demand via the existing skill-injection
  path (`crates/smedja-memory/src/memory.rs:313` `load_workspace_skills`,
  `:340` `inject_workspace_skills`). It is an on-demand lens, not a persistent
  mode — and since the gate was a clone of `clean`, demoting it loses no
  enforcement.
- **Retire the dormant `sre` mode.** Remove `Mode::Sre`
  (`crates/smedja-methodology/src/types.rs:15`), its `parse_mode`/`run_gates`
  arms (`bin/smdjad/src/methodology_gate.rs:20`, `:38`), and stop the TUI
  `/agent sre` arm from setting a methodology mode (the `agent` routing concept
  in `apply_agent` is unrelated and stays).

## Capabilities

### New Capabilities

- `foundational-discipline`: TDD and clean-code discipline are always-on,
  steer-first foundations. Every code-writing turn carries the discipline
  directive in its sealed system prefix; an advisory diff backstop catches only
  egregious test-free implementation; the whole discipline is on by default with
  a per-workspace `.smedja/config.toml` escape.

### Modified Capabilities

- `methodology-commands`: the selectable `tdd` and `ponytail` modes, their TUI
  slash commands, and the dormant `sre` mode are removed. `ponytail` is demoted
  to an on-demand review skill. `methodology.Mode` no longer carries `Tdd`,
  `Ponytail`, or `Sre`.

## Impact

- `crates/smedja-methodology/src/types.rs`: remove `Mode::{Tdd, Ponytail, Sre}`
  (the foundational disciplines are no longer mode variants).
- `crates/smedja-methodology/src/ponytail.rs`: deleted (clone of `clean`).
- `crates/smedja-methodology/src/tdd.rs`: relax `check` to the egregious-only
  backstop and expose an advisory verdict.
- `crates/smedja-methodology/src/lib.rs`: drop the `ponytail` module re-export.
- `bin/smdjad/src/methodology_gate.rs`: `parse_mode`/`run_gates` drop the
  `Tdd`/`Ponytail`/`Sre` arms; add foundational-discipline gate selection from
  `MethodologyConfig`.
- `bin/smdjad/src/orchestrator/mod.rs`: inject the always-on TDD/clean directive
  into the sealed prefix before `seal_prefix()`.
- `bin/smdjad/src/security.rs` (or a sibling loader): add `load_methodology_config`
  resolving the `[methodology]` block.
- `crates/smedja-security/src/config.rs`: referenced only as the model for the
  new `MethodologyConfig` parser (no change required there).
- `bin/smedja-tui/src/main.rs`: remove the `/tdd` and `/ponytail` arms and their
  `SLASH_COMPLETIONS` entries; remove `sre` from the methodology-setting path.
- `.smedja/skills/ponytail.md`: new workspace skill describing the review lens.
- README: the methodology section describes a foundational discipline with a
  workspace escape, not selectable `/tdd` / `/ponytail` modes.
