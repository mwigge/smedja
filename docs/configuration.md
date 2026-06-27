# Configuration Reference

smedja configuration lives in `.smedja/` at the workspace root. All files are optional except `loop.json` which is required for `smj loop run`.

---

## `.smedja/agents.toml`

Defines per-role runner, tier, model, and tool overrides for the current workspace. Committed to the repo — portable across machines.

```toml
[roles.impl]
runner = "local"
tier   = "local"
model  = "Qwen3-14B"
tools  = ["read_file", "edit_file", "bash", "graph_query"]

[roles.review]
runner = "claude"
tier   = "deep"
tools  = ["read_file", "graph_query"]   # review is intentionally read-only

[roles.test]
runner = "local"
tier   = "local"
tools  = ["read_file", "bash"]

[roles.sre]
runner = "claude"
tier   = "deep"
tools  = ["read_file", "otel_query", "metric_query", "log_tail"]

[roles.fix]
runner = "local"
tier   = "local"
tools  = ["read_file", "edit_file", "bash"]
```

The assayer routes by **role + complexity**, not just complexity. A simple fix stays local; an architecture review goes to `claude deep`. When a field is absent, the daemon's default applies.

Generate a starter file:

```sh
smj workspace agents init   # writes .smedja/agents.toml
smj workspace agents show   # print the resolved role→runner→tier→model table
```

**Role set.** Task-type roles (language is detected context, not a role):
`code`/`impl`, `plan`, `research`, `debug`, `ask`, `review`, `test`, `sre`,
the domain roles `data` (SQL) and `iac` (infra-as-code), and `orchestrator`.
Read-only roles (`plan`/`research`/`review`/`ask`/`orchestrator`) can never
mutate the workspace; `iac` mutations are always confirmed even in `auto` mode.
Select the active role with `/agent <role>` in the TUI.

---

## `.smedja/roles/` — per-role rules & skills

Each role auto-loads its own discipline into the system prompt whenever it is
active: the file `.smedja/roles/<role>.md` plus every `*.md` under
`.smedja/roles/<role>/`. These are injected alongside the always-on
`.smedja/skills/*.md`. Use them for role-specific rules — a review checklist, a
research source-hygiene policy, IaC safety rules, language conventions, etc.

```
.smedja/roles/
  plan.md         # planning rules
  review.md       # review checklist
  research.md     # source-hygiene + citation rules
  iac.md          # infra safety (apply/destroy always confirmed)
  code/           # multiple .md files, sorted by name
    rust.md
    style.md
```

---

## Permission modes

Mutating tool calls gate by default (*ask-on-mutation*). Cycle the session
permission mode with **Shift-Tab** in the TUI, or set it via the
`cowork.set_mode` RPC:

| Mode | Behaviour |
|------|-----------|
| `ask` (default) | Stop and confirm every mutation (write/edit/shell) |
| `accept_edits` | Auto-approve known file edits; still ask on shell/unknown tools |
| `plan` | Read-only — all mutations denied |
| `auto` | Auto-approve everything (no gate) |

Reads always pass. Read-only roles and high-risk `iac` mutations override the
mode (always read-only / always-confirm respectively). Disable the claude
PreToolUse approval hook with `SMEDJA_TOOL_GATE=off`.

---

## `.smedja/loop.json`

The loop engine policy contract. Required by `smj loop run`. Its SHA-256 is hashed at load — if the file changes mid-run the loop aborts in the terminal `policy_tampered` state.

```json
{
  "version": 1,
  "limits": {
    "max_attempts": 3,
    "agent_timeout_s": 600
  },
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

| Field | Type | Meaning |
|-------|------|---------|
| `limits.max_attempts` | int | Max role attempts per slice before the loop fails |
| `limits.agent_timeout_s` | int | Per-role wall-clock timeout in seconds |
| `roles[].name` | string | Role name matching `agents.toml` |
| `roles[].runner` | string | Runner to use for this role |
| `roles[].tier` | string | Routing tier: `local`, `fast`, `deep` |
| `roles[].read_only` | bool | Whether this role may write files |
| `roles[].tools` | array | Allowed tool names (empty = use role defaults) |
| `verification.command` | string | Shell command run as a deterministic gate after each slice; exit 0 = pass |
| `review.per_slice` | bool | Run the reviewer after each slice (vs. only at the end) |
| `review.required` | bool | A failing review blocks progress when `true` |
| `publication.max_pr_lines` | int | Max changed lines per published slice |

**Constraint**: the reviewer and implementer must use **different runners** (evaluator/generator separation). The loop fails closed before any role runs if they share a runner.

**Verification timeout**: defaults to 300 s. Override with `SMEDJA_LOOP_VERIFY_TIMEOUT` (seconds).

---

## `.smedja/workspace.toml`

Optional workspace-level settings. Written by `smj workspace init`.

```toml
[workspace]
name = "my-project"
description = "optional description"

