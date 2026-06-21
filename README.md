<div align="center">
  <img src="assets/brand/smedja-social-b-smithy-door.png" alt="smedja — a forged terminal" width="720" />
</div>

<br />

<div align="center">
  <strong>A Rust-native forge for multi-agent AI orchestration.</strong><br/>
  Route across local models, cloud APIs, and specialist roles — with a GPU terminal, full OTel traceability, and a wire protocol that keeps everything interoperable.
</div>

<br />

---

## What is smedja?

*Smedja* is Swedish for smithy — a place where raw material gets shaped into precision instruments. That's the job here: take raw model output, route it through the right agents, forge it into something useful, and do it with full observability from first token to last.

smedja is a Rust rewrite and evolution of [milliways](https://github.com/mwigge/milliways) (Go). The two share the same UDS JSON-RPC 2.0 wire protocol, so they're interoperable during the migration — a milliways Go client talks to `smdjad`, and `smj` talks to `milliwaysd`. Each component migrates independently; no forced cutover.

---

## Workspace

<div align="center">
  <img src="assets/diagrams/readme-workspace.png" alt="smedja workspace layout" width="900" />
</div>

The kitchen/restaurant theme from milliways is retired. Metalworking instead:

| milliways (Go)  | smedja (Rust)      | What it does                       |
|-----------------|--------------------|------------------------------------|
| sommelier       | smedja-assayer     | tests and routes by quality        |
| kitchen/adapter | smedja-adapter     | shapes output to provider spec     |
| pantry          | crucible (in-mem)  | material held under heat           |
| mempalace       | smedja-vault       | cold durable storage               |
| pipeline        | smedja-bellows     | drives throughput                  |
| ledger/history  | smedja-ingot       | the produced unit, audit record    |

---

## Multi-Agent Architecture

`smdjad` runs multiple agent roles in parallel, each isolated in its own git worktree, coordinated by an orchestrator that understands role dependencies.

<div align="center">
  <img src="assets/diagrams/readme-multi-agent-architecture.png" alt="multi-agent architecture with isolated worktrees" width="900" />
</div>

Roles and their defaults live in `.smedja/agents.toml` — committed to the repo, portable across machines, not tied to any specific harness:

```toml
[roles.impl]
runner = "local"
model  = "Qwen3-14B"
tools  = ["read_file", "edit_file", "bash", "graph_query"]

[roles.review]
runner = "claude"
tier   = "deep"
tools  = ["read_file", "graph_query"]  # review is intentionally read-only

[roles.sre]
runner = "claude"
tier   = "deep"
tools  = ["read_file", "otel_query", "metric_query", "log_tail"]
```

The assayer routes by **role + complexity**, not just complexity. A simple fix stays local; an architecture review goes to claude deep. No manual model selection per task.

---

## Loop Pipeline

`smj loop run` takes one OpenSpec task at a time through planning, red/green implementation, deterministic verification, read-only review, and bounded fix retries.

<div align="center">
  <img src="assets/diagrams/loop-pipeline.png" alt="smj loop pipeline from orchestrator through test, implementation, verification, review, and fix retries" width="760" />
</div>

The loop router keeps planning on the strongest tier while pushing mechanical red/green/fix work to local runners.

<div align="center">
  <img src="assets/diagrams/loop-tier-routing.png" alt="tier routing table for loop roles" width="900" />
</div>

The new `smedja-loop` concept binds `.smedja/loop.json` to OpenSpec task state, mines failures into role guides, and keeps evaluators separate from generators through runner configuration.

<div align="center">
  <img src="assets/diagrams/smedja-loop-concept.png" alt="smedja-loop concept overview" width="900" />
</div>

---

## Session Memory and Sharing Between Agents

Every session runs through three memory strata. Context budget is allocated per runner tier — a `fast` runner gets hot + top-K warm; a `deep` runner gets everything.

<div align="center">
  <img src="assets/diagrams/readme-session-memory.png" alt="session memory strata: hot, warm, cold, and archive" width="760" />
</div>

Compaction produces structured JSON, not a free-text summary. Each compacted turn becomes a structured object that can be expanded and replayed — `smj session rollback <id> <turn>` reconstructs any point in history.

### How Parallel Agents Share Memory

When tasks fan out to parallel worktrees, agents share **read** access to `smedja-vault` (cold store) but write to isolated working trees. The orchestrator merges vault writes on task completion:

<div align="center">
  <img src="assets/diagrams/readme-parallel-memory.png" alt="parallel agents share read access to smedja-vault while writing to isolated worktrees" width="760" />
</div>

Both agents pull from the same cold memory — they know what was decided in previous sessions — but neither one touches the other's working tree.

---

## Context Budget Control

Before each request hits the provider, `smedja-adapter` runs three transforms:

**SmartCrusher** strips JSON nulls, zero-value arrays, and repeated keys from tool results before serialisation. Tool-heavy sessions see 30–60% token reduction on tool_result content alone.

**CacheAligner** freezes the system prompt and first N turns as a `stablePrefix`. BuildPrompt never reorders or compacts below that line, so the provider's KV cache stays warm across consecutive turns — no full re-encode on every call.

**Verbosity steering** appends a short `<conciseness>` directive to the system prompt when context exceeds 60% of the window. Roughly 15–30% output token reduction with no measurable task-quality loss.

---

## The Terminal Experience

### Turn Blocks

Agent output is structured, not a flat scroll. Each turn is a discrete `TurnBlock`:

<div align="center">
  <img src="assets/diagrams/readme-turn-block.png" alt="terminal turn block with model, token count, tool calls, and trace footer" width="900" />
</div>

Blocks are selectable (`↑↓`), copyable (`c`), replayable (`r`). The `trace:` in the footer is a W3C `traceparent` — open it in your OTel backend to see the full span tree for that turn.

### Modular Status Bar

Modules evaluated in parallel (rayon) on every render tick, < 50ms target:

<div align="center">
  <img src="assets/diagrams/readme-status-bar.png" alt="modular status bar showing tier, model, context fill, modes, git state, and time" width="900" />
</div>

TOML-configured, Starship-compatible module format. The milliways-specific modules (`tier`, `model`, `context_pct`, `milliways_task`) sit alongside the standard set with the same detection + format + style fields — existing Starship config is portable.

### Cowork Gate

In cowork mode, every tool call pauses for approval — not just Codex, every runner:

<div align="center">
  <img src="assets/diagrams/readme-cowork-gate.png" alt="cowork approval gate for an edit_file tool call" width="900" />
</div>

Deny sends the reason back as a tool error — the agent re-plans from there. Modify replaces the arguments before execution. Every decision is recorded in `smedja-ingot` as an audit event with `tool_name`, `decision`, and `agent_reasoning`.

### Context Rail

`Ctrl-R` opens a right panel showing context slot fill live:

<div align="center">
  <img src="assets/diagrams/readme-context-rail.png" alt="context rail showing live context slot fill" width="760" />
</div>

Green < 60%, yellow 60–80%, red > 80%. The slot breakdown matches the `stablePrefix` model — you see exactly what's locked in the KV cache prefix and what's competing for the remaining budget.

---

## Observability

Every span follows `gen_ai.*` semantic conventions. Every outbound HTTP request — provider API calls, MCP server requests, ACP callbacks — carries a W3C `traceparent`. You can follow a user message from the TUI keystroke through model inference and back to the audit log in a single trace.

<div align="center">
  <img src="assets/diagrams/readme-observability.png" alt="observability trace from TUI keystroke through model inference and audit log" width="900" />
</div>

`smj session cost` reads `smedja-ingot` and prints a per-session cost breakdown by model and runner. `prices.toml` ships bundled — no external API call required.

---

## Spec-First Methodology

`smedja-methodology` enforces a workflow before the agent can touch files. The gate is a compile-time `Mode` enum — not a runtime plugin, not optional in CI.

<div align="center">
  <img src="assets/diagrams/readme-spec-first-methodology.png" alt="spec-first methodology gate from OpenSpec through review" width="760" />
</div>

`--no-spec-gate` disables it per session for quick patches. In normal operation the sequence is: spec → approval → test → implementation → review.

---

## smedja-term

A GPU-accelerated terminal emulator built into the portfolio. wgpu on Metal / Vulkan / DX12, `cosmic-text` for font shaping, `taffy` flexbox for split panes.

The difference from WezTerm: `smedja-term` knows what a smdjad session is. Agent turns render as `AgentBlock` widgets — tier badge, token count, traceparent, inline cowork gate — not raw byte streams. Shell commands render as standard `Block` units (Warp-style): selectable, copyable, independently scrollable.

Custom glyphs (tier badges, status icons, block decorations) register via the **Glyph Protocol** — APC sequences that map vector shapes to Unicode PUA codepoints. No Nerd Font patches required.

<div align="center">
  <img src="assets/diagrams/readme-smedja-term.png" alt="smedja-term window with shell blocks, agent blocks, cowork gate, and status bar" width="900" />
</div>

Config is TOML. A migration tool converts existing WezTerm Lua config.

---

## Getting Started

```bash
# build everything
cargo build --workspace

# start the daemon
smdjad --sock /run/user/1000/smdjad.sock

# open a session
smedja --mode impl

# control CLI
smj session list
smj session cost
smj workspace agents
```

**Requirements:** Rust stable ≥ 1.82, `lld` on Linux, `cargo-sort` for the pre-commit gate.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
