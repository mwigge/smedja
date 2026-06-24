## Context

smedja has several token savers but no unified accounting:

- **Output filtering** — `filter_command_output` compresses `bash`/`run_command` text and records the saving via `record_tokens_saved` (`bin/smdjad/src/executor/mod.rs:209`), the *only* writer of `tokens_saved_ledger` today.
- **SmartCrusher** — `compress_tool_result` strips JSON nulls/empties on the tool-result path (`crates/smedja-adapter/src/crush.rs:32`); the saving is currently folded into the filter ledger row, not attributed.
- **Cold-context omission** — `WorkingMemory` drops cold-stratum turns from the prompt (`crates/smedja-memory/src/memory.rs`); the dropped tokens are never recorded.
- **Provider prompt caching** — `cache_read_input_tokens` is reported and modelled as a telemetry key (`CACHE_READ_TOKENS`, `crates/smedja-telemetry/src/lib.rs:42`) but never recorded as a saving.
- **Lean specs** — smaller cached umbrella context; its payoff shows up as cache reads but is not attributed.

The persistence layer:

- `tokens_saved_ledger` is `(id, session_id, turn_n, command, tokens_saved, created_at)` with no `source` column (`crates/smedja-ingot/src/lib.rs:177`); the current max migration version is **23**, so the next is **24**. `TokensSavedEntry` / `insert_tokens_saved` / `session_tokens_saved` live in `crates/smedja-ingot/src/cost.rs`.
- `metrics_rollup.rs` aggregates billed tokens/cost/turns/errors per runner over five fixed `RollupTier` tiers (`raw`/`hourly`/`daily`/`weekly`/`monthly`) with `RollupTier::bucket_start` doing the UTC truncation; results merge in a `BTreeMap<(bucket_start, key), Accumulator>`. This is the shape to mirror.
- `smj cost` (`bin/smj/src/main.rs:932`) and `smj metrics` (`:969`) are the CLI surface patterns; `metrics_view.rs` renders a per-runner `MetricsRow` snapshot in the TUI.

## Goals / Non-Goals

Goals:
- One ledger, many sources: every saver writes a `source`-tagged row to `tokens_saved_ledger`.
- Record provider cache reads as savings, labelled `cache` and kept distinct from compression savings.
- A savings rollup over the same five time tiers, with an efficiency ratio as the headline trend.
- Surface the efficiency headline as an always-on `st-statusbar` segment (primary glanceable gauge), the per-source breakdown in the TUI metrics panel, and the rollup via `smj savings` (CLI).

Non-Goals:
- Changing the billed `cost_ledger` semantics — savings stay separate from billed input/output sums (the existing invariant in `cost.rs`).
- Replacing the `metrics_rollup` engine — the savings rollup mirrors it but does not merge billed and saved into one table.
- Real-time per-keystroke accounting — savings are recorded per turn / per filtered result, matching today's cadence.
- Implementing the eval-harness token-efficiency metric — only the ratio is exposed for it to consume (owned by `eval-harness`).
- Backfilling historical cache reads — only cache reads observed after this change are recorded.

## Decisions

**Decision 1 — Multi-source ledger via a `source` column (migration 24).**
Add `source TEXT NOT NULL` to `tokens_saved_ledger` with a migration at version 24 (the next after the current max of 23): `ALTER TABLE tokens_saved_ledger ADD COLUMN source TEXT NOT NULL DEFAULT 'filter';` plus `CREATE INDEX IF NOT EXISTS idx_tokens_saved_source ON tokens_saved_ledger(source);`. The `DEFAULT 'filter'` backfills existing rows (all written by the output-filter path, so the default is correct history). `TokensSavedEntry` gains a `source: String` field; `insert_tokens_saved` writes it; a new `session_tokens_saved_by_source` returns `Vec<(source, tokens_saved)>`.
- Rationale: one ledger keeps the query surface and rollup simple; a column is a cheaper, idempotent migration than a second table. The `IF NOT EXISTS` index and defaulted column keep the migration re-runnable, matching migrations 22/23.
- Alternative considered: a per-source table each. Rejected — N tables means N rollup unions and N writers diverge; a discriminator column is the standard.

**Decision 2 — Cache reads ARE savings, but labelled distinctly (`source = 'cache'`).**
Record provider-reported `gen_ai.usage.cache_read_input_tokens` (`CACHE_READ_TOKENS`) as a ledger row with `source = 'cache'`, written once per turn from the orchestrator's cache wiring. This is the biggest, most measurable lever and it closes the loop with `cache-aligner` + `lean-specs`: umbrella-in-cached-prefix → cache reads → recorded savings = proof the strategy pays.
- Philosophical point (recorded deliberately): a cache read is **input not re-paid**, not content compressed away. Both reduce billed tokens, but they are categorically different. So `cache` savings MUST be a distinct source and the headline MUST present cache savings separately from compression savings (`filter` + `crusher` + `cold-context`). Folding them into one number would inflate the "compression" story with what is really a caching win.
- Rationale: provider-reported counts are exact, not estimated, so they are the most trustworthy savings signal available.
- Alternative considered: leave cache reads to telemetry only. Rejected — telemetry is ephemeral; the ledger is the durable, trendable record the economy needs.