[graph]
# Additional directories to index (relative to workspace root)
extra_paths = ["shared/", "libs/"]

[tools]
confirm_edits = false   # set true to enable the edit_file cowork-approval gate
```

### `[tools]`

| Key | Default | Purpose |
|-----|---------|---------|
| `confirm_edits` | `false` | When `true`, `edit_file` tool calls emit a cowork approval request before writing. Full async gate is roadmap; current release logs and proceeds. |

---

## `.smedja/methodology.toml` (or `[methodology]` in config.toml)

Controls the TDD and clean-code discipline enforced by the orchestrator.

```toml
[methodology]
tdd   = true    # enforce test-first discipline (default: true)
clean = true    # enforce no-unwrap / no-println! clean-code gate (default: true)
```

Both default to `true`. A missing or unparseable config resolves to the all-on default and never blocks startup.

- `tdd = false` — drops the TDD steering clause and its advisory backstop
- `clean = false` — disables the hard-block on `unwrap`/`expect`/`println!` outside `#[cfg(test)]`

---

## `.smedja/skills/`

Skill files loaded as Claude Code skills and injected into the system prompt. Each file is a `SKILL.md` following the Claude Code skill format.

Manage via:

```sh
smj skill list
smj skill install <path>
smj skill sync <bundle-dir>   # symlink all skills from an agent-toolkit-bundle directory
```

The built-in `ponytail` skill provides a YAGNI / delete-over-add review lens as an advisory on-demand lens.

---

## `openspec/changes/<name>/tasks.md`

OpenSpec task envelope consumed by `smj loop run --change <name>`. Each unchecked `- [ ] ` line is one slice the pipeline drives.

```markdown
- [ ] Add input validation to the auth middleware
- [ ] Write unit tests for the token refresh path
- [x] Update the README (done)
```

The `[x]` items are already-completed — the loop skips them and only drives `[ ]` lines.

---

## Environment Variables

### Daemon and TUI

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMEDJA_SOCK` | `$XDG_RUNTIME_DIR/smdjad.sock` | Daemon socket path |
| `SMEDJA_DAILY_TOKEN_LIMIT` | *(unset)* | Daily token budget; displayed in `/quota` |
| `SMEDJA_OTLP_ENDPOINT` | *(unset)* | OTLP collector endpoint; enables trace footer in TUI |
| `SMEDJA_TIMELINE_URL` | *(unset)* | URL template for trace deep-links; `{id}` is replaced with the traceparent trace ID |
| `SMEDJA_LOG_FORMAT` | `text` | Log output format: `text` or `json` |
| `NO_COLOR` | *(unset)* | Disable colour output when set to any value |

### Sandbox

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMEDJA_SANDBOX_MODE` | `auto` | Fallback when no backend available: `auto \| required \| off` |
| `SMEDJA_SANDBOX_NETWORK` | `none` | Subprocess network policy: `none \| allowlist \| open` |
| `SMEDJA_SANDBOX_READ_PATHS` | *(empty)* | Colon-separated extra read-allow paths (appended to defaults, not replacing) |
| `SMEDJA_TOOL_SANDBOX` | *(unset)* | Legacy opt-in: set to `docker` to force the Docker backend |
| `SMEDJA_SANDBOX_IMAGE` | `smedja-sandbox:latest` | Docker image name for the sandbox container |

Sandbox read defaults: `/usr /bin /sbin /lib /lib64 /etc /opt` (plus `/System /Library /private/var/db/dyld` on macOS). Home directory and credential paths are **not** in the defaults.

### Local Runner

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMEDJA_LOCAL_ENDPOINT` | `http://127.0.0.1:9090` | OpenAI-compatible local model endpoint |
| `SMEDJA_LOCAL_SWAP_ENDPOINT` | same as above | Hot-swap endpoint (for llama-swap) |
| `SMEDJA_LOCAL_INSTALLER` | `rs-llmctl` | Binary used by `smj local install` |

