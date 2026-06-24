## 1. Workspace methodology config (default-on + escape)

- [x] 1.1 Write a failing test for a `MethodologyConfig { tdd: bool, clean: bool }` parser modelled on `SecurityConfig::from_toml_str` (`crates/smedja-security/src/config.rs:52`): absent `[methodology]` block ⇒ `tdd == true && clean == true`; `[methodology]\ntdd = false` ⇒ `tdd == false && clean == true`
- [x] 1.2 Implement `MethodologyConfig::from_toml_str` so both fields default to `true` and an absent block resolves to the all-true default
- [x] 1.3 Write a failing test for a `load_methodology_config(workspace_root)` loader: missing `.smedja/config.toml` ⇒ all-true default; an unparseable file ⇒ all-true default (never blocks), mirroring `load_security_config` (`bin/smdjad/src/security.rs:22`)
- [x] 1.4 Implement `load_methodology_config` reading `<workspace>/.smedja/config.toml`, logging and falling back to the default on read/parse error

## 2. Relax the TDD backstop (foundational ≠ hard-block)

- [x] 2.1 Write a failing test that the relaxed `tdd::check` does NOT flag a small refactor/helper diff that adds a `+fn ` with no test in the change (the false-positive the current rule produces — `crates/smedja-methodology/src/tdd.rs:34`)
- [x] 2.2 Write a failing test that the relaxed `tdd::check` still raises an advisory verdict when a change adds substantial new implementation with zero tests anywhere in the diff
- [x] 2.3 Relax `tdd::check` (`crates/smedja-methodology/src/tdd.rs:16`) to raise only on the egregious substantial-impl-with-zero-tests case, and surface the result as advisory rather than a blunt always-reject
- [x] 2.4 Run `cargo test -p smedja-methodology`; fix the existing `tdd_fails_when_impl_without_test` expectation (`crates/smedja-methodology/src/tdd.rs:60`) to match the relaxed behaviour

## 3. Always-on steering injection (steer-first)

- [x] 3.1 Write a failing orchestrator test asserting that, on a code-writing turn with default config, the sealed system prefix contains the TDD/clean discipline directive (assert on the prefix pushed before `seal_prefix()` — `bin/smdjad/src/orchestrator/mod.rs:512`)
- [x] 3.2 Implement injection of a fixed TDD/clean discipline directive into the system prefix before `seal_prefix()`, alongside the workspace-skills injection path (`crates/smedja-memory/src/memory.rs:340`)
- [x] 3.3 Write a failing test asserting the directive is omitted when `methodology.tdd == false` (and the clean clause when `methodology.clean == false`), wiring the directive content to `load_methodology_config`
- [x] 3.4 Implement config-gated steering so each discipline's directive clause is present only when its config flag is `true`

## 4. Remove selectable tdd/ponytail modes + retire sre

- [x] 4.1 Write/adjust a failing test for `parse_mode` (`bin/smdjad/src/methodology_gate.rs:71`) asserting `"tdd"`, `"ponytail"`, and `"sre"` now resolve to `None` (ungated), while `"spec"` and `"clean"` still resolve
- [x] 4.2 Remove `Mode::{Tdd, Ponytail, Sre}` from `smedja_methodology::Mode` (`crates/smedja-methodology/src/types.rs:5-8`, `:14-15`), keeping `Spec` and `Clean`
- [x] 4.3 Remove the `"tdd"`, `"ponytail"`, `"sre"` arms from `parse_mode` and the `Tdd`/`Ponytail`/`Sre` arms from `run_gates` (`bin/smdjad/src/methodology_gate.rs:16-20`, `:35-38`), keeping the exhaustive match over `Spec | Clean`
- [x] 4.4 Update `spec_and_sre_modes_do_not_run_diff_gates` (`bin/smdjad/src/methodology_gate.rs:107`) to drop the removed `Sre` reference

## 5. Demote ponytail to a review skill

- [x] 5.1 Delete `crates/smedja-methodology/src/ponytail.rs` and drop its module declaration / re-export from `crates/smedja-methodology/src/lib.rs`
- [x] 5.2 Remove the `ponytail` import and any remaining references in `bin/smdjad/src/methodology_gate.rs:8`
- [x] 5.3 Add `.smedja/skills/ponytail.md` describing the YAGNI / delete-over-add advisory review lens, so it loads on demand via `load_workspace_skills` (`crates/smedja-memory/src/memory.rs:313`)
- [x] 5.4 Run `cargo test -p smedja-methodology`; confirm the removed ponytail tests are gone and the `clean` gate tests still pass

## 6. Remove the TUI /tdd and /ponytail commands

- [x] 6.1 Delete the `"tdd"` and `"ponytail"` arms from `dispatch_slash` (`bin/smedja-tui/src/main.rs:1106-1115`)
- [x] 6.2 Remove `"/tdd"` and `"/ponytail"` from `SLASH_COMPLETIONS` (`bin/smedja-tui/src/main.rs:146`, `:154`) and any matching `HELP_TEXT` lines
- [x] 6.3 Sever the `/agent sre` arm in `apply_agent` (`bin/smedja-tui/src/main.rs:997-1009`) from any methodology-mode meaning — keep `sre` as agent routing only, not a gate selector
- [x] 6.4 Run `cargo test -p smedja-tui` (and any dispatch tests); confirm no test asserts the removed `/tdd` / `/ponytail` behaviour

## 7. Verify

- [x] 7.1 Run `cargo test --workspace` — all green
- [x] 7.2 Run `cargo clippy -p smedja-methodology -p smdjad -p smedja-tui -- -D warnings` — clean for the touched crates
- [x] 7.3 Run `openspec validate methodology-simplify --strict` — clean