**Decision 3 — Savings rollup mirroring `metrics_rollup`, with an efficiency ratio headline.**
Add a savings rollup that aggregates `tokens_saved` by `(tier, bucket_start, source)` over the same five `RollupTier` tiers, reusing `RollupTier::bucket_start` for identical bucketing. Alongside per-source sums, compute the efficiency ratio `saved / (saved + billed_input)` per bucket, where `billed_input` comes from the same-tier `cost_ledger` input-token sum. Three surfaces, by intent:

- **Glanceable headline — the `st-statusbar` segment (primary).** A compact, always-on `EfficiencyModule` segment in the GPU terminal's status bar (e.g. `⬇ 41%` or `−2.3M tok`), rendered every tick beside the existing tier/model/tokens segments. This is the "watch it climb" surface — a persistent efficiency gauge, not a panel you remember to open.
- **Detailed breakdown — the TUI metrics panel.** The per-source savings table + ratio in `bin/smedja-tui/src/metrics_view.rs`, riding the poll loop `metrics-live-fetch` establishes (adds a `savings.summary` fetch + rows; no second cadence).
- **Scripting/CI — `smj savings`.** The same rollup as a CLI companion (`--json`), mirroring `smj cost` / `smj metrics`.

**Data path for the status-bar segment (the scope-crossing part — flag it).** `st-statusbar` modules are pure `evaluate(&ModuleContext) -> Option<Segment>` (`term/crates/st-statusbar/src/lib.rs:80`); the terminal builds `ModuleContext` each render (`term/bin/smedja/src/main.rs:532`) from `st-agent`'s push-socket state, which already accumulates `last_input/output_tokens` off `AgentEvent::TurnEnd`. So the segment requires: (a) the daemon emits a cumulative efficiency/tokens-saved figure on an agent event (extend `AgentEvent`/`AgentEventEnvelope` — a **`smedja-agent-events` schema bump**, `CURRENT_SCHEMA_VERSION`), (b) `st-agent` accumulates it into its state, (c) the terminal threads it into a new `ModuleContext.efficiency`/`tokens_saved` field, (d) a new `EfficiencyModule` renders the segment and is registered in `sb_modules` (`main.rs:557`). This crosses the wire schema + the GPU-terminal stack — heavier than the TUI panel alone, but it's where the persistent nudge lives.
- Rationale: reusing the tier truncation guarantees savings buckets align with billed buckets, so the ratio is well-defined. An always-visible status-bar gauge drives "efficiency over time" better than an on-demand panel; the panel and CLI carry the detail.
- Scope note: if the wire-schema bump + terminal plumbing should be staged, the `st-statusbar` segment can be split into its own follow-up while the TUI panel + `smj savings` land first — the rollup backend (`savings.summary`) is shared by all three surfaces, so the split is clean.
- Alternative considered: extend `metrics_rollup` to also carry savings. Rejected — `metrics_rollups` is keyed on `runner`; savings are keyed on `source`. Different dimensions argue for a parallel rollup, not an overloaded one.

**Decision 4 — Attribution enables a feedback loop.**
Because every saving carries its `source`, the per-source breakdown shows which lever pays and which is weak, so it can be tuned. The efficiency ratio is exposed (via `savings.summary`) so the eval-harness can consume it as a token-efficiency metric and flag regressions. The harness wiring itself is owned by `eval-harness`; this change only guarantees the ratio is computable and exposed.
- Rationale: a metric you cannot attribute cannot be improved; attribution is the precondition for the loop.

## Risks / Trade-offs

- [Risk] Double-counting cache vs compression in the headline → Mitigation: `cache` is a distinct source; the rollup and `smj savings` present cache savings on a separate line and never sum them into the compression total (Decision 2; covered by a scenario).
- [Risk] Estimated savings (`filter`/`crusher`/`cold-context` use `estimate_tokens`) mixing with exact cache counts → Mitigation: the source label distinguishes estimated from provider-reported; the ratio's denominator uses exact billed input, so the ratio is conservative.
- [Risk] Migration 24 on a populated table → Mitigation: `ADD COLUMN ... DEFAULT 'filter'` is a single idempotent ALTER; existing rows are all filter rows, so the default is historically accurate.
- [Risk] Per-turn cache-savings write adds a DB write each turn → Mitigation: one insert per turn, swallowed-on-error like `record_tokens_saved`, negligible against a provider round-trip.