### Loop

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMEDJA_LOOP_VERIFY_TIMEOUT` | `300` | Verification gate wall-clock budget in seconds |

### Storage

| Variable | Default | Purpose |
|----------|---------|---------|
| `XDG_DATA_HOME` | `$HOME/.local/share` | Base for `smedja/ingot.db` |
| `XDG_CONFIG_HOME` | `$HOME/.config` | Base for user-level config |
| `XDG_RUNTIME_DIR` | `/tmp` | Base for the daemon socket |

---

## `~/.config/smedja/config.toml` — user config

The user-level config is read on every TUI startup from `~/.config/smedja/config.toml`.  All sections are optional; missing keys fall back to built-in defaults.

---

### `[tui.colors]` — forge palette overrides

The TUI ships a brand-matched forge palette (warm amber on near-black) that renders at 24-bit true colour, independent of the terminal's own theme.  Every colour slot is overridable via `[tui.colors]`.  Colours are `#rrggbb` hex strings; a missing entry keeps the forge default.

```toml
[tui.colors]
# UI chrome — amber/copper on near-black
bg          = "#0b0d0f"   # main background
panel       = "#111316"   # inner panel fill
header      = "#211811"   # title row background inside panels
border      = "#a9652f"   # primary panel border (copper)
border_dim  = "#3b2a1f"   # inner divider / secondary border
text        = "#d99b55"   # body text, labels (primary amber)
text_bright = "#f7c77e"   # headings, active items (bright amber)
text_dim    = "#8f765b"   # metadata, footers (dim amber)
accent      = "#ffb24a"   # spinner, selected rows (vivid amber)
error       = "#d65f2e"   # error state (forge red-orange)
success     = "#5d946b"   # success state (forge green)
warn        = "#d99b55"   # warning state (same as `text`)
local       = "#4eb9b2"   # `local` tier badge (warm teal)
fast        = "#f7c77e"   # `fast` tier badge (bright gold)
deep        = "#a9652f"   # `deep` tier badge (copper, same as border)

# Code tokens — cool tones, visually distinct from chrome
code_default  = "#ccd0da"   # plain code text (cool near-white)
code_keyword  = "#c792ea"   # keywords (soft violet)
code_string   = "#c3e88d"   # string literals (soft green)
code_number   = "#89ddff"   # numeric literals (light cyan)
code_comment  = "#546e7a"   # comments (dim teal-gray)
code_type     = "#82aaff"   # type identifiers (ice blue)
code_macro    = "#f78c6c"   # macro invocations (warm orange)
code_added    = "#5d946b"   # diff added lines (same as `success`)
code_removed  = "#d65f2e"   # diff removed lines (same as `error`)
```

**Slot reference**

| Slot | Default | Used for |
|------|---------|----------|
| `bg` | `#0b0d0f` | Main terminal background |
| `panel` | `#111316` | Inner panel fill |
| `header` | `#211811` | Title row inside panels |
| `border` | `#a9652f` | Panel outlines |
| `border_dim` | `#3b2a1f` | Session rail, inner dividers |
| `text` | `#d99b55` | Body text, labels |
| `text_bright` | `#f7c77e` | Active items, headings |
| `text_dim` | `#8f765b` | Metadata, footers |
| `accent` | `#ffb24a` | Spinner, selected rows |
| `error` | `#d65f2e` | Error badges |
| `success` | `#5d946b` | Success badges, healthy LSP dot |
| `warn` | `#d99b55` | Warning badges, context fill 60–80% |
| `local` | `#4eb9b2` | Local tier badge |
| `fast` | `#f7c77e` | Fast tier badge |
| `deep` | `#a9652f` | Deep tier badge |
| `code_default` | `#ccd0da` | Plain code text |
| `code_keyword` | `#c792ea` | Language keywords |
| `code_string` | `#c3e88d` | String literals |
| `code_number` | `#89ddff` | Numeric literals |
| `code_comment` | `#546e7a` | Comments |
| `code_type` | `#82aaff` | Type identifiers and primitives |
| `code_macro` | `#f78c6c` | Macro invocations |
| `code_added` | `#5d946b` | Diff addition lines |
| `code_removed` | `#d65f2e` | Diff removal lines |

**Rules**

- Parse errors (bad hex, wrong length) are silently ignored; the slot keeps its forge default.
- Unknown keys are ignored by serde — future slots can be added without breaking old configs.
- The palette is initialised once at startup.  Changes require a TUI restart.
- Set `NO_COLOR=1` to strip all colours regardless of this config.

Context fill rail thresholds (not overridable per-slot, derived from `success`/`warn`/`error`): green < 60 %, yellow 60–80 %, red > 80 %.
