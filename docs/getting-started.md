# Getting Started with smedja

smedja is a Rust-native multi-agent AI orchestration system. It has three entry points:

- **`smdjad`** — the daemon that manages sessions, routes turns, and runs the loop engine
- **`smedja-tui`** — a ratatui agent dashboard that runs inside any terminal
- **`smedja`** — a GPU-accelerated terminal emulator that knows what a smdjad session is
- **`smj`** — the control CLI for everything the daemon exposes

---

## 1. Install

### Arch / CachyOS (recommended for this machine)

```sh
curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
systemctl --user enable --now smdjad
```

Or via PKGBUILD:

```sh
git clone https://github.com/mwigge/smedja
cd smedja/assets
makepkg -si
systemctl --user enable --now smdjad
```

### macOS

```sh
curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
```

Clear the Gatekeeper quarantine flag after install:

```sh
xattr -dr com.apple.quarantine ~/.local/bin/smedja ~/.local/bin/smdjad ~/.local/bin/smj ~/.local/bin/smedja-tui
```

### From source (Rust ≥ 1.82)

```sh
git clone https://github.com/mwigge/smedja
cd smedja
cargo build --release --workspace
cp target/release/{smdjad,smj,smedja-tui,smedja} ~/.local/bin/
```

---

## 2. Configure a provider

smedja needs an LLM provider to generate responses. The easiest way to get started is with Anthropic's API:

```sh
export ANTHROPIC_API_KEY=sk-ant-...   # add to ~/.bashrc or ~/.zshrc to persist
```

Or, to use a local model via an OpenAI-compatible server (Ollama, llama-swap, etc.):

```sh
export SMEDJA_LOCAL_ENDPOINT=http://127.0.0.1:11434/v1
```

smdjad will automatically detect and use a configured provider. If no provider is configured, turns will fail with a DEGRADED status — check `smj status` or the TUI status bar for provider health.

---

## 3. Start the daemon

```sh
smdjad
```

The daemon binds a UDS socket at `$XDG_RUNTIME_DIR/smdjad.sock` (falls back to `/tmp/smdjad.sock`). It signals readiness via `sd_notify(READY=1)` for systemd's `Type=notify`, and exposes a `/health` HTTP readiness probe once serving.

Override the socket path with `--sock` or `SMEDJA_SOCK`.

---

## 5. Open the TUI

```sh
smedja-tui
```

Or inside the smedja GPU terminal:

```sh
smedja          # opens smedja-tui as its default app
```

The TUI creates a fresh session and connects to the daemon. You'll see the status bar at the bottom showing `[tier] [mode] [session-id] [runner]`.

### Flags

| Flag | Purpose |
|------|---------|
| `--mode impl\|review\|test\|sre\|explain` | Set the agent mode for the new session |
| `--tier local\|fast\|deep` | Override the routing tier |
| `--sock <path>` | Connect to a non-default daemon socket |
| `--session <id>` | Attach to an existing session and replay its history |
| `--turn <n>` | Rewind the resumed session to turn `n` before replaying (destructive) |

---

## 6. Send your first message

Type in the input bar at the bottom of the TUI and press `Enter`. The daemon routes the turn to the configured provider and streams the reply into the main panel.

Each turn renders as a **TurnBlock** with a header line showing:
- Turn number
- Runner · Model
- Input → output token counts
- Wall-clock latency
- A W3C `traceparent` in the footer

---

## 7. Basic session workflow

```sh
# Start a fresh session with the review agent
smedja-tui --mode review

# Attach to an existing session by id
smedja-tui --session abc12345

# List all sessions from the CLI
smj session list

# Check token cost for the current session
smj session cost
```

Inside the TUI:

- `/session` — show the current session ID and state
- `/resume` — open an interactive session picker (press `Enter` to reattach)
- `/model` — list available models; `/model qwen3-14b` hot-swaps the local model
- `/switch` — switch runner interactively
- `/clear` — clear the display (keeps the session conversation)
- `/quit` — exit

---

## 8. Workspace setup

Run once per repo to create the `.smedja/` config directory and index the code graph:

```sh
smj workspace init          # writes .smedja/workspace.toml, indexes symbols
smj workspace agents init   # writes .smedja/agents.toml with starter roles
```

See [`docs/configuration.md`](configuration.md) for the full config reference.

---

## 9. Next steps

- [`docs/tui.md`](tui.md) — complete TUI command and keybinding reference
- [`docs/smj.md`](smj.md) — smj CLI reference
- [`docs/configuration.md`](configuration.md) — agents.toml, loop.json, env vars
- [`docs/govctl.md`](govctl.md) — govctl work items, RFCs, ADRs
- [`docs/terminal.md`](terminal.md) — smedja GPU terminal guide
- [`docs/maintenance.md`](maintenance.md) — module ownership and verification guide
