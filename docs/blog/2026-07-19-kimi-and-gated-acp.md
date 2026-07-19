# Kimi joins the forge — and brings gated ACP with it

*2026-07-19 · smedja 0.26.0*

smedja 0.26.0 adds **Kimi (Moonshot AI)** as the third CLI in the provider
ladder, right after Claude and Codex — and, because doing it properly forced
the issue, a **generic Agent Client Protocol (ACP) client** that puts external
agents' tool calls behind the same approval gate as everything else. Gemini
rides in on the same rails as runner number four.

## Kimi, end to end

Kimi gets the same dual-path treatment Claude has always had:

- **API key path** — set `MOONSHOT_API_KEY` (or `KIMI_API_KEY`) and smedja
  talks to Moonshot's OpenAI-compatible endpoint directly as the `moonshot`
  runner: `kimi-k2.7-code-highspeed` on the fast tier, `kimi-k3` (the
  1M-context flagship) on deep. Mainland-China platform keys point at the
  regional endpoint via `MOONSHOT_BASE_URL=https://api.moonshot.cn/v1`.
- **Subscription path** — no key needed. If the Kimi Code CLI (`kimi`) is on
  `$PATH` and logged in (device-code OAuth, `kimi login`), smedja drives it as
  the `kimi-cli` runner.

Everything a runner is expected to do works: `/switch kimi`, `/login kimi`,
`/takeover kimi`, `/tier`, `runner: "kimi"` in `loop.json` and `agents.toml`
roles, `SMEDJA_MODEL_MOONSHOT_*` / `SMEDJA_MODEL_KIMI_*` model pins, doctor
rows, cost metrics, and a kimi brand colour in the TUI.

## The part that mattered: who approves the tools?

Here is the thing we ran into. Kimi's non-interactive prompt mode
(`kimi -p … --output-format stream-json`) **auto-approves the CLI's own tool
calls** — it has to, there is no UI to ask through — and unlike Claude Code it
exposes no hook mechanism to delegate the decision. Wiring it up that way
would have meant kimi silently running shell commands and file writes with no
gate, while every other runner in smedja answers to the session's permission
policy.

The fix is ACP. Kimi Code ships an Agent Client Protocol server
(`kimi acp` — JSON-RPC 2.0 over stdio, protocol v1), and in ACP the agent
*asks the client* before running a tool: `session/request_permission` arrives
with the tool call and a set of allow/reject options, and the agent suspends
until the client answers.

smedja 0.26.0 implements that client generically. The new `AcpProvider`
spawns any ACP-capable agent CLI, streams its output into the normal turn
pipeline, and routes every permission request through the session's cowork
gate — the exact same approve/deny prompt you get for in-process tools, with
the full tool arguments shown, `auto` mode auto-approving, workspace
permission rules applying, and a missing gate failing **closed**. Kimi's CLI
path uses ACP by default; `SMEDJA_KIMI_ACP=off` reverts to the ungated
one-shot mode if you really want it.

We validated the loop live in both directions: a kimi `Write` suspended on
the gate, was approved over RPC, and the file landed; the same call denied
left no file and kimi reported, verbatim, that the user rejected the approval
request.

## Gemini, on the same rails

Since the ACP client is spec-driven — a binary, the args that select ACP
mode, and an optional model-config id — the second consumer cost almost
nothing: **Gemini** is now runner number four. `GEMINI_API_KEY` selects the
native Gemini streaming API (`google` runner); otherwise a `gemini` binary on
`$PATH` is driven via `gemini --acp` with the same gated-permission flow.
Future ACP agents (Goose, Copilot CLI, claude-agent-acp, …) are a constant
and a detection block away.

## Try it

```sh
# subscription path — no API key
kimi login
smedja        # kimi shows up third in /switch; tools ask before they run

# or the API path
export MOONSHOT_API_KEY=sk-…
```

The full ladder now reads: **claude → codex → kimi → gemini → copilot →
poolside → pool → minimax → berget → local**. See the
[configuration reference](../configuration.md) for role wiring and the
README's environment-variable table for every knob.
