# smj CLI Reference

`smj` is the smedja control CLI. It connects to a running `smdjad` daemon via UDS and exposes every daemon capability as a subcommand.

```sh
smj [--sock <path>] <subcommand> [args]
```

`--sock` (or `SMEDJA_SOCK`) overrides the default socket path (`$XDG_RUNTIME_DIR/smdjad.sock`, falling back to `/tmp/smdjad.sock`).

---

## daemon

Manage the `smdjad` process lifecycle.

```sh
smj daemon start      # start smdjad in the background
smj daemon stop       # stop a running smdjad
smj daemon restart    # stop then start
smj daemon status     # check whether smdjad is running
```

---

## session

Session management.

```sh
smj session start [--cowork] [--task <title>]   # create a new session
smj session list                                  # list all sessions
smj session show <id>                             # show session metadata
smj session fork <id> [--turn <n>]               # fork a session at an optional turn
smj session rollback <id> <turn>                 # rewind a session (destructive)
smj session checkpoint <id>                      # list checkpoints
smj session export <id> [--format json|md]       # export turns + audit (json) or a transcript (md)
smj session compact <id>                         # compact the conversation history
smj session tokens <id>                          # per-turn token usage
```

**`session.start --cowork`** — enables cowork mode: every tool call pauses for approval.

**`session.rollback <id> <turn>`** — destructive: prunes all turns after `turn` and marks them deleted in the ingot. Used by `smedja-tui --turn` and `/resume <id> <turn>`.

---

## local

Local-model management. Requires a running `local` runner (llama-swap or compatible proxy) at `SMEDJA_LOCAL_ENDPOINT`.

```sh
smj local list [--json]          # GPU-annotated model inventory: fits | tight | exceeds | unknown
smj local gpu [--json]           # cached GPU snapshot (device, VRAM total/free)
smj local swap <model>           # hot-swap the active model — no daemon restart
smj local install <model>        # install a model via rs-llmctl
```

GPU fit annotations are advisory — VRAM placement is llama-swap's responsibility.

---

## metrics

Local time-tiered rollups of tokens, cost, and errors per runner. Reads from `smedja-ingot` directly — no external dependency.

```sh
smj metrics [--tier daily] [--since 7d] [--until <time>] [--runner <name>] [--json]
```

**Tiers**: `raw` | `hourly` | `daily` | `weekly` (ISO Mon 00:00) | `monthly` (first-of-month)

**`--since` / `--until`**: duration back from now (`7d`, `24h`, `30m`, `90s`) or bare seconds.

```sh
smj metrics --tier daily --since 7d
smj metrics --tier hourly --since 24h --runner claude
smj metrics --tier monthly --since 365d --json
```

---

## savings

Token-economy savings rollup (SmartCrusher + stable-prefix KV-cache savings).

```sh
smj savings [--tier daily] [--since 7d] [--until <time>] [--json]
```

Same tier / time arguments as `smj metrics`.

---

## audit

Audit log management.

```sh
smj audit run [<path>] [--branch <b>] [--pr <n>] [--diff] [--report <file>] [--format md|json]
smj audit query [--session <id>] [--since <dur>] [--action <type>]
smj audit prompt-diff --change <name>
smj audit who --session <id>
smj audit export --change <name> [--format jsonl|csv] [--include-prompts]
```

**`audit run`** runs the Review role over the selected scope (working-tree diff, a path, a branch range, or a PR). The reviewer is read-only (no `edit_file` / `write_file`). Findings are de-duplicated and persisted as `security_finding` audit events.

| Scope flag | What it audits |
|------------|---------------|
| *(none)* | Current working-tree diff (`git diff HEAD`) |
| `<path>` | The given path or whole-repo |
| `--branch <b>` | `<b>...HEAD` range |
| `--pr <n>` | A pull-request reference resolved to a branch range |
| `--diff` | Force working-tree diff |

---

## workspace

Workspace setup and management.

```sh
smj workspace init [<path>]      # create .smedja/, index symbols, write workspace.toml
smj workspace index [--commit-sha <sha>]  # (re-)index the code graph
smj workspace add <path>         # register a directory with the workspace
smj workspace agents show        # print the resolved role→runner→tier→model table
smj workspace agents init        # generate a starter .smedja/agents.toml
```

