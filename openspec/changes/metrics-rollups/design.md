## Context

smedja already holds the raw material for metrics rollups; what is missing is the time-tiered aggregation and a view.

Source-of-truth rows in `smedja-ingot`:

- `cost_ledger` → `CostEntry` (`cost.rs`): per-turn `runner`, `model`, `input_tok`, `output_tok`, `cost_usd` (INTEGER microdollars, read via `read_micros`), `created_at` (INTEGER micros since epoch). Existing aggregations: `session_total`, `session_cost_entries` (per `model`/`runner` `GROUP BY`, session-scoped), `last_model`.
- `audit_events` → `AuditEvent` (`audit.rs`): carries `ts` (INTEGER micros), `tier`, `status` (`Some("error")` on failure), `error_count`, `input_tok`/`output_tok`, and `conversation_id`. `record_timeline_event` (`lib.rs`) already increments a `failure_count` when `status == Some("error")`, but only into the per-conversation `conversation_rollups` table — not time-bucketed.

Existing rollup-shaped surface: `ConversationRollup` + `recent_conversations` (`lib.rs`) — per-conversation counters, no time tiers. This change adds the orthogonal *time* dimension.

View surfaces to mirror:

- `smj cost` (`bin/smj/src/main.rs` `Cmd::Cost`, ~line 721): calls `session.cost`, prints `total_usd` and a per-model/runner table.
- `smj timeline` (`Cmd::Timeline` → `TimelineCmd`, ~line 1225): conversation inspection.
- RPC registration pattern: `router.register("session.cost", …)` (`main.rs` ~line 558) delegating to `handlers::cost::cost(state, params)` (`handlers/cost.rs`).
- TUI: `statusbar.rs` (segment renderers, `ModuleCtx`) and `context_rail.rs` (`ContextRail`, `ContextSlot`, toggled via a visibility flag) are the precedent for a small, toggleable read-only panel.

External-OTel path: `smedja-sre::metric_query` (`crates/smedja-sre/src/metrics.rs`) issues a Prometheus `query_range`. That requires an external Prometheus/SigNoz deployment most local installs do not run. Local rollups must work with only the ingot SQLite file.

## Goals / Non-Goals

Goals:
- Aggregate tokens, cost, turns, and error counts **per runner over time tiers** (raw / hourly / daily / weekly / monthly) from `cost_ledger` and `audit_events`.
- Expose the aggregation through an ingot API, a `metrics.summary` RPC, a `smj metrics` command, and a TUI metrics view.
- Keep cost in exact integer microdollars end-to-end; convert to USD only at the display boundary (as `smj cost` does).
- Make any persisted rollup idempotent and reconstructible from source rows.

Non-Goals:
- A background rollup writer / cron / daemon — rollups are computed on read; materialisation is an explicit, optional call.
- New OTel metric instruments or changes to span emission.
- Mutating or pruning the source `cost_ledger` / `audit_events`.
- Replacing the external-OTel (`smedja-sre`) path — it stays for installs with a real metrics backend.
- Per-session context-window fill metrics (owned by `turn_token_snapshots`).

## Decisions

**Decision: five tiers — raw / hourly / daily / weekly / monthly — bucketed by truncating `created_at`/`ts` to the tier's grid.**
Each tier maps to a bucket-start computation over the micros timestamp: `raw` keeps per-source granularity (no truncation), `hourly`/`daily`/`weekly`/`monthly` floor the timestamp to the start of the hour/day/ISO-week/month (UTC). A `RollupTier` enum owns the truncation and the SQL bucket expression, so callers pass a tier, never raw SQL.
- Rationale: matches the milliways tier set; UTC truncation is deterministic and timezone-stable for an operator tool.
- Alternative: arbitrary user step (like Prometheus `step=60`). Rejected — fixed named tiers are simpler to render as a table and match the predecessor's mental model; the external-OTel path already covers arbitrary steps.

**Decision: retention is the source ledger's retention; rollups add none.**
Rollups never delete or summarise-away source rows. `metrics_rollups` is a derived cache, not a tier of truth. An operator who prunes the ledger loses the corresponding rollups on recompute — which is correct, because rollups must equal what the source rows say.
- Rationale: avoids a second source of truth and the milliways foot-gun where a materialised tier drifts from raw data.
- Trade-off: if the ledger is pruned, historical rollups for pruned periods become unreconstructable. Acceptable: pruning is out of scope and not implemented here.

