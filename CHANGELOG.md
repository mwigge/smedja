# Changelog

All notable changes to smedja are documented here.

Format: `## [version] — YYYY-MM-DD` / `### Added|Fixed|Changed|Removed|Roadmap`.

---

## [0.20.6] — 2026-06-28

### Added

- **Quality panel (Tier 1)** — deterministic post-turn quality scoring (TDD, clean build, file-size, skill-injection gates) displayed in the right rail. `Ctrl-Q` toggles the panel; score bands colour-coded green/amber/red. Two consecutive sub-60 scores push a `CoworkGate` soft-interrupt.
- **Quality panel (Tier 2)** — on-demand LLM review via `/quality` or `Ctrl-Q` hold ≥ 500 ms. Routes to an adversary model (cross-family from the primary provider) and surfaces a rubric-based score with up to 5 actionable findings. Panel shows `[llm]` badge; gracefully falls back when the adversary model is unreachable.
- **Value panel** — `Ctrl-V` toggles a right-rail ROI panel tracking cumulative token cost (and USD estimate) for the active openspec change. Polling reuses the existing 3-second obs interval. `/value` prints a Markdown cost/quality report to the main view.
- **Token cost attribution** — `change_name` column added to `audit_events` (migration 25, `ALTER TABLE … ADD COLUMN`, zero downtime). Active openspec change detected at smdjad startup and stamped on every audit event; `cost.active_change` RPC endpoint exposes the running total.

### Fixed

- **Input wrapping** — multi-line input in the TUI now wraps at the content-area width rather than the full terminal width, preventing overflow past the right rail.

---

## [0.20.5] — 2026-06-28

### Fixed

- **install.sh quickstart message** — Linux installs with systemd now show `quickstart: smedja  (smdjad starts automatically via systemd --user)` instead of the misleading `smdjad & smedja`; on upgrade, the running daemon is restarted automatically via `systemctl --user restart`.

---