**`workspace init`** is safe to re-run — it creates `.smedja/` if absent, indexes the symbol graph, and writes `workspace.toml` with defaults. Run it once per repo.

---

## loop

Loop engine control (requires a `loop.json` policy and `openspec/changes/<name>/tasks.md`).

```sh
smj loop run --change <name> [--max-slices 10]
smj loop status --change <name>
smj loop cancel --change <name>
smj loop retire --change <name>
smj loop list [--status <s>]
```

The loop pipeline: plan → red/green implementation → deterministic verification → read-only review → bounded fix retries. See [`docs/configuration.md`](configuration.md#loopjson) for the policy contract.

---

## task

Project task management.

```sh
smj task list [--status <s>]
smj task show <id>
smj task create <title> [--description <text>]
smj task close <id>
smj task parallel <goal> --roles impl,test,review
smj task status <id>
smj task cancel <id>
smj task export [--change <name>]
smj task import
```

**`task parallel`** fans a goal out to multiple agent roles running concurrently in isolated git worktrees. The orchestrator merges vault writes on task completion.

---

## mcp

MCP server registry.

```sh
smj mcp add <name> <url> [--stdio <command>]
smj mcp list
smj mcp remove <name>
smj mcp refresh [<name>]
```

Registered MCP servers are available to agent tool calls. The `/mcp` HTTP endpoint on the daemon's ACP listener also exposes smedja as an MCP server (read-only, OAuth 2.0). Here "ACP" is smedja's own Agent *Coordination* Protocol (its optional `/acp/v1/…` HTTP session API, enabled by `SMEDJA_ACP_PORT`), not the Zed/JetBrains Agent *Client* Protocol, which smedja does not implement.

---

## skill

Skill file management.

```sh
smj skill list
smj skill install <path>          # path to a SKILL.md file or directory
smj skill update <name> <path>
smj skill remove <name>
smj skill sync <path>             # sync all skills from a bundle dir using symlinks
```

Skills are loaded from `.smedja/skills/` and injected into the system prompt. `smj skill sync` is useful with `agent-toolkit-bundle`.

---

## security

Security posture — advisory by default.

```sh
smj security scan [<path>]        # workspace posture scan; prints advisory findings
smj security report               # summarise recorded security_finding audit events
smj security sbom [--lockfile <path>]  # CycloneDX-style SBOM from Cargo.lock
```

`scan` records findings as `smedja-ingot` audit events. `report` is a read-only query over those events.

---

## eval

Eval harness.

```sh
smj eval run --suite <path> [--online] [--json] [--threshold <f>]
```

Suites live under `evals/` (each is a directory with `suite.toml` and case files). Without `--online`, graded (rubric / live-driver) cases are skipped — deterministic gate only, safe in CI.

```sh
# Offline gate (CI-safe):
cargo xtask eval evals/<suite>

# Full online run (makes provider calls):
smj eval run --suite evals/routing --online
```

---

## sandbox

Docker sandbox management.

```sh
smj sandbox build          # build the smedja-sandbox Docker image
smj sandbox status         # report backend, availability, network policy, fallback mode
```

`smj sandbox status` mirrors the daemon's backend-selection precedence so you can check what `smdjad` would select before deploying.

---

## timeline

Local Agent Timeline inspection — conversation history from the ingot.

```sh
smj timeline conversations [--since <seconds>] [--json]
smj timeline show <conversation_id> [--failures-only] [--json]
smj timeline open <id>          # open in configured backend (Honeycomb, SigNoz, etc.)
```

`timeline show` renders ordered events for a conversation: turns, tool calls, role transitions, errors.

---

## service

Manage `smdjad` as a system service.

```sh
smj service install    # register smdjad as a launchd agent (macOS) or systemd user unit (Linux)
smj service uninstall
smj service start
smj service stop
smj service status
```

---

## prices

Model pricing table management.

```sh
smj prices update [--file <path>]   # update prices.toml from a local file
smj prices update                   # print current prices
```

`prices.toml` ships bundled — `smj session cost` never makes an external API call.

---

## term

Terminal utilities.

```sh
smj term install [--bin-path <url>] [--prefix <dir>]
```

Downloads and installs the smedja binary to `~/.local/bin` (or `--prefix`).

---

## cost

Session cost summary.

```sh
smj cost [--session <id>] [--since <dur>]
```

Reads `smedja-ingot` and prints a per-session cost breakdown by model and runner.
