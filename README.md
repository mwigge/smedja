<div align="center">
  <img src="assets/brand/smedja-social-b-smithy-door.png" alt="smedja — a forged terminal" width="720" />
</div>

<br />

<div align="center">
  <strong>A Rust-native forge for local LLM orchestration.</strong><br/>
  Multi-agent sessions, GPU terminal, full OTel traceability — built the way it should have been from the start.
</div>

<br />

---

## What is smedja?

*Smedja* is Swedish for smithy — a place where raw material gets shaped into precision instruments. That's the job here: take raw model output, route it through the right agents, forge it into something useful, and do it with full observability from first token to last.

smedja is a Rust rewrite and evolution of [milliways](https://github.com/mwigge/milliways) (Go). The two share the same UDS JSON-RPC 2.0 wire protocol, so they're interoperable during the migration — a milliways Go client talks to `smdjad`, and `smj` talks to `milliwaysd`. Each component migrates independently; no forced cutover.

---

## Workspace

```
smedja/
├── crates/
│   ├── smedja-rpc          JSON-RPC 2.0 types + UDS framing codec
│   ├── smedja-memory       WorkingMemory, hot/warm/cold strata, BuildPrompt
│   ├── smedja-adapter      Provider streaming (OpenAI SSE, Gemini, Codex, local)
│   ├── smedja-assayer      Role × complexity → (runner, tier) routing
│   ├── smedja-vault        Vector KV cold store — cosine-sim, SQLite-backed
│   ├── smedja-ingot        Audit log, cost ledger, checkpoints, task history
│   ├── smedja-bellows      Turn lifecycle and event dispatch
│   ├── smedja-methodology  TDD gate, ponytail gate, spec-first enforcement
│   ├── smedja-graph        tree-sitter indexer, graph_query built-in tool
│   └── smedja-sre          SRE tools (otel_query, metric_query, log_tail)
│
├── bin/
│   ├── smedja              ratatui TUI client
│   ├── smdjad              Tokio supervision daemon, UDS server
│   └── smj                 Control CLI (lifecycle, sessions, audit, cost)
│
└── term/
    ├── crates/             st-render, st-pty, st-blocks, st-statusbar, st-glyph …
    └── bin/smedja-term     GPU terminal emulator — wgpu, block model, WezTerm replacement
```

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

```
/parallel "Add OAuth to the API" --roles impl,test,review

smdjad orchestrator
  │
  ├── worktree: task/<id>/impl  ──▶  assayer: role=impl, runner=local
  │                                   model: Qwen3-14B, tools: full
  │
  ├── worktree: task/<id>/test  ──▶  assayer: role=test, runner=local
  │   (unblocks when impl done)       model: Qwen3-14B, tools: bash + edit
  │
  └── worktree: task/<id>/review ──▶ assayer: role=review, runner=claude
      (unblocks when test done)       model: claude-sonnet, tools: read-only
```

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

## Session Memory and Sharing Between Agents

Every session runs through three memory strata. Context budget is allocated per runner tier — a `fast` runner gets hot + top-K warm; a `deep` runner gets everything.

```
┌─────────────────────────────────────────────────────────────┐
│  HOT    current turn + last 5 turns                         │
│         always in context, never compacted                  │
├─────────────────────────────────────────────────────────────┤
│  WARM   turns 6–30 — structured JSON compact                │
│         in context when space allows                        │
├─────────────────────────────────────────────────────────────┤
│  COLD   turns 31+ — vector embeddings in smedja-vault       │
│         fetched on demand, top-5 cosine-sim retrieval       │
├─────────────────────────────────────────────────────────────┤
│  ARCHIVE  completed sessions — smedja-ingot SQLite only     │
└─────────────────────────────────────────────────────────────┘
```

Compaction produces structured JSON, not a free-text summary. Each compacted turn becomes a structured object that can be expanded and replayed — `smj session rollback <id> <turn>` reconstructs any point in history.

### How Parallel Agents Share Memory

When tasks fan out to parallel worktrees, agents share **read** access to `smedja-vault` (cold store) but write to isolated working trees. The orchestrator merges vault writes on task completion:

```
        smedja-vault (shared read)
         ↑ retrieve(query, k=5)        ↑ retrieve(query, k=5)
         │                              │
  agent: impl                    agent: review
  (sees prior session context)   (cross-references prior decisions)
         │                              │
  worktree/impl (isolated)       worktree/review (isolated)
         │                              │
         └──────── orchestrator ─────────┘
                        │
                  merge + open PRs
```

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

```
┌─ turn 14 ── local · Qwen3-14B ── 1,240 → 312 tok ── 2.1s ───────────────┐
│                                                                            │
│  The softcap needs to drop from 50.0 to 30.0 per the Gemma 4 spec.       │
│                                                                            │
│  ▸ read_file  crates/native/src/lib.rs       → 412 lines                 │
│  ▸ edit_file  crates/native/src/lib.rs       → +1/-1  (L432)             │
│    - softcap: f32 = 50.0,                                                 │
│    + softcap: f32 = 30.0,                                                 │
│                                                                            │
└────────────────────────────────── ✓ complete — trace: 4bf92f3a ───────────┘
```

Blocks are selectable (`↑↓`), copyable (`c`), replayable (`r`). The `trace:` in the footer is a W3C `traceparent` — open it in your OTel backend to see the full span tree for that turn.

### Modular Status Bar

Modules evaluated in parallel (rayon) on every render tick, < 50ms target:

```
[local · RDNA4]  [Qwen3-14B]  [ctx: ████░░ 44%]  [tdd · ponytail]  [main ✓]  14:22
    tier              model        context fill          modes          git     time
```

TOML-configured, Starship-compatible module format. The milliways-specific modules (`tier`, `model`, `context_pct`, `milliways_task`) sit alongside the standard set with the same detection + format + style fields — existing Starship config is portable.

### Cowork Gate

In cowork mode, every tool call pauses for approval — not just Codex, every runner:

```
┌─ cowork — step 2 of 4 ──────────────────────────────────────────────────┐
│  tool:    edit_file                                                       │
│  file:    crates/native/src/lib.rs                                       │
│  reason:  reducing softcap from 50.0 to 30.0 per Gemma 4 spec           │
│                                                                           │
│  [a] approve    [d] deny    [m] modify                                   │
└───────────────────────────────────────────────────────────────────────────┘
```

Deny sends the reason back as a tool error — the agent re-plans from there. Modify replaces the arguments before execution. Every decision is recorded in `smedja-ingot` as an audit event with `tool_name`, `decision`, and `agent_reasoning`.

### Context Rail

`Ctrl-R` opens a right panel showing context slot fill live:

```
CONTEXT RAIL
system       ████                   8%
skills       ████                  12%
code-graph   ████                  10%
history      ██████                18%
tools        ██████                16%
working      ████████████          40%
──────────────────────────────────────
remaining    ███                  (52k tok)
```

Green < 60%, yellow 60–80%, red > 80%. The slot breakdown matches the `stablePrefix` model — you see exactly what's locked in the KV cache prefix and what's competing for the remaining budget.

---

## Observability

Every span follows `gen_ai.*` semantic conventions. Every outbound HTTP request — provider API calls, MCP server requests, ACP callbacks — carries a W3C `traceparent`. You can follow a user message from the TUI keystroke through model inference and back to the audit log in a single trace.

```
smedja TUI keystroke
  │  traceparent injected
  ▼
smdjad: turn handler              [gen_ai.operation.name = "chat"]
  │                                [gen_ai.request.model  = "Qwen3-14B"]
  │                                [gen_ai.system         = "local"]
  ├── smedja-assayer route
  │
  ├── smedja-adapter POST ──▶ provider
  │   ↳ model inference             [gen_ai.usage.input_tokens  = 1240]
  │                                 [gen_ai.usage.output_tokens = 312]
  ├── tool: edit_file               [tool.name = "edit_file"]
  │
  └── smedja-ingot write            [audit.action = "tool_exec"]
                                    [task.id      = "…"]
```

`smj session cost` reads `smedja-ingot` and prints a per-session cost breakdown by model and runner. `prices.toml` ships bundled — no external API call required.

---

## Spec-First Methodology

`smedja-methodology` enforces a workflow before the agent can touch files. The gate is a compile-time `Mode` enum — not a runtime plugin, not optional in CI.

```
OpenSpec change active?
        │
        ▼
  spec/ directory exists?
      no ──▶ agent drafts spec first (cannot emit edit_file)
        │
       yes
        │
        ▼
  CoworkGate approves spec
        │
        ▼
  TddGate: failing test must exist before first edit_file
        │
        ▼
  impl role executes
        │
        ▼
  review role (read-only tools only)
```

`--no-spec-gate` disables it per session for quick patches. In normal operation the sequence is: spec → approval → test → implementation → review.

---

## smedja-term

A GPU-accelerated terminal emulator built into the portfolio. wgpu on Metal / Vulkan / DX12, `cosmic-text` for font shaping, `taffy` flexbox for split panes.

The difference from WezTerm: `smedja-term` knows what a smdjad session is. Agent turns render as `AgentBlock` widgets — tier badge, token count, traceparent, inline cowork gate — not raw byte streams. Shell commands render as standard `Block` units (Warp-style): selectable, copyable, independently scrollable.

Custom glyphs (tier badges, status icons, block decorations) register via the **Glyph Protocol** — APC sequences that map vector shapes to Unicode PUA codepoints. No Nerd Font patches required.

```
smedja-term window
├── tab: shell
│     Block [ls -la]          Block [cargo build]
│     ✓ 0ms                   ✓ 4.2s
│
├── tab: milliways session
│     AgentBlock [turn 12]    AgentBlock [turn 13]
│                              ┌─ cowork gate ──────────────┐
│                              │  tool: edit_file            │
│                              │  [a] approve  [d] deny      │
│                              └─────────────────────────────┘
└── status bar: [local · RDNA4]  [Qwen3-14B]  [ctx: 44%]  [main ✓]
```

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
