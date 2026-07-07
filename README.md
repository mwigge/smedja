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

## Install

### macOS

**One-line install (recommended)**

```sh
curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
```

Installs `smdjad`, `smj`, `smedja`, and `smedja-tui` to `~/.local/bin/` and registers `smdjad` as a LaunchAgent so it starts at login.

**DMG**

Download `smedja-darwin-<arch>.dmg` from the [latest release](https://github.com/mwigge/smedja/releases/latest), open it, and drag `smedja.app` to `/Applications`.

**Gatekeeper**

macOS will block unsigned binaries downloaded from the internet. Run once after install to clear the quarantine flag:

```sh
# For the install.sh tarball install:
xattr -dr com.apple.quarantine ~/.local/bin/smedja ~/.local/bin/smdjad ~/.local/bin/smj ~/.local/bin/smedja-tui

# For the .app bundle:
xattr -cr /Applications/smedja.app
```

Alternatively: right-click the binary or app in Finder → Open → Open anyway.

---

### Arch Linux / CachyOS

**One-line install (recommended)**

```sh
curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
```

Installs `smdjad`, `smj`, `smedja`, and `smedja-tui` to `~/.local/bin/`, enables `smdjad` as a systemd user service, and registers `smedja` in the application launcher.

**PKGBUILD**

```sh
git clone https://github.com/mwigge/smedja
cd smedja/assets
makepkg -si
systemctl --user enable --now smdjad
```

---

### Debian / Ubuntu

Download the `.deb` from the [latest release](https://github.com/mwigge/smedja/releases/latest):

```sh
curl -fsSL -O https://github.com/mwigge/smedja/releases/latest/download/smedja-linux-x86_64.deb
sudo dpkg -i smedja-linux-x86_64.deb
```

Enable the daemon:

```sh
systemctl --user enable --now smdjad
```

---

### Fedora

Download the `.rpm` from the [latest release](https://github.com/mwigge/smedja/releases/latest):

```sh
sudo dnf install https://github.com/mwigge/smedja/releases/latest/download/smedja-linux-x86_64.rpm
```

Enable the daemon:

```sh
systemctl --user enable --now smdjad
```

---

### WSL2

Install the Linux tarball as normal — smedja renders via WSLg. Ensure WSLg is enabled in your Windows setup (Windows 11 or Windows 10 with WSLg preview).

```sh
curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
```

If systemd is not available in your WSL2 distro, add this to `~/.bashrc` or `~/.zshrc` to start the daemon automatically:

```sh
pgrep -u "$USER" smdjad >/dev/null || smdjad &
```

---

### Build from source

Requires Rust stable ≥ 1.82.

```bash
git clone https://github.com/mwigge/smedja
cd smedja
cargo build --release --workspace
cp target/release/{smdjad,smj,smedja-tui,smedja} ~/.local/bin/
```

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

The assayer routes by **role plus complexity**, not just complexity. A simple fix stays local; an architecture review goes to claude deep. No manual model selection per task.

**Role set.** Task-type roles (not per-language — language is detected context): `code`/`impl`, `plan`, `research`, `debug`, `ask`, `review`, `test`, `sre`, plus the domain roles `data` (SQL) and `iac` (infra), and `orchestrator`. Each has a default `(client, tier)` and a permission profile — read-only roles (plan/research/review/ask/orchestrator) can't mutate; `iac` always confirms. Set the active role with `/agent <role>`.

**Per-role rules/skills.** Each role auto-loads its discipline from `.smedja/roles/<role>.md` (and `roles/<role>/*.md`) into the system prompt — e.g. a review checklist, research source-hygiene, IaC safety rules — alongside the always-on `.smedja/skills/`.

**Tiers as a control.** `/tier local|fast|deep` resolves the runner's model for that tier and pins it; the last runner + tier persist across restarts. A cost-aware tier ladder (deep→fast→local) is available for orchestrated descent.

### Local model lifecycle

The `local` runner is a control plane over external local-serving tools — smedja **orchestrates**, it does not serve. Inference, weight downloads, quantisation, and GPU placement stay in those tools; smedja drives install via shell-out, reads the model inventory and a GPU snapshot, and issues hot-swap requests over HTTP.

- **rs-llmctl** — the installer/inventory surface. `local.install` shells out to it (`SMEDJA_LOCAL_INSTALLER`, default `rs-llmctl`) to pull a model, then re-queries `/v1/models` and reports success **only** when the model actually appears in the inventory.
- **llama-swap** (or any llama-swap-compatible proxy) — fronts many loaded models behind one OpenAI-compatible endpoint and hot-swaps the active model on request. smedja issues the swap to `SMEDJA_LOCAL_SWAP_ENDPOINT`; if the proxy has no explicit swap endpoint, smedja falls back to setting the active-model label so a model-routing proxy honours the chosen `model`.

```sh
smj local list            # GPU-annotated inventory: each model flagged fits | tight | exceeds | unknown
smj local gpu             # cached GPU snapshot (device, VRAM total/free); "no GPU detected" on CPU-only hosts
smj local swap qwen3-14b  # hot-swap the active local model — no daemon restart, reports round-trip latency
smj local install llama3-8b
```

In the TUI, `/model` is local-aware: with the `local` runner active, bare `/model` lists the GPU-annotated inventory and `/model <name>` hot-swaps via `local.swap` (not the relabel-only `session.set_model`). The picker's fit annotation is **advisory** — VRAM placement is llama-swap's job, so an "exceeds" model can still be selected.

Configuration: `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`) is the OpenAI-compatible base; `SMEDJA_LOCAL_SWAP_ENDPOINT` defaults to the same base; GPU detection shells out to `nvidia-smi` and degrades cleanly to "no GPU detected" when absent. When no healthy local endpoint is detected at startup, the `local.*` RPCs return a structured "local tooling unavailable" error and the daemon still starts — other runners are unaffected.

---

## Loop Pipeline

`smj loop run` takes one OpenSpec task at a time through planning, red/green implementation, deterministic verification, read-only review, and bounded fix retries.

<div align="center">
  <img src="assets/diagrams/loop-pipeline.png" alt="smj loop pipeline from orchestrator through test, implementation, verification, review, and fix retries" width="760" />
</div>

The loop router keeps planning on the strongest tier while pushing mechanical red/green/fix work to local runners.

When a routed provider becomes unusable mid-turn — rate-limited beyond its back-off budget, quota exhausted, context-window exceeded, or down — the orchestrator automatically fails over to the next eligible runner of a compatible tier. Rotation walks a bounded ring (routed provider first, then compatible alternatives in pool priority order, default last; at most three rotations per turn), never degrades a turn below its routed tier, and preserves the assembled prompt and accumulated tool history. Each rotation is visible through the `smedja.error.kind` / `smedja.error.retryable` span attributes.

<div align="center">
  <img src="assets/diagrams/loop-tier-routing.png" alt="tier routing table for loop roles" width="900" />
</div>

The new `smedja-loop` concept binds `.smedja/loop.json` to OpenSpec task state, mines failures into role guides, and keeps evaluators separate from generators through runner configuration.

<div align="center">
  <img src="assets/diagrams/smedja-loop-concept.png" alt="smedja-loop concept overview" width="900" />
</div>

### Workspace layout

`loop.run` consumes a workspace laid out under `.smedja/` plus the OpenSpec change envelope:

```
<workspace>/
├── .smedja/
│   ├── loop.json          # loop engine policy (see below) — required by loop.run
│   ├── agents.toml        # per-role runner/tier/model routing overrides
│   ├── workspace.toml     # optional workspace-level settings
│   └── guides/<role>.md   # failure guides, written by the engine on failure
└── openspec/
    └── changes/<name>/
        └── tasks.md       # the work envelope; pending `- [ ] ` lines become slices
```

The change name passed to `loop.create` selects `openspec/changes/<name>/tasks.md`; each unchecked `- [ ] ` line is one slice the pipeline drives.

### `loop.json` policy

`.smedja/loop.json` is the policy contract. Its SHA-256 is hashed at load; if the file changes mid-run the loop aborts in the terminal `policy_tampered` state. The reviewer and implementer **must** use different runners (evaluator/generator separation) or the loop fails closed before any role runs.

```json
{
  "version": 1,
  "limits": { "max_attempts": 3, "agent_timeout_s": 600 },
  "roles": [
    { "name": "implementer", "runner": "local",   "tier": "local", "read_only": false, "tools": [] },
    { "name": "reviewer",    "runner": "minimax",  "tier": "fast",  "read_only": true,  "tools": [] },
    { "name": "fix",         "runner": "local",    "tier": "local", "read_only": false, "tools": [] }
  ],
  "verification": { "command": ".smedja/bin/verify.sh" },
  "review":       { "per_slice": true, "required": true },
  "publication":  { "max_pr_lines": 400 }
}
```

| Field | Meaning |
|-------|---------|
| `limits.max_attempts` | Maximum role attempts per slice before the loop fails. |
| `limits.agent_timeout_s` | Per-role wall-clock timeout. |
| `roles[]` | Each role's `runner`, `tier`, optional `model`, `read_only`, and allowed `tools`. |
| `verification.command` | Deterministic gate run after each slice; exit code 0 = pass. |
| `review.per_slice` / `required` | Whether the reviewer runs each slice and whether a failing review blocks progress. |
| `publication.max_pr_lines` | Maximum changed lines permitted per published slice. |

The verification gate's wall-clock budget defaults to 300 seconds; override it with the `SMEDJA_LOOP_VERIFY_TIMEOUT` environment variable (seconds). A loop with no `.smedja/loop.json` fails fast rather than running.

---

## Session Memory and Sharing Between Agents

Every session's working window is partitioned into memory strata **by position**: hot (the last few turns), warm (a wider recent window), then cold. A turn's stratum is derived from its distance to the end of the conversation and recomputed each turn — there is no background promotion machinery moving turns between tiers. (A fourth `Archive` stratum is declared for durable cold storage but is not produced for in-memory turns.) Context budget is allocated per runner tier — a `fast` runner gets hot + top-K warm; a `deep` runner gets everything.

<div align="center">
  <img src="assets/diagrams/readme-session-memory.png" alt="positional session memory strata: hot, warm, and cold windows" width="760" />
</div>

Compaction is **summary-based**: the daemon asks a fast model to condense the conversation into a short free-text summary and indexes that summary into cold storage. Point-in-time recovery is separate and exact — before compacting, smedja writes a full pre-compaction **checkpoint** (the entire conversation as JSON), and `smj session rollback <id> <turn>` reconstructs any point in history from those checkpoints. The CLI calls the daemon's `session.rollback` RPC end to end, which loads the target checkpoint's stored messages and prunes later ones in a single transaction.

### How Parallel Agents Share Memory

When tasks fan out to parallel worktrees, agents share **read** access to `smedja-vault` (cold store) but write to isolated working trees. The orchestrator merges vault writes on task completion. Cold retrieval is wired end-to-end: each turn the orchestrator recalls relevant context from the vault and injects it as a bounded `<cold_context>` block, and the `smedja_vault_search` tool embeds the query, runs hybrid cosine + keyword + recency search over the named namespace, and returns ranked results (an empty `results` array on no match). Retrieval is **semantic when a learned embedding backend is configured** — an OpenAI-compatible `/v1/embeddings` endpoint. The zero-config default embedder is a deterministic FNV bag-of-words hash, so out of the box the cosine component is lexical rather than semantic; the learned path also degrades to that same hash if its endpoint is unreachable.

<div align="center">
  <img src="assets/diagrams/readme-parallel-memory.png" alt="parallel agents share read access to smedja-vault while writing to isolated worktrees" width="760" />
</div>

Both agents pull from the same cold memory — they know what was decided in previous sessions — but neither one touches the other's working tree.

---

## Context Budget Control

**SmartCrusher** (`smedja-adapter`) recursively strips JSON `null` fields and empty arrays from tool results before serialisation. Tool-heavy sessions see 30–60% token reduction on tool_result content alone. Implemented and tested.

**Stable-prefix** (`smedja-memory`) tracks a `stable_prefix` boundary in the working window. `seal_prefix()` marks turns below the compaction line so they are never reordered or discarded, and the sealed-prefix length is passed through to the provider on the live turn path: for the Anthropic runner the orchestrator derives `stable_prefix_len` from the sealed prefix and the adapter applies the corresponding KV-cache hints. Cross-provider cache alignment beyond Anthropic's stable-prefix hints is on the roadmap.

**Verbosity steering** (`smedja-memory`) appends a `<conciseness>` directive to the system prompt when context exceeds 60% of the window. Implemented and tested.

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

### Approval & turn control

**Stop and check.** Mutating tool calls (write/edit/shell) gate by default — *ask-on-mutation* — for **every** client, gated where each one actually executes its tools:

- **claude** — a `PreToolUse` hook (`--settings` → `smj tool-gate` → the daemon) gates each tool; disable with `SMEDJA_TOOL_GATE=off`.
- **codex** — the permission mode maps to its `--sandbox` level: `plan` → `read-only`, `ask`/`accept_edits`/`none` → `workspace-write`, and `auto` → full bypass (`--dangerously-bypass-approvals-and-sandbox`, matching auto-mode's own allowlists).
- **minimax / local / API** — gated in-process at the orchestrator's tool loop.

<div align="center">
  <img src="assets/diagrams/readme-cowork-gate.png" alt="approval gate for an edit_file tool call" width="900" />
</div>

In `smedja-tui`, a pending approval surfaces as the inline widget — **`y`** approve, **`n`** deny, **`m`** modify (or `/approve`). Deny returns the reason as a tool error so the agent re-plans; modify rewrites the arguments before execution. Reads always pass. In the GPU terminal (`smedja`), approvals render as text lines in the agent block; the inline widget is deferred.

**Permission modes** — cycle with **Shift-Tab**: `ask` (stop on every mutation) → `accept_edits` (auto-approve known edits, ask on shell/unknown) → `plan` (read-only) → `auto` (no gate). Read-only **roles** (plan/research/review/ask/orchestrator) can never mutate regardless of mode; high-risk **IaC** mutations are *always* confirmed even in `auto`.

**Interrupt** — press **Esc** during a turn to stop a runaway agent: the streaming subprocess is killed and the turn ends.

### Context Rail

`Ctrl-F` (scroll/normal mode) opens a right panel showing context slot fill live:

<div align="center">
  <img src="assets/diagrams/readme-context-rail.png" alt="context rail showing live context slot fill" width="760" />
</div>

Green < 60%, yellow 60–80%, red > 80%. The slot breakdown matches the `stablePrefix` model — you see exactly what's locked in the KV cache prefix and what's competing for the remaining budget.

---

## Observability

Every span follows `gen_ai.*` semantic conventions. Outbound **provider API calls** (Anthropic, OpenAI, Gemini) inject a W3C `traceparent` header, so model inference joins the same trace as the turn that issued it; the MCP HTTP client and the ACP session API do not yet propagate the header. Inbound trace context is honoured, and the TUI status bar's Trace segment surfaces the real `traceparent` of the most recently completed turn. You can follow a user message from the TUI keystroke through model inference and back to the audit log in a single trace.

<div align="center">
  <img src="assets/diagrams/readme-observability.png" alt="observability trace from TUI keystroke through model inference and audit log" width="900" />
</div>

`smj session cost` reads `smedja-ingot` and prints a per-session cost breakdown by model and runner. `prices.toml` ships bundled — no external API call required.

### Local metrics rollups

`smj metrics` aggregates tokens, cost, turns, and error counts **per runner over time** directly from the local ingot (`cost_ledger` for tokens/cost/turns, `audit_events` with `status = 'error'` for errors). It needs nothing but the ingot SQLite file:

```sh
smj metrics --tier daily --since 7d            # per-runner daily table for the last week
smj metrics --tier hourly --since 24h --runner claude
smj metrics --tier monthly --since 365d --json # raw metrics.summary payload
```

Five fixed tiers bucket each source timestamp to a UTC grid: `raw` (per-entry granularity), `hourly`, `daily`, `weekly` (ISO Monday 00:00), and `monthly` (first-of-month 00:00). `--since`/`--until` accept a duration back from now (`7d`, `24h`, `30m`, `90s`) or bare seconds. The command calls the `metrics.summary` RPC, which the daemon also exposes to the TUI metrics view (toggle with **Ctrl-T**); the panel fetches once on open and refreshes on a slow (~3 s) interval while visible, showing live per-runner tokens, cost, and error counts alongside the token-economy savings section. Cost is kept as exact integer microdollars end-to-end and converted to USD only at the display boundary. Rollups are computed on read from the source rows — no background writer, no staleness — with an optional idempotent materialisation into a `metrics_rollups` cache for large histories.

**Local rollups vs external OTel.** These two metrics surfaces are complementary, not overlapping:

- **Local rollups** (`smj metrics` / `metrics.summary`) read smedja's own ingot and always work offline. Use them for cost, token, and error accounting from the ledger smedja already writes — zero external dependencies.
- **External OTel** (`smedja-sre::metric_query`, a Prometheus/SigNoz `query_range`) reads an external metrics backend and works only when one is deployed. Use it for infra-level metrics and cross-service correlation.

Neither is implemented in terms of the other; pick local rollups for offline ledger accounting and the SRE OTel path when you run a real metrics backend.

On startup the daemon signals readiness to the service manager via `sd_notify(READY=1)` (honoured by `Type=notify` systemd units) on every launch. The default daemon serves its RPC over a Unix domain socket and has **no HTTP health endpoint**; an unauthenticated `/health` readiness probe that returns `200` is exposed only when the ACP HTTP surface is enabled with `SMEDJA_ACP_PORT`. For container liveness, enable `SMEDJA_ACP_PORT` and probe `/health`, or check the daemon socket / process directly.

---

## Tool Sandbox

Shell tools (`bash`, `run_command`) run inside a per-platform isolation boundary selected by capability detection. Read-only tools (`read_file`, `list_files`, `graph_query`) are exempt.

**Backends** (selection precedence): Docker when opted in and reachable → the current platform's OS-native backend → none.

| Platform | Backend | Mechanism |
|----------|---------|-----------|
| any | Docker | ephemeral container; opt in with `SMEDJA_TOOL_SANDBOX=docker` and build the image via `smj sandbox build` |
| macOS | Seatbelt | generated `sandbox-exec` profile (zero-config; ships with macOS) |
| Linux | Landlock | Landlock LSM ruleset (zero-config on kernels ≥ 5.13) |

The writable filesystem root is the **confined root** — the active worktree when a task owns one, otherwise the session workspace — with `.git` read-only and an ephemeral `/tmp`. Writes outside the confined root are denied by the kernel boundary.

**Read confinement** — a sandboxed command's filesystem *reads* are confined to a bounded allow-list of system directories plus the confined root, so host secrets (`~/.ssh`, `~/.aws/credentials`, `~/.config`, `~/.gnupg`) are unreadable. This is enforced per backend:

| Platform | Read mechanism |
|----------|----------------|
| Linux (Landlock) | read+execute granted only over the allow-listed system dirs (not all of `/`); home/secret dirs are never granted |
| macOS (Seatbelt) | `(allow file-read*)` scoped to the allow-list plus explicit `(deny file-read*)` over the secret subpaths |
| Docker | structural — only the confined root is bind-mounted, so the host filesystem (and secrets) are simply absent from the container |

The default allow-list is `/usr /bin /sbin /lib /lib64 /etc /opt` (plus `/System /Library /private/var/db/dyld` on macOS for the dyld shared cache); non-existent paths are skipped.

Widen it with **`SMEDJA_SANDBOX_READ_PATHS`** — colon-separated paths *appended* to (never replacing) the defaults. The home directory is deliberately not in the defaults; an operator who needs a toolchain under `$HOME` (for example `$HOME/.cargo`, `$HOME/.rustup`, or a Homebrew prefix under `/opt/homebrew`) opts in explicitly and accepts the wider read surface:

```
SMEDJA_SANDBOX_READ_PATHS=/Users/me/.cargo:/Users/me/.rustup
```

**Failure mode** — if a command needs to read a path that is neither in the defaults nor in `SMEDJA_SANDBOX_READ_PATHS`, it fails with a permission or not-found error (rather than silently succeeding). Add the path to `SMEDJA_SANDBOX_READ_PATHS` to allow it.

**Network policy** — `SMEDJA_SANDBOX_NETWORK` (default `none`). The policy is now enforced for the sandboxed subprocess itself, per backend:

| Policy | Linux (Landlock) | macOS (Seatbelt) | Docker |
|--------|------------------|------------------|--------|
| `none` | fresh network namespace (`unshare --net`) — no route anywhere | `(deny network*)` | `--network none` |
| `allowlist` | host network retained¹ | `(allow network-outbound)`¹ | `--network bridge`¹ |
| `open` | host network retained¹ | `(allow network-outbound)`¹ | `--network bridge`¹ |

- `none` — no egress at all.
- `allowlist` — egress only to destinations not rejected by the daemon's SSRF guard (`is_blocked_ip`), so private/loopback/IMDS ranges stay blocked.
- `open` — general egress, but the same SSRF floor keeps private/loopback/IMDS ranges (including `169.254.169.254`) unreachable.

¹ **Subprocess limitation:** the `is_blocked_ip` SSRF floor runs inside smedja's *own* HTTP clients, not the child process's sockets. A raw subprocess cannot be per-destination IP-filtered without a filtering proxy, so for a subprocess **`allowlist` is treated as `open`-minus-blocked-ranges** — per-host allow-listing is not enforced for subprocesses in this change. Use `none` (full network isolation) or Docker for stronger guarantees.

On Linux, when `none` is requested but a network namespace cannot be created (no `unshare`, no `CAP_NET_ADMIN`, no unprivileged user namespaces), the backend fails closed rather than silently granting egress: under `SMEDJA_SANDBOX_MODE=required` the tool call errors naming the missing capability; under `auto` it falls back to the host with the unconfined marker. The `smedja.sandbox.exec` span records `net_confined=false` in that case.

**Fallback mode** — `SMEDJA_SANDBOX_MODE` (default `auto`) governs behaviour when no backend is available:

- `auto` — run on the host but stamp the result with an unconfined marker and emit a `smedja.sandbox.unconfined` event.
- `required` — fail the tool call closed with a diagnostic naming the missing capability; the command does not run.
- `off` — skip the sandbox entirely.

Each sandboxed execution emits a `smedja.sandbox.exec` span carrying `backend`, `network_policy`, `mode`, `confined_root`, `read_confined`, and `net_confined`. Run `smj sandbox status` to see the selected backend, its availability, the active network policy, and the fallback mode.

---

## Methodology

Test-driven development and clean-code discipline are **foundational**, not selectable modes. They are enforced **steer-first**: on every code-writing turn the orchestrator folds an always-on discipline directive (write a failing test first; no `unwrap`/`expect`/`println!` in library code; small focused functions; early-return over `else`) into the sealed, cacheable system prefix — so the agent is reminded of the discipline every turn. A diff backstop is the secondary check, not a blunt always-reject: the TDD backstop is advisory and fires only on substantial test-free implementation, while the clean-code backstop hard-blocks `unwrap`/`expect`/`println!` outside `#[cfg(test)]`.

The discipline is **on by default with a per-workspace escape**, mirroring the security plane. A `[methodology]` block in `.smedja/config.toml` with boolean `tdd` and `clean` fields (both defaulting to `true`) opts a workspace out of either discipline:

```toml
[methodology]
tdd = false      # drop the TDD steering clause and its advisory backstop
clean = true     # keep the clean-code discipline
```

A missing or unparseable config resolves to the all-on default and never blocks startup.

The spec-first lifecycle (`Mode::Spec`) and the clean gate (`Mode::Clean`) remain the selectable methodology concerns. `--no-spec-gate` disables the spec-first gate per session for quick patches. In normal operation the sequence is: spec → approval → test → implementation → review.

The on-demand **ponytail** review lens (YAGNI / delete-over-add) ships as a workspace skill (`.smedja/skills/ponytail.md`) loaded through the skill-injection path — an advisory lens, not a gate.

<div align="center">
  <img src="assets/diagrams/readme-spec-first-methodology.png" alt="spec-first methodology gate from OpenSpec through review" width="760" />
</div>

---

## Repo Auditor

`/review` is a read-only repo, PR, or branch auditor. It runs the Review role over a selected scope, explores with only `graph_query` / `read_file` / `list_files`, and aggregates the model's output into structured findings written to a deterministic markdown report. The loop is read-only by two independent guarantees: the read-only tool allowlist (any other tool call is rejected and fed back as an error observation) and a `review`-mode session whose `role_allows_write_bash` gate denies write-arity bash — it never constructs an `edit_file` / `write_file` dispatch.

Run it from the CLI with `smj audit run`:

```sh
smj audit run                       # the working-tree diff
smj audit run src/                  # a path or whole-repo scope
smj audit run --branch main         # a branch range, main...HEAD
smj audit run --pr 42               # a pull-request reference
```

Findings are de-duplicated and persisted as `smedja-ingot` audit events; query them later with `smj audit`.

---

## Security Plane

`smedja-security` is a proportionate, **advisory-by-default** security plane — it surfaces findings without blocking the loop. Three subcommands:

```sh
smj security scan [path]            # workspace posture scan; prints advisory findings
smj security report                 # summarise recorded security_finding audit events (read-only)
smj security sbom [--lockfile …]    # emit a CycloneDX-style SBOM from the resolved Cargo.lock
```

`scan` records its findings as `smedja-ingot` audit events, so `report` reads them back as a read-only query.

---

## MCP Server Mode

> **Naming note:** smedja's **ACP** is its own **Agent *Coordination* Protocol** — an optional HTTP session API (`/acp/v1/…`, enabled by `SMEDJA_ACP_PORT`). It is distinct from the industry **Agent *Client* Protocol** (the Zed/JetBrains editor↔agent standard), which smedja does not implement.

smedja is co-mounted as an **MCP server** on the authenticated ACP HTTP listener. The `/mcp` endpoint speaks JSON-RPC 2.0 and routes `tools/call` into the executor under an effective `review`-mode (read-only) session, so the least-privilege guard rejects mutating tools. ACP turn events are forwarded over SSE.

- **OAuth** — HTTP clients authenticate with OAuth 2.0 Authorization Code + PKCE (S256): code verifier/challenge, a loopback redirect listener with `state` validation, token exchange, and refresh-token grant. Tokens are persisted with `0600` filesystem permissions.
- **stdio transport** — a registered MCP server can also be driven as a child process over newline-framed JSON-RPC 2.0; the child is spawned lazily, reused across calls, and killed on drop. All I/O is async.

Manage the server registry with `smj mcp {add,list,remove,refresh}`.

---

## Eval Harness

`smedja-eval` runs a case suite and gates on its pass-rate threshold.

```sh
smj eval run --suite evals/<suite>            # run the suite, print a report, gate on the threshold
smj eval run --suite evals/<suite> --json     # machine-readable summary on stdout
cargo xtask eval evals/<suite>                # the offline gate, for CI
```

Suites live under `evals/` (each a directory with `suite.toml` and case files). `cargo xtask eval` runs the gate offline (deterministic, no provider calls), so it is safe in CI; graded rubric/live-driver cases run only with `--online` through the daemon.

---

## smedja

A GPU-accelerated terminal emulator. wgpu on Metal / Vulkan / DX12, `cosmic-text` for font shaping, `taffy` flexbox for split panes.

The difference from WezTerm: `smedja` knows what a smdjad session is. Agent turns render as `AgentBlock` widgets — tier badge, token count, traceparent, inline cowork gate — not raw byte streams. Shell commands render as standard `Block` units (Warp-style): selectable, copyable, independently scrollable.

**Current state:** Text renders using system fonts via `cosmic-text`. Startup is non-blocking — `FontSystem` initialises with an empty database (< 5 ms) and loads system fonts lazily on first glyph rasterisation. Individual glyph atlas misses are logged as warnings but do not crash the renderer. Background image blit is not yet implemented. Approval events currently display as text lines in the agent block; the inline cowork gate widget (`y`/`n`/`m`) is deferred for the GPU terminal.

Custom glyphs (tier badges, status icons, block decorations) register via the **Glyph Protocol** — APC sequences that map vector shapes to Unicode PUA codepoints. No Nerd Font patches required. A child process emits `ESC _ SMEDJA_GLYPH;id=<id>;format=<svg|png>;data=<base64> ESC \`; the PTY reader assigns the id a PUA codepoint (`U+E000..=U+F8FF`), rasterises the shape (PNG at native size, SVG as a solid-fill approximation at 32×32), and caches the RGBA bitmap keyed by that codepoint. When a registered codepoint appears in a cell the renderer samples its bitmap from a dedicated RGBA colour atlas (font glyphs continue to use the alpha-only atlas). Tier and status badges resolve their domain string (`local`/`fast`/`deep`, `ok`/`fail`/`pending`) to a built-in glyph ID and then to its PUA codepoint, degrading to plain-text labels (`[deep]`, `✓`, …) when the terminal lacks APC support or the glyph is unregistered. SVG fidelity is limited to a solid fill (full vector rendering via `resvg` is out of scope); PNG glyphs render faithfully.

<div align="center">
  <img src="assets/diagrams/readme-smedja-term.png" alt="smedja window with shell blocks, agent blocks, cowork gate, and status bar" width="900" />
</div>

Config is TOML. A migration tool converts existing WezTerm Lua config.

`smedja-tui` is a ratatui agent dashboard that runs as a normal app inside smedja (or any terminal). Launch it with `smedja-tui` from the shell.

### Resuming a session

By default `smedja-tui` creates a fresh session on launch. To reopen a prior conversation:

```sh
# attach to an existing session and replay its history into the view
smedja-tui --session <id>

# resume but rewind the session to a chosen turn first (destructive — prunes
# later turns, mirroring `smj session rollback <id> <turn>`)
smedja-tui --session <id> --turn <n>
```

An unknown `--session <id>` fails fast with `session not found: <id>` before the dashboard opens.

Inside the TUI, `/resume` opens an interactive picker listing resumable sessions (short id, title, mode, last-updated); press Enter on a row to resume it in place without restarting. `/resume <id>` resumes directly, and `/resume <id> <turn>` rewinds to that turn before replaying. Plain resume (no turn) is non-destructive — it only reads history. `/resume` is refused while a turn is in flight.

---

## TUI Reference

See [`docs/tui.md`](docs/tui.md) for the complete manual. Quick reference below.

### Slash Commands

| Command | What it does |
|---------|-------------|
| `/agent [role]` | Set the agent role; omit to list. Roles: `code`/`impl`, `plan`, `research`, `debug`, `ask`, `review`, `test`, `sre`, `data`, `iac`, `orchestrator` — each routes to a default (client, tier) and auto-loads its rules from `.smedja/roles/<role>.md` |
| `/approve [id]` | Approve a pending approval; omit id to list pending (or use the inline `y`/`n`/`m` widget) |
| `/cowork on\|off\|status` | Toggle/inspect cowork approval mode for the session |
| `/briefing` | Show the session briefing |
| `/clear` | Clear the message display (keeps session data) |
| `/drawio <slug>` | Generate a draw.io mxGraph XML diagram |
| `/gov [subcommand]` | govctl artifact management — see `/gov help` |
| `/health` | Check daemon connectivity |
| `/help` | List all slash commands |
| `/login` | Authenticate with the current runner |
| `/loop [subcommand]` | Manage loop runs: `status \| list \| create <goal> \| cancel` |
| `/lsp` | Show LSP server status and diagnostic summary |
| `/metrics` | Show token usage and cost rollup |
| `/model [name]` | List models or hot-swap; local-runner aware (`/model qwen3-14b` hot-swaps) |
| `/pptx <slug>` | Generate a python-pptx presentation script |
| `/quit` | Exit smedja-tui |
| `/quota` | Show daily token usage vs. `SMEDJA_DAILY_TOKEN_LIMIT` |
| `/resume [id [turn]]` | Reopen a prior session; omit id for interactive picker; `turn` rewinds destructively |
| `/review` | Send the current `git diff` for review |
| `/spec` | Browse OpenSpec changes |
| `/switch [runner]` | Switch AI runner interactively or directly |
| `/takeover <runner>` | Fork the session to a new runner |
| `/test [cargo\|npm\|go\|py]` | Run the project test suite; auto-detects manifest |
| `/tier <t>` | Set tier: `local \| fast \| deep` — resolves the runner's model for that tier and pins it (persists across restarts) |
| `/version` | Print current version and check for a newer release |
| `/upgrade` | Download and install the latest release in-place |

### Inline Context Fragments

Fragments are expanded into your message before the turn runs:

| Fragment | What it injects |
|----------|----------------|
| `@file <path>` | A workspace file's contents (path must stay inside the workspace) |
| `@git` | `git status --short` and `git diff HEAD` |
| `@branch` | The current branch name and upstream |
| `@shell <cmd>` | A shell command's stdout (gated by cowork when enabled) |

### Key Bindings — Input Mode

| Key | Action |
|-----|--------|
| `Enter` | Submit message |
| `↑` / `Ctrl-P` | Browse prompt history backwards |
| `↓` / `Ctrl-N` | Browse prompt history forwards |
| `Ctrl-R` | Toggle reverse history search |
| `Ctrl-G` | Open `$EDITOR` / `$VISUAL` to compose a multi-line message |
| `Ctrl-B` | Move cursor left one character |
| `Ctrl-K` | Kill from cursor to end of line (push to kill ring) |
| `Ctrl-U` | Kill from start of line to cursor (push to kill ring) |
| `Ctrl-Y` | Yank most recent kill at cursor |
| `Shift-Tab` | Cycle the permission mode: `ask → accept_edits → plan → auto` |
| paste | Bracketed paste — multi-line text (URLs, snippets) lands as one edit, no premature submit |
| `Esc` | Interrupt the in-flight turn if one is running; otherwise enter scroll/normal mode |

### Key Bindings — Scroll / Normal Mode

| Key | Action |
|-----|--------|
| `i` / `a` | Return to input mode |
| `j` / `k` | Scroll down / up |
| `G` | Scroll to bottom |
| `gg` | Scroll to top |
| `v` | Start visual line-selection mode |
| `y` | Yank current selection to clipboard |
| `t` | Copy the W3C traceparent from the current block |
| `T` | Expand / collapse the thinking block |
| `[` / `]` | Move cursor up / down in the session rail |
| `Esc` | Exit selection / return to input mode |

### Panel Toggles (both modes unless noted)

| Key | Panel |
|-----|-------|
| `Ctrl-A` | Role cockpit — active role, tier, runner, turn status |
| `Ctrl-F` | Context fill rail (scroll mode only) |
| `Ctrl-L` | LSP diagnostic panel |
| `Ctrl-O` | Observability panel |
| `Ctrl-T` | Metrics overlay |
| `Ctrl-W` | Session browser left-rail |

### Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMEDJA_SOCK` | `$XDG_RUNTIME_DIR/smdjad.sock` | Override daemon socket path |
| `SMEDJA_WORKSPACE` | *(daemon cwd)* | Workspace root for the code-graph and LSP when not announced by a client |
| `SMEDJA_TOOL_GATE` | `on` | Set to `off` to disable the claude PreToolUse approval hook |
| `SMEDJA_MODEL_<RUNNER>_<TIER>` | *(built-in default)* | Pin a tier's model so new releases need no rebuild — e.g. `SMEDJA_MODEL_CLAUDE_DEEP=claude-opus-5`, `SMEDJA_MODEL_CODEX_FAST=gpt-6`. `<RUNNER>` ∈ `CLAUDE`/`CODEX`/`COPILOT`/`MINIMAX`/`LOCAL`; `<TIER>` ∈ `FAST`/`DEEP`/`LOCAL` |
| `MINIMAX_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GITHUB_TOKEN` | *(unset)* | Provider keys (in `~/.config/smedja/secrets.env`) — enable the minimax/openai/anthropic-API/copilot runners when their CLI isn't installed |
| `SMEDJA_DAILY_TOKEN_LIMIT` | *(unset — no limit)* | Daily token budget; shown in the `/quota` panel as a usage bar |
| `SMEDJA_SANDBOX_MODE` | `auto` | Sandbox fallback: `auto \| required \| off` |
| `SMEDJA_SANDBOX_NETWORK` | `none` | Subprocess network policy: `none \| allowlist \| open` |
| `SMEDJA_SANDBOX_READ_PATHS` | *(empty)* | Colon-separated extra paths appended to the sandbox read allow-list |
| `SMEDJA_LOCAL_ENDPOINT` | `http://127.0.0.1:9090` | OpenAI-compatible local model endpoint |
| `SMEDJA_LOCAL_SWAP_ENDPOINT` | same as `SMEDJA_LOCAL_ENDPOINT` | Hot-swap endpoint (for llama-swap) |
| `SMEDJA_OTLP_ENDPOINT` | *(unset)* | OTLP collector endpoint; enables the OTel trace footer in the TUI when set |
| `NO_COLOR` | *(unset)* | Disable all colour output when set to any value |

---

## Getting Started

For a full walkthrough see [`docs/getting-started.md`](docs/getting-started.md).

```bash
# 1. start the daemon (socket auto-placed at $XDG_RUNTIME_DIR/smdjad.sock)
smdjad

# 2a. open the agent dashboard TUI inside any terminal
smedja-tui
# optional flags: --mode impl|review|test|sre  --tier fast|deep  --sock /path/to/smdjad.sock

# 2b. or open the GPU terminal (launches smedja-tui as its default app)
smedja

# 3. send a message — type in the TUI input bar and press Enter
#    the daemon routes the turn to the configured provider and streams the reply

# control CLI
smj session list
smj session cost
smj workspace agents
```

The daemon reads `$XDG_RUNTIME_DIR/smdjad.sock` (falls back to `/tmp/smdjad.sock`). The TUI and `smj` CLI use the same default; override with `--sock` or `SMEDJA_SOCK`.

---

## Maintaining the Codebase

The CLI and terminal entrypoints are intentionally thin. Add new behavior in the
owning module, keep command/event routers small, and add tests near the behavior
or in the crate-level test module when the behavior crosses modules.

See [`docs/maintenance.md`](docs/maintenance.md) for the module map, test
placement policy, and verification commands for `smedja-cli` and `st-app`.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