**Decision: aggregation runs over the ingot, joining cost and error dimensions per `(bucket, runner)`.**
Tokens / cost / turns come from `cost_ledger` (`GROUP BY bucket, runner`); error counts come from `audit_events WHERE status = 'error'` (`GROUP BY bucket, runner`). The two `GROUP BY` results are merged in Rust on `(bucket_start, runner)` into one `MetricsBucket`, so a runner that errored without a cost row, or cost without errors, still appears.
- Rationale: the two facts live in different tables with different timestamp columns (`created_at` vs `ts`); a SQL `FULL OUTER JOIN` on a derived bucket key is awkward in SQLite, and a Rust merge over two small grouped result sets is clear and testable.
- Cost stays `Microdollars` (sum of INTEGER micros, read via `read_micros`); USD conversion is display-only.

**Decision: on-read rollup is the default; materialisation is optional and idempotent.**
`metrics_rollup(tier, since, until)` always computes from source rows — correct, zero staleness, no writer needed. `materialise_rollups(tier, until)` upserts the same computed buckets into `metrics_rollups` keyed on `(tier, bucket_start, runner)` (`INSERT … ON CONFLICT DO UPDATE`), for callers that want pre-aggregated reads. Re-running materialisation reproduces identical rows.
- Rationale: smedja's data volumes (a developer's local sessions) make on-read aggregation cheap; on-write rollup would add a writer on the hot path for no benefit at this scale. Materialisation is there for larger histories without committing to a daemon now.
- Alternative: on-write rollup (increment a bucket on every `insert_cost`). Rejected — couples the cost write path to rollup logic, and `record_timeline_event` already shows the upkeep cost of in-line counter maintenance; on-read keeps `insert_cost` untouched.

**Decision: dashboard surface is both a CLI command and a TUI view, both read-only over the same RPC.**
`smj metrics` mirrors `smj cost`'s structure (flags → `metrics.summary` RPC → table / `--json`). The TUI metrics view is a toggleable read-only panel beside the context rail, populated from the same `metrics.summary` response.
- Rationale: the CLI is scriptable and matches the existing `cost`/`timeline` ergonomics; the TUI gives the live "/metrics dashboard" feel the predecessor had. One RPC backs both, so there is a single aggregation code path.
- Alternative: TUI-only (no CLI) or CLI-only. Rejected — `cost` and `timeline` already establish that operators expect both; reusing one RPC keeps the cost low.

**Decision: local rollups and external OTel are complementary, documented as such.**
`metrics.summary` reads the ingot and always works offline. `smedja-sre::metric_query` reads an external Prometheus/SigNoz and works only when one is deployed. The README states the split: use local rollups for cost/token/error accounting from smedja's own ledger; use the SRE OTel path for infra-level metrics and cross-service correlation.
- Rationale: prevents the confusion of two "metrics" surfaces by giving each a clear job; neither is reimplemented in terms of the other.

## Risks / Trade-offs

- [Risk] On-read aggregation over a very large `cost_ledger`/`audit_events` could be slow → Mitigation: an index on `cost_ledger(created_at)` and `audit_events(ts, status)` is added with the migration; `since`/`until` bound every query; `materialise_rollups` exists for the rare large-history case.
- [Risk] Two timestamp columns (`created_at` micros vs `ts` micros) bucketed independently could misalign at tier boundaries → Mitigation: both use the same `RollupTier` truncation expression over a micros integer, so identical instants land in identical buckets; a test asserts a cost row and an error event at the same instant share a bucket.
- [Risk] `metrics_rollups` could drift from source rows if written by hand → Mitigation: it is a derived cache only; `materialise_rollups` always overwrites via upsert with freshly computed values, and a test asserts materialise-then-read equals on-read.
- [Risk] Weekly/monthly truncation edge cases (week start, month length) → Mitigation: define week as ISO-week start (Monday 00:00 UTC) and month as first-of-month 00:00 UTC; cover boundary instants with tests.
- [Risk] Adding a TUI panel could regress the non-blocking render budget → Mitigation: the metrics view fetches via the same async RPC the TUI already uses and renders from a cached snapshot, mirroring `context_rail`'s read-only, toggled pattern.
